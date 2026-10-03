//! Mock upstream endpoints for the media features: image generation and edits (codex, compat and
//! xAI families), xAI video creation/polling and the downloadable video file, and Codex Alpha
//! Search. Replies follow the same script mechanism as the chat endpoints: `Reply::Ok` answers
//! with a canned success, `Error` / `Raw` are rendered as scripted, `Cut` / `StreamError` shorten
//! or break the image stream.

use bytes::Bytes;
use serde_json::{Value, json};

use super::replies::{Family, RBody, Rendered, error_body};
use super::script::Reply;

/// Bytes of the downloadable video the mock serves.
pub const VIDEO_BYTES: &[u8] = b"MOCK-VIDEO-BYTES";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaOp {
    ImageGenerate,
    ImageEdit,
    /// Video creation; the payload names the endpoint (`generations`, `edits`, `extensions`).
    VideoCreate(&'static str),
    /// `GET /videos/{request_id}`.
    VideoPoll(String),
    /// `GET /files/video.mp4` and friends (the URL polled videos point at).
    VideoFile(String),
    Search,
}

/// Maps a request to a media operation. `prefix` is the first path segment (the family).
pub fn route(prefix: &str, method: &str, path: &str) -> Option<MediaOp> {
    match (prefix, method, path) {
        ("codex" | "compat" | "xai", "POST", "/images/generations") => Some(MediaOp::ImageGenerate),
        ("codex" | "compat" | "xai", "POST", "/images/edits") => Some(MediaOp::ImageEdit),
        ("codex", "POST", "/alpha/search") => Some(MediaOp::Search),
        ("xai", "POST", "/videos/generations") => Some(MediaOp::VideoCreate("generations")),
        ("xai", "POST", "/videos/edits") => Some(MediaOp::VideoCreate("edits")),
        ("xai", "POST", "/videos/extensions") => Some(MediaOp::VideoCreate("extensions")),
        ("xai", "GET", p) => {
            if let Some(id) = p.strip_prefix("/videos/") {
                return Some(MediaOp::VideoPoll(id.to_string()));
            }
            p.strip_prefix("/files/").map(|f| MediaOp::VideoFile(f.to_string()))
        }
        _ => None,
    }
}

/// Whether the request asks for a stream (JSON `stream: true` or a multipart `stream` field).
pub fn wants_stream(body: &Value) -> bool {
    body["stream"].as_bool() == Some(true) || body["fields"]["stream"] == json!(["true"])
}

fn headers(content_type: &str) -> Vec<(String, String)> {
    vec![("content-type".into(), content_type.into())]
}

fn full(status: u16, content_type: &str, body: Bytes) -> Rendered {
    Rendered { status, headers: headers(content_type), body: RBody::Full(body) }
}

fn json_reply(status: u16, body: &Value) -> Rendered {
    full(status, "application/json", Bytes::from(body.to_string()))
}

/// Tiny PNG-like payload (base64 of three bytes) that is easy to eyeball.
const B64_IMAGE: &str = "AAEC";

fn image_items(host: &str, request: &Value) -> Vec<Value> {
    let count = request["n"].as_i64().unwrap_or(1).clamp(1, 3);
    (0..count)
        .map(|i| {
            if request["response_format"].as_str() == Some("url") {
                json!({"url": format!("http://{host}/xai/files/image-{i}.png"), "revised_prompt": "mock revised"})
            } else {
                json!({"b64_json": format!("{B64_IMAGE}{i}"), "revised_prompt": "mock revised"})
            }
        })
        .collect()
}

fn sse(events: &[(&str, Value)]) -> Vec<Bytes> {
    events.iter().map(|(name, data)| Bytes::from(format!("event: {name}\ndata: {data}\n\n"))).collect()
}

fn image_stream_events() -> Vec<Bytes> {
    sse(&[
        ("image_generation.partial_image", json!({"type":"image_generation.partial_image","partial_image_index":0,"b64_json":"AAE="})),
        ("image_generation.completed", json!({"type":"image_generation.completed","b64_json":B64_IMAGE,"usage":{"input_tokens":5,"output_tokens":7,"total_tokens":12}})),
    ])
}

fn poll_body(host: &str, id: &str) -> Value {
    match id {
        "vid_failed" => json!({"request_id": id, "status": "failed", "error": {"code": "content_policy", "message": "blocked by policy"}}),
        "vid_string_error" => json!({"request_id": id, "status": "error", "error": "plain failure", "code": "E_PLAIN"}),
        "vid_code_only" => json!({"request_id": id, "code": "quota_exceeded"}),
        "vid_pending" => json!({"request_id": id, "status": "pending", "progress": 10}),
        "vid_nourl" => json!({"request_id": id, "status": "done", "progress": 100}),
        "vid_badurl" => json!({"request_id": id, "status": "done", "video": {"url": "ftp://example.com/v.mp4", "duration": 4}}),
        "vid_missing_file" => json!({"request_id": id, "status": "done", "video": {"url": format!("http://{host}/xai/files/missing.mp4"), "duration": 4}}),
        _ => json!({
            "request_id": id, "status": "done", "progress": 100, "model": "grok-imagine-video",
            "video": {"url": format!("http://{host}/xai/files/video.mp4"), "duration": 4},
            "created_at": 1_700_000_000, "prompt": "a mock video", "size": "720x1280"
        }),
    }
}

/// Renders the scripted reply for a media operation. `request` is the logged request body
/// (JSON, or the multipart form description) and `host` the mock's `Host` header.
pub fn render(op: &MediaOp, reply: &Reply, request: &Value, host: &str) -> Rendered {
    match reply {
        Reply::Error { status, headers, body } => {
            let body = body.clone().unwrap_or_else(|| error_body(Family::Compat, *status));
            let mut headers = headers.clone();
            headers.push(("content-type".into(), "application/json".into()));
            return Rendered { status: *status, headers, body: RBody::Full(Bytes::from(body.to_string())) };
        }
        Reply::Raw { status, content_type, body } => return full(*status, content_type, Bytes::from(body.clone())),
        _ => {}
    }
    match op {
        MediaOp::ImageGenerate | MediaOp::ImageEdit => {
            if wants_stream(request) {
                let mut chunks = image_stream_events();
                let mut abort = false;
                match reply {
                    Reply::Cut { after, abort: a, .. } => {
                        chunks.truncate(*after);
                        abort = *a;
                    }
                    Reply::StreamError { after, .. } => {
                        chunks.truncate(*after);
                        chunks.extend(sse(&[("error", json!({"error": {"message": "mock mid-stream error", "type": "server_error"}}))]));
                    }
                    _ => {}
                }
                return Rendered { status: 200, headers: headers("text/event-stream"), body: RBody::Chunks { chunks, abort } };
            }
            let body = json!({
                "created": 1_700_000_000,
                "data": image_items(host, request),
                "usage": {"input_tokens": 5, "output_tokens": 7, "total_tokens": 12}
            });
            json_reply(200, &body)
        }
        MediaOp::VideoCreate(kind) => json_reply(200, &json!({"request_id": format!("vid_mock_{kind}")})),
        MediaOp::VideoPoll(id) => json_reply(200, &poll_body(host, id)),
        MediaOp::VideoFile(file) => {
            if file == "video.mp4" {
                let mut out = full(200, "video/mp4", Bytes::from_static(VIDEO_BYTES));
                out.headers.push(("etag".into(), "\"mock-etag\"".into()));
                out.headers.push(("cache-control".into(), "max-age=60".into()));
                out
            } else {
                full(404, "text/plain", Bytes::from_static(b"file not found"))
            }
        }
        MediaOp::Search => json_reply(200, &json!({"id": "search_mock", "results": [{"title": "Mock result", "url": "https://example.com/mock"}]})),
    }
}

/// Describes a multipart body as `{"fields": {name: [values]}, "files": [{name, filename,
/// content_type, text}]}` with fields and files sorted, so rebuilds that reorder parts (the Go
/// reference iterates maps) compare equal. `None` when the body is not parseable.
pub fn describe_multipart(content_type: &str, body: &[u8]) -> Option<Value> {
    let boundary = content_type.split(';').filter_map(|p| p.trim().strip_prefix("boundary=")).next()?.trim_matches('"').to_string();
    let text = String::from_utf8_lossy(body).into_owned();
    let delimiter = format!("--{boundary}");
    let mut fields: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut files: Vec<Value> = vec![];
    for part in text.split(&delimiter).skip(1) {
        let part = part.strip_prefix("\r\n").unwrap_or(part);
        if part.starts_with("--") {
            break;
        }
        let (head, data) = part.split_once("\r\n\r\n")?;
        let data = data.strip_suffix("\r\n").unwrap_or(data);
        let header = |name: &str| head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string()));
        let disposition = header("content-disposition")?;
        let param = |key: &str| disposition.split(';').filter_map(|p| p.trim().strip_prefix(&format!("{key}=\""))).next().map(|v| v.trim_end_matches('"').to_string());
        let name = param("name")?;
        match param("filename") {
            Some(filename) => files.push(json!({"name": name, "filename": filename, "content_type": header("content-type"), "text": data})),
            None => fields.entry(name).or_default().push(data.to_string()),
        }
    }
    files.sort_by_key(|f| (f["name"].as_str().unwrap_or_default().to_string(), f["filename"].as_str().unwrap_or_default().to_string()));
    Some(json!({"fields": fields, "files": files}))
}

/// Edit requests rebuilt from multipart forms list their fields in Go map order (random); the
/// order carries no meaning, so top-level keys are sorted for the log.
pub fn stabilize_body(op: &MediaOp, body: &mut Value) {
    if *op != MediaOp::ImageEdit {
        return;
    }
    if let Value::Object(map) = body {
        let mut entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        map.extend(entries);
    }
}

//! `POST /v1/images/generations` and `/v1/images/edits` (Go: openai/openai_images_handlers.go).
//!
//! Three upstream shapes, chosen by model:
//! - Codex image tool models (`gpt-image-*`) are routed raw to the Codex executor (non-stream
//!   body or the executor's own SSE frames are relayed unchanged),
//! - xAI image models get an xAI request and the answer is re-shaped into the OpenAI images
//!   response (streaming re-emits it as `<prefix>.completed` events),
//! - models of an `openai-compatibility` provider flagged `image` pass through and are
//!   re-shaped the same way.
//!
//! Go also carries a Responses-API fallback (`buildImagesResponsesRequest`,
//! `collectImagesFromResponses`, `streamImagesFromResponses`, `sseFrameAccumulator`) that no
//! request can reach: every model accepted by `rejectUnsupportedImagesModel` is one of the three
//! shapes above. It is not ported.

use std::future::Future;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use cpa_config::DisableImageGenerationMode;
use cpa_core::registry::{OPENAI_IMAGE_MODEL_TYPE, lookup_model_info};
use cpa_json::{J, Kind};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, interval_at};

use super::{bad_request_message, invalid_request, ok_reply, read_request_body};
use crate::error::{ErrorMessage, build_error_response_body};
use crate::exec::{ExecArgs, Pipeline};
use crate::forward::{openai_error_reply, with_nonstream_keepalive};
use crate::headers::write_upstream_headers;
use crate::multipart::{self, Form};
use crate::reply::{Reply, set_sse_headers, streaming_response};
use crate::req::ReqInfo;
use crate::responses_error::{sanitize_error_message, stream_error_text};
use crate::sniff::detect_content_type;
use crate::state::AppState;

/// Handler type the executors dispatch image requests on (Go: `SourceFormat` `openai-image`).
pub const IMAGE_HANDLER_TYPE: &str = "openai-image";

const IMAGES_GENERATIONS_PATH: &str = "/v1/images/generations";
const IMAGES_EDITS_PATH: &str = "/v1/images/edits";
const DEFAULT_IMAGES_TOOL_MODEL: &str = "gpt-image-2";
const DEFAULT_XAI_IMAGES_MODEL: &str = "grok-imagine-image";
const XAI_IMAGES_QUALITY_MODEL: &str = "grok-imagine-image-quality";
const XAI_IMAGES_20_MODEL: &str = "grok-imagine-image-2.0";
const XAI_IMAGES_DEFAULT_ASPECT_RATIO: &str = "1:1";
const XAI_IMAGES_DEFAULT_RESOLUTION: &str = "1k";
const CODEX_IMAGE_MODELS: [&str; 5] = ["gpt-image-1.5", "gpt-image-2", "gpt-image-2.5-flare", "gpt-image-2.5-sunburst", "gpt-image-2.5"];
const XAI_IMAGE_MODELS: [&str; 3] = [DEFAULT_XAI_IMAGES_MODEL, XAI_IMAGES_QUALITY_MODEL, XAI_IMAGES_20_MODEL];

// ---------------------------------------------------------------- model classification

/// `imagesModelParts`: text before and after the last `/`.
pub(crate) fn images_model_parts(model: &str) -> (&str, &str) {
    let model = model.trim();
    match model.rfind('/') {
        Some(idx) if idx + 1 < model.len() => (model[..idx].trim(), model[idx + 1..].trim()),
        _ => ("", model),
    }
}

/// `imagesModelBase`.
fn images_model_base(model: &str) -> String {
    images_model_parts(model).1.trim().to_lowercase()
}

/// `isXAIImagesModel`.
fn is_xai_images_model(model: &str) -> bool {
    let (prefix, base) = images_model_parts(model);
    if !XAI_IMAGE_MODELS.contains(&base.trim().to_lowercase().as_str()) {
        return false;
    }
    matches!(prefix.trim().to_lowercase().as_str(), "" | "xai" | "x-ai" | "grok")
}

/// `isCodexImagesToolModel`.
fn is_codex_images_tool_model(model: &str) -> bool {
    CODEX_IMAGE_MODELS.contains(&images_model_base(model).as_str())
}

/// `isOpenAICompatImagesModel`: a registered model of the `openai-image` type.
fn is_openai_compat_images_model(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && lookup_model_info(model, None).is_some_and(|info| info.r#type == OPENAI_IMAGE_MODEL_TYPE)
}

/// `isSupportedImagesModel`.
fn is_supported_images_model(model: &str) -> bool {
    is_codex_images_tool_model(model) || is_xai_images_model(model) || is_openai_compat_images_model(model)
}

/// `rejectUnsupportedImagesModel` message.
fn unsupported_model_message(model: &str) -> String {
    format!(
        "Model {model} is not supported on {IMAGES_GENERATIONS_PATH} or {IMAGES_EDITS_PATH}. Use gpt-image-1.5, gpt-image-2, gpt-image-2.5-flare, gpt-image-2.5-sunburst, gpt-image-2.5, grok-imagine-image, grok-imagine-image-quality, grok-imagine-image-2.0, or a configured openai-compatibility image model."
    )
}

fn reject_unsupported_model(model: &str) -> Option<Reply> {
    (!is_supported_images_model(model)).then(|| bad_request_message(&unsupported_model_message(model)))
}

// ---------------------------------------------------------------- request builders

/// `normalizeImagesResponseFormat`.
fn normalize_response_format(response_format: &str) -> &'static str {
    if response_format.trim().eq_ignore_ascii_case("url") { "url" } else { "b64_json" }
}

/// `canonicalXAIImagesModel`.
fn canonical_xai_images_model(model: &str) -> &'static str {
    match images_model_base(model).as_str() {
        XAI_IMAGES_QUALITY_MODEL => XAI_IMAGES_QUALITY_MODEL,
        XAI_IMAGES_20_MODEL => XAI_IMAGES_20_MODEL,
        _ => DEFAULT_XAI_IMAGES_MODEL,
    }
}

/// `xaiImagesAspectRatio`.
pub(crate) fn xai_images_aspect_ratio<'a>(raw: &str, fallback: &'a str) -> &'a str {
    match raw.trim().to_lowercase().as_str() {
        "1:1" | "square" => "1:1",
        "16:9" | "landscape" => "16:9",
        "9:16" | "portrait" => "9:16",
        "9:20" => "9:20",
        "20:9" => "20:9",
        "4:3" => "4:3",
        "3:4" => "3:4",
        "3:2" => "3:2",
        "2:3" => "2:3",
        _ => fallback,
    }
}

/// `xaiImagesAspectRatioFromSize`.
fn xai_images_aspect_ratio_from_size<'a>(size: &str, fallback: &'a str) -> &'a str {
    match size.trim().to_lowercase().as_str() {
        "1024x1024" | "2048x2048" | "1:1" => "1:1",
        "1792x1024" | "16:9" => "16:9",
        "1024x1792" | "9:16" => "9:16",
        "9:20" => "9:20",
        "20:9" => "20:9",
        "1536x1024" | "3:2" => "3:2",
        "1024x1536" | "2:3" => "2:3",
        _ => fallback,
    }
}

/// `xaiImagesResolution`.
fn xai_images_resolution(raw: &str, size: &str, fallback: &str) -> String {
    let raw = raw.trim().to_lowercase();
    if raw == "1k" || raw == "2k" {
        return raw;
    }
    if size.trim().to_lowercase().contains("2048") {
        return "2k".to_string();
    }
    fallback.to_string()
}

/// `xaiImagesRef`.
fn xai_images_ref(image_url: &str) -> Value {
    json!({"type": "image_url", "url": image_url.trim()})
}

/// `buildXAIImagesBaseRequest`.
fn build_xai_images_base_request(
    model: &str,
    prompt: &str,
    response_format: &str,
    aspect_ratio: &str,
    resolution: &str,
    quality: &str,
    n: i64,
) -> Value {
    let mut req = json!({});
    cpa_json::set(&mut req, "model", canonical_xai_images_model(model));
    cpa_json::set(&mut req, "prompt", prompt.trim());
    cpa_json::set(&mut req, "response_format", normalize_response_format(response_format));
    if !aspect_ratio.is_empty() {
        cpa_json::set(&mut req, "aspect_ratio", aspect_ratio);
    }
    if !resolution.is_empty() {
        cpa_json::set(&mut req, "resolution", resolution);
    }
    let quality = quality.trim();
    if !quality.is_empty() {
        cpa_json::set(&mut req, "quality", quality);
    }
    if n > 0 {
        cpa_json::set(&mut req, "n", n);
    }
    req
}

/// `buildXAIImagesGenerationsRequest`.
fn build_xai_images_generations_request(raw: &Value, model: &str, response_format: &str) -> Value {
    let prompt = raw.g("prompt").str();
    let size = raw.g("size").str();
    let size = size.trim();
    let mut aspect_ratio = xai_images_aspect_ratio(&raw.g("aspect_ratio").str(), "");
    aspect_ratio = xai_images_aspect_ratio_from_size(size, aspect_ratio);
    if aspect_ratio.is_empty() {
        aspect_ratio = XAI_IMAGES_DEFAULT_ASPECT_RATIO;
    }
    let resolution = xai_images_resolution(&raw.g("resolution").str(), size, XAI_IMAGES_DEFAULT_RESOLUTION);
    let quality = raw.g("quality").str();
    let n = json_number_field(raw, "n");
    build_xai_images_base_request(model, &prompt, response_format, aspect_ratio, &resolution, &quality, n)
}

/// `n` when the field is a JSON number, else 0.
fn json_number_field(raw: &Value, key: &str) -> i64 {
    let v = raw.g(key);
    if v.exists() && v.is_number() { v.int() } else { 0 }
}

/// `buildXAIImagesEditRequest`.
#[allow(clippy::too_many_arguments)]
fn build_xai_images_edit_request(
    model: &str,
    prompt: &str,
    images: &[String],
    response_format: &str,
    aspect_ratio: &str,
    resolution: &str,
    quality: &str,
    n: i64,
) -> Value {
    let mut req = build_xai_images_base_request(model, prompt, response_format, aspect_ratio, resolution, quality, n);
    let trimmed: Vec<&str> = images.iter().map(|i| i.trim()).filter(|i| !i.is_empty()).collect();
    if trimmed.len() == 1 {
        cpa_json::set(&mut req, "image", xai_images_ref(trimmed[0]));
        return req;
    }
    for img in trimmed {
        cpa_json::set(&mut req, "images.-1", xai_images_ref(img));
    }
    req
}

/// `collectXAIImagesFromJSON`: image URLs from `image` and `images` in any accepted shape.
fn collect_xai_images_from_json(raw: &Value) -> Vec<String> {
    let mut images = Vec::new();
    let mut append = |url: String| {
        let url = url.trim().to_string();
        if !url.is_empty() {
            images.push(url);
        }
    };
    let from_node = |node: &cpa_json::Res<'_>, append: &mut dyn FnMut(String)| {
        append(node.g("image_url.url").str());
        let image_url = node.g("image_url");
        if image_url.kind() == Kind::String {
            append(image_url.str());
        }
        append(node.g("url").str());
    };
    let image = raw.g("image");
    if image.exists() {
        if image.kind() == Kind::String {
            append(image.str());
        } else if image.kind() == Kind::Json {
            from_node(&image, &mut append);
        }
    }
    let list = raw.g("images");
    if list.is_array() {
        for img in list.array() {
            if img.kind() == Kind::String {
                append(img.str());
                continue;
            }
            from_node(&img, &mut append);
        }
    }
    images
}

/// `xaiImagesEditOptionsFromJSON`: (aspect ratio, resolution, quality, n).
fn xai_images_edit_options_from_json(raw: &Value) -> (String, String, String, i64) {
    let size = raw.g("size").str();
    let size = size.trim();
    let mut aspect_ratio = xai_images_aspect_ratio(&raw.g("aspect_ratio").str(), "");
    aspect_ratio = xai_images_aspect_ratio_from_size(size, aspect_ratio);
    let resolution = xai_images_resolution(&raw.g("resolution").str(), size, "");
    let quality = raw.g("quality").str().trim().to_string();
    (aspect_ratio.to_string(), resolution, quality, json_number_field(raw, "n"))
}

/// `mimeTypeFromOutputFormat`.
fn mime_type_from_output_format(output_format: &str) -> String {
    if output_format.is_empty() {
        return "image/png".into();
    }
    if output_format.contains('/') {
        return output_format.to_string();
    }
    match output_format.trim().to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg".into(),
        "webp" => "image/webp".into(),
        _ => "image/png".into(),
    }
}

/// `multipartFileToDataURL`.
fn multipart_file_to_data_url(file: &multipart::FilePart) -> String {
    use base64::Engine as _;
    let declared = file.header("Content-Type").trim();
    let media_type = if declared.is_empty() { detect_content_type(&file.data) } else { declared };
    format!("data:{media_type};base64,{}", base64::engine::general_purpose::STANDARD.encode(&file.data))
}

/// `buildOpenAICompatImagesJSONRequest`: forces the model and the `stream` flag.
fn build_compat_images_json_request(raw: &Value, image_model: &str, stream: bool) -> Bytes {
    let mut payload = raw.clone();
    let model = image_model.trim();
    if !model.is_empty() {
        cpa_json::set(&mut payload, "model", model);
    }
    if stream {
        cpa_json::set(&mut payload, "stream", true);
    } else {
        cpa_json::delete(&mut payload, "stream");
    }
    Bytes::from(cpa_json::to_vec(&payload))
}

/// `buildOpenAICompatImagesMultipartRequest`: the form re-encoded with the routed model and the
/// `stream` flag; returns the body and its Content-Type.
fn build_compat_images_multipart_request(form: &Form, image_model: &str, stream: bool) -> (Bytes, String) {
    let mut w = multipart::Writer::new();
    w.write_field("model", image_model);
    if stream {
        w.write_field("stream", "true");
    }
    for (key, values) in &form.values {
        if key == "model" || key == "stream" {
            continue;
        }
        for value in values {
            w.write_field(key, value);
        }
    }
    for (key, files) in &form.files {
        for file in files {
            w.write_file(key, file);
        }
    }
    let content_type = w.content_type();
    (Bytes::from(w.finish()), content_type)
}

/// `parseIntField`.
fn parse_int_field(raw: &str, fallback: i64) -> i64 {
    let raw = raw.trim();
    if raw.is_empty() {
        return fallback;
    }
    raw.parse().unwrap_or(fallback)
}

/// `parseBoolField`.
fn parse_bool_field(raw: &str, fallback: bool) -> bool {
    match raw.trim().to_lowercase().as_str() {
        "" => fallback,
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => fallback,
    }
}

// ---------------------------------------------------------------- upstream response shaping

/// One image of an xAI-shaped upstream answer.
struct XaiImageResult {
    b64_json: String,
    url: String,
    revised_prompt: String,
    mime_type: String,
}

/// `extractXAIImagesResponse`: images, creation time and usage of an xAI-shaped answer.
fn extract_xai_images_response(payload: &[u8]) -> Result<(Vec<XaiImageResult>, i64, Option<Value>), String> {
    if !cpa_json::valid(payload) {
        return Err("upstream returned invalid image response JSON".into());
    }
    let root = cpa_json::parse(payload);
    let mut created_at = root.g("created").int();
    if created_at <= 0 {
        created_at = chrono::Utc::now().timestamp();
    }
    let mut results = Vec::new();
    let data = root.g("data");
    if data.is_array() {
        for item in data.array() {
            let mut result = XaiImageResult {
                b64_json: item.g("b64_json").str().trim().to_string(),
                url: item.g("url").str().trim().to_string(),
                revised_prompt: item.g("revised_prompt").str().trim().to_string(),
                mime_type: item.g("mime_type").str().trim().to_string(),
            };
            if result.mime_type.is_empty() {
                result.mime_type = mime_type_from_output_format(item.g("output_format").str().trim());
            }
            if result.b64_json.is_empty() && result.url.is_empty() {
                continue;
            }
            results.push(result);
        }
    }
    if results.is_empty() {
        return Err("upstream did not return image output".into());
    }
    let usage = root.g("usage");
    let usage = (usage.exists() && usage.is_object()).then(|| usage.value());
    Ok((results, created_at, usage))
}

/// Sets the image payload keys shared by the response body and stream events: `url` (or a data
/// URL) in `url` format, else `b64_json` with a URL fallback.
fn set_image_fields(target: &mut Value, img: &XaiImageResult, response_format: &str) {
    if response_format == "url" {
        if !img.url.is_empty() {
            cpa_json::set(target, "url", img.url.as_str());
        } else {
            let mime = mime_type_from_output_format(&img.mime_type);
            cpa_json::set(target, "url", format!("data:{mime};base64,{}", img.b64_json));
        }
    } else if !img.b64_json.is_empty() {
        cpa_json::set(target, "b64_json", img.b64_json.as_str());
    } else {
        cpa_json::set(target, "url", img.url.as_str());
    }
}

/// `buildImagesAPIResponseFromXAI`: the OpenAI images response for an xAI-shaped answer.
fn build_images_api_response_from_xai(payload: &[u8], response_format: &str) -> Result<Vec<u8>, String> {
    let (results, created_at, usage) = extract_xai_images_response(payload)?;
    let mut out = json!({"created": 0, "data": []});
    cpa_json::set(&mut out, "created", created_at);
    let response_format = normalize_response_format(response_format);
    for img in &results {
        let mut item = json!({});
        set_image_fields(&mut item, img, response_format);
        if !img.revised_prompt.is_empty() {
            cpa_json::set(&mut item, "revised_prompt", img.revised_prompt.as_str());
        }
        cpa_json::set(&mut out, "data.-1", item);
    }
    if let Some(usage) = usage {
        cpa_json::set(&mut out, "usage", usage);
    }
    Ok(cpa_json::to_vec(&out))
}

// ---------------------------------------------------------------- streaming plumbing

/// Response head decided by the stream task: a buffered reply, or the SSE headers.
enum Head {
    Reply(Reply),
    Sse(HeaderMap),
}

/// Write side of an image stream. The response head is only committed when something is
/// written, so an upstream failure before any output still gets a proper status (Go:
/// `streamStarted` tracks a keep-alive written while waiting).
struct Sink {
    head: Option<oneshot::Sender<Head>>,
    tx: mpsc::Sender<Bytes>,
    started: bool,
    keepalive: Duration,
    passthrough: bool,
}

/// `writeImagesStreamErrorEvent` frame: `event: error` with the sanitized error body.
fn error_event(err: &ErrorMessage) -> Vec<u8> {
    let safe = sanitize_error_message(err);
    let status = safe.status_or_500();
    let text = stream_error_text(Some(&safe), status);
    let mut out = b"event: error\ndata: ".to_vec();
    out.extend_from_slice(&build_error_response_body(status, &text));
    out.extend_from_slice(b"\n\n");
    out
}

impl Sink {
    /// Commits the SSE head (headers from `setImagesSSEHeaders`, then upstream headers where
    /// unset); a no-op once committed.
    fn commit(&mut self, upstream: Option<&HeaderMap>) {
        if let Some(head) = self.head.take() {
            let mut headers = HeaderMap::new();
            set_sse_headers(&mut headers);
            if let Some(upstream) = upstream {
                write_upstream_headers(&mut headers, upstream);
            }
            let _ = head.send(Head::Sse(headers));
        }
    }

    async fn write(&mut self, bytes: impl Into<Bytes>) -> bool {
        self.tx.send(bytes.into()).await.is_ok()
    }

    /// Resolves when the client went away.
    async fn closed(&mut self) {
        match &mut self.head {
            Some(head) => head.closed().await,
            None => self.tx.closed().await,
        }
    }

    /// Waits for `fut` while emitting keep-alive comments; `None` when the client left.
    async fn wait<T>(&mut self, fut: impl Future<Output = T>) -> Option<T> {
        let mut fut = std::pin::pin!(fut);
        let mut ticker = (!self.keepalive.is_zero()).then(|| interval_at(Instant::now() + self.keepalive, self.keepalive));
        loop {
            tokio::select! {
                out = &mut fut => return Some(out),
                () = self.closed() => return None,
                _ = async { ticker.as_mut().expect("guarded by the branch condition").tick().await }, if ticker.is_some() => {
                    self.commit(None);
                    self.started = true;
                    if !self.write(Bytes::from_static(b": keep-alive\n\n")).await {
                        return None;
                    }
                }
            }
        }
    }

    /// Reports an error: an `error` event when the stream already started, else a plain reply.
    async fn fail(&mut self, err: &ErrorMessage) {
        match self.head.take() {
            Some(head) => {
                let _ = head.send(Head::Reply(openai_error_reply(err, self.passthrough)));
            }
            None => {
                let _ = self.write(error_event(err)).await;
            }
        }
    }
}

/// Spawns `task` with a fresh [`Sink`] and answers with whatever head it commits first.
async fn stream_response<F, Fut>(keepalive: Duration, passthrough: bool, task: F) -> Response
where
    F: FnOnce(Sink) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (head_tx, head_rx) = oneshot::channel();
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(task(Sink { head: Some(head_tx), tx, started: false, keepalive, passthrough }));
    match head_rx.await {
        Ok(Head::Reply(reply)) => reply.into_response(),
        Ok(Head::Sse(headers)) => streaming_response(200, headers, rx),
        Err(_) => Reply::new(500).into_response(),
    }
}

/// Relays executor chunks unchanged (`handleRoutedImages` / `streamOpenAICompatImages`); a
/// stream that ends before any chunk answers 200 with `empty_trailer`.
async fn forward_raw(mut sink: Sink, mut stream: crate::exec::ExecStream, empty_trailer: &'static [u8]) {
    let Some(first) = sink.wait(stream.rx.recv()).await else { return };
    match first {
        Some(Err(err)) => sink.fail(&err).await,
        None => {
            sink.commit(Some(&stream.headers));
            sink.write(Bytes::from_static(empty_trailer)).await;
        }
        Some(Ok(chunk)) => {
            sink.commit(Some(&stream.headers));
            if !sink.write(chunk).await {
                return;
            }
            let ka = sink.keepalive;
            let mut ticker = (!ka.is_zero()).then(|| interval_at(Instant::now() + ka, ka));
            loop {
                tokio::select! {
                    () = sink.closed() => return,
                    item = stream.rx.recv() => match item {
                        Some(Ok(chunk)) => {
                            if !sink.write(chunk).await {
                                return;
                            }
                        }
                        Some(Err(err)) => {
                            sink.write(error_event(&err)).await;
                            return;
                        }
                        None => return,
                    },
                    _ = async { ticker.as_mut().expect("guarded by the branch condition").tick().await }, if ticker.is_some() => {
                        if !sink.write(Bytes::from_static(b": keep-alive\n\n")).await {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// `streamImagesWithModel`: executes without streaming, then emits one `<prefix>.completed`
/// event per image.
async fn stream_from_nonstream(mut sink: Sink, pipeline: Pipeline, model: String, image_req: Bytes, response_format: String, prefix: &'static str) {
    let exec = pipeline.execute(ExecArgs { allow_image_model: true, ..ExecArgs::handler(IMAGE_HANDLER_TYPE, &model, image_req) });
    let Some(result) = sink.wait(exec).await else { return };
    let ok = match result {
        Ok(ok) => ok,
        Err(err) => return sink.fail(&err).await,
    };
    let (results, _, usage) = match extract_xai_images_response(&ok.body) {
        Ok(v) => v,
        Err(text) => return sink.fail(&ErrorMessage::new(502, text)).await,
    };
    sink.commit(Some(&ok.headers));
    let event_name = format!("{prefix}.completed");
    let response_format = normalize_response_format(&response_format);
    for img in &results {
        let mut data = json!({"type": event_name});
        set_image_fields(&mut data, img, response_format);
        if let Some(usage) = &usage {
            cpa_json::set(&mut data, "usage", usage.clone());
        }
        let frame = format!("event: {event_name}\ndata: {}\n\n", String::from_utf8_lossy(&cpa_json::to_vec(&data)));
        if !sink.write(frame).await {
            return;
        }
    }
}

// ---------------------------------------------------------------- dispatch

/// Where a validated image request goes and how its answer is shaped.
enum Route {
    /// Codex image tool model: payload and answer are relayed raw.
    Routed { model: String, body: Bytes },
    /// xAI request; the answer is reshaped.
    Xai { body: Bytes, response_format: String, prefix: &'static str },
    /// `openai-compatibility` image model; the answer is reshaped.
    Compat { model: String, body: Bytes, response_format: String },
}

/// `handleRoutedImages` / `handleXAIImages` / `handleOpenAICompatImages`.
async fn dispatch(st: &AppState, info: &ReqInfo, route: Route, stream: bool) -> Response {
    let pipeline = Pipeline::new(st, info);
    let interval = pipeline.settings.nonstream_keepalive;
    let keepalive = pipeline.settings.stream_keepalive;
    let passthrough = pipeline.settings.passthrough_headers;
    if stream {
        return stream_response(keepalive, passthrough, move |sink| async move {
            match route {
                Route::Routed { model, body } => {
                    let args = ExecArgs { allow_image_model: true, disallow_free_auth: true, ..ExecArgs::handler(IMAGE_HANDLER_TYPE, model.trim(), body) };
                    let mut sink = sink;
                    let Some(stream) = sink.wait(pipeline.execute_stream(args)).await else { return };
                    forward_raw(sink, stream, b"\n").await;
                }
                Route::Compat { model, body, .. } => {
                    let args = ExecArgs { allow_image_model: true, ..ExecArgs::handler(IMAGE_HANDLER_TYPE, model.trim(), body) };
                    let mut sink = sink;
                    let Some(stream) = sink.wait(pipeline.execute_stream(args)).await else { return };
                    forward_raw(sink, stream, b"").await;
                }
                Route::Xai { body, response_format, prefix } => {
                    let model = cpa_json::parse(&body).g("model").str().trim().to_string();
                    stream_from_nonstream(sink, pipeline, model, body, response_format, prefix).await;
                }
            }
        })
        .await;
    }
    with_nonstream_keepalive(interval, async move {
        let (model, body, response_format, disallow_free) = match route {
            Route::Routed { model, body } => (model, body, None, true),
            Route::Xai { body, response_format, .. } => (cpa_json::parse(&body).g("model").str(), body, Some(response_format), false),
            Route::Compat { model, body, response_format } => (model, body, Some(response_format), false),
        };
        let args = ExecArgs { allow_image_model: true, disallow_free_auth: disallow_free, ..ExecArgs::handler(IMAGE_HANDLER_TYPE, model.trim(), body) };
        let ok = match pipeline.execute(args).await {
            Ok(ok) => ok,
            Err(err) => return openai_error_reply(&err, passthrough),
        };
        match response_format {
            None => {
                let body = ok.body.clone();
                ok_reply(ok, body)
            }
            Some(format) => match build_images_api_response_from_xai(&ok.body, &format) {
                Ok(out) => ok_reply(ok, Bytes::from(out)),
                Err(text) => openai_error_reply(&ErrorMessage::new(502, text), passthrough),
            },
        }
    })
    .await
}

fn disabled(st: &AppState) -> bool {
    st.cfg().disable_image_generation == DisableImageGenerationMode::All
}

/// The JSON model/prompt/response-format/stream fields shared by both JSON endpoints.
struct JsonImageRequest {
    raw: Value,
    model: String,
    prompt: String,
    response_format: String,
    stream: bool,
}

/// Validation order of `ImagesGenerations` / `imagesEditsFromJSON`: body, JSON, model, prompt.
fn read_json_request(info: &ReqInfo, body: Bytes) -> Result<JsonImageRequest, Reply> {
    let raw = read_request_body(info, body)?;
    if !cpa_json::valid(&raw) {
        return Err(bad_request_message("Invalid request: body must be valid JSON"));
    }
    let raw = cpa_json::parse(&raw);
    let mut model = raw.g("model").str().trim().to_string();
    if model.is_empty() {
        model = DEFAULT_IMAGES_TOOL_MODEL.to_string();
    }
    if let Some(reply) = reject_unsupported_model(&model) {
        return Err(reply);
    }
    let prompt = raw.g("prompt").str().trim().to_string();
    if prompt.is_empty() {
        return Err(bad_request_message("Invalid request: prompt is required"));
    }
    let mut response_format = raw.g("response_format").str().trim().to_string();
    if response_format.is_empty() {
        response_format = "b64_json".to_string();
    }
    let stream = raw.g("stream").bool();
    Ok(JsonImageRequest { raw, model, prompt, response_format, stream })
}

/// `POST /v1/images/generations`.
pub async fn generations(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    if disabled(&st) {
        return Reply::new(404).into_response();
    }
    let req = match read_json_request(&info, body) {
        Ok(req) => req,
        Err(reply) => return reply.into_response(),
    };
    let route = if is_codex_images_tool_model(&req.model) {
        Route::Routed { body: build_compat_images_json_request(&req.raw, &req.model, req.stream), model: req.model }
    } else if is_xai_images_model(&req.model) {
        let xai = build_xai_images_generations_request(&req.raw, &req.model, &req.response_format);
        Route::Xai { body: Bytes::from(cpa_json::to_vec(&xai)), response_format: req.response_format, prefix: "image_generation" }
    } else {
        Route::Compat { body: build_compat_images_json_request(&req.raw, &req.model, req.stream), model: req.model, response_format: req.response_format }
    };
    dispatch(&st, &info, route, req.stream).await
}

/// `POST /v1/images/edits`: JSON or multipart.
pub async fn edits(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    if disabled(&st) {
        return Reply::new(404).into_response();
    }
    let content_type = info.header("Content-Type").to_lowercase();
    if content_type.starts_with("application/json") {
        return edits_from_json(&st, &info, body).await;
    }
    if content_type.starts_with("multipart/form-data") || content_type.is_empty() {
        return edits_from_multipart(&st, &info, body).await;
    }
    invalid_request(format!("unsupported Content-Type {content_type:?}")).into_response()
}

/// `imagesEditsFromJSON`.
async fn edits_from_json(st: &AppState, info: &ReqInfo, body: Bytes) -> Response {
    let req = match read_json_request(info, body) {
        Ok(req) => req,
        Err(reply) => return reply.into_response(),
    };
    let route = if is_codex_images_tool_model(&req.model) {
        Route::Routed { body: build_compat_images_json_request(&req.raw, &req.model, req.stream), model: req.model }
    } else if is_xai_images_model(&req.model) {
        let images = collect_xai_images_from_json(&req.raw);
        if images.is_empty() {
            return bad_request_message("Invalid request: image is required").into_response();
        }
        let (aspect_ratio, resolution, quality, n) = xai_images_edit_options_from_json(&req.raw);
        let xai = build_xai_images_edit_request(&req.model, &req.prompt, &images, &req.response_format, &aspect_ratio, &resolution, &quality, n);
        Route::Xai { body: Bytes::from(cpa_json::to_vec(&xai)), response_format: req.response_format, prefix: "image_edit" }
    } else {
        Route::Compat { body: build_compat_images_json_request(&req.raw, &req.model, req.stream), model: req.model, response_format: req.response_format }
    };
    dispatch(st, info, route, req.stream).await
}

/// `imagesEditsFromMultipart`.
async fn edits_from_multipart(st: &AppState, info: &ReqInfo, body: Bytes) -> Response {
    let raw_content_type = info.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let form = match multipart::parse_multipart(raw_content_type, body).await {
        Ok(form) => form,
        Err(err) => return invalid_request(err).into_response(),
    };
    let mut model = form.value("model").trim().to_string();
    if model.is_empty() {
        model = DEFAULT_IMAGES_TOOL_MODEL.to_string();
    }
    if let Some(reply) = reject_unsupported_model(&model) {
        return reply.into_response();
    }
    let prompt = form.value("prompt").trim().to_string();
    if prompt.is_empty() {
        return bad_request_message("Invalid request: prompt is required").into_response();
    }
    let image_files = match form.file("image[]") {
        [] => form.file("image"),
        files => files,
    };
    if image_files.is_empty() {
        return bad_request_message("Invalid request: image is required").into_response();
    }
    let mut response_format = form.value("response_format").trim().to_string();
    if response_format.is_empty() {
        response_format = "b64_json".to_string();
    }
    let stream = parse_bool_field(form.value("stream"), false);

    // The rebuilt multipart body carries a new boundary the executors read from the headers.
    let mut info = info.clone();
    if is_xai_images_model(&model) {
        let images: Vec<String> = image_files.iter().map(multipart_file_to_data_url).collect();
        let size = form.value("size");
        let mut aspect_ratio = xai_images_aspect_ratio(form.value("aspect_ratio"), "");
        aspect_ratio = xai_images_aspect_ratio_from_size(size, aspect_ratio);
        let resolution = xai_images_resolution(form.value("resolution"), size, "");
        let quality = form.value("quality").trim().to_string();
        let n = parse_int_field(form.value("n"), 0);
        let xai = build_xai_images_edit_request(&model, &prompt, &images, &response_format, aspect_ratio, &resolution, &quality, n);
        let route = Route::Xai { body: Bytes::from(cpa_json::to_vec(&xai)), response_format, prefix: "image_edit" };
        return dispatch(st, &info, route, stream).await;
    }
    let (body, content_type) = build_compat_images_multipart_request(&form, &model, stream);
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        info.headers.insert(header::CONTENT_TYPE, value);
    }
    let route = if is_codex_images_tool_model(&model) {
        Route::Routed { model, body }
    } else {
        Route::Compat { model, body, response_format }
    };
    dispatch(st, &info, route, stream).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &Value) -> String {
        String::from_utf8(cpa_json::to_vec(v)).unwrap()
    }

    #[test]
    fn supported_models() {
        for model in ["gpt-image-2", "openai/gpt-image-1.5", " GPT-IMAGE-2.5 ", "grok-imagine-image-2.0", "xai/grok-imagine-image", "x-ai/grok-imagine-image-quality", "grok/grok-imagine-image"] {
            assert!(is_supported_images_model(model), "{model}");
        }
        for model in ["", "gpt-5", "other/grok-imagine-image", "sora-2"] {
            assert!(!is_supported_images_model(model), "{model}");
        }
    }

    #[test]
    fn xai_generations_request() {
        let raw = cpa_json::parse(br#"{"prompt":" a cat ","size":"2048x2048","quality":"high","n":2}"#);
        let req = build_xai_images_generations_request(&raw, "xai/grok-imagine-image-2.0", "URL");
        assert_eq!(
            s(&req),
            r#"{"model":"grok-imagine-image-2.0","prompt":"a cat","response_format":"url","aspect_ratio":"1:1","resolution":"2k","quality":"high","n":2}"#
        );
        let defaults = build_xai_images_generations_request(&cpa_json::parse(br#"{"prompt":"x","aspect_ratio":"9:20"}"#), "grok-imagine-image", "b64_json");
        assert_eq!(
            s(&defaults),
            r#"{"model":"grok-imagine-image","prompt":"x","response_format":"b64_json","aspect_ratio":"9:20","resolution":"1k"}"#
        );
        assert_eq!(xai_images_aspect_ratio_from_size("20:9", ""), "20:9");
    }

    #[test]
    fn xai_edit_request_single_and_multiple() {
        let one = build_xai_images_edit_request("grok-imagine-image", "p", &["u1".into()], "b64_json", "", "", "", 0);
        assert_eq!(s(&one), r#"{"model":"grok-imagine-image","prompt":"p","response_format":"b64_json","image":{"type":"image_url","url":"u1"}}"#);
        let two = build_xai_images_edit_request("grok-imagine-image", "p", &["u1".into(), " ".into(), "u2".into()], "url", "1:1", "2k", "q", 3);
        assert_eq!(
            s(&two),
            r#"{"model":"grok-imagine-image","prompt":"p","response_format":"url","aspect_ratio":"1:1","resolution":"2k","quality":"q","n":3,"images":[{"type":"image_url","url":"u1"},{"type":"image_url","url":"u2"}]}"#
        );
    }

    #[test]
    fn xai_edit_inputs_and_options_from_json() {
        let raw = cpa_json::parse(br#"{"image":{"image_url":{"url":"a"}},"images":["b",{"image_url":"c"},{"url":"d"}],"size":"1024x1536","quality":" hd ","n":2}"#);
        assert_eq!(collect_xai_images_from_json(&raw), ["a", "b", "c", "d"]);
        assert_eq!(xai_images_edit_options_from_json(&raw), ("2:3".to_string(), String::new(), "hd".to_string(), 2));
    }

    #[test]
    fn compat_json_request_forces_model_and_stream() {
        let raw = cpa_json::parse(br#"{"model":"m","prompt":"p","stream":false}"#);
        assert_eq!(&build_compat_images_json_request(&raw, "x/m", true)[..], br#"{"model":"x/m","prompt":"p","stream":true}"#);
        assert_eq!(&build_compat_images_json_request(&raw, "x/m", false)[..], br#"{"model":"x/m","prompt":"p"}"#);
    }

    #[test]
    fn multipart_rebuild_keeps_file_content_type() {
        let mut form = Form::default();
        form.values.push(("model".into(), vec!["old".into()]));
        form.values.push(("prompt".into(), vec!["p".into()]));
        form.files.push((
            "image".into(),
            vec![multipart::FilePart { filename: "a.png".into(), headers: vec![("Content-Type".into(), "image/png".into())], data: Bytes::from_static(b"IMG") }],
        ));
        let (body, content_type) = build_compat_images_multipart_request(&form, "gpt-image-2", true);
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(content_type.starts_with("multipart/form-data; boundary="));
        assert!(text.contains("name=\"model\"\r\n\r\ngpt-image-2"));
        assert!(text.contains("name=\"stream\"\r\n\r\ntrue"));
        assert!(!text.contains("old"));
        assert!(text.contains("filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nIMG"));
    }

    #[test]
    fn xai_answer_becomes_openai_images_response() {
        let payload = br#"{"created":7,"data":[{"b64_json":"AAA","revised_prompt":"rp"},{"url":"http://u","output_format":"jpg"}],"usage":{"total_tokens":3}}"#;
        let out = build_images_api_response_from_xai(payload, "b64_json").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"created":7,"data":[{"b64_json":"AAA","revised_prompt":"rp"},{"url":"http://u"}],"usage":{"total_tokens":3}}"#
        );
        let out = build_images_api_response_from_xai(br#"{"data":[{"b64_json":"AAA","mime_type":"image/webp"}],"created":1}"#, "url").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"created":1,"data":[{"url":"data:image/webp;base64,AAA"}]}"#);
        assert_eq!(build_images_api_response_from_xai(br#"{"data":[]}"#, "url").unwrap_err(), "upstream did not return image output");
        assert_eq!(build_images_api_response_from_xai(b"nope", "url").unwrap_err(), "upstream returned invalid image response JSON");
    }

    #[test]
    fn mime_types() {
        assert_eq!(mime_type_from_output_format(""), "image/png");
        assert_eq!(mime_type_from_output_format("image/gif"), "image/gif");
        assert_eq!(mime_type_from_output_format(" JPG "), "image/jpeg");
        assert_eq!(mime_type_from_output_format("bmp"), "image/png");
    }
}

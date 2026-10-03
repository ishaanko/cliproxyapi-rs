//! Video generation endpoints (Go: openai/openai_videos_handlers.go).
//!
//! Two surfaces over the xAI video backend:
//! - native `POST /v1/videos`, `/v1/videos/generations|edits|extensions` and
//!   `GET /v1/videos/:request_id` pass the xAI payload through,
//! - the OpenAI-shaped `POST /openai/v1/videos`, `GET /openai/v1/videos/:video_id` and
//!   `/content` translate to and from the xAI shape (`sora-2` models map to xAI models).
//!
//! A created or polled video is bound to the credential that served it (and the routed model)
//! for `multimedia.video-result-auth-cache-ttl`, so retrieval and download hit the same account.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use cpa_config::GoDuration;
use cpa_json::{J, Kind};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};

use super::images::images_model_parts;
use super::{bad_request_message, invalid_request, ok_reply};
use crate::error::ErrorMessage;
use crate::exec::{ExecArgs, ExecOk, Pipeline};
use crate::forward::{openai_error_reply, with_nonstream_keepalive};
use crate::multipart;
use crate::reply::Reply;
use crate::req::ReqInfo;
use crate::state::AppState;

/// Handler type the xAI executor dispatches video requests on (Go: `openai-video`).
pub const VIDEO_HANDLER_TYPE: &str = "openai-video";

const OPENAI_VIDEOS_PATH: &str = "/openai/v1/videos";
const XAI_VIDEOS_GENERATIONS_API: &str = "/v1/videos/generations";
const XAI_VIDEOS_EDITS_API: &str = "/v1/videos/edits";
const XAI_VIDEOS_EXTENSIONS_API: &str = "/v1/videos/extensions";
const DEFAULT_OPENAI_VIDEOS_MODEL: &str = "sora-2";
const DEFAULT_XAI_VIDEOS_MODEL: &str = "grok-imagine-video";
const XAI_VIDEOS_15_MODEL: &str = "grok-imagine-video-1.5";
const XAI_VIDEOS_15_PREVIEW_ALIAS: &str = "grok-imagine-video-1.5-preview";
const DEFAULT_VIDEOS_SECONDS: &str = "4";
const DEFAULT_VIDEOS_SIZE: &str = "720x1280";
const DEFAULT_VIDEOS_RESOLUTION: &str = "720p";
const MAX_XAI_VIDEO_REFERENCES: usize = 7;
const DEFAULT_VIDEO_AUTH_BINDING_TTL: Duration = Duration::from_secs(3 * 3600);

// ---------------------------------------------------------------- credential bindings

/// The credential (and routed model) that served a video id.
#[derive(Debug, Clone)]
pub struct VideoAuthBinding {
    pub auth_id: String,
    pub model: String,
    expires_at: Instant,
}

/// Video id -> serving credential, expiring entries (Go: `videoAuthBindingStore`).
#[derive(Default)]
pub struct VideoAuthBindingStore {
    entries: RwLock<HashMap<String, VideoAuthBinding>>,
}

static VIDEO_AUTH_BINDINGS: LazyLock<VideoAuthBindingStore> = LazyLock::new(VideoAuthBindingStore::default);

impl VideoAuthBindingStore {
    /// `setWithModel`: blank ids are ignored, a non-positive `ttl` means the default.
    pub fn set_with_model(&self, video_id: &str, auth_id: &str, model: &str, ttl: Duration) {
        let (video_id, auth_id) = (video_id.trim(), auth_id.trim());
        if video_id.is_empty() || auth_id.is_empty() {
            return;
        }
        let ttl = if ttl.is_zero() { DEFAULT_VIDEO_AUTH_BINDING_TTL } else { ttl };
        let now = Instant::now();
        let mut entries = self.entries.write();
        entries.retain(|_, e| now <= e.expires_at);
        entries.insert(
            video_id.to_string(),
            VideoAuthBinding { auth_id: auth_id.to_string(), model: model.trim().to_string(), expires_at: now + ttl },
        );
    }

    /// `getBinding`: expired entries are dropped on lookup.
    pub fn get_binding(&self, video_id: &str) -> Option<VideoAuthBinding> {
        let video_id = video_id.trim();
        if video_id.is_empty() {
            return None;
        }
        let now = Instant::now();
        let entry = self.entries.read().get(video_id).cloned()?;
        if now > entry.expires_at {
            let mut entries = self.entries.write();
            if entries.get(video_id).is_some_and(|e| now > e.expires_at) {
                entries.remove(video_id);
            }
            return None;
        }
        Some(entry)
    }

    /// `get`: just the credential id.
    pub fn get(&self, video_id: &str) -> Option<String> {
        self.get_binding(video_id).map(|b| b.auth_id)
    }
}

/// `videoAuthBindingTTL`: `video-result-auth-cache-ttl` when it parses to a positive duration.
fn video_auth_binding_ttl(raw: &str) -> Duration {
    let raw = raw.trim();
    if !raw.is_empty()
        && let Ok(ttl) = GoDuration::parse(raw)
        && ttl.0 > 0
    {
        return ttl.to_std();
    }
    DEFAULT_VIDEO_AUTH_BINDING_TTL
}

/// `videoIDFromPayload`: `request_id`, else `id`.
fn video_id_from_payload(payload: &Value) -> String {
    let id = payload.g("request_id").str().trim().to_string();
    if id.is_empty() { payload.g("id").str().trim().to_string() } else { id }
}

// ---------------------------------------------------------------- model classification

/// `videosModelBase`.
fn videos_model_base(model: &str) -> String {
    images_model_parts(model).1.trim().to_lowercase()
}

/// `isXAIVideosModel`.
fn is_xai_videos_model(model: &str) -> bool {
    let (prefix, base) = images_model_parts(model);
    let base = base.trim().to_lowercase();
    if base != DEFAULT_XAI_VIDEOS_MODEL && base != XAI_VIDEOS_15_MODEL && base != XAI_VIDEOS_15_PREVIEW_ALIAS {
        return false;
    }
    matches!(prefix.trim().to_lowercase().as_str(), "" | "xai" | "x-ai" | "grok")
}

/// `isSoraVideosModel`.
fn is_sora_videos_model(model: &str) -> bool {
    let base = images_model_parts(model).1.trim().to_lowercase();
    base == DEFAULT_OPENAI_VIDEOS_MODEL || base.starts_with(&format!("{DEFAULT_OPENAI_VIDEOS_MODEL}-"))
}

/// `isSupportedVideosModel`.
fn is_supported_videos_model(model: &str) -> bool {
    is_xai_videos_model(model) || is_sora_videos_model(model)
}

/// `canonicalXAIVideosModel`: the model sent upstream and reported to the client.
fn canonical_xai_videos_model(model: &str) -> &'static str {
    if is_sora_videos_model(model) {
        return DEFAULT_XAI_VIDEOS_MODEL;
    }
    match videos_model_base(model).as_str() {
        XAI_VIDEOS_15_MODEL | XAI_VIDEOS_15_PREVIEW_ALIAS => XAI_VIDEOS_15_MODEL,
        _ => DEFAULT_XAI_VIDEOS_MODEL,
    }
}

/// `routingXAIVideosModel`: the model used to pick a credential (keeps the preview alias).
fn routing_xai_videos_model(model: &str) -> &'static str {
    if is_sora_videos_model(model) {
        return DEFAULT_XAI_VIDEOS_MODEL;
    }
    match videos_model_base(model).as_str() {
        XAI_VIDEOS_15_MODEL => XAI_VIDEOS_15_MODEL,
        XAI_VIDEOS_15_PREVIEW_ALIAS => XAI_VIDEOS_15_PREVIEW_ALIAS,
        _ => DEFAULT_XAI_VIDEOS_MODEL,
    }
}

// ---------------------------------------------------------------- request builders

/// What the OpenAI-shaped create response reports about the request.
struct XaiVideoCreateMetadata {
    model: String,
    routing_model: String,
    prompt: String,
    seconds: String,
    size: String,
    created_at: i64,
}

/// `normalizeXAIVideosSeconds`: (seconds text, clamped duration).
fn normalize_xai_videos_seconds(raw: &str) -> Result<(String, i64), String> {
    let seconds = raw.trim();
    let seconds = if seconds.is_empty() { DEFAULT_VIDEOS_SECONDS } else { seconds };
    let duration: i64 = seconds.parse().map_err(|_| "seconds must be an integer".to_string())?;
    let duration = duration.clamp(1, 15);
    Ok((duration.to_string(), duration))
}

/// `xaiVideosSizeOptions`: (size, aspect ratio, resolution).
fn xai_videos_size_options(raw: &str) -> Result<(String, &'static str, &'static str), String> {
    let size = raw.trim();
    let size = if size.is_empty() { DEFAULT_VIDEOS_SIZE } else { size };
    match size {
        "720x1280" | "1024x1792" => Ok((size.to_string(), "9:16", DEFAULT_VIDEOS_RESOLUTION)),
        "1280x720" | "1792x1024" => Ok((size.to_string(), "16:9", DEFAULT_VIDEOS_RESOLUTION)),
        _ => Err("size must be one of 720x1280, 1280x720, 1024x1792, or 1792x1024".to_string()),
    }
}

/// `xaiVideosAspectRatio` (empty fallback).
fn xai_videos_aspect_ratio(raw: &str) -> &'static str {
    match raw.trim().to_lowercase().as_str() {
        "1:1" | "square" => "1:1",
        "16:9" | "landscape" => "16:9",
        "9:16" | "portrait" => "9:16",
        "4:3" => "4:3",
        "3:4" => "3:4",
        "3:2" => "3:2",
        "2:3" => "2:3",
        _ => "",
    }
}

/// `xaiVideosResolution` (empty fallback).
fn xai_videos_resolution(raw: &str) -> &'static str {
    match raw.trim().to_lowercase().as_str() {
        "480p" => "480p",
        "720p" => "720p",
        _ => "",
    }
}

/// `xaiVideosInputImageURL`: `input_reference`, `image`, then `image_url`.
fn xai_videos_input_image_url(raw: &Value) -> Result<String, String> {
    let input_ref = raw.g("input_reference");
    if input_ref.exists() {
        let image_url = input_ref.g("image_url").str().trim().to_string();
        let file_id = input_ref.g("file_id").str().trim().to_string();
        if !image_url.is_empty() && !file_id.is_empty() {
            return Err("input_reference must provide exactly one of image_url or file_id".into());
        }
        if !file_id.is_empty() {
            return Err("input_reference.file_id is not supported for xAI video generation; use input_reference.image_url".into());
        }
        if !image_url.is_empty() {
            return Ok(image_url);
        }
    }
    let image = raw.g("image");
    if image.exists() {
        if image.kind() == Kind::String {
            return Ok(image.str().trim().to_string());
        }
        for path in ["url", "image_url.url"] {
            let value = image.g(path).str().trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    Ok(raw.g("image_url").str().trim().to_string())
}

/// `collectXAIVideoReferenceImages`: `reference_images` then `reference_image_urls`.
fn collect_xai_video_reference_images(raw: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["reference_images", "reference_image_urls"] {
        let list = raw.g(key);
        if !list.is_array() {
            continue;
        }
        for item in list.array() {
            if item.kind() == Kind::String {
                push_trimmed(&mut out, item.str());
                continue;
            }
            let url = item.g("url").str();
            if !url.is_empty() {
                push_trimmed(&mut out, url);
                continue;
            }
            let url = item.g("image_url.url").str();
            if !url.is_empty() {
                push_trimmed(&mut out, url);
            }
        }
    }
    out
}

fn push_trimmed(out: &mut Vec<String>, value: String) {
    let value = value.trim();
    if !value.is_empty() {
        out.push(value.to_string());
    }
}

/// `buildXAIVideosCreateRequest`: the xAI request and the facts for the OpenAI response.
fn build_xai_videos_create_request(raw: &Value, model: &str) -> Result<(Value, XaiVideoCreateMetadata), String> {
    let prompt = raw.g("prompt").str().trim().to_string();
    if prompt.is_empty() {
        return Err("prompt is required".into());
    }
    let (seconds, duration) = normalize_xai_videos_seconds(&raw.g("seconds").str())?;
    let (size, mut aspect_ratio, mut resolution) = xai_videos_size_options(&raw.g("size").str())?;
    let value = xai_videos_aspect_ratio(&raw.g("aspect_ratio").str());
    if !value.is_empty() {
        aspect_ratio = value;
    }
    let value = xai_videos_resolution(&raw.g("resolution").str());
    if !value.is_empty() {
        resolution = value;
    }
    let image_url = xai_videos_input_image_url(raw)?;
    let reference_images = collect_xai_video_reference_images(raw);
    if reference_images.len() > MAX_XAI_VIDEO_REFERENCES {
        return Err(format!("reference_images supports at most {MAX_XAI_VIDEO_REFERENCES} images on xAI"));
    }
    if !image_url.is_empty() && !reference_images.is_empty() {
        return Err("image and reference_images cannot be combined on xAI".into());
    }

    let mut req = json!({});
    cpa_json::set(&mut req, "model", canonical_xai_videos_model(model));
    cpa_json::set(&mut req, "prompt", prompt.as_str());
    cpa_json::set(&mut req, "duration", duration);
    cpa_json::set(&mut req, "aspect_ratio", aspect_ratio);
    cpa_json::set(&mut req, "resolution", resolution);
    if !image_url.is_empty() {
        cpa_json::set(&mut req, "image.url", image_url);
    }
    for image in reference_images {
        cpa_json::set(&mut req, "reference_images.-1.url", image);
    }
    let meta = XaiVideoCreateMetadata {
        model: canonical_xai_videos_model(model).to_string(),
        routing_model: routing_xai_videos_model(model).to_string(),
        prompt,
        seconds,
        size,
        created_at: chrono::Utc::now().timestamp(),
    };
    Ok((req, meta))
}

/// `openAIVideoStatus`: the OpenAI status for an xAI status, empty when unknown.
fn openai_video_status(status: &str) -> &'static str {
    match status.trim().to_lowercase().as_str() {
        "queued" | "pending" => "queued",
        "in_progress" | "processing" | "running" => "in_progress",
        "completed" | "done" | "succeeded" | "success" => "completed",
        "failed" | "error" | "expired" | "cancelled" | "canceled" => "failed",
        _ => "",
    }
}

/// `buildVideosCreateAPIResponseFromXAI`.
fn build_videos_create_api_response_from_xai(payload: &Value, meta: &XaiVideoCreateMetadata) -> Result<Vec<u8>, String> {
    let request_id = video_id_from_payload(payload);
    if request_id.is_empty() {
        return Err("xAI video response did not include request_id".into());
    }
    let mut out = json!({"object": "video", "progress": 0, "status": "queued"});
    cpa_json::set(&mut out, "id", request_id);
    cpa_json::set(&mut out, "model", meta.model.as_str());
    cpa_json::set(&mut out, "prompt", meta.prompt.as_str());
    cpa_json::set(&mut out, "seconds", meta.seconds.as_str());
    cpa_json::set(&mut out, "size", meta.size.as_str());
    cpa_json::set(&mut out, "created_at", meta.created_at);
    let status = openai_video_status(&payload.g("status").str());
    if !status.is_empty() {
        cpa_json::set(&mut out, "status", status);
    }
    let progress = payload.g("progress");
    if progress.exists() {
        cpa_json::set(&mut out, "progress", progress.value());
    }
    Ok(cpa_json::to_vec(&out))
}

/// `buildVideosFailedAPIResponse`: a failed video resource with a fresh id.
fn build_videos_failed_api_response(model: &str, code: &str, message: &str) -> Vec<u8> {
    let model = model.trim();
    let model = if model.is_empty() { DEFAULT_XAI_VIDEOS_MODEL } else { model };
    let code = code.trim();
    let code = if code.is_empty() { "invalid_request_error" } else { code };
    let message = message.trim();
    let message = if message.is_empty() { "Video generation failed" } else { message };
    let mut out = json!({"object": "video", "status": "failed", "progress": 0});
    cpa_json::set(&mut out, "id", format!("video_{}", uuid::Uuid::new_v4().simple()));
    cpa_json::set(&mut out, "model", model);
    cpa_json::set(&mut out, "error.code", code);
    cpa_json::set(&mut out, "error.message", message);
    cpa_json::to_vec(&out)
}

/// `writeVideosFailedError`: `application/json` failed-video body with `status` (400 default).
fn videos_failed_error(status: u16, model: &str, code: &str, message: &str) -> Reply {
    let status = if status == 0 { 400 } else { status };
    Reply::new(status).content_type("application/json").with_body(build_videos_failed_api_response(model, code, message))
}

/// `buildVideosRetrieveAPIResponseFromXAI`.
fn build_videos_retrieve_api_response_from_xai(video_id: &str, payload: &Value, fallback_model: &str) -> Vec<u8> {
    let mut out = json!({"object": "video"});
    cpa_json::set(&mut out, "id", video_id);
    let mut model = payload.g("model").str().trim().to_string();
    if model.is_empty() {
        model = canonical_xai_videos_model(fallback_model).to_string();
    }
    cpa_json::set(&mut out, "model", model);
    for field in ["created_at", "completed_at", "expires_at", "prompt", "remixed_from_video_id", "size"] {
        let value = payload.g(field);
        if value.exists() {
            cpa_json::set(&mut out, field, value.value());
        }
    }
    let status = openai_video_status(&payload.g("status").str());
    if !status.is_empty() {
        cpa_json::set(&mut out, "status", status);
    }
    let progress = payload.g("progress");
    if progress.exists() {
        cpa_json::set(&mut out, "progress", progress.value());
    }
    let seconds = payload.g("seconds");
    let duration = payload.g("video.duration");
    if seconds.exists() {
        cpa_json::set(&mut out, "seconds", seconds.value());
    } else if duration.exists() {
        cpa_json::set(&mut out, "seconds", duration.str());
    }
    let video_url = payload.g("video.url").str().trim().to_string();
    if !video_url.is_empty() {
        cpa_json::set(&mut out, "video_url", video_url);
    }
    set_openai_video_error_from_xai(&mut out, payload);
    cpa_json::to_vec(&out)
}

/// `setOpenAIVideoErrorFromXAI`: maps an xAI `error` / `code` onto the failed OpenAI resource.
fn set_openai_video_error_from_xai(out: &mut Value, payload: &Value) {
    let err_payload = payload.g("error");
    if err_payload.exists() {
        mark_openai_video_failed(out);
        if err_payload.kind() == Kind::Json {
            let message = err_payload.g("message").str().trim().to_string();
            if !message.is_empty() {
                let mut code = payload.g("code").str().trim().to_string();
                if code.is_empty() {
                    code = err_payload.g("code").str().trim().to_string();
                }
                if code.is_empty() {
                    code = "video_generation_failed".into();
                }
                cpa_json::set(out, "error.code", code);
                cpa_json::set(out, "error.message", message);
            }
            return;
        }
        let message = err_payload.str().trim().to_string();
        if !message.is_empty() {
            let mut code = payload.g("code").str().trim().to_string();
            if code.is_empty() {
                code = "video_generation_failed".into();
            }
            cpa_json::set(out, "error.code", code);
            cpa_json::set(out, "error.message", message);
        }
        return;
    }
    let code = payload.g("code").str().trim().to_string();
    if !code.is_empty() {
        mark_openai_video_failed(out);
        cpa_json::set(out, "error.code", code.as_str());
        cpa_json::set(out, "error.message", code);
    }
}

/// `markOpenAIVideoFailed`: status `failed` and progress 0 unless already present.
fn mark_openai_video_failed(out: &mut Value) {
    if !out.g("status").exists() {
        cpa_json::set(out, "status", "failed");
    }
    if !out.g("progress").exists() {
        cpa_json::set(out, "progress", 0);
    }
}

/// `xaiVideoContentURLFromPayload`: the downloadable `video.url` (http or https only).
fn xai_video_content_url_from_payload(payload: &Value) -> Result<String, String> {
    let raw_url = payload.g("video.url").str().trim().to_string();
    if raw_url.is_empty() {
        return Err("xAI video response did not include video.url".into());
    }
    match url::Url::parse(&raw_url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some_and(|h| !h.is_empty()) => Ok(raw_url),
        _ => Err("xAI video response included invalid video.url".into()),
    }
}

// ---------------------------------------------------------------- request readers

/// `c.ContentType()`: the media type of the Content-Type header, lower-cased.
fn media_type(info: &ReqInfo) -> String {
    let value = info.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    value.split([';', ' ']).next().unwrap_or("").trim().to_lowercase()
}

/// `readVideosCreateRequest` / `readXAIVideosNativeRequest` JSON branch: decoded and validated
/// body, the error text as the Go error would render.
fn read_json_body(info: &ReqInfo, body: Bytes) -> Result<Value, String> {
    let raw = crate::body::decode_request_body(&info.headers, body)?;
    if !cpa_json::valid(&raw) {
        return Err("body must be valid JSON".into());
    }
    Ok(cpa_json::parse(&raw))
}

/// `videosCreateRequestFromForm`: the OpenAI form fields as the JSON request.
async fn videos_create_request_from_form(info: &ReqInfo, body: Bytes, multipart_form: bool) -> Value {
    // gin ignores form parse failures: the values then simply read as empty.
    let form = if multipart_form {
        let ct = info.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
        multipart::parse_multipart(ct, body).await.unwrap_or_default()
    } else {
        multipart::parse_urlencoded(&body)
    };
    let mut raw = json!({});
    for field in ["model", "prompt", "seconds", "size", "aspect_ratio", "resolution"] {
        let value = form.value(field).trim();
        if !value.is_empty() {
            cpa_json::set(&mut raw, field, value);
        }
    }
    let first = |keys: &[&str]| keys.iter().map(|k| form.value(k)).find(|v| !v.trim().is_empty()).unwrap_or("").trim().to_string();
    let value = first(&["input_reference[image_url]", "input_reference.image_url", "image_url"]);
    if !value.is_empty() {
        cpa_json::set(&mut raw, "input_reference.image_url", value);
    }
    let value = first(&["input_reference[file_id]", "input_reference.file_id", "file_id"]);
    if !value.is_empty() {
        cpa_json::set(&mut raw, "input_reference.file_id", value);
    }
    let refs = form.value("reference_image_urls").trim();
    if !refs.is_empty() {
        for r in refs.split(',').map(str::trim).filter(|r| !r.is_empty()) {
            cpa_json::set(&mut raw, "reference_image_urls.-1", r);
        }
    }
    raw
}

// ---------------------------------------------------------------- execution

/// A video execution: the pipeline, the credential pinning and the picked credential.
struct VideoExec {
    pipeline: Pipeline,
    selected: Arc<Mutex<String>>,
    ttl: Duration,
}

impl VideoExec {
    fn new(st: &AppState, info: &ReqInfo) -> Self {
        let pipeline = Pipeline::new(st, info);
        let ttl = video_auth_binding_ttl(&pipeline.cfg.video_result_auth_cache_ttl);
        VideoExec { pipeline, selected: Arc::new(Mutex::new(String::new())), ttl }
    }

    /// `ExecuteWithAuthManager(ctx, "openai-video", model, payload, "")`, optionally pinned to
    /// the credential bound to `pin_video_id`.
    async fn run(&self, model: &str, payload: Bytes, pin_video_id: Option<&str>) -> Result<ExecOk, ErrorMessage> {
        let pinned = pin_video_id.and_then(|id| VIDEO_AUTH_BINDINGS.get(id));
        let selected = self.selected.clone();
        let mut args = ExecArgs::handler(VIDEO_HANDLER_TYPE, model, payload);
        args.pinned_auth_id = pinned.as_deref();
        args.on_selected_auth = Some(Arc::new(move |auth_id: &str| *selected.lock() = auth_id.to_string()));
        self.pipeline.execute(args).await
    }

    /// `bindVideoAuthID`.
    fn bind(&self, video_id: &str, model: &str) {
        let auth_id = self.selected.lock().clone();
        VIDEO_AUTH_BINDINGS.set_with_model(video_id, &auth_id, routing_xai_videos_model(model), self.ttl);
    }

    /// `bindVideoAuthIDAndModelFromPayload`.
    fn bind_from_payload(&self, payload: &Value, model: &str) {
        let video_id = video_id_from_payload(payload);
        if !video_id.is_empty() {
            self.bind(&video_id, model);
        }
    }
}

/// `modelWithVideoAuthBinding`: the model recorded with the binding, else `fallback`.
fn model_with_video_auth_binding(video_id: &str, fallback: &str) -> String {
    match VIDEO_AUTH_BINDINGS.get_binding(video_id) {
        Some(b) if !b.model.trim().is_empty() => b.model.trim().to_string(),
        _ => fallback.to_string(),
    }
}

/// Non-stream execution wrapper shared by the video handlers (keep-alive newlines, JSON reply).
async fn run_reply<F>(st: &AppState, info: &ReqInfo, f: impl FnOnce(VideoExec, bool) -> F) -> Response
where
    F: std::future::Future<Output = Reply> + Send + 'static,
{
    let exec = VideoExec::new(st, info);
    let interval = exec.pipeline.settings.nonstream_keepalive;
    let passthrough = exec.pipeline.settings.passthrough_headers;
    with_nonstream_keepalive(interval, f(exec, passthrough)).await
}

fn missing_id_reply(what: &str) -> Reply {
    bad_request_message(&format!("Invalid request: {what} is required"))
}

// ---------------------------------------------------------------- handlers

/// `POST /openai/v1/videos` (`VideosCreate`): OpenAI-shaped create.
pub async fn videos_create(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    let content_type = media_type(&info);
    let raw = if matches!(content_type.as_str(), "multipart/form-data" | "application/x-www-form-urlencoded") {
        Ok(videos_create_request_from_form(&info, body, content_type == "multipart/form-data").await)
    } else {
        read_json_body(&info, body)
    };
    let raw = match raw {
        Ok(raw) => raw,
        Err(err) => {
            return videos_failed_error(400, DEFAULT_XAI_VIDEOS_MODEL, "invalid_request_error", &format!("Invalid request: {err}")).into_response();
        }
    };
    let mut video_model = raw.g("model").str().trim().to_string();
    if video_model.is_empty() {
        video_model = DEFAULT_XAI_VIDEOS_MODEL.to_string();
    }
    if !is_supported_videos_model(&video_model) {
        let path = info.path.trim();
        let path = if path.is_empty() { OPENAI_VIDEOS_PATH } else { path };
        let message = format!("Model {video_model} is not supported on {path}. Use {DEFAULT_OPENAI_VIDEOS_MODEL}.");
        return videos_failed_error(400, &video_model, "invalid_request_error", &message).into_response();
    }
    let (xai_req, meta) = match build_xai_videos_create_request(&raw, &video_model) {
        Ok(v) => v,
        Err(err) => {
            return videos_failed_error(400, canonical_xai_videos_model(&video_model), "invalid_request_error", &format!("Invalid request: {err}"))
                .into_response();
        }
    };
    run_reply(&st, &info, |exec, passthrough| async move {
        let routing_model = if meta.routing_model.trim().is_empty() { routing_xai_videos_model(&meta.model).to_string() } else { meta.routing_model.trim().to_string() };
        let payload = Bytes::from(cpa_json::to_vec(&xai_req));
        let ok = match exec.run(&routing_model, payload, None).await {
            Ok(ok) => ok,
            Err(err) => return openai_error_reply(&err, passthrough),
        };
        let upstream = cpa_json::parse(&ok.body);
        match build_videos_create_api_response_from_xai(&upstream, &meta) {
            Err(text) => openai_error_reply(&ErrorMessage::new(502, text), passthrough),
            Ok(out) => {
                exec.bind_from_payload(&cpa_json::parse(&out), &routing_model);
                ok_reply(ok, Bytes::from(out))
            }
        }
    })
    .await
}

/// `POST /v1/videos`, `/v1/videos/generations|edits|extensions`: native xAI passthrough.
pub async fn xai_native_post(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    let mut raw = match read_json_body(&info, body) {
        Ok(raw) => raw,
        Err(err) => return invalid_request(err).into_response(),
    };
    let mut video_model = raw.g("model").str().trim().to_string();
    if video_model.is_empty() {
        video_model = DEFAULT_XAI_VIDEOS_MODEL.to_string();
    }
    if !is_xai_videos_model(&video_model) {
        return bad_request_message(&format!(
            "Model {video_model} is not supported on {XAI_VIDEOS_GENERATIONS_API}, {XAI_VIDEOS_EDITS_API}, or {XAI_VIDEOS_EXTENSIONS_API}. Use {DEFAULT_XAI_VIDEOS_MODEL}."
        ))
        .into_response();
    }
    let routing_model = routing_xai_videos_model(&video_model);
    cpa_json::set(&mut raw, "model", canonical_xai_videos_model(&video_model));
    collect_native(&st, &info, raw, routing_model.to_string(), true).await
}

/// `GET /v1/videos/:request_id` (`XAIVideosRetrieve`).
pub async fn xai_retrieve(State(st): State<AppState>, info: ReqInfo, Path(request_id): Path<String>) -> Response {
    let request_id = request_id.trim();
    if request_id.is_empty() {
        return missing_id_reply("request_id").into_response();
    }
    collect_native(&st, &info, json!({"request_id": request_id}), DEFAULT_XAI_VIDEOS_MODEL.to_string(), false).await
}

/// `collectXAIVideosNative`: execute and relay the xAI answer; creations bind their credential,
/// polls reuse the bound one.
async fn collect_native(st: &AppState, info: &ReqInfo, raw: Value, model: String, bind_created: bool) -> Response {
    run_reply(st, info, |exec, passthrough| async move {
        let video_id = video_id_from_payload(&raw);
        let mut execution_model = model.clone();
        if !bind_created {
            execution_model = model_with_video_auth_binding(&video_id, &model);
        }
        let payload = Bytes::from(cpa_json::to_vec(&raw));
        let ok = match exec.run(&execution_model, payload, (!bind_created).then_some(video_id.as_str())).await {
            Ok(ok) => ok,
            Err(err) => return openai_error_reply(&err, passthrough),
        };
        if bind_created {
            exec.bind_from_payload(&cpa_json::parse(&ok.body), &execution_model);
        } else {
            exec.bind(&video_id, &execution_model);
        }
        let body = ok.body.clone();
        ok_reply(ok, body)
    })
    .await
}

/// `GET /openai/v1/videos/:video_id` (`VideosRetrieve`): OpenAI-shaped status.
pub async fn videos_retrieve(State(st): State<AppState>, info: ReqInfo, Path(video_id): Path<String>) -> Response {
    let video_id = video_id.trim().to_string();
    if video_id.is_empty() {
        return missing_id_reply("video_id").into_response();
    }
    run_reply(&st, &info, |exec, passthrough| async move {
        let execution_model = model_with_video_auth_binding(&video_id, DEFAULT_XAI_VIDEOS_MODEL);
        let payload = Bytes::from(cpa_json::to_vec(&json!({"request_id": video_id})));
        let ok = match exec.run(&execution_model, payload, Some(&video_id)).await {
            Ok(ok) => ok,
            Err(err) => return openai_error_reply(&err, passthrough),
        };
        let out = build_videos_retrieve_api_response_from_xai(&video_id, &cpa_json::parse(&ok.body), DEFAULT_OPENAI_VIDEOS_MODEL);
        exec.bind(&video_id, &execution_model);
        ok_reply(ok, Bytes::from(out))
    })
    .await
}

/// `GET /openai/v1/videos/:video_id/content` (`VideosContent`): retrieves the finished video's
/// URL, then downloads it through the credential's proxy and relays status, content headers
/// and body. With non-stream keep-alive enabled the body is buffered so the keep-alive
/// newlines can precede it; otherwise it is streamed.
pub async fn videos_content(State(st): State<AppState>, info: ReqInfo, Path(video_id): Path<String>) -> Response {
    let video_id = video_id.trim().to_string();
    if video_id.is_empty() {
        return missing_id_reply("video_id").into_response();
    }
    let variant = info.query_first("variant").unwrap_or("").trim().to_string();
    let variant = if variant.is_empty() { "video".to_string() } else { variant };
    if variant != "video" {
        return invalid_request(format!("variant {variant:?} is not available for xAI video downloads")).into_response();
    }
    let exec = VideoExec::new(&st, &info);
    let interval = exec.pipeline.settings.nonstream_keepalive;
    let passthrough = exec.pipeline.settings.passthrough_headers;
    if interval.is_zero() {
        return match fetch_content(&st, exec, &video_id, passthrough).await {
            Err(reply) => reply.into_response(),
            Ok(resp) => stream_content(resp),
        };
    }
    with_nonstream_keepalive(interval, async move {
        match fetch_content(&st, exec, &video_id, passthrough).await {
            Err(reply) => reply,
            Ok(resp) => buffer_content(resp, passthrough).await,
        }
    })
    .await
}

/// Retrieves the video and opens the download: the successful upstream response, or the
/// error reply to send.
async fn fetch_content(st: &AppState, exec: VideoExec, video_id: &str, passthrough: bool) -> Result<reqwest::Response, Reply> {
    let execution_model = model_with_video_auth_binding(video_id, DEFAULT_XAI_VIDEOS_MODEL);
    let payload = Bytes::from(cpa_json::to_vec(&json!({"request_id": video_id})));
    let ok = exec.run(&execution_model, payload, Some(video_id)).await.map_err(|err| openai_error_reply(&err, passthrough))?;
    exec.bind(video_id, &execution_model);
    let content_url = xai_video_content_url_from_payload(&cpa_json::parse(&ok.body))
        .map_err(|text| openai_error_reply(&ErrorMessage::new(502, text), passthrough))?;
    download(st, video_id, &content_url, passthrough).await
}

/// `writeVideoContentFromURL` request half: GET through the bound credential's proxy.
async fn download(st: &AppState, video_id: &str, content_url: &str, passthrough: bool) -> Result<reqwest::Response, Reply> {
    let cfg = st.cfg();
    let auth = VIDEO_AUTH_BINDINGS.get(video_id).and_then(|id| st.manager.get(&id));
    let client = cpa_executors::helps::proxy::new_proxy_aware_http_client("", Some(&cfg), auth.as_ref(), None);
    // Go's default transport identifies itself as Go-http-client.
    let resp = client.get(content_url).header(header::USER_AGENT, "Go-http-client/1.1").send().await.map_err(|err| {
        let status = if err.is_timeout() { 504 } else { 502 };
        openai_error_reply(&ErrorMessage::new(status, err.to_string()), passthrough)
    })?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.bytes().await.unwrap_or_default();
    let text = String::from_utf8_lossy(&body).trim().to_string();
    let message = if text.is_empty() {
        format!("video content download failed: {} {}", status.as_u16(), status.canonical_reason().unwrap_or(""))
    } else {
        format!("video content download failed: {text}")
    };
    Err(openai_error_reply(&ErrorMessage::new(status.as_u16(), message), passthrough))
}

/// `copyVideoContentHeaders` plus the `application/octet-stream` default.
fn content_headers(resp: &reqwest::Response) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in ["Content-Type", "Content-Length", "Content-Disposition", "Cache-Control", "ETag", "Last-Modified"] {
        if let Some(value) = resp.headers().get(name)
            && !value.is_empty()
            && let Ok(name) = HeaderName::from_bytes(name.as_bytes())
        {
            headers.insert(name, value.clone());
        }
    }
    if !headers.contains_key(header::CONTENT_TYPE) {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    }
    headers
}

fn stream_content(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
    let headers = content_headers(&resp);
    let mut response = Response::new(Body::from_stream(resp.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

async fn buffer_content(resp: reqwest::Response, passthrough: bool) -> Reply {
    let mut reply = Reply::new(resp.status().as_u16());
    reply.headers = content_headers(&resp);
    match resp.bytes().await {
        Ok(body) => reply.with_body(body),
        Err(err) => openai_error_reply(&ErrorMessage::new(502, err.to_string()), passthrough),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(raw: &str, model: &str) -> Result<(String, XaiVideoCreateMetadata), String> {
        build_xai_videos_create_request(&cpa_json::parse(raw.as_bytes()), model).map(|(v, m)| (String::from_utf8(cpa_json::to_vec(&v)).unwrap(), m))
    }

    #[test]
    fn model_validation() {
        for model in [
            "grok-imagine-video",
            "xai/grok-imagine-video",
            "x-ai/grok-imagine-video-1.5",
            "grok/grok-imagine-video-1.5-preview",
            "sora-2",
            "sora-2-pro",
        ] {
            assert!(is_supported_videos_model(model), "{model}");
        }
        assert!(!is_xai_videos_model("sora-2"));
        for model in ["codex/grok-imagine-video", "codex/grok-imagine-video-1.5", "codex/grok-imagine-video-1.5-preview", "gpt-4"] {
            assert!(!is_supported_videos_model(model), "{model}");
        }
    }

    #[test]
    fn canonical_and_routing_models() {
        assert_eq!(canonical_xai_videos_model("sora-2"), "grok-imagine-video");
        assert_eq!(canonical_xai_videos_model("xai/grok-imagine-video-1.5-preview"), "grok-imagine-video-1.5");
        assert_eq!(routing_xai_videos_model("xai/grok-imagine-video-1.5-preview"), "grok-imagine-video-1.5-preview");
        assert_eq!(routing_xai_videos_model("grok-imagine-video-1.5"), "grok-imagine-video-1.5");
        assert_eq!(routing_xai_videos_model("unknown"), "grok-imagine-video");
    }

    #[test]
    fn create_request_maps_sora_and_defaults() {
        let (req, meta) = build(r#"{"model":"sora-2","prompt":"a cat playing piano","seconds":"8"}"#, "sora-2").unwrap();
        assert_eq!(
            req,
            r#"{"model":"grok-imagine-video","prompt":"a cat playing piano","duration":8,"aspect_ratio":"9:16","resolution":"720p"}"#
        );
        assert_eq!((meta.model.as_str(), meta.routing_model.as_str(), meta.seconds.as_str(), meta.size.as_str()), ("grok-imagine-video", "grok-imagine-video", "8", "720x1280"));
    }

    #[test]
    fn create_request_options_and_clamping() {
        let (req, meta) = build(
            r#"{"prompt":"p","seconds":"99","size":"1792x1024","aspect_ratio":"square","resolution":"480p","input_reference":{"image_url":" http://i "}}"#,
            "xai/grok-imagine-video-1.5-preview",
        )
        .unwrap();
        assert_eq!(
            req,
            r#"{"model":"grok-imagine-video-1.5","prompt":"p","duration":15,"aspect_ratio":"1:1","resolution":"480p","image":{"url":"http://i"}}"#
        );
        assert_eq!(meta.seconds, "15");
        let (req, _) = build(r#"{"prompt":"p","seconds":"0"}"#, "grok-imagine-video").unwrap();
        assert!(req.contains(r#""duration":1"#));
    }

    #[test]
    fn create_request_references_and_errors() {
        let (req, _) = build(r#"{"prompt":"p","reference_images":["a",{"url":"b"},{"image_url":{"url":"c"}}],"reference_image_urls":["d"]}"#, "grok-imagine-video").unwrap();
        assert!(req.ends_with(r#""reference_images":[{"url":"a"},{"url":"b"},{"url":"c"},{"url":"d"}]}"#), "{req}");
        let err = |raw: &str| build(raw, "grok-imagine-video").err().unwrap();
        assert_eq!(err(r#"{"prompt":" "}"#), "prompt is required");
        assert_eq!(err(r#"{"prompt":"p","seconds":"x"}"#), "seconds must be an integer");
        assert_eq!(err(r#"{"prompt":"p","size":"1x1"}"#), "size must be one of 720x1280, 1280x720, 1024x1792, or 1792x1024");
        assert_eq!(
            err(r#"{"prompt":"p","input_reference":{"file_id":"f"}}"#),
            "input_reference.file_id is not supported for xAI video generation; use input_reference.image_url"
        );
        assert_eq!(err(r#"{"prompt":"p","input_reference":{"file_id":"f","image_url":"u"}}"#), "input_reference must provide exactly one of image_url or file_id");
        assert_eq!(err(r#"{"prompt":"p","image":"u","reference_images":["a"]}"#), "image and reference_images cannot be combined on xAI");
        let many = format!(r#"{{"prompt":"p","reference_images":[{}]}}"#, (0..8).map(|i| format!("\"u{i}\"")).collect::<Vec<_>>().join(","));
        assert_eq!(err(&many), "reference_images supports at most 7 images on xAI");
    }

    #[test]
    fn create_response_from_xai() {
        let meta = XaiVideoCreateMetadata { model: "grok-imagine-video".into(), routing_model: String::new(), prompt: "p".into(), seconds: "4".into(), size: "720x1280".into(), created_at: 9 };
        let out = build_videos_create_api_response_from_xai(&cpa_json::parse(br#"{"request_id":"r1"}"#), &meta).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"object":"video","progress":0,"status":"queued","id":"r1","model":"grok-imagine-video","prompt":"p","seconds":"4","size":"720x1280","created_at":9}"#
        );
        let out = build_videos_create_api_response_from_xai(&cpa_json::parse(br#"{"id":"r2","status":"processing","progress":40}"#), &meta).unwrap();
        assert!(String::from_utf8(out).unwrap().contains(r#""progress":40,"status":"in_progress","id":"r2""#));
        assert_eq!(build_videos_create_api_response_from_xai(&cpa_json::parse(b"{}"), &meta).unwrap_err(), "xAI video response did not include request_id");
    }

    fn retrieve(payload: &str) -> Value {
        cpa_json::parse(&build_videos_retrieve_api_response_from_xai("v1", &cpa_json::parse(payload.as_bytes()), "sora-2"))
    }

    #[test]
    fn retrieve_response_from_xai() {
        let out = retrieve(r#"{"status":"done","progress":100,"model":"grok-imagine-video","prompt":"p","video":{"url":"https://v/x.mp4","duration":6},"created_at":5}"#);
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            r#"{"object":"video","id":"v1","model":"grok-imagine-video","created_at":5,"prompt":"p","status":"completed","progress":100,"seconds":"6","video_url":"https://v/x.mp4"}"#
        );
        assert_eq!(retrieve("{}").g("model").str(), "grok-imagine-video");
    }

    #[test]
    fn retrieve_normalizes_errors() {
        let out = retrieve(r#"{"error":"boom","code":"E1"}"#);
        assert_eq!(
            (out.g("status").str(), out.g("progress").int(), out.g("error.code").str(), out.g("error.message").str()),
            ("failed".into(), 0, "E1".into(), "boom".into())
        );
        let out = retrieve(r#"{"error":{"message":"bad","code":"inner"}}"#);
        assert_eq!((out.g("error.code").str(), out.g("error.message").str()), ("inner".into(), "bad".into()));
        let out = retrieve(r#"{"error":{"message":"bad"}}"#);
        assert_eq!(out.g("error.code").str(), "video_generation_failed");
        let out = retrieve(r#"{"code":"only_code","status":"queued"}"#);
        assert_eq!((out.g("status").str(), out.g("error.message").str()), ("queued".into(), "only_code".into()));
    }

    #[test]
    fn content_url_validation() {
        let url = |p: &str| xai_video_content_url_from_payload(&cpa_json::parse(p.as_bytes()));
        assert_eq!(url(r#"{"video":{"url":" https://vidgen.x.ai/v.mp4 "}}"#).unwrap(), "https://vidgen.x.ai/v.mp4");
        assert_eq!(url("{}").unwrap_err(), "xAI video response did not include video.url");
        assert_eq!(url(r#"{"video":{"url":"ftp://h/x"}}"#).unwrap_err(), "xAI video response included invalid video.url");
        assert_eq!(url(r#"{"video":{"url":"/relative"}}"#).unwrap_err(), "xAI video response included invalid video.url");
    }

    #[test]
    fn failed_response_shape() {
        let out = cpa_json::parse(&build_videos_failed_api_response(" ", "", " "));
        assert_eq!((out.g("status").str(), out.g("model").str(), out.g("error.code").str(), out.g("error.message").str()), ("failed".into(), "grok-imagine-video".into(), "invalid_request_error".into(), "Video generation failed".into()));
        assert!(out.g("id").str().starts_with("video_"));
    }

    #[test]
    fn binding_ttl_follows_config() {
        assert_eq!(video_auth_binding_ttl(""), DEFAULT_VIDEO_AUTH_BINDING_TTL);
        assert_eq!(video_auth_binding_ttl("45m"), Duration::from_secs(45 * 60));
        assert_eq!(video_auth_binding_ttl("bogus"), DEFAULT_VIDEO_AUTH_BINDING_TTL);
        assert_eq!(video_auth_binding_ttl("-5s"), DEFAULT_VIDEO_AUTH_BINDING_TTL);
    }

    #[test]
    fn binding_store_expires_entries() {
        let store = VideoAuthBindingStore::default();
        store.set_with_model(" v1 ", " auth-a ", " m ", Duration::from_millis(30));
        let b = store.get_binding("v1").unwrap();
        assert_eq!((b.auth_id.as_str(), b.model.as_str()), ("auth-a", "m"));
        store.set_with_model("", "a", "", Duration::ZERO);
        store.set_with_model("v2", "", "", Duration::ZERO);
        assert!(store.get("v2").is_none());
        std::thread::sleep(Duration::from_millis(50));
        assert!(store.get("v1").is_none());
    }
}

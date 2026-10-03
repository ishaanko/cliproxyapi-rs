//! Codex image endpoints (Go: codex_openai_images.go): `/v1/images/generations` and
//! `/v1/images/edits` for the `gpt-image-*` models are forwarded to the Codex backend's own
//! `/images/generations` and `/images/edits` endpoints, bodies converted to JSON, answers (and
//! SSE streams) relayed unchanged.
//!
//! Go also has a Responses-API based image path (`codexPrepareOpenAIImageRequest`,
//! `codexExtractImageResults`, `codexBuildImagesAPIResponse`, the partial-image frame builders).
//! It only runs for a model that is not one of the direct image models, and the image handlers
//! only route those five models here, so it is unreachable and not ported.

use base64::Engine as _;
use cpa_auth::Auth;
use cpa_core::thinking::parse_suffix;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, Response, StreamResult};
use cpa_translator::Format;
use http::HeaderMap;
use tokio::sync::{mpsc, oneshot};

use super::CodexExecutor;
use super::creds::codex_creds;
use super::headers::{apply_codex_headers, apply_model_header_overrides, set_header};
use super::request::{apply_prompt_cache_and_ids, prompt_cache_id};
use super::terminal::new_status_err_with_cooling;
use crate::helps::content_type::detect_content_type;
use crate::helps::payload::payload_request_path;
use crate::helps::usage::parse::StreamUsageBuffer;
use crate::helps::usage::{UsageReporter, parse_openai_usage};
use crate::openai_compat::META_HANDLER_TYPE;
use crate::openai_compat::images::{FilePart, Form, parse_form, parse_media_type, prepare_images_payload};

const IMAGE_SOURCE_FORMAT: &str = "openai-image";
const IMAGES_GENERATIONS_PATH: &str = "/v1/images/generations";
const IMAGES_EDITS_PATH: &str = "/v1/images/edits";
const DIRECT_IMAGES_GENERATIONS: &str = "/images/generations";
const DIRECT_IMAGES_EDIT: &str = "/images/edits";
const DIRECT_IMAGE_MODELS: [&str; 5] = ["gpt-image-1.5", "gpt-image-2", "gpt-image-2.5-flare", "gpt-image-2.5-sunburst", "gpt-image-2.5"];

/// `isCodexOpenAIImageRequest`: an image-handler request for an images endpoint path.
pub(super) fn is_image_request(opts: &Options) -> bool {
    let handler = opts.metadata.get(META_HANDLER_TYPE).and_then(Value::as_str).unwrap_or_default();
    if !handler.trim().eq_ignore_ascii_case(IMAGE_SOURCE_FORMAT) {
        return false;
    }
    let path = payload_request_path(opts);
    let path = path.trim();
    path == IMAGES_GENERATIONS_PATH || path == IMAGES_EDITS_PATH || path.ends_with(IMAGES_GENERATIONS_PATH) || path.ends_with(IMAGES_EDITS_PATH)
}

/// `codexOpenAIImageBaseModel`: suffix and provider prefix removed, lower-cased.
fn image_base_model(model: &str) -> String {
    let model = parse_suffix(model).model_name;
    let mut model = model.trim();
    if let Some(idx) = model.rfind('/')
        && idx + 1 < model.len()
    {
        model = model[idx + 1..].trim();
    }
    model.trim().to_lowercase()
}

/// `codexDirectOpenAIImageModel`: the first of payload model and route model that names a
/// direct image model.
fn direct_model(req: &Request) -> String {
    let payload_model = cpa_json::parse(&req.payload).g("model").str();
    [payload_model.as_str(), req.model.as_str()]
        .into_iter()
        .map(image_base_model)
        .find(|base| DIRECT_IMAGE_MODELS.contains(&base.as_str()))
        .unwrap_or_default()
}

/// `codexDirectOpenAIImageEndpoint`.
fn direct_endpoint(req: &Request, opts: &Options) -> &'static str {
    if direct_model(req).is_empty() {
        return "";
    }
    let path = payload_request_path(opts);
    let path = path.trim();
    if path.ends_with(IMAGES_GENERATIONS_PATH) {
        DIRECT_IMAGES_GENERATIONS
    } else if path.ends_with(IMAGES_EDITS_PATH) {
        DIRECT_IMAGES_EDIT
    } else {
        ""
    }
}

fn plain_error(message: impl Into<String>) -> ExecError {
    ExecError::new(0, message)
}

/// `codexPrepareDirectOpenAIImageBody`: (body, Content-Type, model).
fn prepare_direct_body(req: &Request, opts: &Options, stream: bool) -> Result<(Vec<u8>, String, String), ExecError> {
    let model = direct_model(req);
    if model.is_empty() {
        return Err(plain_error(format!("unsupported direct OpenAI image model {:?}", req.model)));
    }
    let content_type = opts.headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let path = payload_request_path(opts);
    let (body, content_type) = if path.trim().ends_with(IMAGES_EDITS_PATH) {
        prepare_edit_payload(&req.payload, &model, &content_type, stream)?
    } else {
        prepare_images_payload(&req.payload, &model, &content_type, stream)?
    };
    Ok((body, content_type, model))
}

/// `codexPrepareDirectOpenAIImageEditPayload`: JSON passes through, multipart becomes JSON.
fn prepare_edit_payload(payload: &[u8], model: &str, content_type: &str, stream: bool) -> Result<(Vec<u8>, String), ExecError> {
    if cpa_json::valid(payload) {
        return prepare_images_payload(payload, model, content_type, stream);
    }
    let unsupported = || plain_error(format!("unsupported OpenAI image edit Content-Type {content_type:?}"));
    let Some((media_type, params)) = parse_media_type(content_type.trim()) else { return Err(unsupported()) };
    if !media_type.trim().to_lowercase().starts_with("multipart/") {
        return Err(unsupported());
    }
    let boundary = params.get("boundary").map(|b| b.trim()).unwrap_or_default();
    if boundary.is_empty() {
        return Err(plain_error("multipart boundary is missing"));
    }
    rewrite_edit_multipart_to_json(payload, model, boundary, stream)
}

/// `codexRewriteOpenAIImageEditMultipartToJSON`.
fn rewrite_edit_multipart_to_json(payload: &[u8], model: &str, boundary: &str, stream: bool) -> Result<(Vec<u8>, String), ExecError> {
    let form = parse_form(payload, boundary).map_err(|e| plain_error(format!("read multipart form failed: {e}")))?;
    let mut out = serde_json::json!({});
    cpa_json::set(&mut out, "model", model);
    if stream {
        cpa_json::set(&mut out, "stream", true);
    }
    for (key, values) in &form.values {
        let key = key.trim();
        if key.is_empty() || key == "model" || key == "stream" {
            continue;
        }
        set_edit_form_values(&mut out, key, values);
    }
    if let Some(mask) = form.files.iter().find(|f| f.key == "mask") {
        cpa_json::set(&mut out, "mask.image_url", file_to_data_url(mask));
    }
    let image_files = image_files(&form);
    let existing = out.g("images");
    if !existing.exists() || existing.is_array() {
        let mut items: Vec<Value> = existing.array().iter().map(|i| i.value()).collect();
        for file in &image_files {
            items.push(serde_json::json!({"image_url": file_to_data_url(file)}));
        }
        if !image_files.is_empty() {
            cpa_json::set(&mut out, "images", Value::Array(items));
        }
    } else {
        for file in &image_files {
            cpa_json::set(&mut out, "images.-1.image_url", file_to_data_url(file));
        }
    }
    Ok((cpa_json::to_vec(&out), "application/json".into()))
}

/// `codexMultipartImageFiles`: `image[]` files, else `image`.
fn image_files(form: &Form) -> Vec<&FilePart> {
    let list: Vec<&FilePart> = form.files.iter().filter(|f| f.key == "image[]").collect();
    if list.is_empty() { form.files.iter().filter(|f| f.key == "image").collect() } else { list }
}

/// `codexMultipartFileToDataURL`.
fn file_to_data_url(file: &FilePart) -> String {
    let declared = file.headers.get("Content-Type").and_then(|v| v.first()).map(|v| v.trim()).unwrap_or_default();
    let media_type = if declared.is_empty() { detect_content_type(&file.data) } else { declared };
    format!("data:{media_type};base64,{}", base64::engine::general_purpose::STANDARD.encode(&file.data))
}

/// `codexSetOpenAIImageEditFormValues`: one value sets the field, several make an array.
fn set_edit_form_values(out: &mut Value, key: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let path = match key {
        "mask[file_id]" => "mask.file_id",
        "mask[image_url]" => "mask.image_url",
        other => other,
    };
    if path.is_empty() {
        return;
    }
    if let [value] = values {
        cpa_json::set(out, path, edit_form_json_value(path, value));
        return;
    }
    let items: Vec<Value> = values.iter().map(|v| edit_form_json_value(key, v)).collect();
    cpa_json::set(out, path, Value::Array(items));
}

/// `codexOpenAIImageEditFormJSONValue`: integer fields become numbers, the rest trimmed strings.
fn edit_form_json_value(key: &str, value: &str) -> Value {
    let value = value.trim();
    if matches!(key.trim().to_lowercase().as_str(), "n" | "output_compression" | "partial_images")
        && let Ok(parsed) = value.parse::<i64>()
    {
        return Value::from(parsed);
    }
    Value::String(value.to_string())
}

fn usage_metadata(detail: &crate::helps::usage::accounting::Detail) -> Metadata {
    let mut meta = Metadata::new();
    meta.insert("usage".to_string(), UsageReporter::usage_metadata(detail));
    meta
}

impl CodexExecutor {
    /// Builds the upstream POST: `(url, headers, body, model)` (Go: the shared head of
    /// `executeDirectOpenAIImage` and `executeDirectOpenAIImageStream`).
    fn build_direct_image_request(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        endpoint: &str,
        stream: bool,
    ) -> Result<(String, HeaderMap, Vec<u8>, String), ExecError> {
        let cfg = self.config();
        let (body, content_type, model) = prepare_direct_body(req, opts, stream)?;
        let (api_key, configured_base) = codex_creds(auth);
        let url = Self::http_url(&configured_base, endpoint);
        // The source format `openai-image` takes none of the format-specific cache branches.
        let cache_id = prompt_cache_id(Format::Codex, req, opts, &body, true);
        let body = if cpa_json::valid(&body) { apply_prompt_cache_and_ids(body, &cache_id) } else { body };
        let mut headers = HeaderMap::new();
        if !cache_id.is_empty() {
            set_header(&mut headers, "Session-Id", &cache_id);
        }
        // Downstream User-Agent values are not forwarded to reduce Cloudflare blocks.
        let mut client = opts.headers.clone();
        client.remove("user-agent");
        let session_id = self.session_id(opts, &req.payload);
        apply_codex_headers(&mut headers, auth, &api_key, stream, &cfg, &client, session_id.as_deref());
        apply_model_header_overrides(&mut headers, &model);
        if !content_type.is_empty() {
            set_header(&mut headers, "Content-Type", &content_type);
        }
        Ok((url, headers, body, model))
    }

    /// The reporter of a direct image call, keyed by the image model (Go: reporter built after the
    /// body is prepared, with the OpenAI reasoning-effort format).
    fn image_reporter(&self, auth: &Auth, opts: &Options, model: &str, body: &[u8]) -> UsageReporter {
        let reporter = UsageReporter::new("codex", "CodexExecutor", model, Some(auth), Some(opts));
        reporter.set_translated_reasoning_effort(body, "openai");
        reporter
    }

    /// Reads a whole response body into the request log (`RecordAPIResponseError` on failure,
    /// `AppendAPIResponseChunk` on success), marking TTFT at the first body byte.
    async fn read_logged_body(&self, cfg: &cpa_config::Config, opts: &Options, resp: reqwest::Response, reporter: &UsageReporter) -> Result<bytes::Bytes, ExecError> {
        match super::exec_http::read_all_marking(resp, reporter).await {
            Ok(data) => {
                opts.api_log.append_api_response_chunk(cfg, &data);
                Ok(bytes::Bytes::from(data))
            }
            Err(e) => {
                let err = crate::helps::status::transport_error(&e);
                opts.api_log.record_api_response_error(cfg, &err.message);
                Err(err)
            }
        }
    }

    /// `executeDirectOpenAIImage`: one JSON answer, usage read from the body.
    pub(super) async fn execute_openai_image(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let endpoint = direct_endpoint(&req, &opts);
        let cfg = self.config();
        let (url, headers, body, model) = self.build_direct_image_request(auth, &req, &opts, endpoint, false)?;
        let reporter = self.image_reporter(auth, &opts, &model, &body);
        let result = async {
            let resp = self.send_http(&cfg, auth, &opts, &url, headers, body, &reporter).await?;
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            let data = self.read_logged_body(&cfg, &opts, resp, &reporter).await?;
            if !(200..300).contains(&status) {
                return Err(new_status_err_with_cooling(status, &data, cfg.codex.model_level_cooling));
            }
            let detail = parse_openai_usage(&data);
            reporter.publish(detail.clone());
            reporter.ensure_published();
            Ok(Response { payload: data, metadata: usage_metadata(&detail), headers: resp_headers })
        }
        .await;
        reporter.track_failure(&result);
        result
    }

    /// `executeDirectOpenAIImageStream`: the upstream SSE bytes are relayed as read; usage is
    /// collected from the data lines of each chunk.
    pub(super) async fn execute_openai_image_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let endpoint = direct_endpoint(&req, &opts);
        let cfg = self.config();
        let (url, headers, body, model) = self.build_direct_image_request(auth, &req, &opts, endpoint, true)?;
        let reporter = self.image_reporter(auth, &opts, &model, &body);
        let mut resp = match self.send_http(&cfg, auth, &opts, &url, headers, body, &reporter).await {
            Ok(resp) => resp,
            Err(err) => {
                reporter.publish_failure(&err);
                return Err(err);
            }
        };
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        if !(200..300).contains(&status) {
            let err = match self.read_logged_body(&cfg, &opts, resp, &reporter).await {
                Ok(data) => new_status_err_with_cooling(status, &data, cfg.codex.model_level_cooling),
                Err(err) => err,
            };
            reporter.publish_failure(&err);
            return Err(err);
        }
        let api_log = opts.api_log.clone();
        let (tx, rx) = mpsc::channel(16);
        let (usage_tx, usage_rx) = oneshot::channel::<Value>();
        tokio::spawn(async move {
            let mut usage = StreamUsageBuffer::default();
            loop {
                let next = tokio::select! {
                    () = tx.closed() => break,
                    next = resp.chunk() => next,
                };
                match next {
                    Ok(Some(chunk)) => {
                        reporter.mark_first_response_byte();
                        api_log.append_api_response_chunk(&cfg, &chunk);
                        for line in chunk.split(|b| *b == b'\n') {
                            usage.observe_openai_stream(line.trim_ascii());
                        }
                        if tx.send(Ok(chunk)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        let err = crate::helps::status::transport_error(&err);
                        api_log.record_api_response_error(&cfg, &err.message);
                        reporter.publish_failure(&err);
                        let _ = tx.send(Err(err)).await;
                        break;
                    }
                }
            }
            // Go publishes in a defer: also when the client went away.
            reporter.publish_buffer(&usage);
            reporter.ensure_published();
            if let Some(detail) = usage.detail() {
                let _ = usage_tx.send(UsageReporter::usage_metadata(&detail));
            }
        });
        let mut result = StreamResult::new(resp_headers, rx);
        result.usage = Some(usage_rx);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn request(model: &str, payload: &str) -> Request {
        Request { model: model.into(), payload: Bytes::from(payload.to_string()), format: Format::OpenAI, metadata: Metadata::new() }
    }

    fn options(path: &str, content_type: &str) -> Options {
        let mut opts = Options::new(Format::OpenAI);
        opts.metadata.insert(META_HANDLER_TYPE.into(), Value::from(IMAGE_SOURCE_FORMAT));
        opts.metadata.insert("request_path".into(), Value::from(path));
        if !content_type.is_empty() {
            opts.headers.insert("content-type", content_type.parse().unwrap());
        }
        opts
    }

    #[test]
    fn image_requests_are_recognized_by_handler_type_and_path() {
        assert!(is_image_request(&options("/v1/images/generations", "")));
        assert!(is_image_request(&options("/prefix/v1/images/edits", "")));
        assert!(!is_image_request(&options("/v1/responses", "")));
        let mut plain = Options::new(Format::OpenAI);
        plain.metadata.insert("request_path".into(), Value::from("/v1/images/edits"));
        assert!(!is_image_request(&plain));
    }

    #[test]
    fn direct_models_and_endpoints() {
        let opts = options("/v1/images/generations", "");
        assert_eq!(direct_model(&request("codex/gpt-image-2.5(high)", "{}")), "gpt-image-2.5");
        assert_eq!(direct_model(&request("x", r#"{"model":"OpenAI/GPT-Image-1.5"}"#)), "gpt-image-1.5");
        assert_eq!(direct_model(&request("gpt-5", "{}")), "");
        assert_eq!(direct_endpoint(&request("gpt-image-2", "{}"), &opts), "/images/generations");
        assert_eq!(direct_endpoint(&request("gpt-image-2", "{}"), &options("/v1/images/edits", "")), "/images/edits");
        assert_eq!(direct_endpoint(&request("gpt-5", "{}"), &opts), "");
    }

    #[test]
    fn generation_body_gets_model_and_stream() {
        let req = request("gpt-image-2", r#"{"model":"codex/gpt-image-2","prompt":"p","stream":true}"#);
        let (body, ct, model) = prepare_direct_body(&req, &options("/v1/images/generations", "application/json"), false).unwrap();
        assert_eq!((model.as_str(), ct.as_str()), ("gpt-image-2", "application/json"));
        assert_eq!(String::from_utf8(body).unwrap(), r#"{"model":"gpt-image-2","prompt":"p"}"#);
        let (body, _, _) = prepare_direct_body(&req, &options("/v1/images/generations", ""), true).unwrap();
        assert_eq!(String::from_utf8(body).unwrap(), r#"{"model":"gpt-image-2","prompt":"p","stream":true}"#);
    }

    #[test]
    fn multipart_edit_becomes_json_with_data_urls() {
        let body = "--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit me\r\n--b\r\nContent-Disposition: form-data; name=\"n\"\r\n\r\n2\r\n--b\r\nContent-Disposition: form-data; name=\"mask[file_id]\"\r\n\r\nf1\r\n--b\r\nContent-Disposition: form-data; name=\"image[]\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNG1\r\n--b\r\nContent-Disposition: form-data; name=\"image[]\"; filename=\"b.png\"\r\n\r\nPNG2\r\n--b\r\nContent-Disposition: form-data; name=\"mask\"; filename=\"m.png\"\r\nContent-Type: image/png\r\n\r\nMASK\r\n--b--\r\n";
        let req = request("gpt-image-2", body);
        let opts = options("/v1/images/edits", "multipart/form-data; boundary=b");
        let (out, ct, _) = prepare_direct_body(&req, &opts, true).unwrap();
        assert_eq!(ct, "application/json");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"model":"gpt-image-2","stream":true,"prompt":"edit me","n":2,"mask":{"file_id":"f1","image_url":"data:image/png;base64,TUFTSw=="},"images":[{"image_url":"data:image/png;base64,UE5HMQ=="},{"image_url":"data:text/plain; charset=utf-8;base64,UE5HMg=="}]}"#
        );
    }

    #[test]
    fn edit_content_type_errors() {
        let req = request("gpt-image-2", "not json");
        let err = prepare_direct_body(&req, &options("/v1/images/edits", "text/plain"), false).unwrap_err();
        assert_eq!(err.message, r#"unsupported OpenAI image edit Content-Type "text/plain""#);
        let err = prepare_direct_body(&req, &options("/v1/images/edits", "multipart/form-data"), false).unwrap_err();
        assert_eq!(err.message, "multipart boundary is missing");
    }
}

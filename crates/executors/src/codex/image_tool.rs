//! Codex image generation through the Responses API `image_generation` tool (Go:
//! codex_openai_images.go, the non-direct half: `executeOpenAIImage` after the direct-model check,
//! `executeOpenAIImageStream`, `codexPrepareOpenAIImageRequest`, `codexExtractImageResults`,
//! `codexBuildImagesAPIResponse`, the partial and completed frame builders).
//!
//! It runs for an images request whose model is none of the direct `gpt-image-*` models, which
//! happens when a Codex credential also serves a model name that the image handlers accepted
//! through another provider (an `openai-compatibility` model flagged `image`). The request is
//! rewritten into a Responses call on the base model (`gpt-image-2-base-model`, default
//! `gpt-5.4-mini`) with `tool_choice` forcing the image tool; the SSE answer is folded back into
//! an OpenAI images response, or into `image_generation.*` / `image_edit.*` stream events.

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_json::lazy::Doc;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Options, Request, Response, StreamResult};
use tokio::sync::mpsc;

use super::CodexExecutor;
use super::exec_http::publish_image_tool_usage;
use super::images::{file_to_data_url, form_value, image_files, plain_error};
use super::terminal::{OutputItems, new_status_err_with_cooling, status_error};
use crate::helps::payload::{PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model, set_bool_if_different, set_string_if_different};
use crate::helps::sse::{LineReader, STREAM_SCANNER_BUFFER};
use crate::helps::thinking::apply_thinking_with_source_payload;
use crate::helps::usage::UsageReporter;
use crate::helps::usage::parse::parse_codex_usage;
use crate::openai_compat::images::{parse_form, parse_media_type};

const SOURCE_FORMAT: &str = "openai-image";
const GENERATIONS_PATH: &str = "/v1/images/generations";
const EDITS_PATH: &str = "/v1/images/edits";
const DEFAULT_IMAGE_TOOL_MODEL: &str = "gpt-image-2";
/// Base model of the Responses call unless `gpt-image-2-base-model` names another `gpt-*` model.
const DEFAULT_MAIN_MODEL: &str = "gpt-5.4-mini";

/// A client images request rewritten into a Responses body.
struct PreparedRequest {
    body: Vec<u8>,
    /// `url` or `b64_json`.
    response_format: &'static str,
    /// `image_generation` or `image_edit`: prefix of the stream event names.
    stream_prefix: &'static str,
}

/// One `image_generation_call` output item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ImageCallResult {
    pub result: String,
    pub revised_prompt: String,
    pub output_format: String,
    pub size: String,
    pub background: String,
    pub quality: String,
}

/// What `codexExtractImageResults` found in a completed event.
#[derive(Debug, Default)]
pub(super) struct ExtractedImages {
    pub results: Vec<ImageCallResult>,
    pub created_at: i64,
    /// `response.tool_usage.image_gen` when it is an object.
    pub usage: Option<Value>,
    pub first_meta: ImageCallResult,
}

/// `resolveGPTImage2BaseModel`.
fn resolve_main_model(cfg: &Config) -> String {
    let model = cfg.gpt_image_2_base_model.trim();
    if model.is_empty() || !model.to_lowercase().starts_with("gpt-") {
        return DEFAULT_MAIN_MODEL.to_string();
    }
    model.to_string()
}

// ---------------------------------------------------------------- request building

/// `codexPrepareOpenAIImageRequest`.
fn prepare_request(req: &Request, opts: &Options) -> Result<PreparedRequest, ExecError> {
    let path = payload_request_path(opts);
    if path.ends_with(GENERATIONS_PATH) {
        return prepare_generation_json(&req.payload, &req.model);
    }
    if !path.ends_with(EDITS_PATH) {
        return Err(plain_error(format!("unsupported OpenAI image endpoint path {path:?}")));
    }
    let content_type = opts.headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").trim().to_string();
    // Go ignores the parse error here: only a multipart media type takes the form path, and a
    // multipart type with broken parameters fails again when the form path parses it.
    let multipart = match parse_media_type(&content_type) {
        Some((media_type, _)) => media_type.to_lowercase().starts_with("multipart/"),
        None => content_type.split(';').next().unwrap_or_default().trim().to_lowercase().starts_with("multipart/"),
    };
    if multipart {
        return prepare_edit_multipart(&req.payload, &req.model, &content_type);
    }
    prepare_edit_json(&req.payload, &req.model)
}

/// `codexPrepareOpenAIImageGenerationJSON`.
fn prepare_generation_json(raw: &[u8], route_model: &str) -> Result<PreparedRequest, ExecError> {
    if !cpa_json::valid(raw) {
        return Err(plain_error("invalid OpenAI image generation request JSON"));
    }
    let root = cpa_json::parse(raw);
    let prompt = root.g("prompt").str().trim().to_string();
    let tool = build_image_tool(&root, route_model, "generate", &["size", "quality", "background", "output_format", "moderation"], &["output_compression", "partial_images"]);
    Ok(PreparedRequest { body: build_responses_request(&prompt, &[], Some(tool)), response_format: response_format_of(&root), stream_prefix: "image_generation" })
}

/// `codexPrepareOpenAIImageEditJSON`.
fn prepare_edit_json(raw: &[u8], route_model: &str) -> Result<PreparedRequest, ExecError> {
    if !cpa_json::valid(raw) {
        return Err(plain_error("invalid OpenAI image edit request JSON"));
    }
    let root = cpa_json::parse(raw);
    let prompt = root.g("prompt").str().trim().to_string();
    let mut images = Vec::new();
    let list = root.g("images");
    if list.is_array() {
        for img in list.array() {
            let url = img.g("image_url").str().trim().to_string();
            if !url.is_empty() {
                images.push(url);
            }
        }
    }
    let mut tool = build_image_tool(&root, route_model, "edit", &["size", "quality", "background", "output_format", "input_fidelity", "moderation"], &["output_compression", "partial_images"]);
    let mask = root.g("mask.image_url").str().trim().to_string();
    if !mask.is_empty() {
        cpa_json::set(&mut tool, "input_image_mask.image_url", mask);
    }
    Ok(PreparedRequest { body: build_responses_request(&prompt, &images, Some(tool)), response_format: response_format_of(&root), stream_prefix: "image_edit" })
}

/// `codexPrepareOpenAIImageEditMultipart`.
fn prepare_edit_multipart(raw: &[u8], route_model: &str, content_type: &str) -> Result<PreparedRequest, ExecError> {
    let Some((_, params)) = parse_media_type(content_type) else {
        return Err(plain_error("parse multipart content type failed: mime: invalid media parameter"));
    };
    let boundary = params.get("boundary").map(|b| b.trim()).unwrap_or_default();
    if boundary.is_empty() {
        return Err(plain_error("multipart boundary is required"));
    }
    let form = parse_form(raw, boundary).map_err(|e| plain_error(format!("parse multipart form failed: {e}")))?;
    let prompt = form_value(&form, "prompt");
    let response_format = normalize_response_format(&form_value(&form, "response_format"));
    let mut tool = serde_json::json!({"type": "image_generation", "action": "edit"});
    cpa_json::set(&mut tool, "model", tool_model(&form_value(&form, "model"), route_model));
    for field in ["size", "quality", "background", "output_format", "input_fidelity", "moderation"] {
        let value = form_value(&form, field);
        if !value.is_empty() {
            cpa_json::set(&mut tool, field, value);
        }
    }
    for field in ["output_compression", "partial_images"] {
        let value = form_value(&form, field);
        if !value.is_empty()
            && let Ok(parsed) = value.parse::<i64>()
        {
            cpa_json::set(&mut tool, field, parsed);
        }
    }
    let images: Vec<String> = image_files(&form).into_iter().map(file_to_data_url).collect();
    if let Some(mask) = form.files.iter().find(|f| f.key == "mask") {
        cpa_json::set(&mut tool, "input_image_mask.image_url", file_to_data_url(mask));
    }
    Ok(PreparedRequest { body: build_responses_request(&prompt, &images, Some(tool)), response_format, stream_prefix: "image_edit" })
}

fn response_format_of(root: &Value) -> &'static str {
    normalize_response_format(&root.g("response_format").str())
}

/// `codexNormalizeImageResponseFormat`: only `url` (any case) is kept, everything else is base64.
fn normalize_response_format(response_format: &str) -> &'static str {
    if response_format.trim().eq_ignore_ascii_case("url") { "url" } else { "b64_json" }
}

/// `codexOpenAIImageToolModel`: the request model, else the route model, else `gpt-image-2`.
fn tool_model(request_model: &str, route_model: &str) -> String {
    let model = request_model.trim();
    let model = if model.is_empty() { route_model.trim() } else { model };
    if model.is_empty() { DEFAULT_IMAGE_TOOL_MODEL.to_string() } else { model.to_string() }
}

/// `codexBuildOpenAIImageTool`: the tool from the client's string and integer options.
fn build_image_tool(raw: &Value, route_model: &str, action: &str, string_fields: &[&str], number_fields: &[&str]) -> Value {
    let mut tool = serde_json::json!({"type": "image_generation", "action": ""});
    cpa_json::set(&mut tool, "action", action);
    cpa_json::set(&mut tool, "model", tool_model(&raw.g("model").str(), route_model));
    for field in string_fields {
        let value = raw.g(field).str().trim().to_string();
        if !value.is_empty() {
            cpa_json::set(&mut tool, field, value);
        }
    }
    for field in number_fields {
        let value = raw.g(field);
        if value.is_number() {
            cpa_json::set(&mut tool, field, value.int());
        }
    }
    tool
}

/// `codexBuildImagesResponsesRequest`: the forced-tool Responses body on the default base model.
fn build_responses_request(prompt: &str, images: &[String], tool: Option<Value>) -> Vec<u8> {
    let mut req = cpa_json::parse(
        br#"{"instructions":"","stream":true,"reasoning":{"effort":"medium","summary":"auto"},"parallel_tool_calls":true,"include":["reasoning.encrypted_content"],"model":"","store":false,"tool_choice":{"type":"image_generation"},"tools":[]}"#,
    );
    cpa_json::set(&mut req, "model", DEFAULT_MAIN_MODEL);
    if let Some(tool) = tool {
        cpa_json::set(&mut req, "tools", Value::Array(vec![tool]));
    }
    let mut content = vec![serde_json::json!({"type": "input_text", "text": prompt})];
    content.extend(images.iter().filter(|img| !img.trim().is_empty()).map(|img| serde_json::json!({"type": "input_image", "image_url": img})));
    cpa_json::set(&mut req, "input", serde_json::json!([{"type": "message", "role": "user", "content": content}]));
    cpa_json::to_vec(&req)
}

/// `prepareCodexOpenAIImageBody`: thinking, payload rules and the fixed upstream fields.
fn prepare_body(cfg: &Config, body: Vec<u8>, req: &Request, opts: &Options, main_model: &str) -> Result<Vec<u8>, ExecError> {
    let main_model = if main_model.trim().is_empty() { DEFAULT_MAIN_MODEL } else { main_model.trim() };
    let out = apply_thinking_with_source_payload(&body, &body, &body, main_model, SOURCE_FORMAT, "codex", "codex").map_err(super::request::thinking_error)?;
    let requested_model = payload_requested_model(opts, &req.model);
    let request_path = payload_request_path(opts);
    let rules = PayloadRequest {
        cfg: Some(cfg),
        target_executor: "",
        model: main_model,
        protocol: "codex",
        from_protocol: SOURCE_FORMAT,
        root: "",
        requested_model: &requested_model,
        request_path: &request_path,
        headers: Some(&opts.headers),
    };
    let out = apply_payload_config(&rules, &out, &body);
    let mut v = cpa_json::parse(&out);
    set_string_if_different(&mut v, "model", main_model);
    set_bool_if_different(&mut v, "stream", true);
    for path in ["previous_response_id", "prompt_cache_retention", "safety_identifier", "stream_options"] {
        cpa_json::delete(&mut v, path);
    }
    let instructions = v.g("instructions");
    if !instructions.exists() || instructions.is_null() {
        cpa_json::set(&mut v, "instructions", "");
    }
    Ok(cpa_json::to_vec(&v))
}

// ---------------------------------------------------------------- answer building

/// `codexExtractImageResults`: image calls of a completed event. The completed `response.output`
/// wins; the items collected from `response.output_item.done` events (by `output_index`, then
/// those without one) are only used when that output is empty.
pub(super) fn extract_image_results(completed: &[u8], items: &OutputItems) -> Result<ExtractedImages, String> {
    let root = cpa_json::parse(completed);
    if root.g("type").str() != "response.completed" {
        return Err("unexpected event type".into());
    }
    let mut created_at = root.g("response.created_at").int();
    if created_at <= 0 {
        created_at = chrono::Utc::now().timestamp();
    }
    let mut out = ExtractedImages { created_at, ..Default::default() };
    let mut append = |item: &Value| {
        if item.g("type").str() != "image_generation_call" {
            return;
        }
        let result = item.g("result").str().trim().to_string();
        if result.is_empty() {
            return;
        }
        let field = |name: &str| item.g(name).str().trim().to_string();
        let entry = ImageCallResult {
            result,
            revised_prompt: field("revised_prompt"),
            output_format: field("output_format"),
            size: field("size"),
            background: field("background"),
            quality: field("quality"),
        };
        if out.results.is_empty() {
            out.first_meta = entry.clone();
        }
        out.results.push(entry);
    };
    let output = root.g("response.output");
    let output_items = if output.is_array() { output.array() } else { Vec::new() };
    if !output_items.is_empty() {
        for item in &output_items {
            append(&item.value());
        }
    } else if !items.is_empty() {
        for raw in items.by_index.values().chain(items.fallback.iter()) {
            append(&cpa_json::parse(raw));
        }
    }
    let usage = root.g("response.tool_usage.image_gen");
    if usage.is_object() {
        out.usage = Some(usage.value());
    }
    Ok(out)
}

/// `codexMimeTypeFromOutputFormat`.
fn mime_type_from_output_format(output_format: &str) -> &'static str {
    match output_format.trim().to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

fn data_url(output_format: &str, b64: &str) -> String {
    format!("data:{};base64,{b64}", mime_type_from_output_format(output_format))
}

/// `codexBuildImagesAPIResponse`: the OpenAI images response of the collected calls.
fn build_images_api_response(extracted: &ExtractedImages, response_format: &str) -> Vec<u8> {
    let mut out = serde_json::json!({"created": 0, "data": []});
    cpa_json::set(&mut out, "created", extracted.created_at);
    let meta = &extracted.first_meta;
    for (key, value) in [("background", &meta.background), ("output_format", &meta.output_format), ("quality", &meta.quality), ("size", &meta.size)] {
        if !value.is_empty() {
            cpa_json::set(&mut out, key, value.as_str());
        }
    }
    if let Some(usage) = &extracted.usage {
        cpa_json::set(&mut out, "usage", usage.clone());
    }
    let url = normalize_response_format(response_format) == "url";
    let data: Vec<Value> = extracted
        .results
        .iter()
        .map(|img| {
            let mut item = serde_json::json!({});
            if !img.revised_prompt.is_empty() {
                cpa_json::set(&mut item, "revised_prompt", img.revised_prompt.as_str());
            }
            if url {
                cpa_json::set(&mut item, "url", data_url(&img.output_format, &img.result));
            } else {
                cpa_json::set(&mut item, "b64_json", img.result.as_str());
            }
            item
        })
        .collect();
    cpa_json::set(&mut out, "data", Value::Array(data));
    cpa_json::to_vec(&out)
}

/// `codexBuildSSEFrame`.
fn build_sse_frame(event_name: &str, data: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    if !event_name.trim().is_empty() {
        buf.extend_from_slice(b"event: ");
        buf.extend_from_slice(event_name.as_bytes());
        buf.push(b'\n');
    }
    buf.extend_from_slice(b"data: ");
    buf.extend_from_slice(&cpa_json::to_vec(data));
    buf.extend_from_slice(b"\n\n");
    buf
}

/// `codexBuildImagePartialFrame`: `<prefix>.partial_image` event of a partial image, `None` when
/// the event carries no image.
fn build_partial_frame(payload: &impl J, response_format: &str, stream_prefix: &str) -> Option<Vec<u8>> {
    let b64 = payload.g("partial_image_b64").str().trim().to_string();
    if b64.is_empty() {
        return None;
    }
    let output_format = payload.g("output_format").str();
    let event_name = format!("{}.partial_image", stream_prefix.trim());
    let mut data = serde_json::json!({"type": "", "partial_image_index": 0});
    cpa_json::set(&mut data, "type", event_name.as_str());
    cpa_json::set(&mut data, "partial_image_index", payload.g("partial_image_index").int());
    if normalize_response_format(response_format) == "url" {
        cpa_json::set(&mut data, "url", data_url(&output_format, &b64));
    } else {
        cpa_json::set(&mut data, "b64_json", b64);
    }
    Some(build_sse_frame(&event_name, &data))
}

/// `codexBuildImageCompletedFrame`: `<prefix>.completed` event of one image.
fn build_completed_frame(img: &ImageCallResult, usage: Option<&Value>, response_format: &str, stream_prefix: &str) -> Vec<u8> {
    let event_name = format!("{}.completed", stream_prefix.trim());
    let mut data = serde_json::json!({"type": ""});
    cpa_json::set(&mut data, "type", event_name.as_str());
    if let Some(usage) = usage {
        cpa_json::set(&mut data, "usage", usage.clone());
    }
    if normalize_response_format(response_format) == "url" {
        cpa_json::set(&mut data, "url", data_url(&img.output_format, &img.result));
    } else {
        cpa_json::set(&mut data, "b64_json", img.result.as_str());
    }
    build_sse_frame(&event_name, &data)
}

/// Attaches the upstream response headers to an error for the usage record and quota observation
/// of the failed attempt (Go: `RecordAPIResponseMetadata` stores them in the request context).
pub(super) fn with_recorded_headers(mut err: ExecError, headers: &http::HeaderMap) -> ExecError {
    err.response_headers = headers.clone();
    err
}

/// `data:` frame payload of an SSE line (trimmed), `None` for other lines.
fn data_payload(line: &[u8]) -> Option<&[u8]> {
    line.strip_prefix(b"data:").map(crate::helps::text::trim_space)
}

// ---------------------------------------------------------------- execution

impl CodexExecutor {
    /// The upstream `POST /responses`: `(url, headers, body)` (Go: the shared head of
    /// `executeOpenAIImage` and `executeOpenAIImageStream`). Unlike the direct image calls the
    /// client's own headers, user agent included, are forwarded.
    fn build_image_tool_request(&self, cfg: &Config, auth: &Auth, req: &Request, opts: &Options, body: Vec<u8>, main_model: &str) -> (String, http::HeaderMap, Vec<u8>) {
        let (api_key, configured_base) = super::creds::codex_creds(auth);
        let url = Self::http_url(&configured_base, "/responses");
        let cache_id = super::request::prompt_cache_id(cpa_translator::Format::Codex, req, opts, &body, true);
        let body = super::request::apply_prompt_cache_and_ids(body, &cache_id);
        let mut headers = http::HeaderMap::new();
        if !cache_id.is_empty() {
            super::headers::set_header(&mut headers, "Session-Id", &cache_id);
        }
        let session_id = self.session_id(opts, &req.payload);
        super::headers::apply_codex_headers(&mut headers, auth, &api_key, true, cfg, &opts.headers, session_id.as_deref());
        super::headers::apply_model_header_overrides(&mut headers, main_model);
        (url, headers, body)
    }

    /// `executeOpenAIImage` (Responses tool half): the SSE answer is read whole and its completed
    /// event becomes one OpenAI images response.
    pub(super) async fn execute_image_tool(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let prepared = prepare_request(&req, &opts)?;
        let cfg = self.config();
        let main_model = resolve_main_model(&cfg);
        let reporter = UsageReporter::new("codex", "CodexExecutor", &main_model, Some(auth), Some(&opts));
        let result = self.image_tool_call(&cfg, auth, &req, &opts, prepared, &main_model, &reporter).await;
        reporter.track_failure(&result);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn image_tool_call(&self, cfg: &Config, auth: &Auth, req: &Request, opts: &Options, prepared: PreparedRequest, main_model: &str, reporter: &UsageReporter) -> Result<Response, ExecError> {
        let body = prepare_body(cfg, prepared.body, req, opts, main_model)?;
        reporter.set_translated_reasoning_effort(&body, "codex");
        let (url, headers, body) = self.build_image_tool_request(cfg, auth, req, opts, body, main_model);
        let resp = self.send_http(cfg, auth, opts, &url, headers, body.clone(), reporter).await?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        // Go records the response headers once the response arrived, so every later failure
        // carries them into its usage record.
        let payload = self
            .read_image_tool_answer(cfg, opts, resp, status, &body, prepared.response_format, reporter)
            .await
            .map_err(|err| with_recorded_headers(err, &resp_headers))?;
        Ok(Response { payload: Bytes::from(payload), metadata: Default::default(), headers: resp_headers })
    }

    /// Reads the whole upstream SSE answer and builds the images response of its completed event.
    #[allow(clippy::too_many_arguments)]
    async fn read_image_tool_answer(&self, cfg: &Config, opts: &Options, resp: reqwest::Response, status: u16, body: &[u8], response_format: &str, reporter: &UsageReporter) -> Result<Vec<u8>, ExecError> {
        let data = self.read_logged_body(cfg, opts, resp, reporter).await?;
        if !(200..300).contains(&status) {
            return Err(new_status_err_with_cooling(status, &data, cfg.codex.model_level_cooling));
        }
        let mut items = OutputItems::default();
        for line in data.split(|b| *b == b'\n') {
            let Some(event_data) = data_payload(line) else { continue };
            reporter.observe_response_model(event_data);
            let event = Doc::new(event_data);
            match event.g("type").str().as_str() {
                "response.output_item.done" => items.collect(&event, event_data),
                "response.completed" => {
                    if let Some(detail) = parse_codex_usage(event_data) {
                        reporter.publish(detail);
                    }
                    publish_image_tool_usage(reporter, body, event_data);
                    let extracted = extract_image_results(event_data, &items).map_err(plain_error)?;
                    if extracted.results.is_empty() {
                        return Err(status_error(502, "upstream did not return image output"));
                    }
                    return Ok(build_images_api_response(&extracted, response_format));
                }
                _ => {}
            }
        }
        Err(status_error(504, "stream error: stream disconnected before completion"))
    }

    /// `executeOpenAIImageStream` (Responses tool half): partial images and the completed images
    /// are emitted as `<prefix>.partial_image` and `<prefix>.completed` events.
    pub(super) async fn execute_image_tool_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let prepared = prepare_request(&req, &opts)?;
        let cfg = self.config();
        let main_model = resolve_main_model(&cfg);
        let reporter = UsageReporter::new("codex", "CodexExecutor", &main_model, Some(auth), Some(&opts));
        let result = self.image_tool_stream_start(&cfg, auth, &req, &opts, prepared, &main_model, &reporter).await;
        // Go: the deferred TrackFailure sees only the setup error; the reader goroutine reports its own.
        reporter.track_failure(&result);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn image_tool_stream_start(&self, cfg: &std::sync::Arc<Config>, auth: &Auth, req: &Request, opts: &Options, prepared: PreparedRequest, main_model: &str, reporter: &UsageReporter) -> Result<StreamResult, ExecError> {
        let body = prepare_body(cfg, prepared.body, req, opts, main_model)?;
        reporter.set_translated_reasoning_effort(&body, "codex");
        let (url, headers, body) = self.build_image_tool_request(cfg, auth, req, opts, body, main_model);
        let resp = self.send_http(cfg, auth, opts, &url, headers, body.clone(), reporter).await?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        if !(200..300).contains(&status) {
            let err = match self.read_logged_body(cfg, opts, resp, reporter).await {
                Ok(data) => new_status_err_with_cooling(status, &data, cfg.codex.model_level_cooling),
                Err(err) => err,
            };
            return Err(with_recorded_headers(err, &resp_headers));
        }
        let api_log = opts.api_log.clone();
        let cfg = std::sync::Arc::clone(cfg);
        let reporter = reporter.clone();
        let recorded = resp_headers.clone();
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let mut lines = LineReader::from_response(resp, STREAM_SCANNER_BUFFER);
            let mut items = OutputItems::default();
            let mut first = true;
            while let Some(next) = lines.next_line_or_closed(&tx).await {
                let line = match next {
                    Ok(line) => line,
                    Err(err) => {
                        api_log.record_api_response_error(&cfg, &err.to_string());
                        let err = with_recorded_headers(err.into(), &recorded);
                        reporter.publish_failure(&err);
                        let _ = tx.send(Err(err)).await;
                        return;
                    }
                };
                if first {
                    reporter.mark_first_response_byte();
                    first = false;
                }
                api_log.append_api_response_chunk(&cfg, &line);
                let Some(event_data) = data_payload(&line) else { continue };
                reporter.observe_response_model(event_data);
                let event = Doc::new(event_data);
                match event.g("type").str().as_str() {
                    "response.output_item.done" => items.collect(&event, event_data),
                    "response.image_generation_call.partial_image" => {
                        if let Some(frame) = build_partial_frame(&event, prepared.response_format, prepared.stream_prefix)
                            && tx.send(Ok(Bytes::from(frame))).await.is_err()
                        {
                            return;
                        }
                    }
                    "response.completed" => {
                        if let Some(detail) = parse_codex_usage(event_data) {
                            reporter.publish(detail);
                        }
                        publish_image_tool_usage(&reporter, &body, event_data);
                        let extracted = match extract_image_results(event_data, &items) {
                            Ok(extracted) => extracted,
                            Err(msg) => {
                                let _ = tx.send(Err(with_recorded_headers(plain_error(msg), &recorded))).await;
                                return;
                            }
                        };
                        if extracted.results.is_empty() {
                            let _ = tx.send(Err(with_recorded_headers(status_error(502, "upstream did not return image output"), &recorded))).await;
                            return;
                        }
                        for img in &extracted.results {
                            let frame = build_completed_frame(img, extracted.usage.as_ref(), prepared.response_format, prepared.stream_prefix);
                            if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
                    _ => {}
                }
            }
        });
        Ok(StreamResult::new(resp_headers, rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `image_generation_call` item JSON.
    fn image_gen_item(result: &str, format: &str) -> String {
        format!(r#"{{"type":"image_generation_call","result":"{result}","output_format":"{format}"}}"#)
    }

    fn items_by_index(entries: &[(i64, String)]) -> OutputItems {
        let mut items = OutputItems::default();
        for (index, item) in entries {
            items.by_index.insert(*index, item.clone().into_bytes());
        }
        items
    }

    fn edit_options(content_type: &str) -> Options {
        let mut opts = Options::new(cpa_translator::Format::OpenAI);
        opts.metadata.insert("request_path".into(), Value::from(EDITS_PATH));
        if !content_type.is_empty() {
            opts.headers.insert("content-type", content_type.parse().unwrap());
        }
        opts
    }

    fn image_request(model: &str, payload: &str) -> Request {
        Request { model: model.into(), payload: Bytes::from(payload.to_string()), format: cpa_translator::Format::OpenAI, metadata: Default::default() }
    }

    #[test]
    fn multipart_edit_becomes_a_forced_tool_request() {
        let form = "--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\n edit me \r\n--b\r\nContent-Disposition: form-data; name=\"size\"\r\n\r\n1024x1024\r\n--b\r\nContent-Disposition: form-data; name=\"output_compression\"\r\n\r\n55\r\n--b\r\nContent-Disposition: form-data; name=\"partial_images\"\r\n\r\nmany\r\n--b\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\nURL\r\n--b\r\nContent-Disposition: form-data; name=\"image[]\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNG1\r\n--b\r\nContent-Disposition: form-data; name=\"mask\"; filename=\"m.png\"\r\nContent-Type: image/png\r\n\r\nMASK\r\n--b--\r\n";
        let prepared = prepare_request(&image_request("route-model", form), &edit_options("multipart/form-data; boundary=b")).unwrap();
        assert_eq!((prepared.response_format, prepared.stream_prefix), ("url", "image_edit"));
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(
            body["tools"],
            serde_json::json!([{"type": "image_generation", "action": "edit", "model": "route-model", "size": "1024x1024", "output_compression": 55,
                "input_image_mask": {"image_url": "data:image/png;base64,TUFTSw=="}}])
        );
        assert_eq!(
            body["input"][0]["content"],
            serde_json::json!([{"type": "input_text", "text": "edit me"}, {"type": "input_image", "image_url": "data:image/png;base64,UE5HMQ=="}])
        );
    }

    #[test]
    fn prepare_errors_follow_the_request_shape() {
        let generations = |payload: &str| {
            let mut opts = edit_options("");
            opts.metadata.insert("request_path".into(), Value::from(GENERATIONS_PATH));
            prepare_request(&image_request("m", payload), &opts).err().map(|e| e.message)
        };
        assert_eq!(generations("{nope").as_deref(), Some("invalid OpenAI image generation request JSON"));
        assert!(generations(r#"{"prompt":"p"}"#).is_none());
        let edit = prepare_request(&image_request("m", "{nope"), &edit_options("application/json")).err().map(|e| e.message);
        assert_eq!(edit.as_deref(), Some("invalid OpenAI image edit request JSON"));
        let no_boundary = prepare_request(&image_request("m", "x"), &edit_options("multipart/form-data")).err().map(|e| e.message);
        assert_eq!(no_boundary.as_deref(), Some("multipart boundary is required"));
        let json_body = prepare_request(&image_request("m", r#"{"prompt":"p"}"#), &edit_options("multipart/form-data; boundary=b")).err().map(|e| e.message);
        assert_eq!(json_body.as_deref(), Some("parse multipart form failed: multipart: NextPart: EOF"));
        let mut other = edit_options("");
        other.metadata.insert("request_path".into(), Value::from("/v1/images/variations"));
        let unsupported = prepare_request(&image_request("m", "{}"), &other).err().map(|e| e.message);
        assert_eq!(unsupported.as_deref(), Some(r#"unsupported OpenAI image endpoint path "/v1/images/variations""#));
    }

    #[test]
    fn base_model_must_be_a_gpt_model() {
        let mut cfg = Config::default();
        assert_eq!(resolve_main_model(&cfg), "gpt-5.4-mini");
        cfg.gpt_image_2_base_model = " GPT-5.5 ".into();
        assert_eq!(resolve_main_model(&cfg), "GPT-5.5");
        cfg.gpt_image_2_base_model = "claude-sonnet-4".into();
        assert_eq!(resolve_main_model(&cfg), "gpt-5.4-mini");
    }

    #[test]
    fn extract_from_completed_output() {
        let completed = format!(r#"{{"type":"response.completed","response":{{"created_at":111,"output":[{}]}}}}"#, image_gen_item("AAA", "png"));
        let got = extract_image_results(completed.as_bytes(), &OutputItems::default()).unwrap();
        assert_eq!(got.created_at, 111);
        assert_eq!(got.results.len(), 1);
        assert_eq!(got.results[0].result, "AAA");
        assert_eq!(got.first_meta.output_format, "png");
    }

    #[test]
    fn extract_falls_back_to_collected_items_in_output_index_order() {
        // Completed event has an empty output; images arrived via output_item.done.
        let completed = br#"{"type":"response.completed","response":{"created_at":222,"output":[]}}"#;
        let items = items_by_index(&[(2, image_gen_item("SECOND", "png")), (0, image_gen_item("FIRST", "jpg"))]);
        let got = extract_image_results(completed, &items).unwrap();
        assert_eq!(got.created_at, 222);
        let order: Vec<&str> = got.results.iter().map(|r| r.result.as_str()).collect();
        assert_eq!(order, ["FIRST", "SECOND"]);
    }

    #[test]
    fn extract_prefers_completed_output_over_items() {
        let completed = format!(r#"{{"type":"response.completed","response":{{"created_at":333,"output":[{}]}}}}"#, image_gen_item("FROM_OUTPUT", "png"));
        let items = items_by_index(&[(0, image_gen_item("FROM_ITEMS", "png"))]);
        let got = extract_image_results(completed.as_bytes(), &items).unwrap();
        assert_eq!(got.results.len(), 1);
        assert_eq!(got.results[0].result, "FROM_OUTPUT");
    }

    #[test]
    fn extract_rejects_other_event_types() {
        assert!(extract_image_results(br#"{"type":"response.in_progress"}"#, &OutputItems::default()).is_err());
    }

    #[test]
    fn extract_reads_the_fallback_list() {
        // Items collected without an output_index land in the fallback list.
        let completed = br#"{"type":"response.completed","response":{"created_at":444}}"#;
        let mut items = OutputItems::default();
        items.fallback.push(image_gen_item("FB", "webp").into_bytes());
        let got = extract_image_results(completed, &items).unwrap();
        assert_eq!(got.results.len(), 1);
        assert_eq!(got.results[0].result, "FB");
        assert_eq!(got.first_meta.output_format, "webp");
    }
}

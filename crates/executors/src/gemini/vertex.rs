//! Vertex AI Gemini executor (Go: gemini_vertex_executor.go): service-account credentials
//! (project and location in the URL, bearer token) or API keys (`x-goog-api-key`, no project).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::signature::sanitize_gemini_request_thought_signatures;
use cpa_core::thinking::parse_suffix;
use cpa_json::{J, Value, json};
use cpa_runtime::executor::{ExecError, Executor, Options, Request, Response, StreamResult};
use cpa_translator::{Ctx, Format, Param};
use http::HeaderMap;
use serde_json::Map;

use super::common::{
    PumpSetup, StreamPump, apply_custom_headers, compact_unsupported, error_body,
    fix_gemini_image_aspect_ratio, is_count_tokens_action, json_headers, observed_lines, original_payload, post_json,
    pre_send, read_body, set_header, set_model, thinking_error, translate_request, upstream_error,
    usage_metadata,
};
use crate::helps::http_request;
use crate::helps::home_refresh::refresh_auth_via_home;
use crate::helps::gemini_content_turns::{ensure_leading_user_content_value, ensure_trailing_user_content_value};
use super::vertex_payload::strip_vertex_openai_responses_tool_call_ids;
use super::vertex_token;
use crate::ConfigRx;
use crate::helps::apply_patch::{apply_patch_original_request, apply_patch_translation_error, gateway_error};
use crate::helps::gemini_log::UpstreamLog;
use crate::helps::payload::{PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model};
use crate::helps::proxy::new_proxy_aware_http_client;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::session::ensure_session_id;
use crate::helps::status::status_err;
use crate::helps::thinking::apply_request_thinking;
use crate::helps::usage::{UsageReporter, parse_gemini_stream_usage, parse_gemini_usage};

/// Public Vertex Generative AI API version.
const VERTEX_API_VERSION: &str = "v1";
const GLOBAL_HOST: &str = "https://aiplatform.googleapis.com";
const DEFAULT_LOCATION: &str = "us-central1";

/// Imagen models use the `:predict` action instead of `:generateContent`.
fn is_imagen_model(model: &str) -> bool {
    model.to_lowercase().contains("imagen")
}

fn vertex_action(model: &str, stream: bool) -> &'static str {
    if is_imagen_model(model) {
        "predict"
    } else if stream {
        "streamGenerateContent"
    } else {
        "generateContent"
    }
}

/// Rewrites an Imagen `predictions` response as a Gemini candidate with `inlineData` parts so the
/// regular Gemini response translators apply (Go: convertImagenToGeminiResponse).
pub(super) fn convert_imagen_to_gemini_response(data: &[u8], model: &str) -> Vec<u8> {
    let v = cpa_json::parse(data);
    let Some(Value::Array(predictions)) = v.g("predictions").v().cloned() else {
        return data.to_vec();
    };
    let mut parts = Vec::new();
    for pred in &predictions {
        let image = pred.g("bytesBase64Encoded").str();
        let mut mime = pred.g("mimeType").str();
        if mime.is_empty() {
            mime = "image/png".into();
        }
        if !image.is_empty() {
            parts.push(json!({"inlineData": {"data": image, "mimeType": mime}}));
        }
    }
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    // Go marshals maps with sorted keys.
    let response = json!({
        "candidates": [{
            "content": {"parts": parts, "role": "model"},
            "finishReason": "STOP",
        }],
        "modelVersion": model,
        "responseId": format!("imagen-{nanos}"),
        "usageMetadata": {"candidatesTokenCount": 0, "promptTokenCount": 0, "totalTokenCount": 0},
    });
    cpa_json::to_vec(&response)
}

/// Builds an Imagen `predict` request from a Gemini-style (or OpenAI-style) payload (Go:
/// convertToImagenRequest). The prompt comes from `contents.0.parts.0.text`, else the first
/// non-empty `messages[].content`, else `prompt`.
pub(super) fn convert_to_imagen_request(payload: &[u8]) -> Result<Vec<u8>, ExecError> {
    let v = cpa_json::parse(payload);
    let mut prompt = String::new();
    let contents_text = v.g("contents.0.parts.0.text");
    if contents_text.exists() {
        prompt = contents_text.str();
    }
    if prompt.is_empty() {
        let messages = v.g("messages.#.content");
        if messages.is_array() {
            for msg in messages.array() {
                let text = msg.str();
                if !text.is_empty() {
                    prompt = text;
                    break;
                }
            }
        }
    }
    if prompt.is_empty() {
        let direct = v.g("prompt");
        if direct.exists() {
            prompt = direct.str();
        }
    }
    if prompt.is_empty() {
        return Err(pre_send(ExecError::new(0, "imagen: no prompt found in request")));
    }
    let mut instance = Map::new();
    instance.insert("prompt".into(), json!(prompt));
    let mut parameters = Map::new();
    parameters.insert("sampleCount".into(), json!(1));
    let aspect = v.g("aspectRatio");
    if aspect.exists() {
        parameters.insert("aspectRatio".into(), json!(aspect.str()));
    }
    let sample_count = v.g("sampleCount");
    if sample_count.exists() {
        parameters.insert("sampleCount".into(), json!(sample_count.int()));
    }
    let negative = v.g("negativePrompt");
    if negative.exists() {
        instance.insert("negativePrompt".into(), json!(negative.str()));
    }
    // Go marshals maps with sorted keys: instances before parameters, and so on.
    let mut sorted_params: Vec<_> = parameters.into_iter().collect();
    sorted_params.sort_by(|a, b| a.0.cmp(&b.0));
    let mut sorted_instance: Vec<_> = instance.into_iter().collect();
    sorted_instance.sort_by(|a, b| a.0.cmp(&b.0));
    let out = json!({
        "instances": [Map::from_iter(sorted_instance)],
        "parameters": Map::from_iter(sorted_params),
    });
    Ok(cpa_json::to_vec(&out))
}

/// How a Vertex request authenticates.
enum Creds {
    ApiKey { key: String, base_url: String },
    ServiceAccount { project_id: String, location: String, service_account: Map<String, Value> },
}

/// API key and base URL from attributes (the key falls back to metadata `access_token`).
fn api_creds(auth: &Auth) -> (String, String) {
    let mut key = auth.attributes.get("api_key").cloned().unwrap_or_default();
    let base_url = auth.attributes.get("base_url").cloned().unwrap_or_default();
    if key.is_empty()
        && let Some(token) = auth.metadata.get("access_token").and_then(Value::as_str)
    {
        key = token.to_string();
    }
    (key, base_url)
}

/// Project, location and normalized service account from auth metadata.
fn service_account_creds(auth: &Auth) -> Result<Creds, ExecError> {
    let fail = |msg: String| pre_send(ExecError::new(0, msg));
    if auth.metadata.is_empty() {
        return Err(fail("vertex executor: missing auth metadata".into()));
    }
    let meta_str =
        |key: &str| auth.metadata.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default();
    let mut project_id = meta_str("project_id");
    if project_id.is_empty() {
        project_id = meta_str("project");
    }
    if project_id.is_empty() {
        return Err(fail("vertex executor: missing project_id in credentials".into()));
    }
    let mut location = meta_str("location");
    if location.is_empty() {
        location = DEFAULT_LOCATION.to_string();
    }
    let Some(Value::Object(sa)) = auth.metadata.get("service_account") else {
        return Err(fail("vertex executor: missing service_account in credentials".into()));
    };
    let normalized =
        cpa_auth::vertex::normalize_service_account_map(sa).map_err(|e| fail(format!("vertex executor: {e}")))?;
    Ok(Creds::ServiceAccount { project_id, location, service_account: normalized })
}

fn resolve_creds(auth: &Auth) -> Result<Creds, ExecError> {
    let (key, base_url) = api_creds(auth);
    if key.is_empty() { service_account_creds(auth) } else { Ok(Creds::ApiKey { key, base_url }) }
}

/// Regional host, or the global host for `global`.
fn vertex_base_url(location: &str) -> String {
    let loc = location.trim();
    if loc == "global" {
        return GLOBAL_HOST.to_string();
    }
    let loc = if loc.is_empty() { DEFAULT_LOCATION } else { loc };
    format!("https://{loc}-aiplatform.googleapis.com")
}

/// Vertex AI Gemini executor.
pub struct GeminiVertexExecutor {
    cfg: ConfigRx,
}

impl GeminiVertexExecutor {
    pub fn new(cfg: ConfigRx) -> Self {
        Self { cfg }
    }

    fn reporter(&self, model: &str, auth: &Auth, opts: &Options) -> UsageReporter {
        UsageReporter::new("vertex", "GeminiVertexExecutor", model, Some(auth), Some(opts))
    }
}

/// Upstream URL up to (excluding) the query.
fn endpoint(creds: &Creds, model: &str, action: &str) -> String {
    match creds {
        Creds::ServiceAccount { project_id, location, .. } => format!(
            "{}/{VERTEX_API_VERSION}/projects/{project_id}/locations/{location}/publishers/google/models/{model}:{action}",
            vertex_base_url(location)
        ),
        Creds::ApiKey { base_url, .. } => {
            let base = if base_url.is_empty() { GLOBAL_HOST } else { base_url };
            format!("{base}/{VERTEX_API_VERSION}/publishers/google/models/{model}:{action}")
        }
    }
}

/// Auth header (bearer token for service accounts) and custom headers.
async fn request_headers(
    cfg: &Config,
    auth: &Auth,
    opts: &Options,
    creds: &Creds,
    session_id: Option<&str>,
) -> Result<HeaderMap, ExecError> {
    let mut headers = json_headers();
    match creds {
        Creds::ApiKey { key, .. } => set_header(&mut headers, "x-goog-api-key", key)?,
        Creds::ServiceAccount { service_account, .. } => {
            // The token exchange honors the credential or global proxy, never the per-request one.
            let token_client = new_proxy_aware_http_client("", Some(cfg), Some(auth), None);
            match vertex_token::access_token(&token_client, service_account).await {
                Ok(token) if !token.is_empty() => {
                    set_header(&mut headers, "authorization", &format!("Bearer {token}"))?
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::error!("vertex executor: access token error: {err}");
                    return Err(pre_send(status_err(500, "internal server error")));
                }
            }
        }
    }
    apply_custom_headers(&mut headers, auth, opts, session_id);
    Ok(headers)
}

/// Gemini-format request body for a Vertex generate/stream/count call.
struct VertexBody {
    body: Vec<u8>,
    from: Format,
}

impl GeminiVertexExecutor {
    /// Translation, thinking, payload rules, id stripping and signature sanitizing (no turn-shape
    /// fixes yet; callers add them once the action is known).
    fn translate_gemini(
        &self,
        cfg: &Config,
        req: &Request,
        opts: &Options,
        base_model: &str,
        stream: bool,
    ) -> Result<VertexBody, ExecError> {
        let from = opts.source_format;
        let to = Format::Gemini;
        let original_translated =
            translate_request(cfg, &opts.headers, from, to, base_model, original_payload(req, opts), stream, false);
        let body = translate_request(cfg, &opts.headers, from, to, base_model, &req.payload, stream, false);
        let body = apply_request_thinking(&body, req, opts, from.as_str(), to.as_str(), "vertex", false)
            .map_err(thinking_error)?;
        let body = fix_gemini_image_aspect_ratio(base_model, body);
        let requested_model = payload_requested_model(opts, &req.model);
        let request_path = payload_request_path(opts);
        let payload_req = PayloadRequest {
            cfg: Some(cfg),
            target_executor: "",
            model: base_model,
            protocol: to.as_str(),
            from_protocol: from.as_str(),
            root: "",
            requested_model: &requested_model,
            request_path: &request_path,
            headers: Some(&opts.headers),
        };
        let body = apply_payload_config(&payload_req, &body, &original_translated);
        let mut v = cpa_json::parse(&body);
        set_model(&mut v, base_model);
        let body = strip_vertex_openai_responses_tool_call_ids(&cpa_json::to_vec(&v), from.as_str());
        let body = sanitize_gemini_request_thought_signatures(&body, "contents");
        Ok(VertexBody { body, from })
    }

    /// Final shape: leading user turn, trailing user turn unless counting, no `session_id`.
    fn finalize(body: &[u8], trailing_user: bool) -> Vec<u8> {
        let mut v = cpa_json::parse(body);
        ensure_leading_user_content_value(&mut v, "contents");
        if trailing_user {
            ensure_trailing_user_content_value(&mut v, "contents");
        }
        cpa_json::delete(&mut v, "session_id");
        cpa_json::to_vec(&v)
    }
}

#[async_trait]
impl Executor for GeminiVertexExecutor {
    fn identifier(&self) -> &str {
        "vertex"
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let cfg = self.cfg.borrow().clone();
        let session_id = ensure_session_id(None, "", &opts, &req.payload);
        if opts.alt == "responses/compact" {
            return Err(compact_unsupported());
        }
        let creds = resolve_creds(auth)?;
        let base_model = parse_suffix(&req.model).model_name;
        let reporter = self.reporter(&base_model, auth, &opts);
        let result =
            self.execute_inner(&cfg, auth, &req, &opts, &creds, &base_model, session_id.as_deref(), &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let cfg = self.cfg.borrow().clone();
        let session_id = ensure_session_id(None, "", &opts, &req.payload);
        if opts.alt == "responses/compact" {
            return Err(compact_unsupported());
        }
        let creds = resolve_creds(auth)?;
        let base_model = parse_suffix(&req.model).model_name;
        let reporter = self.reporter(&base_model, auth, &opts);
        let result =
            self.stream_inner(&cfg, auth, req, opts, &creds, &base_model, session_id.as_deref(), &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        let cfg = self.cfg.borrow().clone();
        if let Some(result) = refresh_auth_via_home(&cfg, auth).await {
            return result;
        }
        Ok(auth.clone())
    }

    async fn count_tokens(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let cfg = self.cfg.borrow().clone();
        let creds = resolve_creds(auth)?;
        let session_id = ensure_session_id(None, "", &opts, &req.payload);
        let base_model = parse_suffix(&req.model).model_name;
        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let to = Format::Gemini;

        let translated = translate_request(&cfg, &opts.headers, from, to, &base_model, &req.payload, false, false);
        let translated = apply_request_thinking(&translated, &req, &opts, from.as_str(), to.as_str(), "vertex", false)
            .map_err(thinking_error)?;
        let translated = fix_gemini_image_aspect_ratio(&base_model, translated);
        let mut v = cpa_json::parse(&translated);
        cpa_json::set(&mut v, "model", base_model.as_str());
        let translated = strip_vertex_openai_responses_tool_call_ids(&cpa_json::to_vec(&v), from.as_str());
        let mut v = cpa_json::parse(&translated);
        for key in ["tools", "generationConfig", "safetySettings"] {
            cpa_json::delete(&mut v, key);
        }
        let translated = sanitize_gemini_request_thought_signatures(&cpa_json::to_vec(&v), "contents");
        let mut v = cpa_json::parse(&translated);
        ensure_leading_user_content_value(&mut v, "contents");
        let translated = cpa_json::to_vec(&v);

        let url = endpoint(&creds, &base_model, "countTokens");
        let headers = request_headers(&cfg, auth, &opts, &creds, session_id.as_deref()).await?;
        let log = UpstreamLog::new(&opts, &cfg);
        log.request(auth, "vertex", &url, &headers, &translated);
        let client = new_proxy_aware_http_client(&opts.proxy_url, Some(&cfg), Some(auth), None);
        let resp = log.tap_err(post_json(&client, &url, headers, translated).await)?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        log.metadata(status, &resp_headers);
        if !(200..300).contains(&status) {
            let body = error_body(resp).await;
            log.chunk(&body);
            return Err(upstream_error(status, &body));
        }
        let data = log.tap_err(read_body(resp).await)?;
        log.chunk(&data);
        let count = cpa_json::parse(&data).g("totalTokens").int();
        let payload = cpa_translator::translate_token_count(&Ctx::default(), to, response_format, count, &data);
        Ok(Response { payload: Bytes::from(payload), headers: resp_headers, ..Default::default() })
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }

    /// Go: GeminiVertexExecutor.PrepareRequest.
    async fn prepare_request(&self, req: &mut reqwest::Request, auth: &Auth) -> Result<(), ExecError> {
        match resolve_creds(auth)? {
            Creds::ApiKey { key, .. } => {
                http_request::set_header(req, "x-goog-api-key", &key);
                http_request::del_header(req, "Authorization");
            }
            Creds::ServiceAccount { service_account, .. } => {
                let cfg = self.cfg.borrow().clone();
                let token_client = new_proxy_aware_http_client("", Some(&cfg), Some(auth), None);
                let token = vertex_token::access_token(&token_client, &service_account)
                    .await
                    .map_err(|e| ExecError::new(0, e))?;
                if token.trim().is_empty() {
                    return Err(ExecError::new(401, "missing access token"));
                }
                http_request::set_header(req, "Authorization", &format!("Bearer {token}"));
                http_request::del_header(req, "x-goog-api-key");
            }
        }
        Ok(())
    }

    /// Go: GeminiVertexExecutor.HttpRequest.
    async fn http_request(&self, auth: &Auth, mut req: reqwest::Request) -> Result<reqwest::Response, ExecError> {
        self.prepare_request(&mut req, auth).await?;
        let cfg = self.cfg.borrow().clone();
        let client = new_proxy_aware_http_client("", Some(&cfg), Some(auth), None);
        http_request::execute(&client, req).await
    }
}

impl GeminiVertexExecutor {
    #[allow(clippy::too_many_arguments)]
    async fn execute_inner(
        &self,
        cfg: &Arc<Config>,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        creds: &Creds,
        base_model: &str,
        session_id: Option<&str>,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        let response_format = opts.response_format_or_source();
        let imagen_sa = matches!(creds, Creds::ServiceAccount { .. }) && is_imagen_model(base_model);
        let body = if imagen_sa {
            convert_to_imagen_request(&req.payload)?
        } else {
            self.translate_gemini(cfg, req, opts, base_model, false)?.body
        };
        let mut action = vertex_action(base_model, false);
        let count_tokens = is_count_tokens_action(req);
        if count_tokens {
            action = "countTokens";
        }
        let body = Self::finalize(&body, !count_tokens);
        let mut url = endpoint(creds, base_model, action);
        if !opts.alt.is_empty() && !count_tokens {
            url.push_str(&format!("?$alt={}", opts.alt));
        }
        reporter.set_translated_reasoning_effort(&body, Format::Gemini.as_str());

        let headers = request_headers(cfg, auth, opts, creds, session_id).await?;
        let log = UpstreamLog::new(opts, cfg);
        log.request(auth, "vertex", &url, &headers, &body);
        let client = new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(auth), None);
        reporter.start_response_ttft();
        let resp = log.tap_err(post_json(&client, &url, headers, body.clone()).await)?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        log.metadata(status, &resp_headers);
        if !(200..300).contains(&status) {
            let body = error_body(resp).await;
            log.chunk(&body);
            return Err(upstream_error(status, &body));
        }
        let data = log.tap_err(read_body(resp).await)?;
        log.chunk(&data);
        reporter.mark_first_response_byte();
        reporter.observe_response_model(&data);
        let data = if imagen_sa { convert_imagen_to_gemini_response(&data, base_model) } else { data.to_vec() };

        let mut param = Param::default();
        let original = apply_patch_original_request(req, opts);
        let out = cpa_translator::translate_non_stream(
            &Ctx::default(),
            Format::Gemini,
            response_format,
            &req.model,
            &original,
            &body,
            &data,
            &mut param,
        );
        let out = match out {
            Some(out) if apply_patch_translation_error(&param).is_none() && !out.is_empty() => out,
            _ => return Err(gateway_error()),
        };
        let detail = parse_gemini_usage(&data);
        reporter.publish(detail.clone());
        let out = if response_format == Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out };
        Ok(Response { payload: Bytes::from(out), metadata: usage_metadata(&detail), headers: resp_headers })
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream_inner(
        &self,
        cfg: &Arc<Config>,
        auth: &Auth,
        req: Request,
        opts: Options,
        creds: &Creds,
        base_model: &str,
        session_id: Option<&str>,
        reporter: &UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        let response_format = opts.response_format_or_source();
        let built = self.translate_gemini(cfg, &req, &opts, base_model, true)?;
        let action = vertex_action(base_model, true);
        let body = Self::finalize(&built.body, true);
        let mut url = endpoint(creds, base_model, action);
        // Imagen models do not stream: no SSE query.
        if !is_imagen_model(base_model) {
            if opts.alt.is_empty() {
                url.push_str("?alt=sse");
            } else {
                url.push_str(&format!("?$alt={}", opts.alt));
            }
        }
        reporter.set_translated_reasoning_effort(&body, Format::Gemini.as_str());

        let headers = request_headers(cfg, auth, &opts, creds, session_id).await?;
        let log = UpstreamLog::new(&opts, cfg);
        log.request(auth, "vertex", &url, &headers, &body);
        let client = new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(auth), None);
        reporter.start_response_ttft();
        let resp = log.tap_err(post_json(&client, &url, headers, body.clone()).await)?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        log.metadata(status, &resp_headers);
        if !(200..300).contains(&status) {
            let body = error_body(resp).await;
            log.chunk(&body);
            return Err(upstream_error(status, &body));
        }

        let (mut pump, rx, usage_rx) = StreamPump::new(PumpSetup {
            reporter: reporter.clone(),
            from: built.from,
            upstream: Format::Gemini,
            response: response_format,
            req: &req,
            opts: &opts,
            body,
            ctx: Ctx::default(),
        });
        let reporter = reporter.clone();
        tokio::spawn(async move {
            let mut lines = observed_lines(reporter.clone(), resp);
            let mut scan_err = None;
            loop {
                let line = tokio::select! {
                    _ = pump.tx.closed() => return,
                    line = lines.next_line() => line,
                };
                let line = match line {
                    None => break,
                    Some(Ok(line)) => line,
                    Some(Err(err)) => {
                        scan_err = Some(err);
                        break;
                    }
                };
                log.chunk(&line);
                reporter.observe_response_model(&line);
                if let Some(detail) = parse_gemini_stream_usage(&line) {
                    pump.usage.observe(detail, true);
                }
                if !pump.feed(&line).await {
                    return;
                }
            }
            if pump.end_apply_patch().await {
                return;
            }
            if !pump.feed(b"[DONE]").await {
                return;
            }
            if let Some(err) = scan_err {
                log.error(&err.to_string());
                pump.fail(err.into()).await;
            }
            pump.finish();
        });
        let mut result = StreamResult::new(resp_headers, rx);
        result.usage = Some(usage_rx);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imagen_action_and_hosts() {
        assert_eq!(vertex_action("imagen-4.0-generate-001", false), "predict");
        assert_eq!(vertex_action("Imagen-3", true), "predict");
        assert_eq!(vertex_action("gemini-2.5-pro", true), "streamGenerateContent");
        assert_eq!(vertex_action("gemini-2.5-pro", false), "generateContent");
        assert_eq!(vertex_base_url("global"), "https://aiplatform.googleapis.com");
        assert_eq!(vertex_base_url(" "), "https://us-central1-aiplatform.googleapis.com");
        assert_eq!(vertex_base_url("europe-west4"), "https://europe-west4-aiplatform.googleapis.com");
    }

    #[test]
    fn endpoints_for_service_account_and_api_key() {
        let sa = Creds::ServiceAccount {
            project_id: "proj".into(),
            location: "us-east5".into(),
            service_account: Map::new(),
        };
        assert_eq!(
            endpoint(&sa, "gemini-2.5-pro", "generateContent"),
            "https://us-east5-aiplatform.googleapis.com/v1/projects/proj/locations/us-east5/publishers/google/models/gemini-2.5-pro:generateContent"
        );
        let key = Creds::ApiKey { key: "k".into(), base_url: String::new() };
        assert_eq!(
            endpoint(&key, "gemini-2.5-pro", "countTokens"),
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.5-pro:countTokens"
        );
        let custom = Creds::ApiKey { key: "k".into(), base_url: "http://127.0.0.1:1/vx".into() };
        assert_eq!(endpoint(&custom, "m", "predict"), "http://127.0.0.1:1/vx/v1/publishers/google/models/m:predict");
    }

    #[test]
    fn imagen_request_prompt_sources_and_parameters() {
        let gemini = br#"{"contents":[{"parts":[{"text":"a cat"}]}],"aspectRatio":"16:9","sampleCount":2,"negativePrompt":"dog"}"#;
        let v = cpa_json::parse(&convert_to_imagen_request(gemini).unwrap());
        assert_eq!(v.g("instances.0.prompt").str(), "a cat");
        assert_eq!(v.g("instances.0.negativePrompt").str(), "dog");
        assert_eq!(v.g("parameters.aspectRatio").str(), "16:9");
        assert_eq!(v.g("parameters.sampleCount").int(), 2);
        let messages = br#"{"messages":[{"content":""},{"content":"from messages"}]}"#;
        assert_eq!(
            cpa_json::parse(&convert_to_imagen_request(messages).unwrap()).g("instances.0.prompt").str(),
            "from messages"
        );
        let direct = br#"{"prompt":"direct"}"#;
        assert_eq!(cpa_json::parse(&convert_to_imagen_request(direct).unwrap()).g("parameters.sampleCount").int(), 1);
        assert!(convert_to_imagen_request(b"{}").unwrap_err().message.contains("no prompt"));
    }

    #[test]
    fn imagen_response_becomes_a_gemini_candidate() {
        let data = br#"{"predictions":[{"bytesBase64Encoded":"AAAA","mimeType":"image/jpeg"},{"bytesBase64Encoded":"BBBB"},{"mimeType":"image/png"}]}"#;
        let v = cpa_json::parse(&convert_imagen_to_gemini_response(data, "imagen-4"));
        assert_eq!(v.g("candidates.0.content.parts.#").int(), 2);
        assert_eq!(v.g("candidates.0.content.parts.0.inlineData.mimeType").str(), "image/jpeg");
        assert_eq!(v.g("candidates.0.content.parts.1.inlineData.mimeType").str(), "image/png");
        assert_eq!(v.g("candidates.0.finishReason").str(), "STOP");
        assert_eq!(v.g("modelVersion").str(), "imagen-4");
        assert!(v.g("responseId").str().starts_with("imagen-"));
        assert_eq!(convert_imagen_to_gemini_response(b"{}", "m"), b"{}".to_vec());
    }

    #[test]
    fn creds_prefer_api_key_then_service_account() {
        let mut auth = Auth::new("v", "vertex");
        assert!(resolve_creds(&auth).err().unwrap().message.contains("missing auth metadata"));
        auth.metadata.insert("access_token".into(), json!("tok"));
        match resolve_creds(&auth).unwrap() {
            Creds::ApiKey { key, base_url } => assert_eq!((key.as_str(), base_url.as_str()), ("tok", "")),
            _ => panic!("expected api key"),
        }
        let mut sa_auth = Auth::new("v", "vertex");
        sa_auth.metadata.insert("project".into(), json!(" p "));
        assert!(resolve_creds(&sa_auth).err().unwrap().message.contains("missing service_account"));
        let mut none = Auth::new("v", "vertex");
        none.metadata.insert("location".into(), json!("x"));
        assert!(resolve_creds(&none).err().unwrap().message.contains("missing project_id"));
    }
}

//! Shared handler pipeline (Go: sdk/api/handlers handlers_routing / handlers_execution /
//! handlers_stream): model -> provider routing, execution metadata, non-stream / stream / count
//! execution through the auth manager, and the stream bootstrap-retry read.
//!
//! Plugin interceptors, model routers and the Home mode of the Go pipeline are not ported.

use std::sync::Arc;

use axum::http::HeaderMap;
use bytes::Bytes;
use cpa_config::Config;
use cpa_core::format::{Format, constant};
use cpa_core::util::{get_provider_name, resolve_auto_model};
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, SelectedAuthCallback, StreamResult, meta};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::error::{ErrorMessage, enrich_auth_selection_error, exec_error_message, is_selection_error};
use crate::headers::filter_upstream_headers;
use crate::req::ReqInfo;
use crate::sse_validate::SseJsonValidator;
use crate::state::{AppState, HandlerSettings};
use crate::thinking::{extract_reasoning_effort, parse_suffix};

/// Result of a stream execution: filtered upstream headers plus the chunk channel. An `Err`
/// item is terminal; a closed channel is a clean end.
pub struct ExecStream {
    pub headers: HeaderMap,
    pub rx: mpsc::Receiver<Result<Bytes, ErrorMessage>>,
}

impl ExecStream {
    /// A stream that fails before any chunk (the Go "error before data" shape).
    pub fn failed(err: ErrorMessage) -> Self {
        let (tx, rx) = mpsc::channel(1);
        let _ = tx.try_send(Err(err));
        ExecStream {
            headers: HeaderMap::new(),
            rx,
        }
    }
}

/// Successful non-stream execution.
pub struct ExecOk {
    pub body: Bytes,
    /// Upstream headers to forward (empty unless `passthrough-headers`).
    pub headers: HeaderMap,
}

/// Per-call execution parameters (Go: `modelExecutionOptions` plus the positional arguments).
#[derive(Clone)]
pub struct ExecArgs<'a> {
    pub entry: Format,
    /// Response schema; `None` means the entry format.
    pub exit: Option<Format>,
    pub model: &'a str,
    pub body: Bytes,
    pub alt: &'a str,
    pub allow_image_model: bool,
    pub forced_provider: Option<&'a str>,
    pub auth_selection_model: Option<&'a str>,
    pub execution_session_id: Option<&'a str>,
    pub pinned_auth_id: Option<&'a str>,
    /// The client is connected over a Responses websocket (lets the Codex executor use its upstream websocket).
    pub downstream_websocket: bool,
    /// The request continues a response and is valid only on the session's live upstream
    /// websocket (Go: `WithRequiredUpstreamWebsocket`).
    pub required_upstream_websocket: bool,
    /// Called with the auth id of every credential pick (Go: `WithSelectedAuthIDCallback`).
    pub on_selected_auth: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

impl<'a> ExecArgs<'a> {
    pub fn new(entry: Format, model: &'a str, body: Bytes, alt: &'a str) -> Self {
        ExecArgs {
            entry,
            exit: None,
            model,
            body,
            alt,
            allow_image_model: false,
            forced_provider: None,
            auth_selection_model: None,
            execution_session_id: None,
            pinned_auth_id: None,
            downstream_websocket: false,
            required_upstream_websocket: false,
            on_selected_auth: None,
        }
    }
}

/// Request-scoped execution context (Go: `BaseAPIHandler` + the request's gin context).
#[derive(Clone)]
pub struct Pipeline {
    pub state: AppState,
    pub cfg: Arc<Config>,
    pub settings: HandlerSettings,
    pub info: ReqInfo,
}

impl Pipeline {
    pub fn new(state: &AppState, info: &ReqInfo) -> Self {
        let cfg = state.cfg();
        let settings = HandlerSettings::from_config(&cfg);
        Pipeline {
            state: state.clone(),
            cfg,
            settings,
            info: info.clone(),
        }
    }

    // ------------------------------------------------------------- routing

    /// `providersForExecution` + `getRequestDetailsWithOptions`: candidate providers and the
    /// normalized model name.
    pub fn providers_for_execution(
        &self,
        model: &str,
        allow_image: bool,
        forced_provider: Option<&str>,
    ) -> Result<(Vec<String>, String), ErrorMessage> {
        if let Some(forced) = forced_provider.map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()) {
            let normalized = model.trim().to_string();
            validate_image_only_model(&normalized, allow_image)?;
            return Ok((vec![forced], normalized));
        }
        request_details(model, allow_image)
    }

    // ------------------------------------------------------------- metadata

    /// `requestExecutionMetadata` + the per-request keys set by every execute variant.
    fn build_metadata(&self, a: &ExecArgs<'_>, normalized_model: &str) -> Metadata {
        let mut md = Metadata::new();
        let idempotency = self.info.header("Idempotency-Key");
        if !idempotency.is_empty() {
            md.insert("idempotency_key".into(), json!(idempotency));
        }
        let request_path = self.info.route.trim();
        let request_path = if request_path.is_empty() { self.info.path.trim() } else { request_path };
        if !request_path.is_empty() {
            md.insert(meta::REQUEST_PATH.into(), json!(request_path));
        }
        if let Some(pinned) = a.pinned_auth_id.map(str::trim).filter(|s| !s.is_empty()) {
            md.insert(meta::PINNED_AUTH_ID.into(), json!(pinned));
        }
        if let Some(session) = a.execution_session_id.map(str::trim).filter(|s| !s.is_empty()) {
            md.insert(meta::EXECUTION_SESSION_ID.into(), json!(session));
        }
        // Request facts the usage record reports (Go: ClientRequestMetadata, GetRequestID).
        let remote_ip = self.info.remote.map(|a| a.ip().to_string()).unwrap_or_default();
        let forwarded = self.info.headers.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(", ");
        let user_agent = self.info.header("User-Agent");
        for (key, value) in [
            (meta::CLIENT_IP, remote_ip.as_str()),
            (meta::RESOLVED_CLIENT_IP, self.info.client_ip.trim()),
            (meta::X_FORWARDED_FOR, forwarded.trim()),
            (meta::USER_AGENT, user_agent.trim()),
            (cpa_runtime::conductor::usage::META_REQUEST_ID, self.info.request_id.trim()),
            (meta::TRACE_ID, self.info.request_id.trim()),
        ] {
            if !value.is_empty() {
                md.insert(key.into(), json!(value));
            }
        }
        if let Some(key) = &self.info.api_key {
            md.insert(cpa_runtime::conductor::usage::META_CLIENT_API_KEY.into(), json!(key));
            let scope = caller_scope(key);
            if !scope.is_empty() {
                md.insert(meta::CALLER_SCOPE.into(), json!(scope));
            }
        }
        if a.downstream_websocket {
            md.insert(cpa_executors::codex::META_DOWNSTREAM_WEBSOCKET.into(), json!(true));
        }
        if a.required_upstream_websocket {
            md.insert(cpa_executors::codex::META_REQUIRED_UPSTREAM_WEBSOCKET.into(), json!(true));
        }
        md.insert(meta::REQUESTED_MODEL.into(), json!(a.model));
        if let Some(sel) = a.auth_selection_model.map(str::trim).filter(|s| !s.is_empty()) {
            md.insert(meta::AUTH_SELECTION_MODEL.into(), json!(sel));
        }
        let effort = extract_reasoning_effort(&a.body, a.entry.as_str(), normalized_model);
        if !effort.is_empty() {
            md.insert(meta::REASONING_EFFORT.into(), json!(effort));
        }
        md.insert(meta::SERVICE_TIER.into(), json!(service_tier(&a.body)));
        md.insert(meta::GENERATE.into(), json!(generate_flag(&a.body)));
        md
    }

    fn build_request(&self, a: &ExecArgs<'_>, normalized_model: &str, stream: bool, count: bool) -> (Request, Options) {
        let metadata = self.build_metadata(a, normalized_model);
        let req = Request {
            model: normalized_model.to_string(),
            payload: a.body.clone(),
            format: a.entry,
            metadata: Metadata::new(),
        };
        let mut opts = Options::new(a.entry);
        opts.stream = stream;
        opts.alt = a.alt.to_string();
        opts.headers = self.info.headers.clone();
        opts.query = self.info.query.clone();
        opts.original_request = a.body.clone();
        if !count {
            opts.response_format = Some(a.exit.unwrap_or(a.entry));
        }
        opts.metadata = metadata;
        opts.api_log = self.info.api_log.exec_handle();
        // Every credential pick (including failover) refreshes the trace id header value.
        let (trace, request_id) = (self.info.trace.clone(), self.info.request_id.clone());
        let on_selected = a.on_selected_auth.clone();
        opts.selected_auth = Some(SelectedAuthCallback(Arc::new(move |auth_id, index| {
            trace.record(index, &request_id);
            if let Some(cb) = &on_selected {
                cb(auth_id);
            }
        })));
        (req, opts)
    }

    fn providers(&self, a: &ExecArgs<'_>) -> Result<(Vec<String>, String), ErrorMessage> {
        let (providers, normalized) = self.providers_for_execution(a.model, a.allow_image_model, a.forced_provider)?;
        Ok((adjust_providers_for_entry(a.entry, providers), normalized))
    }

    // ------------------------------------------------------------- execution

    /// `ExecuteWithAuthManager`: non-streaming execution.
    pub async fn execute(&self, a: ExecArgs<'_>) -> Result<ExecOk, ErrorMessage> {
        let (providers, normalized) = self.providers(&a)?;
        let (req, opts) = self.build_request(&a, &normalized, false, false);
        let resp = self
            .state
            .manager
            .execute(&providers, req, opts)
            .await
            .map_err(|e| exec_error_message(&enrich_auth_selection_error(&e, &providers, &normalized)))?;
        Ok(self.finish_ok(resp.payload, &resp.headers))
    }

    /// `ExecuteCountWithAuthManager`.
    pub async fn execute_count(&self, a: ExecArgs<'_>) -> Result<ExecOk, ErrorMessage> {
        let (providers, normalized) = self.providers(&a)?;
        let (req, opts) = self.build_request(&a, &normalized, false, true);
        let resp = self
            .state
            .manager
            .execute_count(&providers, req, opts)
            .await
            .map_err(|e| exec_error_message(&enrich_auth_selection_error(&e, &providers, &normalized)))?;
        Ok(self.finish_ok(resp.payload, &resp.headers))
    }

    fn finish_ok(&self, body: Bytes, headers: &HeaderMap) -> ExecOk {
        let headers = if self.settings.passthrough_headers {
            filter_upstream_headers(headers)
        } else {
            HeaderMap::new()
        };
        ExecOk { body, headers }
    }

    /// `ExecuteStreamWithAuthManager`: streaming execution with the bootstrap read (retries
    /// before the first deliverable payload when `streaming.bootstrap-retries` allows).
    pub async fn execute_stream(&self, a: ExecArgs<'_>) -> ExecStream {
        let (providers, normalized) = match self.providers(&a) {
            Ok(v) => v,
            Err(e) => return ExecStream::failed(e),
        };
        let (req, opts) = self.build_request(&a, &normalized, true, false);
        let enrich = |e: &ExecError| enrich_auth_selection_error(e, &providers, &normalized);

        let first = self.state.manager.execute_stream(&providers, req.clone(), opts.clone()).await;
        let mut stream: StreamResult = match first {
            Ok(s) => s,
            Err(e) => return ExecStream::failed(exec_error_message(&enrich(&e))),
        };

        let response_format = a.exit.unwrap_or(a.entry);
        let new_validator = || (response_format == Format::OpenAIResponse).then(SseJsonValidator::default);
        let mut validator = new_validator();
        let max_retries = self.settings.bootstrap_retries;
        let mut retries = 0u32;
        let mut bootstrap_payload: Option<Bytes> = None;
        let mut bootstrap_err: Option<ErrorMessage> = None;

        loop {
            match read_initial(&mut stream, &mut validator).await {
                Initial::Payload(p) => {
                    bootstrap_payload = Some(p);
                    break;
                }
                Initial::Closed => break,
                Initial::Rejected(err) => {
                    bootstrap_err = Some(err);
                    break;
                }
                Initial::Failed(err) => {
                    if retries >= max_retries || !bootstrap_eligible(err.status) {
                        bootstrap_err = Some(exec_error_message(&err));
                        break;
                    }
                    retries += 1;
                    match self.state.manager.execute_stream(&providers, req.clone(), opts.clone()).await {
                        Err(retry_err) => {
                            // No credential left to retry with: keep the original upstream failure.
                            let original = exec_error_message(&err);
                            bootstrap_err = Some(if is_selection_error(&retry_err) && original.status_or_500() >= 500 {
                                original
                            } else {
                                exec_error_message(&enrich(&retry_err))
                            });
                            break;
                        }
                        Ok(next) => {
                            stream = next;
                            validator = new_validator();
                        }
                    }
                }
            }
        }

        let headers = if self.settings.passthrough_headers {
            filter_upstream_headers(&stream.headers)
        } else {
            HeaderMap::new()
        };
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            if let Some(err) = bootstrap_err {
                let _ = tx.send(Err(err)).await;
                return;
            }
            if let Some(payload) = bootstrap_payload {
                let sent = tokio::select! {
                    r = tx.send(Ok(payload)) => r.is_ok(),
                    () = tx.closed() => false,
                };
                if !sent {
                    return;
                }
            }
            forward_rest(stream, validator, tx).await;
        });
        ExecStream { headers, rx }
    }
}

/// `bootstrapEligible`.
fn bootstrap_eligible(status: u16) -> bool {
    status == 0 || matches!(status, 401 | 402 | 403 | 408 | 429) || status >= 500
}

enum Initial {
    Payload(Bytes),
    /// Upstream closed without a deliverable payload.
    Closed,
    /// Upstream produced an error before any payload.
    Failed(ExecError),
    /// The SSE validator rejected the first payload (502, never retried).
    Rejected(ErrorMessage),
}

/// `readInitialStreamChunks`: skips empty payloads until the first deliverable one.
async fn read_initial(stream: &mut StreamResult, validator: &mut Option<SseJsonValidator>) -> Initial {
    loop {
        match stream.chunks.recv().await {
            None => return Initial::Closed,
            Some(Err(err)) => return Initial::Failed(err),
            Some(Ok(chunk)) => {
                if chunk.is_empty() {
                    continue;
                }
                match validate_payload(validator, chunk) {
                    Ok(Some(p)) => return Initial::Payload(p),
                    Ok(None) => continue,
                    Err(e) => return Initial::Rejected(e),
                }
            }
        }
    }
}

/// Runs a payload through the Responses SSE validator; `Ok(None)` when it is still incomplete.
fn validate_payload(validator: &mut Option<SseJsonValidator>, chunk: Bytes) -> Result<Option<Bytes>, ErrorMessage> {
    let Some(v) = validator else {
        return Ok(Some(chunk));
    };
    match v.add_chunk(&chunk) {
        Ok(out) if out.is_empty() => Ok(None),
        Ok(out) => Ok(Some(Bytes::from(out))),
        Err(msg) => Err(ErrorMessage::new(502, msg)),
    }
}

/// Post-bootstrap pump: forwards every chunk, surfaces terminal errors and the validator's
/// end-of-stream check, stops when the consumer drops the receiver.
async fn forward_rest(
    mut stream: StreamResult,
    mut validator: Option<SseJsonValidator>,
    tx: mpsc::Sender<Result<Bytes, ErrorMessage>>,
) {
    loop {
        // A dropped consumer (client disconnect) must release the upstream stream promptly, not
        // only after the next chunk arrives and fails to send.
        let next = tokio::select! {
            item = stream.chunks.recv() => item,
            () = tx.closed() => return,
        };
        let Some(item) = next else {
            if let Some(v) = validator.as_mut()
                && let Err(msg) = v.finish()
            {
                let _ = tx.send(Err(ErrorMessage::new(502, msg))).await;
            }
            return;
        };
        match item {
            Err(err) => {
                let _ = tx.send(Err(exec_error_message(&err))).await;
                return;
            }
            Ok(chunk) => {
                if chunk.is_empty() {
                    continue;
                }
                match validate_payload(&mut validator, chunk) {
                    Ok(Some(p)) => {
                        if tx.send(Ok(p)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------ routing helpers

/// `getRequestDetailsWithOptions`.
pub fn request_details(model: &str, allow_image: bool) -> Result<(Vec<String>, String), ErrorMessage> {
    let initial = parse_suffix(model);
    let resolved = if initial.model_name == "auto" {
        let base = resolve_auto_model(&initial.model_name);
        if initial.has_suffix {
            format!("{base}({})", initial.raw_suffix)
        } else {
            base
        }
    } else {
        resolve_auto_model(model)
    };
    let parsed = parse_suffix(&resolved);
    let base = parsed.model_name.trim().to_string();
    validate_image_only_model(&base, allow_image)?;

    let mut providers = get_provider_name(&base);
    if providers.is_empty() && base != resolved {
        providers = get_provider_name(&resolved);
    }
    if providers.is_empty() {
        let text = unknown_provider_body(model);
        return Err(ErrorMessage::new(400, text));
    }
    Ok((providers, resolved))
}

/// The pre-rendered JSON error body of an unroutable model (the model name is escaped).
fn unknown_provider_body(model: &str) -> String {
    let message = cpa_core::util::go_json_string(&format!("unknown provider for model {model}"));
    format!(r#"{{"error":{{"message":{message},"type":"invalid_request_error","code":"model_not_found","param":"model"}}}}"#)
}

/// `routeModelBaseName`: the segment after the last `/`.
fn route_model_base_name(model: &str) -> String {
    let model = model.trim();
    match model.rfind('/') {
        Some(idx) if idx + 1 < model.len() => model[idx + 1..].trim().to_string(),
        _ => model.to_string(),
    }
}

fn is_openai_image_only_model(model: &str) -> bool {
    matches!(
        route_model_base_name(model).trim().to_lowercase().as_str(),
        "gpt-image-1.5"
            | "gpt-image-2"
            | "gpt-image-2.5-flare"
            | "gpt-image-2.5-sunburst"
            | "gpt-image-2.5"
            | "grok-imagine-image"
            | "grok-imagine-image-quality"
            | "grok-imagine-image-2.0"
    )
}

/// `validateImageOnlyModel`.
fn validate_image_only_model(model: &str, allow_image: bool) -> Result<(), ErrorMessage> {
    let suffix = parse_suffix(model);
    let base = suffix.model_name.trim();
    let base = if base.is_empty() { model.trim() } else { base };
    if is_openai_image_only_model(base) && !allow_image {
        return Err(ErrorMessage::new(
            503,
            format!(
                "model {} is only supported on /v1/images/generations and /v1/images/edits",
                route_model_base_name(base)
            ),
        ));
    }
    Ok(())
}

/// `adjustExecutionProvidersForEntryProtocol`: the interactions entry prefers the native
/// `gemini-interactions` provider; protocols that cannot use it exclude it.
pub fn adjust_providers_for_entry(entry: Format, providers: Vec<String>) -> Vec<String> {
    if entry == Format::Interactions {
        return prefer_provider(providers, constant::GEMINI_INTERACTIONS);
    }
    let supports_native = matches!(
        entry,
        Format::Interactions | Format::OpenAI | Format::OpenAIResponse | Format::Claude | Format::Gemini
    );
    if supports_native {
        providers
    } else {
        exclude_provider(providers, constant::GEMINI_INTERACTIONS)
    }
}

fn norm(p: &str) -> String {
    p.trim().to_lowercase()
}

fn prefer_provider(providers: Vec<String>, preferred: &str) -> Vec<String> {
    let preferred = norm(preferred);
    if preferred.is_empty() || providers.len() < 2 {
        return providers;
    }
    let Some(idx) = providers.iter().position(|p| norm(p) == preferred) else {
        return providers;
    };
    if idx == 0 {
        return providers;
    }
    let mut out = Vec::with_capacity(providers.len());
    out.push(providers[idx].clone());
    out.extend(providers[..idx].iter().cloned());
    out.extend(providers[idx + 1..].iter().cloned());
    out
}

fn exclude_provider(mut providers: Vec<String>, excluded: &str) -> Vec<String> {
    let excluded = norm(excluded);
    if excluded.is_empty() {
        return providers;
    }
    if let Some(idx) = providers.iter().position(|p| norm(p) == excluded) {
        providers.remove(idx);
    }
    providers
}

// ------------------------------------------------------------------ metadata helpers

/// `coresession.CallerScope`: irreversible namespace for a downstream credential.
pub fn caller_scope(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    hasher.update(b"cli-proxy-api:caller-scope:v1\x00");
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

/// `setServiceTierMetadata`: `service_tier` (trimmed, default `auto`).
fn service_tier(body: &[u8]) -> String {
    if cpa_json::valid(body) {
        let root = cpa_json::parse(body);
        let node = cpa_json::J::g(&root, "service_tier");
        if node.exists() {
            let value = node.str();
            let value = value.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    "auto".to_string()
}

/// `setGenerateMetadata`: only an explicit boolean `false` disables generation.
fn generate_flag(body: &[u8]) -> bool {
    if !cpa_json::valid(body) {
        return true;
    }
    let root = cpa_json::parse(body);
    let node = cpa_json::J::g(&root, "generate");
    !(node.exists() && node.is_bool() && !node.bool())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_only_models_are_rejected_on_chat_routes() {
        let err = validate_image_only_model("openai/gpt-image-2(high)", false).unwrap_err();
        assert_eq!(err.status, 503);
        assert_eq!(
            err.text,
            "model gpt-image-2 is only supported on /v1/images/generations and /v1/images/edits"
        );
        assert!(validate_image_only_model("gpt-image-2", true).is_ok());
        assert!(validate_image_only_model("gpt-5", false).is_ok());
    }

    #[test]
    fn unknown_model_error_body_is_json_with_escaped_name() {
        let err = request_details("no\"such", false).unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(
            err.text,
            r#"{"error":{"message":"unknown provider for model no\"such","type":"invalid_request_error","code":"model_not_found","param":"model"}}"#
        );
    }

    #[test]
    fn interactions_provider_adjustment() {
        let providers = vec!["gemini".to_string(), "gemini-interactions".to_string()];
        assert_eq!(
            adjust_providers_for_entry(Format::Interactions, providers.clone()),
            vec!["gemini-interactions", "gemini"]
        );
        assert_eq!(adjust_providers_for_entry(Format::Claude, providers.clone()), providers);
        assert_eq!(adjust_providers_for_entry(Format::Codex, providers), vec!["gemini"]);
    }

    #[test]
    fn metadata_defaults() {
        assert_eq!(service_tier(br#"{"service_tier":" flex "}"#), "flex");
        assert_eq!(service_tier(b"{}"), "auto");
        assert!(generate_flag(b"{}"));
        assert!(generate_flag(br#"{"generate":"no"}"#));
        assert!(!generate_flag(br#"{"generate":false}"#));
        assert_eq!(caller_scope(" "), "");
        assert_eq!(caller_scope("k").len(), 64);
    }

    #[test]
    fn bootstrap_eligibility() {
        for s in [0, 401, 402, 403, 408, 429, 500, 503] {
            assert!(bootstrap_eligible(s), "{s}");
        }
        for s in [400, 404, 409, 422] {
            assert!(!bootstrap_eligible(s), "{s}");
        }
    }
}

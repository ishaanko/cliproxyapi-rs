//! Shared handler pipeline (Go: sdk/api/handlers handlers_routing / handlers_execution /
//! handlers_stream): model -> provider routing, execution metadata, non-stream / stream / count
//! execution through the auth manager, and the stream bootstrap-retry read.
//!
//! Plugin interceptors, model routers and plugin executors live in `plugin_exec.rs`; Home mode is handled inside the conductor.

use std::sync::Arc;
use std::task::{Context, Poll};

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
use crate::thinking::{extract_reasoning_effort, metadata_keys, parse_suffix};

/// Result of a stream execution: filtered upstream headers plus the chunk source. An `Err`
/// item is terminal; an ended source is a clean end.
pub struct ExecStream {
    pub headers: HeaderMap,
    pub rx: ExecRx,
}

impl ExecStream {
    /// A stream that fails before any chunk (the Go "error before data" shape).
    pub fn failed(err: ErrorMessage) -> Self {
        ExecStream {
            headers: HeaderMap::new(),
            rx: ExecRx::once(Err(err)),
        }
    }
}

/// Chunk source of an [`ExecStream`]. Plugin pumps hand over a plain channel; the built-in
/// pipeline hands over the conductor's channel wrapped in an adapter that converts errors and runs
/// the Responses SSE validator when polled, so the consumer's own task does that work and no pump
/// task or extra channel exists per stream.
pub enum ExecRx {
    Chan(mpsc::Receiver<Result<Bytes, ErrorMessage>>),
    Direct(Box<DirectRx>),
}

/// The conductor's chunk channel plus the post-bootstrap pump logic (Go: forwardStreamChunks).
pub struct DirectRx {
    /// `None` once the upstream ended, failed or was never started (dropping it releases it).
    chunks: Option<mpsc::Receiver<Result<Bytes, ExecError>>>,
    validator: Option<SseJsonValidator>,
    /// Bootstrap payload or error that precedes the rest of the stream.
    first: Option<Result<Bytes, ErrorMessage>>,
}

impl From<mpsc::Receiver<Result<Bytes, ErrorMessage>>> for ExecRx {
    fn from(rx: mpsc::Receiver<Result<Bytes, ErrorMessage>>) -> Self {
        ExecRx::Chan(rx)
    }
}

impl ExecRx {
    /// A source that yields `item` and then ends.
    pub fn once(item: Result<Bytes, ErrorMessage>) -> Self {
        ExecRx::Direct(Box::new(DirectRx { chunks: None, validator: None, first: Some(item) }))
    }

    /// A source that ends at once.
    pub fn empty() -> Self {
        ExecRx::Direct(Box::new(DirectRx { chunks: None, validator: None, first: None }))
    }

    /// Next item; `None` at a clean end.
    pub async fn recv(&mut self) -> Option<Result<Bytes, ErrorMessage>> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, ErrorMessage>>> {
        match self {
            ExecRx::Chan(rx) => rx.poll_recv(cx),
            ExecRx::Direct(d) => d.poll_recv(cx),
        }
    }
}

impl DirectRx {
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, ErrorMessage>>> {
        if let Some(item) = self.first.take() {
            return Poll::Ready(Some(item));
        }
        let Some(chunks) = self.chunks.as_mut() else {
            return Poll::Ready(None);
        };
        loop {
            match chunks.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    self.chunks = None;
                    if let Some(v) = self.validator.as_mut()
                        && let Err(msg) = v.finish()
                    {
                        return Poll::Ready(Some(Err(ErrorMessage::new(502, msg))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Err(err))) => {
                    self.chunks = None;
                    return Poll::Ready(Some(Err(exec_error_message(&err))));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    match validate_payload(&mut self.validator, chunk) {
                        Ok(Some(p)) => return Poll::Ready(Some(Ok(p))),
                        Ok(None) => {}
                        Err(e) => {
                            self.chunks = None;
                            return Poll::Ready(Some(Err(e)));
                        }
                    }
                }
            }
        }
    }
}

/// Successful non-stream execution.
pub struct ExecOk {
    pub body: Bytes,
    /// Upstream headers to forward (empty unless `passthrough-headers`).
    pub headers: HeaderMap,
}

/// Callback receiving the auth id of each credential pick.
pub type SelectedAuthFn = Arc<dyn Fn(&str) + Send + Sync>;

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
    pub on_selected_auth: Option<SelectedAuthFn>,
    /// Plugin whose interceptors and routers are skipped: the caller of a nested host model
    /// execution (Go: `SkipInterceptorPluginID` / `SkipRouterPluginID`).
    pub skip_plugin_id: Option<&'a str>,
    /// Outbound proxy override for this execution only (Go: `ProxyURL`).
    pub proxy_url: Option<&'a str>,
    /// Request path override reported to executors and plugins (Go: `Path`).
    pub request_path: Option<&'a str>,
    /// The call is a plugin host model callback (Go: `InternalSource`).
    pub internal_source: bool,
    /// Explicit request headers / query instead of the inbound ones (Go: `modelExecutionHeaders`).
    pub headers: Option<HeaderMap>,
    pub query: Option<Vec<(String, String)>>,
    /// Handler-level source type the translator `Format` cannot express (`openai-image`,
    /// `openai-video`); stored as the `handler_type` metadata the executors read.
    pub handler_type: Option<&'a str>,
    /// Skip known free-tier credentials (Go: `WithDisallowFreeAuth`).
    pub disallow_free_auth: bool,
    /// Client frames for a steering executor stream (Go: `WithWebsocketInput`).
    pub ws_input: Option<cpa_runtime::executor::WebsocketInput>,
    /// Live credential-state check of the bound socket (Go: `WithWebsocketAuthCheck`).
    pub ws_auth_check: Option<cpa_runtime::executor::WebsocketAuthCheck>,
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
            skip_plugin_id: None,
            proxy_url: None,
            request_path: None,
            internal_source: false,
            headers: None,
            query: None,
            handler_type: None,
            disallow_free_auth: false,
            ws_input: None,
            ws_auth_check: None,
        }
    }

    /// Image / video endpoints: the entry format is only a placeholder, executors dispatch on
    /// the handler type (Go: `SourceFormat` `openai-image` / `openai-video`).
    pub fn handler(handler_type: &'a str, model: &'a str, body: Bytes) -> Self {
        let mut args = ExecArgs::new(Format::OpenAI, model, body, "");
        args.handler_type = Some(handler_type);
        args
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
        if let Some(handler_type) = a.handler_type {
            md.insert(cpa_executors::openai_compat::META_HANDLER_TYPE.into(), json!(handler_type));
        }
        if a.disallow_free_auth {
            md.insert(meta::DISALLOW_FREE_AUTH.into(), json!(true));
        }
        if a.downstream_websocket {
            md.insert(cpa_executors::codex::META_DOWNSTREAM_WEBSOCKET.into(), json!(true));
        }
        if a.required_upstream_websocket {
            md.insert(cpa_executors::codex::META_REQUIRED_UPSTREAM_WEBSOCKET.into(), json!(true));
        }
        if let Some(path) = a.request_path.map(str::trim).filter(|p| !p.is_empty()) {
            md.insert(meta::REQUEST_PATH.into(), json!(path));
        }
        md.insert(meta::REQUESTED_MODEL.into(), json!(a.model));
        if a.internal_source {
            md.insert("source".into(), json!("plugin_host_model_callback"));
        }
        if let Some(sel) = a.auth_selection_model.map(str::trim).filter(|s| !s.is_empty()) {
            md.insert(meta::AUTH_SELECTION_MODEL.into(), json!(sel));
        }
        // One validation and top-level scan of the body serves the effort, service tier and generate flag.
        let provider = a.handler_type.unwrap_or(a.entry.as_str());
        let body_root = crate::bodyview::mini_root(&a.body, metadata_keys(provider));
        let effort = extract_reasoning_effort(body_root.as_ref(), provider, normalized_model);
        if !effort.is_empty() {
            md.insert(meta::REASONING_EFFORT.into(), json!(effort));
        }
        md.insert(meta::SERVICE_TIER.into(), json!(service_tier(body_root.as_ref())));
        md.insert(meta::GENERATE.into(), json!(generate_flag(body_root.as_ref())));
        md
    }

    pub(crate) fn build_request(&self, a: &ExecArgs<'_>, normalized_model: &str, stream: bool, count: bool) -> (Request, Options) {
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
        opts.headers = match &a.headers {
            Some(h) if !h.is_empty() => h.clone(),
            _ => self.info.headers.clone(),
        };
        opts.query = match &a.query {
            Some(q) if !q.is_empty() => q.clone(),
            _ => self.info.query.clone(),
        };
        if let Some(p) = a.proxy_url.map(str::trim).filter(|p| !p.is_empty()) {
            opts.proxy_url = p.to_string();
        }
        opts.original_request = a.body.clone();
        if !count {
            opts.response_format = Some(a.exit.unwrap_or(a.entry));
        }
        opts.metadata = metadata;
        opts.api_log = self.info.api_log.exec_handle();
        opts.ws_input = a.ws_input.clone();
        opts.ws_auth_check = a.ws_auth_check.clone();
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

    pub(crate) fn providers(&self, a: &ExecArgs<'_>) -> Result<(Vec<String>, String), ErrorMessage> {
        let (providers, normalized) = self.providers_for_execution(a.model, a.allow_image_model, a.forced_provider)?;
        if a.handler_type.is_some() {
            // Handler-level types never use the native Interactions provider.
            return Ok((exclude_provider(providers, constant::GEMINI_INTERACTIONS), normalized));
        }
        Ok((adjust_providers_for_entry(a.entry, providers), normalized))
    }

    // ------------------------------------------------------------- execution

    /// `ExecuteWithAuthManager`: non-streaming execution.
    pub async fn execute(&self, a: ExecArgs<'_>) -> Result<ExecOk, ErrorMessage> {
        if let Some(pcx) = self.plugin_cx(&a) {
            return Box::pin(self.execute_plugins(&pcx, a, false)).await;
        }
        let (providers, normalized) = self.providers(&a)?;
        let (req, opts) = self.build_request(&a, &normalized, false, false);
        // The conductor futures are tens of KB; boxing keeps the handler's own future small
        // (every await point of a handler would otherwise carry and move them inline).
        let resp = Box::pin(self.state.manager.execute(&providers, req, opts))
            .await
            .map_err(|e| exec_error_message(&enrich_auth_selection_error(&e, &providers, &normalized)))?;
        Ok(self.finish_ok(resp.payload, &resp.headers))
    }

    /// `ExecuteCountWithAuthManager`.
    pub async fn execute_count(&self, a: ExecArgs<'_>) -> Result<ExecOk, ErrorMessage> {
        if let Some(pcx) = self.plugin_cx(&a) {
            return Box::pin(self.execute_plugins(&pcx, a, true)).await;
        }
        let (providers, normalized) = self.providers(&a)?;
        let (req, opts) = self.build_request(&a, &normalized, false, true);
        let resp = Box::pin(self.state.manager.execute_count(&providers, req, opts))
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
        if let Some(pcx) = self.plugin_cx(&a) {
            return Box::pin(self.execute_stream_plugins(&pcx, a)).await;
        }
        let (providers, normalized) = match self.providers(&a) {
            Ok(v) => v,
            Err(e) => return ExecStream::failed(e),
        };
        let (req, opts) = self.build_request(&a, &normalized, true, false);
        let enrich = |e: &ExecError| enrich_auth_selection_error(e, &providers, &normalized);

        // Retries (only with `streaming.bootstrap-retries`) need their own copy of the request.
        let retry_src = (self.settings.bootstrap_retries > 0).then(|| (req.clone(), opts.clone()));
        let first = Box::pin(self.state.manager.execute_stream(&providers, req, opts)).await;
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
                    // `retry_src` exists exactly when `max_retries > 0`.
                    let src = retry_src.as_ref().filter(|_| retries < max_retries && bootstrap_eligible(err.status));
                    let Some((retry_req, retry_opts)) = src.cloned() else {
                        bootstrap_err = Some(exec_error_message(&err));
                        break;
                    };
                    retries += 1;
                    match Box::pin(self.state.manager.execute_stream(&providers, retry_req, retry_opts)).await {
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
        // The consumer polls the conductor's channel directly (see [`ExecRx`]).
        let rx = match (bootstrap_err, bootstrap_payload) {
            (Some(err), _) => ExecRx::once(Err(err)),
            (None, first) => ExecRx::Direct(Box::new(DirectRx {
                chunks: Some(stream.chunks),
                validator,
                first: first.map(Ok),
            })),
        };
        ExecStream { headers, rx }
    }
}

/// `bootstrapEligible`.
pub(crate) fn bootstrap_eligible(status: u16) -> bool {
    status == 0 || matches!(status, 401 | 402 | 403 | 408 | 429) || status >= 500
}

pub(crate) enum Initial {
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
pub(crate) fn validate_payload(validator: &mut Option<SseJsonValidator>, chunk: Bytes) -> Result<Option<Bytes>, ErrorMessage> {
    let Some(v) = validator else {
        return Ok(Some(chunk));
    };
    match v.add_chunk(&chunk) {
        Ok(out) if out.is_empty() => Ok(None),
        Ok(out) => Ok(Some(Bytes::from(out))),
        Err(msg) => Err(ErrorMessage::new(502, msg)),
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
pub(crate) fn validate_image_only_model(model: &str, allow_image: bool) -> Result<(), ErrorMessage> {
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

pub fn exclude_provider(mut providers: Vec<String>, excluded: &str) -> Vec<String> {
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
fn service_tier(root: Option<&serde_json::Value>) -> String {
    if let Some(root) = root {
        let node = cpa_json::J::g(root, "service_tier");
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
fn generate_flag(root: Option<&serde_json::Value>) -> bool {
    let Some(root) = root else {
        return true;
    };
    let node = cpa_json::J::g(root, "generate");
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
        let root = |b: &[u8]| crate::bodyview::mini_root(b, metadata_keys("claude"));
        assert_eq!(service_tier(root(br#"{"service_tier":" flex "}"#).as_ref()), "flex");
        assert_eq!(service_tier(root(b"{}").as_ref()), "auto");
        assert_eq!(service_tier(root(b"{").as_ref()), "auto");
        assert!(generate_flag(root(b"{}").as_ref()));
        assert!(generate_flag(root(br#"{"generate":"no"}"#).as_ref()));
        assert!(!generate_flag(root(br#"{"generate":false}"#).as_ref()));
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

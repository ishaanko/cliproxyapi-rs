//! Plugin hooks of the handler pipeline (Go: sdk/api/handlers `handlers_interceptors.go`,
//! `handlers_routing.go` and the plugin branches of `handlers_execution.go` /
//! `handlers_stream.go`): model routing, request/response/stream interceptors, request
//! lifecycle notifications and execution through a routed plugin executor.
//!
//! Everything here is inert unless the plugin host has an active plugin; [`Pipeline`] only
//! enters this module through [`Pipeline::plugin_cx`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::http::HeaderMap;
use bytes::Bytes;
use chrono::Utc;
use cpa_core::format::Format;
use cpa_executors::helps::usage::UsageReporter;
use cpa_plugin::convert::{headers_from_go, headers_to_go, plugin_visible_metadata, query_to_go};
use cpa_plugin::{CallCtx, Host};
use cpa_pluginapi::api::{
    Header, ModelRouteRequest, RequestCompletion, RequestInterceptRequest, ResponseInterceptRequest, StreamChunkInterceptRequest,
    WebSocketResponseEvent as PluginWsEvent,
};
use cpa_runtime::executor::{
    Metadata, Options, Request, RequestAfterAuthInterceptRequest, RequestAfterAuthInterceptResponse, RequestAfterAuthInterceptor,
    WebSocketResponseEvent, WebSocketResponseObserver, meta,
};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::error::{ErrorMessage, exec_error_message, normalized_termination_status};
use crate::exec::{ExecArgs, ExecOk, Pipeline};
use crate::headers::filter_upstream_headers;

/// Maximum history chunks and bytes retained for stream interceptors.
const MAX_STREAM_HISTORY_CHUNKS: usize = 64;
const MAX_STREAM_HISTORY_BYTES: usize = 1 << 20;

/// Plugin host plus the per-request call context (Go: the request `context.Context` and the
/// interceptor skip id). Dropping it cancels calls still running for this request.
#[derive(Clone)]
pub(crate) struct PluginCx {
    pub host: Arc<Host>,
    pub ctx: CallCtx,
    pub skip: String,
    pub trace_id: String,
    _cancel: Arc<tokio_util::sync::DropGuard>,
}

/// A model router decision (Go: `modelRouteDecision`).
#[derive(Debug, Clone, Default)]
pub(crate) struct RouteDecision {
    pub executor_plugin_id: String,
    pub provider: String,
    pub model: String,
}

impl Pipeline {
    /// The plugin context when the host has any active plugin.
    pub(crate) fn plugin_cx(&self, a: &ExecArgs<'_>) -> Option<PluginCx> {
        let host = self.state.plugins.clone()?;
        if !host.has_active_plugins() {
            return None;
        }
        Some(self.new_plugin_cx(host, a.skip_plugin_id.unwrap_or("")))
    }

    pub(crate) fn new_plugin_cx(&self, host: Arc<Host>, skip: &str) -> PluginCx {
        let ctx = CallCtx::background().with_request_id(self.info.request_id.clone()).with_ext(Arc::new(self.info.clone()));
        let guard = ctx.token().clone().drop_guard();
        PluginCx { host, ctx, skip: skip.trim().to_string(), trace_id: self.info.request_id.clone(), _cancel: Arc::new(guard) }
    }

    // ---------------------------------------------------------------- routing

    /// `applyModelRouter`: the first plugin router decision that is usable.
    pub(crate) async fn apply_model_router(&self, pcx: &PluginCx, a: &ExecArgs<'_>, stream: bool) -> RouteDecision {
        let host = &pcx.host;
        let enabled = if pcx.skip.is_empty() { host.has_model_routers() } else { host.has_model_routers_except(&pcx.skip) };
        if !enabled {
            return RouteDecision::default();
        }
        let mut md = self.router_metadata(a);
        md.insert(meta::REQUESTED_MODEL.into(), Value::String(a.model.to_string()));
        if a.internal_source {
            md.insert("source".into(), Value::String("plugin_host_model_callback".into()));
        }
        let headers = a.headers.clone().unwrap_or_else(|| self.info.headers.clone());
        let query = a.query.clone().unwrap_or_else(|| self.info.query.clone());
        let req = ModelRouteRequest {
            source_format: a.entry.as_str().to_string(),
            requested_model: a.model.to_string(),
            stream,
            headers: headers_to_go(&headers),
            query: query_to_go(&query),
            body: a.body.to_vec(),
            metadata: plugin_visible_metadata(&md),
            ..Default::default()
        };
        let Some(resp) = host.route_model(&pcx.ctx, req, &pcx.skip).await else { return RouteDecision::default() };
        if !resp.handled {
            return RouteDecision::default();
        }
        match resp.target_kind.as_str() {
            cpa_pluginapi::api::ROUTE_TARGET_SELF | cpa_pluginapi::api::ROUTE_TARGET_EXECUTOR => {
                RouteDecision { executor_plugin_id: resp.target.trim().to_string(), ..Default::default() }
            }
            cpa_pluginapi::api::ROUTE_TARGET_PROVIDER => RouteDecision {
                provider: resp.target.trim().to_lowercase(),
                model: resp.target_model.trim().to_string(),
                ..Default::default()
            },
            _ => RouteDecision::default(),
        }
    }

    /// `requestExecutionMetadata` as the router sees it.
    fn router_metadata(&self, a: &ExecArgs<'_>) -> Metadata {
        let mut md = Metadata::new();
        let idempotency = self.info.header("Idempotency-Key");
        if !idempotency.is_empty() {
            md.insert("idempotency_key".into(), Value::String(idempotency));
        }
        let path = self.info.route.trim();
        let path = if path.is_empty() { self.info.path.trim() } else { path };
        if !path.is_empty() {
            md.insert(meta::REQUEST_PATH.into(), Value::String(path.to_string()));
        }
        if let Some(pinned) = a.pinned_auth_id.map(str::trim).filter(|s| !s.is_empty()) {
            md.insert(meta::PINNED_AUTH_ID.into(), Value::String(pinned.to_string()));
        }
        if let Some(session) = a.execution_session_id.map(str::trim).filter(|s| !s.is_empty()) {
            md.insert(meta::EXECUTION_SESSION_ID.into(), Value::String(session.to_string()));
        }
        if let Some(key) = &self.info.api_key {
            let scope = crate::exec::caller_scope(key);
            if !scope.is_empty() {
                md.insert(meta::CALLER_SCOPE.into(), Value::String(scope));
            }
        }
        md
    }

    // ---------------------------------------------------------------- interceptors

    fn interceptors_enabled(pcx: &PluginCx) -> bool {
        pcx.host.has_request_interceptors()
    }

    /// `applyRequestInterceptorsBeforeAuth`; `Err` is a plugin termination.
    pub(crate) async fn intercept_before_auth(
        &self,
        pcx: &PluginCx,
        entry: Format,
        original_model: &str,
        request_id: &str,
        mut req: Request,
        mut opts: Options,
    ) -> Result<(Request, Options), ErrorMessage> {
        if !Self::interceptors_enabled(pcx) {
            return Ok((req, opts));
        }
        let (resp, ran) = pcx
            .host
            .intercept_request_before_auth(
                &pcx.ctx,
                RequestInterceptRequest {
                    request_id: request_id.to_string(),
                    trace_id: pcx.trace_id.clone(),
                    source_format: entry.as_str().to_string(),
                    to_format: String::new(),
                    model: req.model.clone(),
                    requested_model: original_model.to_string(),
                    stream: opts.stream,
                    headers: headers_to_go(&opts.headers),
                    body: req.payload.to_vec(),
                    metadata: plugin_visible_metadata(&opts.metadata),
                },
                &pcx.skip,
            )
            .await;
        if ran {
            opts.headers = headers_from_go(&resp.headers);
        }
        if !resp.body.is_empty() {
            req.payload = Bytes::from(resp.body.clone());
            opts.original_request = Bytes::from(resp.body.clone());
        }
        if !resp.path.trim().is_empty() {
            opts.metadata.insert(meta::REQUEST_PATH.into(), Value::String(resp.path.trim().to_string()));
        }
        if resp.terminate {
            return Err(termination_error(resp.status_code, &resp.response_headers, &resp.response_body));
        }
        Ok((req, opts))
    }

    /// `requestAfterAuthInterceptor`: the hook the conductor runs per attempt, recording what the
    /// plugins changed so the response interceptors see the executed request.
    pub(crate) fn after_auth_interceptor(
        &self,
        pcx: &PluginCx,
        capture: Arc<AfterAuthCapture>,
        request_id: String,
    ) -> Option<RequestAfterAuthInterceptor> {
        if !Self::interceptors_enabled(pcx) {
            return None;
        }
        let pcx = pcx.clone();
        Some(RequestAfterAuthInterceptor(Arc::new(move |req| {
            let pcx = pcx.clone();
            let capture = capture.clone();
            let request_id = request_id.clone();
            Box::pin(async move {
                let resp = apply_after_auth(&pcx, &req, &request_id).await;
                capture.record(&req, &resp);
                resp
            })
        })))
    }

    /// `webSocketResponseObserver`.
    pub(crate) fn ws_observer(&self, pcx: &PluginCx, request_id: String) -> Option<WebSocketResponseObserver> {
        if !pcx.host.has_websocket_response_observers() {
            return None;
        }
        let pcx = pcx.clone();
        Some(WebSocketResponseObserver(Arc::new(move |ev: WebSocketResponseEvent| {
            let pcx = pcx.clone();
            let request_id = request_id.clone();
            tokio::spawn(async move {
                let event = PluginWsEvent {
                    request_id: if ev.request_id.is_empty() { request_id } else { ev.request_id },
                    trace_id: if ev.trace_id.is_empty() { pcx.trace_id.clone() } else { ev.trace_id },
                    source_format: ev.source_format,
                    model: ev.model,
                    requested_model: ev.requested_model,
                    provider: ev.provider,
                    auth_id: ev.auth_id,
                    auth_label: ev.auth_label,
                    auth_type: ev.auth_type,
                    event_type: ev.event_type,
                    payload: ev.payload.to_vec(),
                    metadata: plugin_visible_metadata(&ev.metadata),
                };
                pcx.host.observe_websocket_response_event(&pcx.ctx, event, &pcx.skip).await;
            });
        })))
    }

    /// `applyResponseInterceptors`: returns the final body and the downstream headers.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn apply_response_interceptors(
        &self,
        pcx: &PluginCx,
        request_id: &str,
        handler_type: Format,
        model: &str,
        requested_model: &str,
        opts: &Options,
        raw_headers: &HeaderMap,
        original_request: &[u8],
        request_body: &[u8],
        body: Bytes,
        passthrough: bool,
    ) -> (Bytes, HeaderMap) {
        let raw_go = headers_to_go(raw_headers);
        let (resp, ran) = pcx
            .host
            .intercept_response(
                &pcx.ctx,
                ResponseInterceptRequest {
                    request_id: request_id.to_string(),
                    source_format: handler_type.as_str().to_string(),
                    model: model.to_string(),
                    requested_model: requested_model.to_string(),
                    stream: false,
                    request_headers: headers_to_go(&opts.headers),
                    response_headers: raw_go.clone(),
                    original_request: original_request.to_vec(),
                    request_body: request_body.to_vec(),
                    body: body.to_vec(),
                    status_code: 200,
                    metadata: plugin_visible_metadata(&opts.metadata),
                },
                &pcx.skip,
            )
            .await;
        let final_headers = if ran { headers_from_go(&resp.headers) } else { raw_headers.clone() };
        let headers = downstream_headers_after_interceptors(raw_headers, &final_headers, passthrough);
        let body = if resp.body.is_empty() { body } else { Bytes::from(resp.body) };
        (body, headers)
    }

    /// `applyRequestInterceptorsAfterPluginExecutorRoute`.
    pub(crate) async fn intercept_after_plugin_route(
        &self,
        pcx: &PluginCx,
        executor_plugin_id: &str,
        entry: Format,
        original_model: &str,
        request_id: &str,
        mut req: Request,
        mut opts: Options,
    ) -> Result<(Request, Options), ErrorMessage> {
        if !Self::interceptors_enabled(pcx) {
            return Ok((req, opts));
        }
        let to_format = pcx.host.plugin_executor_request_to_format(executor_plugin_id, &req, &opts).unwrap_or(entry);
        let (resp, ran) = pcx
            .host
            .intercept_request_after_auth(
                &pcx.ctx,
                RequestInterceptRequest {
                    request_id: request_id.to_string(),
                    trace_id: pcx.trace_id.clone(),
                    source_format: opts.source_format.as_str().to_string(),
                    to_format: to_format.as_str().to_string(),
                    model: req.model.clone(),
                    requested_model: original_model.to_string(),
                    stream: opts.stream,
                    headers: headers_to_go(&opts.headers),
                    body: req.payload.to_vec(),
                    metadata: plugin_visible_metadata(&opts.metadata),
                },
                &pcx.skip,
            )
            .await;
        if ran {
            // Go merges the full header set back through `mergeRequestInterceptorHeaders`.
            opts.headers = merge_request_interceptor_headers(&opts.headers, Some(&headers_from_go(&resp.headers)), &[]);
        }
        if !resp.body.is_empty() {
            req.payload = Bytes::from(resp.body.clone());
            opts.original_request = Bytes::from(resp.body.clone());
        }
        let path = resp.path.trim();
        if !path.is_empty() {
            opts.metadata.insert(meta::REQUEST_PATH.into(), Value::String(path.to_string()));
        }
        if resp.terminate {
            return Err(termination_error(resp.status_code, &resp.response_headers, &resp.response_body));
        }
        Ok((req, opts))
    }

    // ---------------------------------------------------------------- plugin executor route

    /// `executeWithPluginExecutor`.
    pub(crate) async fn execute_with_plugin_executor(
        &self,
        pcx: &PluginCx,
        a: &ExecArgs<'_>,
        executor_plugin_id: &str,
    ) -> Result<ExecOk, ErrorMessage> {
        let (req, opts) = self.plugin_executor_request(pcx, a, false, a.exit.unwrap_or(a.entry));
        let lifecycle = Lifecycle::new(pcx, a.entry, a.model, a.model, false, &opts.metadata);
        let (req, opts) = match self.intercept_before_auth(pcx, a.entry, a.model, &lifecycle.request_id, req, opts).await {
            Ok(v) => v,
            Err(e) => {
                lifecycle.complete_error(&e);
                return Err(e);
            }
        };
        let (req, opts) = match self.intercept_after_plugin_route(pcx, executor_plugin_id, a.entry, a.model, &lifecycle.request_id, req, opts).await {
            Ok(v) => v,
            Err(e) => {
                lifecycle.complete_error(&e);
                return Err(e);
            }
        };
        let reporter = (!a.internal_source).then(|| {
            let r = UsageReporter::new(executor_plugin_id, "", a.model, None, Some(&opts));
            r.set_translated_reasoning_effort(&req.payload, a.entry.as_str());
            r
        });
        let response_protocol = a.exit.unwrap_or(a.entry);
        let resp = match pcx.host.execute_plugin_executor(&pcx.ctx, executor_plugin_id, req.clone(), opts.clone()).await {
            Ok(r) => r,
            Err(e) => {
                if let Some(r) = &reporter
                    && !pcx.ctx.has_nested()
                {
                    r.publish_failure(&e);
                    self.publish_route_usage(r, a);
                }
                let msg = exec_error_message(&e);
                lifecycle.complete_error(&msg);
                return Err(msg);
            }
        };
        if let Some(r) = &reporter
            && !pcx.ctx.has_nested()
        {
            r.publish(cpa_plugin::usage_helpers::parse_plugin_executor_response_usage(response_protocol.as_str(), &resp.payload));
            r.ensure_published();
            self.publish_route_usage(r, a);
        }
        let passthrough = a.internal_source || self.settings.passthrough_headers;
        let (body, headers) = self
            .apply_response_interceptors(
                pcx,
                &lifecycle.request_id,
                response_protocol,
                a.model,
                a.model,
                &opts,
                &resp.headers,
                &opts.original_request,
                &req.payload,
                resp.payload,
                passthrough,
            )
            .await;
        lifecycle.complete("succeeded", 200, None);
        Ok(ExecOk { body, headers })
    }

    /// `countWithPluginExecutor`.
    pub(crate) async fn count_with_plugin_executor(
        &self,
        pcx: &PluginCx,
        a: &ExecArgs<'_>,
        executor_plugin_id: &str,
    ) -> Result<ExecOk, ErrorMessage> {
        let (req, opts) = self.plugin_executor_request(pcx, a, false, a.entry);
        let lifecycle = Lifecycle::new(pcx, a.entry, a.model, a.model, false, &opts.metadata);
        let (req, opts) = match self.intercept_before_auth(pcx, a.entry, a.model, &lifecycle.request_id, req, opts).await {
            Ok(v) => v,
            Err(e) => {
                lifecycle.complete_error(&e);
                return Err(e);
            }
        };
        let (req, opts) = match self.intercept_after_plugin_route(pcx, executor_plugin_id, a.entry, a.model, &lifecycle.request_id, req, opts).await {
            Ok(v) => v,
            Err(e) => {
                lifecycle.complete_error(&e);
                return Err(e);
            }
        };
        let resp = match pcx.host.count_plugin_executor(&pcx.ctx, executor_plugin_id, req.clone(), opts.clone()).await {
            Ok(r) => r,
            Err(e) => {
                let msg = exec_error_message(&e);
                lifecycle.complete_error(&msg);
                return Err(msg);
            }
        };
        let passthrough = a.internal_source || self.settings.passthrough_headers;
        let (body, headers) = self
            .apply_response_interceptors(
                pcx,
                &lifecycle.request_id,
                a.entry,
                a.model,
                a.model,
                &opts,
                &resp.headers,
                &opts.original_request,
                &req.payload,
                resp.payload,
                passthrough,
            )
            .await;
        lifecycle.complete("succeeded", 200, None);
        Ok(ExecOk { body, headers })
    }

    /// `pluginExecutorRequest`.
    pub(crate) fn plugin_executor_request(&self, pcx: &PluginCx, a: &ExecArgs<'_>, stream: bool, response: Format) -> (Request, Options) {
        let (req, mut opts) = self.build_request(a, a.model, stream, false);
        opts.response_format = Some(response);
        opts.selected_auth = None;
        opts.websocket_response_observer = self.ws_observer(pcx, String::new());
        (req, opts)
    }

    /// Records the usage of a handler-level plugin executor call (Go: the reporter publishing to
    /// the usage manager).
    fn publish_route_usage(&self, reporter: &UsageReporter, a: &ExecArgs<'_>) {
        let Some(rec) = reporter.record() else { return };
        let path = self.info.route.trim();
        let path = if path.is_empty() { self.info.path.trim() } else { path };
        let tokens = cpa_runtime::usage::TokenUsage {
            input_tokens: rec.detail.input_tokens,
            output_tokens: rec.detail.output_tokens,
            reasoning_tokens: rec.detail.reasoning_tokens,
            cached_tokens: rec.detail.cached_tokens,
            total_tokens: rec.detail.total_tokens,
        };
        let model = rec.model.clone();
        let alias = if !rec.alias.is_empty() && rec.alias != model { rec.alias.clone() } else { String::new() };
        let record = cpa_runtime::usage::UsageRecord {
            timestamp: rec.requested_at,
            latency_ms: i64::try_from(rec.latency.as_millis()).unwrap_or(i64::MAX),
            ttft_ms: i64::try_from(rec.ttft.as_millis()).unwrap_or(i64::MAX),
            source: rec.source.clone(),
            auth_index: rec.auth_index.clone(),
            auth_type: rec.auth_type.clone(),
            provider: rec.provider.clone(),
            executor_type: rec.executor_type.clone(),
            model,
            alias,
            endpoint: if path.is_empty() { String::new() } else { format!("{} {path}", self.info.method) },
            api_key: rec.api_key.clone(),
            request_id: self.info.request_id.clone(),
            failed: rec.failed,
            stream: rec.stream,
            fail: cpa_runtime::usage::UsageFailure { status_code: rec.fail.status_code, body: rec.fail.body.clone() },
            tokens,
            extra: Default::default(),
        };
        let _ = a;
        self.state.usage.record(record);
    }
}

/// Go `directTerminationError`.
fn termination_error(status: i64, headers: &Header, body: &[u8]) -> ErrorMessage {
    ErrorMessage {
        status: normalized_termination_status(status),
        direct: Some(Arc::new(crate::error::DirectResponse { body: Bytes::copy_from_slice(body), headers: headers_from_go(headers) })),
        ..Default::default()
    }
}

/// `applyRequestInterceptorsAfterAuth`: the after-auth call made by the conductor.
async fn apply_after_auth(pcx: &PluginCx, req: &RequestAfterAuthInterceptRequest, request_id: &str) -> RequestAfterAuthInterceptResponse {
    if !pcx.host.has_request_interceptors() {
        return RequestAfterAuthInterceptResponse::default();
    }
    let (resp, ran) = pcx
        .host
        .intercept_request_after_auth(
            &pcx.ctx,
            RequestInterceptRequest {
                request_id: request_id.to_string(),
                trace_id: pcx.trace_id.clone(),
                source_format: req.source_format.as_str().to_string(),
                to_format: req.to_format.as_str().to_string(),
                model: req.model.clone(),
                requested_model: req.requested_model.clone(),
                stream: req.stream,
                headers: headers_to_go(&req.headers),
                body: req.body.to_vec(),
                metadata: plugin_visible_metadata(&req.metadata),
            },
            &pcx.skip,
        )
        .await;
    RequestAfterAuthInterceptResponse {
        path: resp.path.trim().to_string(),
        headers: ran.then(|| headers_from_go(&resp.headers)),
        body: Bytes::from(resp.body),
        clear_headers: resp.clear_headers,
        terminate: resp.terminate,
        status_code: normalized_termination_status(resp.status_code),
        response_headers: headers_from_go(&resp.response_headers),
        response_body: Bytes::from(resp.response_body),
    }
}

/// What the after-auth interceptors changed, applied to the request the response interceptors
/// report (Go: `requestAfterAuthCapture`).
#[derive(Default)]
pub(crate) struct AfterAuthCapture {
    state: Mutex<CaptureState>,
}

#[derive(Default)]
struct CaptureState {
    set: bool,
    headers: HeaderMap,
    body: Bytes,
    original_request: Bytes,
    original_request_replaced: bool,
    path: String,
}

impl AfterAuthCapture {
    fn record(&self, req: &RequestAfterAuthInterceptRequest, resp: &RequestAfterAuthInterceptResponse) {
        let headers = merge_request_interceptor_headers(&req.headers, resp.headers.as_ref(), &resp.clear_headers);
        let mut st = self.state.lock();
        st.set = true;
        st.headers = headers;
        st.original_request_replaced = !resp.body.is_empty();
        st.body = resp.body.clone();
        st.original_request = resp.body.clone();
        st.path = resp.path.trim().to_string();
    }

    /// `apply`: the request as executed.
    pub(crate) fn apply(&self, mut req: Request, mut opts: Options) -> (Request, Options) {
        let st = self.state.lock();
        if !st.set {
            return (req, opts);
        }
        if st.original_request_replaced {
            req.payload = st.body.clone();
            opts.original_request = st.original_request.clone();
        }
        opts.headers = st.headers.clone();
        if !st.path.is_empty() {
            opts.metadata.insert(meta::REQUEST_PATH.into(), Value::String(st.path.clone()));
        }
        (req, opts)
    }
}

/// `mergeRequestInterceptorHeaders`: remove `clear`, replace the keys of `updates`.
fn merge_request_interceptor_headers(current: &HeaderMap, updates: Option<&HeaderMap>, clear: &[String]) -> HeaderMap {
    if updates.is_none() && clear.is_empty() {
        return current.clone();
    }
    let mut out = current.clone();
    for key in clear {
        if let Ok(name) = axum::http::HeaderName::from_bytes(key.trim().as_bytes()) {
            out.remove(&name);
        }
    }
    if let Some(updates) = updates {
        let names: Vec<_> = updates.keys().cloned().collect();
        for name in names {
            out.remove(&name);
            for v in updates.get_all(&name) {
                out.append(name.clone(), v.clone());
            }
        }
    }
    out
}

/// `diffHeaders`: headers of `next` whose values differ from `base`.
fn diff_headers(base: &HeaderMap, next: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    let names: Vec<_> = next.keys().cloned().collect();
    for name in names {
        let a: Vec<&[u8]> = base.get_all(&name).iter().map(|v| v.as_bytes()).collect();
        let b: Vec<&[u8]> = next.get_all(&name).iter().map(|v| v.as_bytes()).collect();
        if a == b {
            continue;
        }
        for v in next.get_all(&name) {
            out.append(name.clone(), v.clone());
        }
    }
    out
}

/// `downstreamHeadersAfterInterceptors`.
pub(crate) fn downstream_headers_after_interceptors(base_raw: &HeaderMap, final_raw: &HeaderMap, passthrough: bool) -> HeaderMap {
    if passthrough {
        filter_upstream_headers(final_raw)
    } else {
        filter_upstream_headers(&diff_headers(base_raw, final_raw))
    }
}

// ---------------------------------------------------------------- lifecycle

/// One notification per intercepted request (Go: `requestLifecycleTracker`).
pub(crate) struct Lifecycle {
    pub request_id: String,
    host: Arc<Host>,
    ctx: CallCtx,
    skip: String,
    completion: Mutex<RequestCompletion>,
    done: AtomicBool,
}

impl Lifecycle {
    pub(crate) fn new(
        pcx: &PluginCx,
        source_format: Format,
        model: &str,
        requested_model: &str,
        stream: bool,
        metadata: &Metadata,
    ) -> Arc<Self> {
        let request_id = uuid::Uuid::new_v4().to_string();
        Arc::new(Lifecycle {
            request_id: request_id.clone(),
            host: pcx.host.clone(),
            ctx: pcx.ctx.clone(),
            skip: pcx.skip.clone(),
            completion: Mutex::new(RequestCompletion {
                request_id,
                trace_id: pcx.trace_id.clone(),
                source_format: source_format.as_str().to_string(),
                model: model.to_string(),
                requested_model: requested_model.to_string(),
                stream,
                started_at: Some(Utc::now()),
                metadata: plugin_visible_metadata(metadata),
                ..Default::default()
            }),
            done: AtomicBool::new(false),
        })
    }

    /// Sends the terminal event once.
    pub(crate) fn complete(&self, outcome: &str, status: i64, err: Option<&str>) {
        if self.done.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut completion = self.completion.lock().clone();
        completion.outcome = outcome.to_string();
        completion.status_code = status;
        completion.completed_at = Some(Utc::now());
        if let Some(e) = err {
            completion.error = e.to_string();
        }
        self.host.complete_request(&self.ctx, completion, &self.skip);
    }

    /// `completeError`: rejected for plugin terminations, failed otherwise.
    pub(crate) fn complete_error(&self, msg: &ErrorMessage) {
        let outcome = if msg.direct.is_some() { "rejected" } else { "failed" };
        self.complete(outcome, i64::from(msg.status), Some(&msg.text));
    }

    pub(crate) fn canceled(&self) {
        self.complete("canceled", 0, Some("context canceled"));
    }
}

// ---------------------------------------------------------------- stream interceptors

/// Per-stream interceptor state (Go: the closures of `executeStreamWithAuthManagerFormats`).
pub(crate) struct StreamTransform {
    pcx: PluginCx,
    request_id: String,
    response_protocol: Format,
    model: String,
    requested_model: String,
    active: bool,
    raw_headers: HeaderMap,
    base_headers: HeaderMap,
    request_headers: Header,
    original_request: Vec<u8>,
    request_body: Vec<u8>,
    metadata: Map<String, Value>,
    initialized: bool,
    chunk_index: i64,
    history: Vec<Vec<u8>>,
}

impl StreamTransform {
    pub(crate) fn new(pcx: &PluginCx, request_id: &str, response_protocol: Format, model: &str, requested_model: &str, headers: &HeaderMap) -> Self {
        StreamTransform {
            pcx: pcx.clone(),
            request_id: request_id.to_string(),
            response_protocol,
            model: model.to_string(),
            requested_model: requested_model.to_string(),
            active: pcx.host.has_stream_interceptors(),
            raw_headers: headers.clone(),
            base_headers: headers.clone(),
            request_headers: Header::new(),
            original_request: Vec::new(),
            request_body: Vec::new(),
            metadata: Map::new(),
            initialized: false,
            chunk_index: 0,
            history: Vec::new(),
        }
    }

    /// New upstream stream after a bootstrap retry.
    pub(crate) fn reset(&mut self, headers: &HeaderMap) {
        self.raw_headers = headers.clone();
        self.base_headers = headers.clone();
        self.initialized = false;
        self.chunk_index = 0;
        self.history.clear();
    }

    /// The executed request, once known (after-auth capture applied).
    pub(crate) fn set_request(&mut self, req: &Request, opts: &Options) {
        self.request_headers = headers_to_go(&opts.headers);
        self.original_request = opts.original_request.to_vec();
        self.request_body = req.payload.to_vec();
        self.metadata = plugin_visible_metadata(&opts.metadata);
    }

    /// Downstream response headers after the stream interceptors (Go: `upstreamHeaders`).
    pub(crate) fn downstream_headers(&self, passthrough: bool) -> HeaderMap {
        downstream_headers_after_interceptors(&self.base_headers, &self.raw_headers, passthrough)
    }

    /// `applyStreamHeaderInit`: the header-only interceptor call.
    pub(crate) async fn init_headers(&mut self) {
        if !self.active || self.initialized {
            return;
        }
        let (resp, ran) = self
            .pcx
            .host
            .intercept_stream_chunk(
                &self.pcx.ctx,
                StreamChunkInterceptRequest {
                    request_id: self.request_id.clone(),
                    source_format: self.response_protocol.as_str().to_string(),
                    model: self.model.clone(),
                    requested_model: self.requested_model.clone(),
                    request_headers: self.request_headers.clone(),
                    response_headers: headers_to_go(&self.raw_headers),
                    original_request: self.original_request.clone(),
                    request_body: self.request_body.clone(),
                    chunk_index: cpa_pluginapi::api::STREAM_CHUNK_HEADER_INIT_INDEX,
                    metadata: self.metadata.clone(),
                    ..Default::default()
                },
                &self.pcx.skip,
            )
            .await;
        if ran {
            self.raw_headers = headers_from_go(&resp.headers);
        }
        self.initialized = true;
    }

    /// `transformStreamPayload` minus the SSE validation: `None` when a plugin dropped the chunk.
    pub(crate) async fn transform(&mut self, payload: Bytes) -> Option<Bytes> {
        self.init_headers().await;
        if !self.active {
            self.chunk_index += 1;
            return Some(payload);
        }
        let host = &self.pcx.host;
        let mut req = StreamChunkInterceptRequest {
            request_id: self.request_id.clone(),
            source_format: self.response_protocol.as_str().to_string(),
            model: self.model.clone(),
            requested_model: self.requested_model.clone(),
            request_headers: self.request_headers.clone(),
            response_headers: headers_to_go(&self.raw_headers),
            body: payload.to_vec(),
            chunk_index: self.chunk_index,
            metadata: self.metadata.clone(),
            ..Default::default()
        };
        if host.stream_chunk_payload_includes_history() {
            req.history_chunks = self.history.clone();
        }
        if host.stream_chunk_payload_includes_request_body() {
            req.original_request = self.original_request.clone();
            req.request_body = self.request_body.clone();
        }
        let (resp, ran) = host.intercept_stream_chunk(&self.pcx.ctx, req, &self.pcx.skip).await;
        if ran {
            self.raw_headers = headers_from_go(&resp.headers);
        }
        let payload = if resp.body.is_empty() { payload } else { Bytes::from(resp.body) };
        self.chunk_index += 1;
        if resp.drop_chunk {
            return None;
        }
        Some(payload)
    }

    /// Adds a delivered payload to the bounded interceptor history.
    pub(crate) fn delivered(&mut self, payload: &[u8]) {
        if !self.active || !self.pcx.host.stream_chunk_payload_includes_history() || payload.is_empty() {
            return;
        }
        self.history.push(payload.to_vec());
        let mut total: usize = self.history.iter().map(Vec::len).sum();
        while self.history.len() > MAX_STREAM_HISTORY_CHUNKS || total > MAX_STREAM_HISTORY_BYTES {
            let first = self.history.remove(0);
            total -= first.len();
        }
    }
}

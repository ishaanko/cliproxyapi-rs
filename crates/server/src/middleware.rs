//! HTTP middleware (Go: internal/api/server_middleware.go, internal/logging/gin_logger.go).

use std::net::SocketAddr;
use std::time::Instant;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use cpa_core::util::mask_sensitive_query;

use crate::access::{self, AuthFailure, Principal};
use crate::bodytee::TeeBody;
use crate::clientip;
use crate::logging::{self, REQUEST_ID, go_duration_string};
use crate::reply::Reply;
use crate::req::{AuthenticatedKey, RequestId, TraceHandle, parse_query};
use crate::reqlog::ApiLogHandle;
use crate::safemode;
use crate::state::AppState;

/// `corsExposedResponseHeaders`.
const CORS_EXPOSED: &str = "X-CPA-TRACE-ID, X-CPA-VERSION, X-CPA-COMMIT, X-CPA-BUILD-DATE, X-CPA-SUPPORT-PLUGIN, X-CPA-HOME-VERSION, X-CPA-HOME-BUILD-DATE, X-SERVER-VERSION, X-SERVER-BUILD-DATE, Location, Retry-After, X-Request-Id, OpenAI-Request-Id";

fn apply_cors(headers: &mut axum::http::HeaderMap) {
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("*"));
    headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static(CORS_EXPOSED));
}

/// `corsMiddleware`: CORS headers on every response except websocket 101s; `OPTIONS` answers 204 without reaching auth.
pub async fn cors(req: Request, next: Next) -> Response {
    let mut resp = if req.method() == Method::OPTIONS {
        Reply::new(204).into_response()
    } else {
        next.run(req).await
    };
    // A websocket handshake is written by gorilla straight on the hijacked connection, so the
    // gin-level CORS headers never reach a 101.
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        apply_cors(resp.headers_mut());
    }
    // axum's 405 handling adds `Allow`; gin answers 404 without it.
    resp.headers_mut().remove(header::ALLOW);
    resp
}

/// Paths the Home heartbeat gate lets through: management, plugin resources and the panel page.
fn is_home_gate_exempt(path: &str) -> bool {
    path == "/v0/management"
        || path.starts_with("/v0/management/")
        || path == "/v8/management"
        || path.starts_with("/v8/management/")
        || path.starts_with("/v0/resource/plugins/")
        || path == "/management.html"
}

/// `homeHeartbeatMiddleware`: while Home mode is on, every endpoint answers a bare 503 until the
/// Home control connection reports a healthy heartbeat.
pub async fn home_heartbeat(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if !st.cfg().home.enabled || is_home_gate_exempt(req.uri().path()) {
        return next.run(req).await;
    }
    match cpa_home::kv::current() {
        Some(client) if client.heartbeat_ok() => next.run(req).await,
        _ => Reply::new(503).into_response(),
    }
}

/// `exampleAPIKeySafeModeMiddleware`.
pub async fn safe_mode(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if !st.example_api_key_safe_mode {
        return next.run(req).await;
    }
    let cfg = st.cfg();
    if !safemode::has_example_api_keys(&cfg.api_keys) {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    let query = parse_query(req.uri().query().unwrap_or(""));
    let is_panel = path == "/management.html";
    if is_panel && query.iter().any(|(k, v)| k == "safe-mode" && v == "configure") {
        return next.run(req).await;
    }
    if (path == "/" || is_panel) && (req.method() == Method::GET || req.method() == Method::HEAD) {
        let keys = safemode::example_api_keys(&cfg.api_keys);
        let mut reply = Reply::new(200)
            .content_type("text/html; charset=utf-8")
            .with_header(header::CACHE_CONTROL, "no-store");
        if req.method() != Method::HEAD {
            reply = reply.with_body(safemode::warning_page_html(&keys, "/management.html?safe-mode=configure"));
        }
        return reply.into_response();
    }
    if !safemode::is_proxy_path(&path) {
        return next.run(req).await;
    }
    Reply::json(
        403,
        r#"{"error":"unsafe_example_api_key","message":"Proxy API endpoints are disabled because api-keys contains template values. Open /management.html?safe-mode=configure, update api-keys in Management, then retry."}"#
            .as_bytes()
            .to_vec(),
    )
    .with_header(axum::http::HeaderName::from_static("x-cpa-safe-mode"), "example-api-key")
    .into_response()
}

/// Largest request body buffered for plugin frontend auth. Go reads without a limit; this only
/// bounds unauthenticated memory use at a size no real request approaches.
const PLUGIN_AUTH_BODY_LIMIT: usize = 256 * 1024 * 1024;

/// `Manager.Authenticate` over the config api-keys and plugin frontend auth providers (shared by
/// the proxy and realtime middlewares). The body is buffered and restored when a plugin provider
/// needs it (Go: `readAndRestoreRequestBody`). `Ok(None)` means no provider is registered.
pub async fn authenticate_request(st: &AppState, req: &mut Request) -> Result<Option<Principal>, AuthFailure> {
    let cfg = st.cfg();
    let query = parse_query(req.uri().query().unwrap_or(""));
    let (plugins, exclusive) = match &st.plugins {
        Some(host) => (host.frontend_auth_providers(), host.exclusive_frontend_auth_provider().is_some()),
        None => (Vec::new(), false),
    };
    if plugins.is_empty() {
        return access::authenticate(req.headers(), &query, &cfg.api_keys);
    }
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let mut buffered: Option<bytes::Bytes> = None;
    let outcome = {
        let body_slot = &mut buffered;
        let req_body = req.body_mut();
        let access_req = access::AccessRequest { method: &method, path: &path, headers: &headers, query: &query };
        access::authenticate_chain(&access_req, &cfg.api_keys, &plugins, exclusive, || async move {
            let taken = std::mem::take(req_body);
            let bytes = axum::body::to_bytes(taken, PLUGIN_AUTH_BODY_LIMIT).await.map_err(|e| e.to_string())?;
            *body_slot = Some(bytes.clone());
            Ok(bytes)
        })
        .await
    };
    if let Some(bytes) = buffered {
        *req.body_mut() = axum::body::Body::from(bytes);
    }
    outcome
}

/// `AuthMiddleware` for the proxy route groups: API-key check, open when no provider is
/// registered; plugin frontend auth providers extend (or, when exclusive, replace) the chain.
pub async fn api_key_auth(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    match authenticate_request(&st, &mut req).await {
        Ok(Some(principal)) => {
            req.extensions_mut().insert(AuthenticatedKey(principal));
            next.run(req).await
        }
        Ok(None) => next.run(req).await,
        Err(failure) => auth_failure_reply(failure).into_response(),
    }
}

fn auth_failure_reply(failure: AuthFailure) -> Reply {
    Reply::json(failure.status(), format!(r#"{{"error":{}}}"#, cpa_core::util::go_json_string(failure.message())).into_bytes())
}

/// `CPATraceIDMiddleware`: installs the shared trace state and stamps `X-CPA-TRACE-ID` on the
/// response once a credential was selected (set when the response headers are committed).
pub async fn trace_header(mut req: Request, next: Next) -> Response {
    let trace = TraceHandle::default();
    req.extensions_mut().insert(trace.clone());
    let mut resp = next.run(req).await;
    let id = trace.0.get();
    if let Ok(value) = HeaderValue::from_str(&id)
        && !id.is_empty()
    {
        resp.headers_mut().insert("x-cpa-trace-id", value);
    }
    resp
}

/// `isAIAPIPath`: request ids are generated only for these prefixes.
fn is_ai_api_path(path: &str) -> bool {
    ["/v1", "/v1beta", "/openai/v1", "/backend-api/codex"]
        .iter()
        .any(|p| path == *p || path.strip_prefix(p).is_some_and(|r| r.starts_with('/')))
}

/// `GinLogrusLogger`: request id, access log line after the response body completes.
pub async fn access_log(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let start = Instant::now();
    let path = req.uri().path().to_string();
    let raw_query = mask_sensitive_query(req.uri().query().unwrap_or(""));
    let method = req.method().clone();
    let remote = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    let cfg = st.cfg();
    let client_ip = clientip::resolve(remote, req.headers(), &cfg.trusted_proxies);

    let request_id = if is_ai_api_path(&path) {
        logging::generate_request_id()
    } else {
        String::new()
    };
    if !request_id.is_empty() {
        req.extensions_mut().insert(RequestId(request_id.clone()));
    }
    let api_log = ApiLogHandle::default();
    api_log.0.set_error_logging(cfg.request_log);
    drop(cfg);
    req.extensions_mut().insert(api_log);

    let scope_id = request_id.clone();
    // The json memo lets repeated parses of one large request body share work within the request.
    let resp = REQUEST_ID.scope(scope_id, cpa_json::scope(next.run(req))).await;

    let status = resp.status();
    let healthz_ok = path == "/healthz"
        && (method == Method::GET || method == Method::HEAD)
        && status.is_success();
    if healthz_ok {
        return resp;
    }

    let (parts, body) = resp.into_parts();
    let display_path = if raw_query.is_empty() { path } else { format!("{path}?{raw_query}") };
    let on_done = Box::new(move || {
        let mut latency = start.elapsed();
        latency = if latency.as_secs() > 60 {
            std::time::Duration::from_secs(latency.as_secs())
        } else {
            std::time::Duration::from_millis(latency.as_millis() as u64)
        };
        let line = format!(
            "{:>3} | {:>13} | {:>15} | {:<7} \"{}\"",
            status.as_u16(),
            go_duration_string(latency),
            client_ip,
            method.as_str(),
            display_path
        );
        let rid = if request_id.is_empty() { "--------".to_string() } else { request_id };
        if status.as_u16() >= 500 {
            tracing::error!(request_id = %rid, "{line}");
        } else if status.as_u16() >= 400 {
            tracing::warn!(request_id = %rid, "{line}");
        } else {
            tracing::info!(request_id = %rid, "{line}");
        }
    });
    Response::from_parts(parts, TeeBody::wrap(body, None, on_done))
}

/// `GinLogrusRecovery` panic handler: log and answer a bare 500.
pub fn recover_panic(err: Box<dyn std::any::Any + Send + 'static>) -> Response<axum::body::Body> {
    let detail = if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        (*s).to_string()
    } else {
        "unknown panic".to_string()
    };
    tracing::error!(panic = %detail, "recovered from panic");
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    resp
}

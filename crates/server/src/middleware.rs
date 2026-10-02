//! HTTP middleware (Go: internal/api/server_middleware.go, internal/logging/gin_logger.go).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use cpa_core::util::mask_sensitive_query;

use crate::access::{self, AuthFailure};
use crate::bodytee::TeeBody;
use crate::clientip;
use crate::logging::{self, REQUEST_ID, go_duration_string};
use crate::reply::Reply;
use crate::req::{AuthenticatedKey, RequestId, parse_query};
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

/// `corsMiddleware`: CORS headers on every response; `OPTIONS` answers 204 without reaching auth.
pub async fn cors(req: Request, next: Next) -> Response {
    let mut resp = if req.method() == Method::OPTIONS {
        Reply::new(204).into_response()
    } else {
        next.run(req).await
    };
    apply_cors(resp.headers_mut());
    // axum's 405 handling adds `Allow`; gin answers 404 without it.
    resp.headers_mut().remove(header::ALLOW);
    resp
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

/// `AuthMiddleware` for the proxy route groups: API-key check, open when no keys are configured.
pub async fn api_key_auth(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let cfg = st.cfg();
    let query = parse_query(req.uri().query().unwrap_or(""));
    match access::authenticate(req.headers(), &query, &cfg.api_keys) {
        Ok(Some(principal)) => {
            req.extensions_mut().insert(AuthenticatedKey(principal));
            next.run(req).await
        }
        Ok(None) => next.run(req).await,
        Err(failure) => auth_failure_reply(failure).into_response(),
    }
}

fn auth_failure_reply(failure: AuthFailure) -> Reply {
    Reply::json(failure.status(), format!(r#"{{"error":"{}"}}"#, failure.message()).into_bytes())
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
    let trusted = st.cfg().trusted_proxies.clone();
    let client_ip = clientip::resolve(remote, req.headers(), &trusted);

    let request_id = if is_ai_api_path(&path) {
        logging::generate_request_id()
    } else {
        String::new()
    };
    if !request_id.is_empty() {
        req.extensions_mut().insert(RequestId(request_id.clone()));
    }
    req.extensions_mut().insert(ApiLogHandle::default());

    let scope_id = request_id.clone();
    let resp = REQUEST_ID.scope(scope_id, next.run(req)).await;

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

/// Shared handle type used by routers that need the config without the whole state.
pub type SharedState = Arc<AppState>;

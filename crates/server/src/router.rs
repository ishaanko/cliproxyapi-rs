//! Route table and middleware stack (Go: internal/api/server.go, server_routes.go).

use axum::Router;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, Method};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::Response;
use axum::routing::{any, get, post};
use constant_time_eq_lite::eq as ct_eq;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;

use crate::handlers::{alpha_search, claude, gemini, images, openai, responses, videos};
use crate::middleware::{access_log, api_key_auth, cors, recover_panic, safe_mode, trace_header};
use crate::reply::Reply;
use crate::req::ReqInfo;
use crate::state::AppState;
use crate::ui;
use crate::ws;

const OAUTH_CALLBACK_SUCCESS_HTML: &str = r#"<html><head><meta charset="utf-8"><title>Authentication successful</title><script>setTimeout(function(){window.close();},5000);</script></head><body><h1>Authentication successful!</h1><p>You can close this window.</p><p>This window will close automatically in 5 seconds.</p></body></html>"#;

/// Proxy routes plus the global middleware stack, without management routes.
pub fn build_router(state: AppState) -> Router {
    apply_global_layers(proxy_routes(&state), &state)
}

/// Same as [`build_router`] with the management router merged *under* the global middleware
/// (CORS, access log, safe mode, panic recovery), as in the Go engine.
pub fn build_router_with_management(state: AppState, management: Router) -> Router {
    apply_global_layers(proxy_routes(&state).merge(management), &state)
}

/// Go middleware order: logger, recovery, request logging, CORS, safe mode (outermost first).
///
/// The stack wraps the whole router service (not each route), so `OPTIONS` short-circuits before
/// routing and unmatched requests are still logged and CORS-decorated.
pub fn apply_global_layers(router: Router, state: &AppState) -> Router {
    let stack = ServiceBuilder::new()
        .layer(from_fn(trailing_slash_redirect))
        .layer(from_fn_with_state(state.clone(), access_log))
        .layer(CatchPanicLayer::custom(recover_panic))
        .layer(from_fn_with_state(state.clone(), crate::reqlog::request_log))
        .layer(from_fn(trace_header))
        .layer(from_fn(cors))
        .layer(from_fn_with_state(state.clone(), safe_mode))
        .layer(from_fn(head_not_found))
        .layer(DefaultBodyLimit::disable())
        .service(router);
    Router::new().fallback_service(stack)
}

/// Registered `(method, gin pattern)` pairs of the proxy surface, used only to decide whether a
/// trailing-slash redirect applies.
const ROUTE_TABLE: &[(&str, &str)] = &[
    ("GET", "/healthz"),
    ("HEAD", "/healthz"),
    ("GET", "/"),
    ("GET", "/management.html"),
    ("GET", "/anthropic/callback"),
    ("GET", "/codex/callback"),
    ("GET", "/antigravity/callback"),
    ("GET", "/callback"),
    ("GET", "/devin/callback"),
    ("GET", "/v1/models"),
    ("POST", "/v1/chat/completions"),
    ("POST", "/v1/completions"),
    ("POST", "/v1/images/generations"),
    ("POST", "/v1/images/edits"),
    ("POST", "/v1/videos"),
    ("POST", "/v1/videos/generations"),
    ("POST", "/v1/videos/edits"),
    ("POST", "/v1/videos/extensions"),
    ("GET", "/v1/videos/:request_id"),
    ("POST", "/v1/messages"),
    ("POST", "/v1/messages/count_tokens"),
    ("GET", "/v1/responses"),
    ("POST", "/v1/responses"),
    ("POST", "/v1/responses/compact"),
    ("POST", "/v1/alpha/search"),
    ("POST", "/openai/v1/videos"),
    ("GET", "/openai/v1/videos/:video_id/content"),
    ("GET", "/openai/v1/videos/:video_id"),
    ("GET", "/backend-api/codex/responses"),
    ("POST", "/backend-api/codex/responses"),
    ("POST", "/backend-api/codex/responses/compact"),
    ("GET", "/v1/ws"),
    ("POST", "/backend-api/codex/alpha/search"),
    ("GET", "/v1beta/models"),
    ("POST", "/v1beta/interactions"),
    ("GET", "/v1beta/models/*action"),
    ("POST", "/v1beta/models/*action"),
    // Management routes come from `cpa_management::GIN_ROUTES` (the Go registration). The
    // observability feeds below are extensions of this server that Go does not have.
    ("GET", "/v8/management/observability/usage/summary"),
    ("GET", "/v8/management/observability/requests"),
];

fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (mut p, mut s) = (pattern.split('/'), path.split('/'));
    loop {
        match (p.next(), s.next()) {
            (None, None) => return true,
            (Some(seg), Some(_)) if seg.starts_with('*') => return true,
            (Some(seg), Some(actual)) if seg.starts_with(':') => {
                if actual.is_empty() {
                    return false;
                }
            }
            (Some(seg), Some(actual)) if seg == actual => {}
            _ => return false,
        }
    }
}

fn route_exists(method: &str, path: &str) -> bool {
    ROUTE_TABLE
        .iter()
        .chain(cpa_management::GIN_ROUTES)
        .chain(crate::realtime::ROUTES)
        .any(|(m, pattern)| *m == method && pattern_matches(pattern, path))
}

/// gin registers `HEAD` only for `/healthz`; every other `HEAD` is an unrouted 404 (axum would
/// answer it with the `GET` handler).
async fn head_not_found(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if req.method() == Method::HEAD && req.uri().path() != "/healthz" {
        return Reply::new(404).into_response();
    }
    next.run(req).await
}

/// gin's `RedirectTrailingSlash`: a request that only differs from a registered route by a
/// trailing slash is redirected (301 for GET, 307 otherwise) before any middleware runs.
async fn trailing_slash_redirect(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path();
    let method = req.method().as_str();
    if !route_exists(method, path) {
        let alt = if path.len() > 1 && path.ends_with('/') {
            path[..path.len() - 1].to_string()
        } else {
            format!("{path}/")
        };
        if route_exists(method, &alt) {
            let code = if req.method() == Method::GET { 301 } else { 307 };
            let mut location = alt;
            if let Some(q) = req.uri().query() {
                location.push('?');
                location.push_str(q);
            }
            let mut reply = Reply::new(code).with_header(axum::http::header::LOCATION, &location);
            if req.method() == Method::GET || req.method() == Method::HEAD {
                let href = location.replace('&', "&amp;").replace('"', "&#34;").replace('<', "&lt;").replace('>', "&gt;");
                let label = if code == 301 { "Moved Permanently" } else { "Temporary Redirect" };
                reply = reply
                    .content_type("text/html; charset=utf-8")
                    .with_body(format!("<a href=\"{href}\">{label}</a>.\n\n"));
            }
            return reply.into_response();
        }
    }
    next.run(req).await
}

fn proxy_routes(state: &AppState) -> Router {
    let auth = || from_fn_with_state(state.clone(), api_key_auth);

    let v1 = Router::new()
        .route("/models", get(openai::unified_models))
        .route("/chat/completions", post(openai::chat_completions))
        .route("/completions", post(openai::completions))
        .route("/images/generations", post(images::generations))
        .route("/images/edits", post(images::edits))
        .route("/videos", post(videos::xai_native_post))
        .route("/videos/generations", post(videos::xai_native_post))
        .route("/videos/edits", post(videos::xai_native_post))
        .route("/videos/extensions", post(videos::xai_native_post))
        .route("/videos/{request_id}", get(videos::xai_retrieve))
        .route("/messages", post(claude::messages))
        .route("/messages/count_tokens", post(claude::count_tokens))
        .route("/responses", get(ws::responses_websocket).post(responses::responses))
        .route("/responses/compact", post(responses::compact))
        .route("/alpha/search", post(alpha_search::alpha_search))
        .route("/live", post(crate::realtime::live_call))
        .route("/live/{call_id}", get(crate::realtime::live_sideband))
        .route_layer(from_fn(crate::reqlog::capture_handler_errors))
        .route_layer(auth())
        .method_not_allowed_fallback(fallback);

    let openai_v1 = Router::new()
        .route("/videos", post(videos::videos_create))
        .route("/videos/{video_id}/content", get(videos::videos_content))
        .route("/videos/{video_id}", get(videos::videos_retrieve))
        .route_layer(from_fn(crate::reqlog::capture_handler_errors))
        .route_layer(auth())
        .method_not_allowed_fallback(fallback);

    let codex_direct = Router::new()
        .route("/responses", get(ws::responses_websocket).post(responses::responses))
        .route("/responses/compact", post(responses::compact))
        .route("/alpha/search", post(alpha_search::alpha_search))
        .route_layer(from_fn(crate::reqlog::capture_handler_errors))
        .route_layer(auth())
        .method_not_allowed_fallback(fallback);

    let v1beta = Router::new()
        .route("/models", get(gemini::list_models))
        .route("/interactions", post(gemini::interactions))
        .route("/models/", post(gemini::post_action_root).get(gemini::get_model_root))
        .route("/models/{*action}", post(gemini::post_action).get(gemini::get_model))
        .route_layer(from_fn(crate::reqlog::capture_handler_errors))
        .route_layer(auth())
        .method_not_allowed_fallback(fallback);

    let live = cpa_live::Handler::new(state.manager.clone(), state.config.clone());
    let mut router = Router::new()
        .route("/healthz", get(healthz_get).head(healthz_head))
        .route("/", get(root))
        .route("/management.html", get(management_html))
        .route("/anthropic/callback", get(callback_anthropic))
        .route("/codex/callback", get(callback_codex))
        .route("/antigravity/callback", get(callback_antigravity))
        .route("/callback", get(callback_devin))
        .route("/devin/callback", get(callback_devin))
        .route(
            "/v1/ws",
            get(crate::aistudio::relay_websocket)
                .route_layer(from_fn_with_state(state.clone(), crate::aistudio::ws_auth_gate)),
        )
        .nest("/v1", v1)
        .nest("/openai/v1", openai_v1)
        .nest("/backend-api/codex", codex_direct)
        .nest("/v1beta", v1beta)
        .merge(crate::realtime::routes(state, &live))
        .layer(axum::Extension(live));
    if state.keep_alive.is_some() {
        router = router.route("/keep-alive", get(keep_alive));
    }
    router
        .fallback(any(fallback))
        .method_not_allowed_fallback(fallback)
        .with_state(state.clone())
}

async fn healthz_get() -> Response {
    Reply::json(200, r#"{"status":"ok"}"#.as_bytes().to_vec()).into_response()
}

async fn healthz_head() -> Response {
    Reply::new(200).into_response()
}

/// `GET /`: the Go JSON banner for API clients. Browsers (`Accept` includes `text/html`) get the
/// embedded UI when present and enabled; `/management.html` serves it unconditionally.
async fn root(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let wants_html = headers
        .get_all(axum::http::header::ACCEPT)
        .iter()
        .any(|v| v.to_str().is_ok_and(|v| v.to_ascii_lowercase().contains("text/html")));
    if wants_html
        && !st.cfg().remote_management.disable_control_panel
        && let Some(index) = ui::index()
    {
        return index.into_response();
    }
    Reply::json(
        200,
        r#"{"endpoints":["POST /v1/chat/completions","POST /v1/completions","GET /v1/models"],"message":"CLI Proxy API Server"}"#
            .as_bytes()
            .to_vec(),
    )
    .into_response()
}

/// `GET /management.html` (`serveManagementControlPanel`): 404 when disabled or not embedded.
async fn management_html(State(st): State<AppState>) -> Response {
    if st.cfg().remote_management.disable_control_panel {
        return Reply::new(404).into_response();
    }
    match ui::index() {
        Some(index) => index.into_response(),
        None => Reply::new(404).into_response(),
    }
}

/// Unmatched routes: embedded UI assets, else an empty 404 (also used for wrong methods).
async fn fallback(method: Method, uri: axum::http::Uri) -> Response {
    if (method == Method::GET || method == Method::HEAD)
        && let Some(asset) = ui::asset(uri.path())
    {
        return asset.into_response();
    }
    Reply::new(404).into_response()
}

#[derive(serde::Deserialize, Default)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

impl CallbackQuery {
    fn error_text(&self) -> String {
        match self.error.as_deref() {
            Some(e) if !e.is_empty() => e.to_string(),
            _ => self.error_description.clone().unwrap_or_default(),
        }
    }
}

fn success_html() -> Reply {
    Reply::new(200)
        .content_type("text/html; charset=utf-8")
        .with_body(OAUTH_CALLBACK_SUCCESS_HTML)
}

/// Provider redirect landing page: hands the result to the pending login session, always 200.
async fn provider_callback(st: &AppState, provider: &str, q: &CallbackQuery) -> Response {
    let state = q.state.clone().unwrap_or_default();
    if !state.is_empty() {
        let cfg = st.cfg();
        let dir = std::path::PathBuf::from(&cfg.auth_dir);
        // Failures are swallowed like Go; the page is shown regardless.
        let _ = st
            .oauth_sessions
            .submit_callback(Some(&dir), provider, &state, q.code.as_deref().unwrap_or(""), &q.error_text())
            .await;
    }
    success_html().into_response()
}

async fn callback_anthropic(State(st): State<AppState>, Query(q): Query<CallbackQuery>) -> Response {
    provider_callback(&st, "anthropic", &q).await
}

async fn callback_codex(State(st): State<AppState>, Query(q): Query<CallbackQuery>) -> Response {
    provider_callback(&st, "codex", &q).await
}

async fn callback_antigravity(State(st): State<AppState>, Query(q): Query<CallbackQuery>) -> Response {
    provider_callback(&st, "antigravity", &q).await
}

/// Devin redirect: values are trimmed, `no-store`, and failures are visible (400).
async fn callback_devin(State(st): State<AppState>, Query(q): Query<CallbackQuery>) -> Response {
    let code = q.code.clone().unwrap_or_default().trim().to_string();
    let state = q.state.clone().unwrap_or_default().trim().to_string();
    let err = q.error_text().trim().to_string();
    let no_store = |reply: Reply| reply.with_header(axum::http::header::CACHE_CONTROL, "no-store");
    if code.is_empty() && err.is_empty() {
        return no_store(Reply::json(400, r#"{"error":"code or error is required"}"#.as_bytes().to_vec())).into_response();
    }
    let cfg = st.cfg();
    let dir = std::path::PathBuf::from(&cfg.auth_dir);
    if st.oauth_sessions.submit_callback(Some(&dir), "devin", &state, &code, &err).await.is_err() {
        return no_store(Reply::json(400, r#"{"error":"invalid or expired OAuth callback"}"#.as_bytes().to_vec())).into_response();
    }
    no_store(success_html()).into_response()
}

/// `GET /keep-alive` (`handleKeepAlive`): local-password gated heartbeat.
async fn keep_alive(State(st): State<AppState>, headers: HeaderMap, _info: ReqInfo) -> Response {
    let Some(ka) = st.keep_alive.clone() else {
        return Reply::new(404).into_response();
    };
    if !ka.password.is_empty() {
        let mut provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim()
            .to_string();
        if !provided.is_empty()
            && let Some((scheme, rest)) = provided.split_once(' ')
            && scheme.eq_ignore_ascii_case("bearer")
        {
            provided = rest.to_string();
        }
        if provided.is_empty() {
            provided = headers
                .get("x-local-password")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .trim()
                .to_string();
        }
        if !ct_eq(provided.as_bytes(), ka.password.as_bytes()) {
            return Reply::json(401, r#"{"error":"invalid password"}"#.as_bytes().to_vec()).into_response();
        }
    }
    let _ = ka.heartbeat.try_send(());
    Reply::json(200, r#"{"status":"ok"}"#.as_bytes().to_vec()).into_response()
}

/// Minimal constant-time byte comparison.
mod constant_time_eq_lite {
    pub fn eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

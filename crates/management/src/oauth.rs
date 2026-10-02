//! OAuth login endpoints (Go: `auth_files_provider_oauth.go`, `auth_files_v8.go`,
//! `oauth_callback.go`, `oauth_sessions.go`, `vertex_import.go`) and the provider redirect routes
//! (`server_routes.go`). Flows, sessions and credential persistence live in `cpa-auth`; this
//! module maps them onto HTTP.

use axum::Router;
use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{HeaderValue, Method, Uri, header};
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use cpa_auth::login::FlowKind;
use cpa_auth::sessions::CallbackRequest;
use cpa_auth::{AuthFlowError, LoginOptions, Provider};
use serde_json::json;

use crate::http::{ApiError, ApiResult, content_type_is, json_response, ok_json, query_trim};
use crate::state::ManagementState;

/// Default `expires_in` of the Codex device flow when the upstream does not say.
const CODEX_DEVICE_EXPIRES_IN_SECS: u64 = 900;

const OAUTH_CALLBACK_SUCCESS_HTML: &str = "<html><head><meta charset=\"utf-8\"><title>Authentication successful</title><script>setTimeout(function(){window.close();},5000);</script></head><body><h1>Authentication successful!</h1><p>You can close this window.</p><p>This window will close automatically in 5 seconds.</p></body></html>";

fn is_webui(uri: &Uri) -> bool {
    matches!(query_trim(uri, "is_webui").to_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// v8 provider names exactly as `StartOAuthV8` switches on them (no aliases).
fn v8_provider(name: &str) -> Option<Provider> {
    Some(match name {
        "claude" => Provider::Claude,
        "codex" => Provider::Codex,
        "antigravity" => Provider::Antigravity,
        "kimi" => Provider::Kimi,
        "kimi-ai" => Provider::KimiAi,
        "xai" => Provider::Xai,
        "devin" => Provider::Devin,
        "meta" => Provider::Meta,
        _ => return None,
    })
}

/// Maps a failed start to the response message the Go handlers use per provider.
fn start_error(provider: Provider, device: bool, err: &AuthFlowError) -> ApiError {
    tracing::error!("failed to start {} login: {err}", provider.key());
    let text = err.to_string();
    let msg = if text.starts_with("failed to start callback server") || text.starts_with("failed to listen") {
        "failed to start callback server"
    } else if text.starts_with("callback server unavailable") {
        "callback server unavailable"
    } else if device || matches!(provider, Provider::Xai | Provider::Meta) {
        "failed to start device authorization flow"
    } else {
        "failed to generate authorization url"
    };
    ApiError::new(500, msg)
}

/// `GET /oauth/auth-url?provider=`: starts a login and returns `{"status","url","state"}` plus
/// `flow`, `user_code` and `expires_in` for device flows. `provider=codex&flow=device` starts the
/// Codex device-code flow.
pub(crate) async fn auth_url(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let uri = req.uri();
    let name = query_trim(uri, "provider").to_lowercase();
    if name.is_empty() {
        return Err(ApiError::bad_request("provider is required"));
    }
    let Some(provider) = v8_provider(&name) else {
        return Err(ApiError::new(404, "provider_not_found"));
    };
    let cfg = st.cfg();
    let mut opts = LoginOptions::management(cfg.proxy_url.trim());
    let codex_device = provider == Provider::Codex && query_trim(uri, "flow").eq_ignore_ascii_case("device");
    if codex_device {
        opts.metadata.insert(cpa_auth::codex::LOGIN_MODE_METADATA_KEY.into(), cpa_auth::codex::LOGIN_MODE_DEVICE.into());
    }
    match provider {
        Provider::Claude | Provider::Codex | Provider::Antigravity if is_webui(uri) && !codex_device => {
            let route = match provider {
                Provider::Claude => "anthropic",
                Provider::Codex => "codex",
                _ => "antigravity",
            };
            opts.webui_callback_target = Some(st.loopback_url(&format!("/{route}/callback")));
        }
        Provider::Kimi => {
            let domain = ["domain", "channel"].iter().map(|k| query_trim(uri, k)).find(|d| !d.is_empty());
            opts.kimi_domain = domain;
        }
        Provider::Devin => {
            // Devin validates the redirect strictly: http, 127.0.0.1 and /callback.
            opts.devin_redirect_uri = Some(format!("http://127.0.0.1:{}/callback", st.server_port()));
        }
        _ => {}
    }

    let session = st.login.start_login(provider, opts).await.map_err(|e| start_error(provider, codex_device, &e))?;
    let start = session.start_info();
    let mut body = start.to_json();
    if codex_device && start.flow == FlowKind::Device && start.expires_in.is_none() {
        body["expires_in"] = CODEX_DEVICE_EXPIRES_IN_SECS.into();
    }
    Ok(ok_json(&body))
}

/// `GET /oauth/status?state=`.
pub(crate) async fn status(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let (code, body) = st.oauth.poll_status(&query_trim(req.uri(), "state"));
    Ok(json_response(code, &body))
}

/// `DELETE /oauth/session?state=`.
pub(crate) async fn cancel_session(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let (code, body) = st.oauth.cancel_status(&query_trim(req.uri(), "state"));
    Ok(json_response(code, &body))
}

/// `GET|POST /oauth/callback` (no management key; the pending `state` is the credential).
pub(crate) async fn callback(State(st): State<ManagementState>, req: Request) -> Response {
    let cb = if req.method() == Method::POST {
        let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap_or_default();
        match serde_json::from_slice::<CallbackRequest>(&body) {
            Ok(cb) => cb,
            Err(_) => return json_response(400, &json!({"status": "error", "error": "invalid body"})),
        }
    } else {
        let uri = req.uri();
        let error = [query_trim(uri, "error"), query_trim(uri, "error_description")].into_iter().find(|e| !e.is_empty()).unwrap_or_default();
        CallbackRequest {
            provider: query_trim(uri, "provider"),
            code: query_trim(uri, "code"),
            state: query_trim(uri, "state"),
            error,
            redirect_url: String::new(),
        }
    };
    let dir = st.auth_dir();
    let (code, body) = st.oauth.handle_oauth_callback(dir.as_deref(), &cb);
    json_response(code, &body)
}

// ---- credential import ----

/// `POST /oauth/import?provider=vertex`.
pub(crate) async fn import(State(st): State<ManagementState>, req: Request) -> ApiResult {
    match query_trim(req.uri(), "provider").to_lowercase().as_str() {
        "" => Err(ApiError::bad_request("provider is required")),
        "vertex" => import_vertex(&st, req).await,
        _ => Err(ApiError::new(404, "provider_not_found")),
    }
}

async fn import_vertex(st: &ManagementState, req: Request) -> ApiResult {
    if st.auth_dir().is_none() {
        return Err(ApiError::new(503, "auth directory not configured"));
    }
    let query_location = query_trim(req.uri(), "location");
    if !content_type_is(req.headers(), "multipart/form-data") {
        return Err(ApiError::bad_request("file required"));
    }
    let mut multipart = Multipart::from_request(req, &()).await.map_err(|_| ApiError::bad_request("file required"))?;
    let mut file: Option<Bytes> = None;
    let mut form_location = String::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        match (field.name().unwrap_or("").to_string(), field.file_name().is_some()) {
            (name, true) if name == "file" && file.is_none() => {
                file = Some(field.bytes().await.map_err(|e| ApiError::bad_request(format!("failed to read file: {e}")))?);
            }
            (name, false) if name == "location" => form_location = field.text().await.unwrap_or_default(),
            _ => {}
        }
    }
    let Some(data) = file else {
        return Err(ApiError::bad_request("file required"));
    };
    let location = [form_location.trim().to_string(), query_location].into_iter().find(|l| !l.is_empty()).unwrap_or_default();

    let imported = cpa_auth::vertex::import_service_account(&data, &location, None).map_err(|e| {
        let text = e.to_string();
        if let Some(msg) = text.strip_prefix("invalid json: ") {
            ApiError::with_message(400, "invalid json", msg)
        } else if let Some(msg) = text.strip_prefix("invalid service account: ") {
            ApiError::with_message(400, "invalid service account", msg)
        } else {
            ApiError::bad_request(text)
        }
    })?;
    let mut auth = imported.auth;
    let saved = st.login.save_record(&mut auth).map_err(|e| ApiError::with_message(500, "save_failed", e.to_string()))?;
    Ok(ok_json(&json!({
        "status": "ok",
        "auth-file": saved.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        "project_id": imported.project_id,
        "email": imported.email,
        "location": imported.location,
    })))
}

// ---- provider redirect routes (main server, no management key) ----

async fn provider_redirect(st: ManagementState, provider: &'static str, uri: Uri) -> Response {
    let code = query_trim(&uri, "code");
    let state = query_trim(&uri, "state");
    let error = [query_trim(&uri, "error"), query_trim(&uri, "error_description")].into_iter().find(|e| !e.is_empty()).unwrap_or_default();
    if !state.is_empty() {
        // A redirect for an unknown or finished session is swallowed; the page is the same.
        let _ = st.oauth.submit_callback(st.auth_dir().as_deref(), provider, &state, &code, &error);
    }
    html_ok()
}

fn html_ok() -> Response {
    let mut resp = Response::new(axum::body::Body::from(OAUTH_CALLBACK_SUCCESS_HTML));
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    resp
}

/// `/callback` and `/devin/callback`: stricter than the other redirects.
async fn devin_redirect(State(st): State<ManagementState>, uri: Uri) -> Response {
    let code = query_trim(&uri, "code");
    let state = query_trim(&uri, "state");
    let error = [query_trim(&uri, "error"), query_trim(&uri, "error_description")].into_iter().find(|e| !e.is_empty()).unwrap_or_default();
    let mut resp = if code.is_empty() && error.is_empty() {
        json_response(400, &json!({"error": "code or error is required"}))
    } else if st.oauth.submit_callback(st.auth_dir().as_deref(), "devin", &state, &code, &error).is_err() {
        json_response(400, &json!({"error": "invalid or expired OAuth callback"}))
    } else {
        html_ok()
    };
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// The OAuth loopback callbacks the main server serves outside the management group:
/// `/anthropic/callback`, `/codex/callback`, `/antigravity/callback`, `/callback`,
/// `/devin/callback`. Merge into the server router (these paths are not part of [`crate::router`]).
pub fn oauth_redirect_router(state: ManagementState) -> Router {
    let provider_route = |provider: &'static str| {
        get(move |State(st): State<ManagementState>, uri: Uri| provider_redirect(st, provider, uri))
    };
    Router::new()
        .route("/anthropic/callback", provider_route("anthropic"))
        .route("/codex/callback", provider_route("codex"))
        .route("/antigravity/callback", provider_route("antigravity"))
        .route("/callback", get(devin_redirect))
        .route("/devin/callback", get(devin_redirect))
        .with_state(state)
}

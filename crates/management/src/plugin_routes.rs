//! Plugin-owned routes of the management API (Go: `pluginManagementNoRoute`,
//! `pluginResourceNoRoute`, `ServePluginAuthURL`): requests no built-in route matched are offered
//! to the plugin host.

use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use cpa_plugin::{CallCtx, PluginHttpResponse};
use serde_json::{Map, Value, json};

use crate::http::{empty, json_response, query_pairs};
use crate::state::ManagementState;

/// The plugin host's answer as an HTTP response.
fn write_plugin_response(resp: PluginHttpResponse) -> Response {
    let (status, headers, body) = resp.into_parts();
    let mut out = Response::new(Body::from(body));
    // net/http panics on an out-of-range status; gin's recovery turns that into a 500.
    *out.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (name, value) in headers {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_bytes(value.as_bytes())) {
            out.headers_mut().append(n, v);
        }
    }
    out
}

/// The unrouted `/v0/management` request after the availability and key checks: the plugin auth
/// URL endpoints, then plugin-declared routes, else a bare 404.
pub(crate) async fn management_fallback(State(st): State<ManagementState>, req: Request) -> Response {
    let Some(host) = st.plugins.clone() else { return empty(404) };
    let (parts, body) = req.into_parts();
    let path = parts.extensions.get::<OriginalUri>().map(|u| u.0.path().to_string()).unwrap_or_else(|| parts.uri.path().to_string());
    let query = parts.uri.query().map(|_| query_pairs(&parts.uri)).unwrap_or_default();
    if let Some(resp) = serve_plugin_auth_url(&st, &path, &query).await {
        return resp;
    }
    let body = match axum::body::to_bytes(body, crate::MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return write_plugin_response(plugin_http_error("failed to read plugin management request body", 400)),
    };
    let mut headers = parts.headers.clone();
    headers.remove(axum::http::header::HOST);
    match host.serve_management_http(&CallCtx::background(), parts.method.as_str(), &path, &headers, &query, &body).await {
        Some(resp) => write_plugin_response(resp),
        None => empty(404),
    }
}

fn plugin_http_error(message: &str, status: u16) -> PluginHttpResponse {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("Content-Type".to_string(), vec!["text/plain; charset=utf-8".to_string()]);
    headers.insert("X-Content-Type-Options".to_string(), vec!["nosniff".to_string()]);
    PluginHttpResponse { status, headers, body: format!("{message}\n").into_bytes() }
}

/// `GET /v0/resource/plugins/...`: unauthenticated plugin resources.
pub(crate) async fn resource_fallback(State(st): State<ManagementState>, req: Request) -> Response {
    // Go `pluginResourceNoRoute`: hidden when Home mode is enabled.
    if st.cfg().home.enabled {
        return empty(404);
    }
    let Some(host) = st.plugins.clone() else { return empty(404) };
    let (parts, _) = req.into_parts();
    let path = parts.extensions.get::<OriginalUri>().map(|u| u.0.path().to_string()).unwrap_or_else(|| parts.uri.path().to_string());
    let query = query_pairs(&parts.uri);
    let mut headers = parts.headers.clone();
    headers.remove(axum::http::header::HOST);
    match host.serve_resource_http(&CallCtx::background(), parts.method.as_str(), &path, &headers, &query).await {
        Some(resp) => write_plugin_response(resp),
        None => empty(404),
    }
}

/// `NormalizePluginOAuthCallbackProvider`.
fn plugin_provider(raw: &str) -> Option<String> {
    cpa_auth::sessions::normalize_plugin_callback_provider(raw)
}

/// `pluginAuthProviderFromURL`.
fn provider_from_url(path: &str, query: &[(String, String)]) -> Option<(String, bool)> {
    let path = path.trim();
    if path == "/v8/management/oauth/auth-url" {
        let raw = query.iter().find(|(k, _)| k == "provider").map(|(_, v)| v.as_str()).unwrap_or("");
        return plugin_provider(raw).map(|p| (p, true));
    }
    let name = path.strip_prefix("/v0/management/")?.strip_suffix("-auth-url")?;
    plugin_provider(name).map(|p| (p, false))
}

/// `ServePluginAuthURL`: starts a login through the plugin that owns the provider; `None` when
/// the request is not a plugin login URL.
pub(crate) async fn serve_plugin_auth_url(st: &ManagementState, path: &str, query: &[(String, String)]) -> Option<Response> {
    let host = st.plugins.clone()?;
    let (provider, v8) = provider_from_url(path, query)?;
    if !host.has_auth_provider(&provider) {
        return None;
    }
    let callback_path = if v8 { "/v8/management/oauth/callback" } else { "/v0/management/oauth-callback" };
    let err = |status: u16, msg: &str| Some(json_response(status, &json!({"error": msg})));
    if st.cfg().port <= 0 {
        tracing::error!("failed to compute plugin auth callback URL: server port is not configured");
        return err(500, "failed to generate authorization url");
    }
    let base_url = st.loopback_url(callback_path);
    let metadata = query_metadata(query, v8);
    let resp = match host.start_login(&CallCtx::background(), &provider, &base_url, metadata).await {
        Ok(Some(r)) => r,
        Ok(None) => return None,
        Err(e) => {
            tracing::error!("failed to start plugin auth login: {e}");
            return err(500, "failed to generate authorization url");
        }
    };
    let state = resp.state.trim().to_string();
    if state.is_empty() {
        tracing::error!(provider = %provider, "plugin auth provider returned empty state");
        return err(502, "invalid oauth state");
    }
    if let Err(e) = cpa_auth::oauth::validate_oauth_state(&state) {
        tracing::error!(provider = %provider, "plugin auth provider returned invalid state: {e}");
        return err(502, "invalid oauth state");
    }
    let metadata = (!resp.metadata.is_empty()).then(|| resp.metadata.clone());
    if let Err(e) = st.oauth.register_plugin(&state, &provider, metadata) {
        tracing::error!(provider = %provider, "failed to register plugin oauth session: {e}");
        return err(502, "failed to generate authorization url");
    }
    Some(json_response(200, &json!({"status": "ok", "url": resp.url, "state": state})))
}

/// `queryValuesToMetadata`: single values stay strings, repeated keys become arrays; the v8
/// `provider` selector is not forwarded.
fn query_metadata(query: &[(String, String)], v8: bool) -> Option<Map<String, Value>> {
    let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
    for (k, v) in query {
        if v8 && k == "provider" {
            continue;
        }
        match grouped.iter_mut().find(|(key, _)| key == k) {
            Some((_, values)) => values.push(v.clone()),
            None => grouped.push((k.clone(), vec![v.clone()])),
        }
    }
    if grouped.is_empty() {
        return None;
    }
    let mut out = Map::new();
    for (k, mut values) in grouped {
        let value = if values.len() == 1 { Value::String(values.remove(0)) } else { json!(values) };
        out.insert(k, value);
    }
    Some(out)
}

/// `GetAuthStatus` for a pending plugin login: polls the plugin and saves a finished login.
/// `None` when the session is not a pending plugin session (the built-in status applies).
pub(crate) async fn plugin_login_status(st: &ManagementState, state: &str) -> Option<(u16, Value)> {
    let host = st.plugins.clone()?;
    let state = state.trim();
    let session = st.oauth.get(state)?;
    if !session.is_plugin || session.completed || !session.status.is_empty() || !host.has_auth_provider(&session.provider) {
        return None;
    }
    let ctx = CallCtx::background();
    let fail = |message: &str| {
        st.oauth.set_error(state, message);
        Some((200, json!({"status": "error", "error": message})))
    };
    let resp = match host.poll_login(&ctx, &session.provider, state, session.metadata.clone()).await {
        Ok(Some(r)) => r,
        Ok(None) => return None,
        Err(e) => {
            let message = e.message.trim().to_string();
            return fail(if message.is_empty() { "Authentication failed" } else { &message });
        }
    };
    match resp.status.as_str() {
        "" | "pending" => Some((200, json!({"status": "wait"}))),
        "error" => {
            let message = resp.message.trim().to_string();
            fail(if message.is_empty() { "Authentication failed" } else { &message })
        }
        "success" => {
            let datas = if resp.auths.is_empty() { vec![resp.auth.clone()] } else { resp.auths.clone() };
            let records: Option<Vec<_>> = datas.iter().map(|d| host.auth_data_to_core_auth(d, "", "")).collect();
            let Some(records) = records.filter(|r| !r.is_empty()) else {
                return fail("Authentication failed");
            };
            let login = st.login.clone();
            let saved = crate::http::blocking(move || Ok(save_plugin_login_records(&login, records))).await;
            match saved {
                Ok(Ok(())) => {
                    st.oauth.complete(state);
                    Some((200, json!({"status": "ok"})))
                }
                Ok(Err(e)) => {
                    tracing::error!(provider = %session.provider, "failed to save plugin auth tokens: {e}");
                    fail("Failed to save authentication tokens")
                }
                Err(_) => fail("Failed to save authentication tokens"),
            }
        }
        _ => Some((200, json!({"status": "wait"}))),
    }
}

/// `savePluginLoginRecords`: saves every record, removing the files already written when one
/// fails.
fn save_plugin_login_records(login: &cpa_auth::Manager, records: Vec<cpa_auth::Auth>) -> Result<(), String> {
    let mut saved: Vec<std::path::PathBuf> = Vec::new();
    for mut record in records {
        match login.save_record(&mut record) {
            Ok(path) => {
                if let Some(p) = path.filter(|p| !p.as_os_str().is_empty()) {
                    saved.push(p);
                }
            }
            Err(e) => {
                for path in saved.iter().rev() {
                    if let Err(err) = std::fs::remove_file(path) {
                        tracing::warn!(path = %path.display(), "failed to roll back plugin auth token: {err}");
                    }
                }
                return Err(e.to_string());
            }
        }
    }
    Ok(())
}

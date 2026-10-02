//! `POST /requests/api-call` (Go: `api_tools.go`), `GET /server/latest-version`
//! (`config_basic.go`) and the plugin endpoints this build does not provide.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::extract::State;
use axum::response::Response;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use cpa_auth::http::{ProxySetting, parse_proxy};
use cpa_auth::{Auth, AuthFlowError};
use cpa_config::Config;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::credentials::auth_by_index;
use crate::http::{ApiError, ApiResult, ok_json, ok_struct};
use crate::state::ManagementState;

const API_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Refresh a bit early to avoid requests racing token expiry.
const ANTIGRAVITY_SKEW_SECS: i64 = 30;

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiCallRequest {
    auth_index: Option<String>,
    #[serde(rename = "authIndex")]
    auth_index_camel: Option<String>,
    #[serde(rename = "AuthIndex")]
    auth_index_pascal: Option<String>,
    method: String,
    url: String,
    proxy_url: String,
    header: Option<BTreeMap<String, String>>,
    data: String,
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

// ---- token lookup ----

/// `tokenValueFromMetadata`.
fn token_from_metadata(meta: &serde_json::Map<String, Value>) -> String {
    let s = |k: &str| meta.get(k).and_then(Value::as_str).and_then(non_empty);
    if let Some(v) = s("accessToken").or_else(|| s("access_token")) {
        return v;
    }
    match meta.get("token") {
        Some(Value::String(t)) => {
            if let Some(v) = non_empty(t) {
                return v;
            }
        }
        Some(Value::Object(t)) => {
            for k in ["access_token", "accessToken"] {
                if let Some(v) = t.get(k).and_then(Value::as_str).and_then(non_empty) {
                    return v;
                }
            }
        }
        _ => {}
    }
    for k in ["id_token", "api_key", "session_token", "cookie"] {
        if let Some(v) = s(k) {
            return v;
        }
    }
    String::new()
}

/// `tokenValueForAuth`: metadata token, then `api_key` / `session_token` attributes.
fn token_for_auth(auth: &Auth) -> String {
    let m = token_from_metadata(&auth.metadata);
    if !m.is_empty() {
        return m;
    }
    ["api_key", "session_token"]
        .iter()
        .map(|k| auth.attr(k))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

fn meta_token(auth: &Auth) -> String {
    let usable = |v: String| (!v.is_empty() && !v.starts_with("dca:")).then_some(v);
    [
        auth.meta_str("api_key"),
        auth.meta_str("access_token"),
        auth.attr("api_key"),
        auth.attr("access_token"),
    ]
    .into_iter()
    .find_map(usable)
    .unwrap_or_default()
}

fn xai_token(auth: &Auth) -> String {
    [
        auth.attr("api_key"),
        auth.meta_str("api_key"),
        auth.meta_str("access_token"),
        auth.meta_str("accessToken"),
    ]
    .into_iter()
    .find(|v| !v.is_empty())
    .unwrap_or_default()
}

fn antigravity_needs_refresh(auth: &Auth, now: DateTime<Utc>) -> bool {
    let skew = chrono::Duration::seconds(ANTIGRAVITY_SKEW_SECS);
    if let Some(Value::String(exp)) = auth.metadata.get("expired")
        && let Ok(t) = DateTime::parse_from_rfc3339(exp.trim())
    {
        return t.with_timezone(&Utc) <= now + skew;
    }
    let int = |k: &str| {
        auth.metadata
            .get(k)
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            })
            .unwrap_or(0)
    };
    let (expires_in, ts_ms) = (int("expires_in"), int("timestamp"));
    if expires_in > 0 && ts_ms > 0 {
        let exp = DateTime::from_timestamp_millis(ts_ms)
            .map(|t| t + chrono::Duration::seconds(expires_in));
        return exp.is_none_or(|e| e <= now + skew);
    }
    true
}

/// Refreshes `auth` with the request proxy applied to the exchange only, and stores the result.
/// Returns the refreshed auth.
async fn refresh_for_call(
    st: &ManagementState,
    auth: &Auth,
    request_proxy: &str,
) -> Result<Auth, AuthFlowError> {
    let mut input = auth.clone();
    if !request_proxy.is_empty() {
        input.proxy_url = request_proxy.to_string();
    }
    let global = st.cfg().proxy_url.trim().to_string();
    let mut refreshed = cpa_auth::refresh_auth(&input, &global).await?;
    refreshed.proxy_url = auth.proxy_url.clone();
    let now = Utc::now();
    refreshed.last_refreshed_at = Some(now);
    refreshed.updated_at = Some(now);
    if let Err(e) = st.registry.update(refreshed.clone()).await {
        tracing::warn!("management APICall: failed to store refreshed credential: {e}");
    }
    Ok(refreshed)
}

/// `resolveTokenForAuth`: the credential's token, refreshing OAuth tokens of providers whose
/// tokens are short-lived (antigravity, xai) or minted on demand (meta).
async fn resolve_token(
    st: &ManagementState,
    auth: &Auth,
    request_proxy: &str,
) -> Result<String, AuthFlowError> {
    let now = Utc::now();
    match auth.provider.trim().to_lowercase().as_str() {
        "antigravity" => {
            if auth.metadata.is_empty() {
                return Err(AuthFlowError::other("antigravity oauth metadata missing"));
            }
            let current = token_from_metadata(&auth.metadata);
            if !current.is_empty() && !antigravity_needs_refresh(auth, now) {
                return Ok(current);
            }
            if auth.meta_str("refresh_token").is_empty() {
                return Err(AuthFlowError::other("antigravity refresh token missing"));
            }
            let refreshed = refresh_for_call(st, auth, request_proxy).await?;
            let token = token_from_metadata(&refreshed.metadata);
            if token.is_empty() {
                return Err(AuthFlowError::other(
                    "antigravity oauth token refresh returned empty access_token",
                ));
            }
            Ok(token)
        }
        "meta" => {
            let token = meta_token(auth);
            if !token.is_empty() {
                return Ok(token);
            }
            let refreshed = refresh_for_call(st, auth, request_proxy).await?;
            Ok(meta_token(&refreshed))
        }
        "xai" => {
            let current = xai_token(auth);
            let lead = chrono::Duration::from_std(cpa_auth::xai::REFRESH_LEAD).unwrap_or_default();
            if !current.is_empty() && auth.has_valid_access_token(now + lead) {
                return Ok(current);
            }
            if auth.meta_str("refresh_token").is_empty() {
                return Ok(current);
            }
            let refreshed = refresh_for_call(st, auth, request_proxy).await?;
            let token = xai_token(&refreshed);
            if token.is_empty() {
                return Err(AuthFlowError::other(
                    "xai oauth token refresh returned empty access_token",
                ));
            }
            Ok(token)
        }
        _ => Ok(token_for_auth(auth)),
    }
}

// ---- proxy selection ----

fn eq_ci(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Index of the config API-key entry behind an API-key credential (`resolveAPIKeyConfig`).
fn resolve_api_key_entry(entries: &[(&str, &str, &str)], auth: &Auth) -> Option<usize> {
    let (attr_key, attr_base) = (auth.attr("api_key"), auth.attr("base_url"));
    for (i, (key, base, _)) in entries.iter().enumerate() {
        if !attr_key.is_empty() && !attr_base.is_empty() {
            if eq_ci(key, &attr_key) && eq_ci(base, &attr_base) {
                return Some(i);
            }
            continue;
        }
        if !attr_key.is_empty()
            && eq_ci(key, &attr_key)
            && (base.trim().is_empty() || eq_ci(base, &attr_base))
        {
            return Some(i);
        }
        if attr_key.is_empty() && !attr_base.is_empty() && eq_ci(base, &attr_base) {
            return Some(i);
        }
    }
    if !attr_key.is_empty() {
        return entries.iter().position(|(key, _, _)| eq_ci(key, &attr_key));
    }
    None
}

/// `proxyURLFromAPIKeyConfig`: proxy of the config entry an API-key credential came from.
fn proxy_from_api_key_config(cfg: &Config, auth: &Auth) -> String {
    let (kind, account) = auth.account_info();
    if !kind.eq_ignore_ascii_case("api_key") {
        return String::new();
    }
    let compat_name = auth.attr("compat_name");
    let provider = auth.provider.trim().to_lowercase();
    if !compat_name.is_empty() || provider == "openai-compatibility" {
        let account = account.trim();
        if account.is_empty() {
            return String::new();
        }
        let candidates = [
            compat_name,
            auth.attr("provider_key"),
            auth.provider.trim().to_string(),
        ];
        for compat in cfg.openai_compatibility.iter().filter(|c| !c.disabled) {
            if candidates
                .iter()
                .any(|c| !c.is_empty() && eq_ci(c, &compat.name))
            {
                return compat
                    .api_key_entries
                    .iter()
                    .find(|e| eq_ci(&e.api_key, account))
                    .map(|e| e.proxy_url.trim().to_string())
                    .unwrap_or_default();
            }
        }
        return String::new();
    }
    macro_rules! entry_proxy {
        ($list:expr) => {{
            let view: Vec<(&str, &str, &str)> = $list
                .iter()
                .map(|e| {
                    (
                        e.api_key.as_str(),
                        e.base_url.as_str(),
                        e.proxy_url.as_str(),
                    )
                })
                .collect();
            resolve_api_key_entry(&view, auth)
                .map(|i| view[i].2.trim().to_string())
                .unwrap_or_default()
        }};
    }
    match provider.as_str() {
        "gemini" => entry_proxy!(cfg.gemini_key),
        "gemini-interactions" => entry_proxy!(cfg.interactions_key),
        "claude" => entry_proxy!(cfg.claude_key),
        "codex" => entry_proxy!(cfg.codex_key),
        "xai" => entry_proxy!(cfg.xai_key),
        "meta" => entry_proxy!(cfg.meta_key),
        _ => String::new(),
    }
}

/// A client without environment proxies (Go: the cloned default transport with `Proxy = nil`),
/// routed through `proxy` when it is a real proxy URL.
fn build_client(proxy: &ProxySetting) -> reqwest::Client {
    // Go's default transport asks for gzip only and identifies as Go-http-client.
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(API_CALL_TIMEOUT)
        .no_proxy()
        .brotli(false)
        .deflate(false)
        .user_agent("Go-http-client/1.1");
    if let ProxySetting::Proxy(p) = proxy
        && let Ok(proxy) = reqwest::Proxy::all(p)
    {
        builder = builder.proxy(proxy);
    }
    builder.build().unwrap_or_default()
}

/// Request proxy, else credential proxy, config entry proxy, global proxy; the first usable one
/// wins and `direct`/`none` pins a direct connection.
fn select_proxy(cfg: &Config, auth: Option<&Auth>, request_proxy: &str) -> ProxySetting {
    if !request_proxy.is_empty() {
        return parse_proxy(request_proxy).unwrap_or(ProxySetting::Direct);
    }
    let mut candidates = Vec::new();
    if let Some(a) = auth {
        candidates.push(a.proxy_url.trim().to_string());
        candidates.push(proxy_from_api_key_config(cfg, a));
    }
    candidates.push(cfg.proxy_url.trim().to_string());
    candidates
        .iter()
        .filter(|c| !c.is_empty())
        .find_map(|c| match parse_proxy(c) {
            Ok(ProxySetting::Inherit) | Err(_) => None,
            Ok(p) => Some(p),
        })
        .unwrap_or(ProxySetting::Direct)
}

// ---- handlers ----

/// `POST /requests/api-call`: a generic HTTP request on behalf of the caller, with `$TOKEN$`
/// replaced by the selected credential's token. The result is always `200` with
/// `{status_code, header, body}`; upstream failures do not change the status.
pub(crate) async fn api_call(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let mut req: ApiCallRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("invalid body"))?;
    let method = req.method.trim().to_uppercase();
    if method.is_empty() {
        return Err(ApiError::bad_request("missing method"));
    }
    let url_str = req.url.trim().to_string();
    if url_str.is_empty() {
        return Err(ApiError::bad_request("missing url"));
    }
    match url::Url::parse(&url_str) {
        Ok(u) if u.host_str().is_some_and(|h| !h.is_empty()) => {}
        _ => return Err(ApiError::bad_request("invalid url")),
    }
    let request_proxy = req.proxy_url.trim().to_string();
    if !request_proxy.is_empty() && parse_proxy(&request_proxy).is_err() {
        return Err(ApiError::bad_request("invalid proxy_url"));
    }

    let auth_index = [
        &req.auth_index,
        &req.auth_index_camel,
        &req.auth_index_pascal,
    ]
    .into_iter()
    .flatten()
    .find_map(|v| non_empty(v))
    .unwrap_or_default();
    let auth = auth_by_index(&st, &auth_index);
    let mut headers = req.header.take().unwrap_or_default();

    let mut token: Option<(String, bool)> = None;
    // Token resolution is async, so it is done up front when a placeholder is present.
    let needs_token =
        headers.values().any(|v| v.contains("$TOKEN$")) || req.data.contains("$TOKEN$");
    if needs_token {
        let resolved = match &auth {
            Some(a) => match resolve_token(&st, a, &request_proxy).await {
                Ok(t) => (t, false),
                Err(e) => {
                    tracing::debug!("management APICall token resolution failed: {e}");
                    (String::new(), true)
                }
            },
            None => (String::new(), false),
        };
        token = Some(resolved);
    }
    let token_value = |token: &Option<(String, bool)>| -> Result<String, ApiError> {
        let (t, errored) = token.clone().unwrap_or_default();
        if !t.is_empty() {
            return Ok(t);
        }
        Err(ApiError::bad_request(if errored {
            "auth token refresh failed"
        } else if !auth_index.is_empty() && auth.is_none() {
            "auth credential not found for auth_index"
        } else {
            "auth token not found"
        }))
    };
    for value in headers.values_mut() {
        if value.contains("$TOKEN$") {
            *value = value.replace("$TOKEN$", &token_value(&token)?);
        }
    }
    if req.data.contains("$TOKEN$") {
        let t = token_value(&token)?;
        let replacement = if serde_json::from_str::<serde::de::IgnoredAny>(&req.data).is_ok()
            && t.contains(['"', '\\', '\r', '\n', '\t'])
        {
            let quoted = serde_json::to_string(&t).unwrap_or_default();
            quoted
                .get(1..quoted.len().saturating_sub(1))
                .unwrap_or("")
                .to_string()
        } else {
            t
        };
        req.data = req.data.replace("$TOKEN$", &replacement);
    }

    let http_method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| ApiError::bad_request("failed to build request"))?;
    let client = build_client(&select_proxy(&st.cfg(), auth.as_ref(), &request_proxy));
    let mut builder = client.request(http_method, &url_str);
    for (key, value) in &headers {
        if key.eq_ignore_ascii_case("host") {
            let host = value.trim();
            if !host.is_empty() {
                builder = builder.header(reqwest::header::HOST, host);
            }
            continue;
        }
        builder = builder.header(key.as_str(), value.as_str());
    }
    if !req.data.is_empty() {
        builder = builder.body(req.data.clone());
    }
    let built = builder
        .build()
        .map_err(|_| ApiError::bad_request("failed to build request"))?;
    let resp = client.execute(built).await.map_err(|e| {
        tracing::debug!("management APICall request failed: {}", e.without_url());
        ApiError::new(502, "request failed")
    })?;
    let status = resp.status().as_u16();
    let mut header: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in resp.headers() {
        header
            .entry(canonical_header_name(name.as_str()))
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|_| ApiError::new(502, "failed to read response"))?;
    Ok(ok_struct(
        &json!({"status_code": status, "header": header, "body": String::from_utf8_lossy(&bytes)}),
    ))
}

/// Go canonical header form (`content-type` -> `Content-Type`).
fn canonical_header_name(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut c = part.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + &c.as_str().to_ascii_lowercase())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn github_token() -> String {
    for name in ["GITHUB_TOKEN", "github_token"] {
        if let Some(t) = std::env::var(name).ok().and_then(|v| non_empty(&v)) {
            return t;
        }
    }
    let git_url = std::env::var("GITSTORE_GIT_URL")
        .unwrap_or_default()
        .to_lowercase();
    if !git_url.contains("github.com") {
        return String::new();
    }
    std::env::var("GITSTORE_GIT_TOKEN")
        .ok()
        .and_then(|v| non_empty(&v))
        .unwrap_or_default()
}

/// `GET /server/latest-version`: tag of the latest GitHub release.
pub(crate) async fn latest_version(State(st): State<ManagementState>) -> ApiResult {
    const URL: &str = "https://api.github.com/repos/router-for-me/CLIProxyAPI/releases/latest";
    let proxy = st.cfg().proxy_url.trim().to_string();
    let client = cpa_auth::http::build_client(&proxy, Some(Duration::from_secs(10)))
        .map_err(|e| ApiError::with_message(500, "request_create_failed", e.to_string()))?;
    let mut req = client
        .get(URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "CLIProxyAPI");
    let token = github_token();
    if !token.is_empty() {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| ApiError::with_message(502, "request_failed", e.without_url().to_string()))?;
    let status = resp.status();
    if status.as_u16() != 200 {
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(1024).collect();
        return Err(ApiError::with_message(
            502,
            "unexpected_status",
            format!("status {}: {}", status.as_u16(), snippet.trim()),
        ));
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Release {
        tag_name: String,
        name: String,
    }
    let info: Release = resp
        .json()
        .await
        .map_err(|e| ApiError::with_message(502, "decode_failed", e.without_url().to_string()))?;
    let version = [info.tag_name, info.name]
        .into_iter()
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty());
    match version {
        Some(v) => Ok(ok_json(&json!({"latest-version": v}))),
        None => Err(ApiError::with_message(
            502,
            "invalid_response",
            "missing release version",
        )),
    }
}

// ---- plugins (not provided by this build) ----

/// `GET /plugins`: the shape Go returns when no plugin is installed.
pub(crate) async fn list_plugins(State(st): State<ManagementState>) -> ApiResult {
    let cfg = st.cfg();
    Ok(ok_struct(
        &json!({"plugins_enabled": false, "plugins_dir": cfg.plugins.dir, "plugins": []}),
    ))
}

/// Plugin install, store, quota and delete endpoints answer like a disabled feature.
pub(crate) async fn plugins_unavailable() -> Response {
    crate::http::json_response(
        501,
        &json!({"error": "plugins_not_supported", "message": "plugins are not supported by this server"}),
    )
}

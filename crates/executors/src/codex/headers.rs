//! Upstream request headers for Codex HTTP and websocket calls (Go: codex_executor_request.go and
//! codex_websockets_request.go).
//!
//! Everything is built on a case-insensitive [`HeaderMap`]; [`WireHeaders`] re-applies Go's header
//! casing (and the case-preserved `session_id` / `ChatGPT-Account-ID`) when the websocket handshake
//! is written by hand.

use std::collections::HashMap;

use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::misc::ensure_header;
use cpa_core::registry::model_override_headers;
use cpa_core::util::apply_custom_headers_from_attrs;
use cpa_json::J;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use super::creds::{auth_uses_api_key, is_cloaking_disabled};

pub const USER_AGENT: &str = "codex-tui/0.154.0 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.154.0)";
pub const ORIGINATOR: &str = "codex-tui";
pub const RESPONSES_LITE_HEADER: &str = "X-OpenAI-Internal-Codex-Responses-Lite";
pub const WEBSOCKET_BETA_HEADER_VALUE: &str = "responses_websockets=2026-02-06";
pub const ROUTING_HINT_HEADER: &str = "X-Codex-Routing-Hint";

/// Trimmed first non-blank value of `name` (case-insensitive by construction).
pub fn header_value(headers: &HeaderMap, name: &str) -> String {
    let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else { return String::new() };
    headers
        .get_all(&name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Sets `name` to `value`; invalid names or values are ignored.
pub fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
        headers.insert(name, value);
    }
}

pub fn del_header(headers: &mut HeaderMap, name: &str) {
    if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
        headers.remove(name);
    }
}

/// Go `ensureHeaderWithPriority`: existing, then client, then config, then fallback.
pub fn ensure_header_with_priority(target: &mut HeaderMap, source: &HeaderMap, key: &str, config_value: &str, fallback: &str) {
    if !header_value(target, key).is_empty() {
        return;
    }
    let from_source = header_value(source, key);
    for candidate in [from_source.as_str(), config_value.trim(), fallback.trim()] {
        if !candidate.is_empty() {
            set_header(target, key, candidate);
            return;
        }
    }
}

/// Go `ensureHeaderWithConfigPrecedence`: existing, then config, then client, then fallback.
pub fn ensure_header_with_config_precedence(target: &mut HeaderMap, source: &HeaderMap, key: &str, config_value: &str, fallback: &str) {
    if !header_value(target, key).is_empty() {
        return;
    }
    let from_source = header_value(source, key);
    for candidate in [config_value.trim(), from_source.as_str(), fallback.trim()] {
        if !candidate.is_empty() {
            set_header(target, key, candidate);
            return;
        }
    }
}

/// `(user_agent, beta_features)` config defaults, OAuth credentials only (Go: codexHeaderDefaults).
pub fn header_defaults(cfg: &Config, auth: &Auth) -> (String, String) {
    if auth_uses_api_key(auth) {
        return (String::new(), String::new());
    }
    (cfg.codex_header_defaults.user_agent.trim().to_string(), cfg.codex_header_defaults.beta_features.trim().to_string())
}

/// First non-blank session header among `Session-Id`, `Session_id`, `session_id`.
pub fn session_header_value(headers: &HeaderMap) -> String {
    for key in ["Session-Id", "Session_id", "session_id"] {
        let value = header_value(headers, key);
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

fn attrs_map(auth: &Auth) -> HashMap<String, String> {
    auth.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Applies the auth's `header:*` attributes (static values and `$Header` references).
pub fn apply_custom_headers(headers: &mut HeaderMap, auth: &Auth, client: &HeaderMap, session_id: Option<&str>) {
    apply_custom_headers_from_attrs(headers, &attrs_map(auth), Some(client), session_id);
}

/// Forces the pinned CLI identity unless cloaking is disabled (Go: applyCodexCloakingHeaders).
pub fn apply_cloaking_headers(headers: &mut HeaderMap, cfg: &Config, auth: &Auth) {
    if is_cloaking_disabled(cfg, auth) {
        return;
    }
    set_header(headers, "User-Agent", USER_AGENT);
    set_header(headers, "Originator", ORIGINATOR);
}

/// Headers of an HTTP/SSE (or compact) Responses call (Go: applyCodexHeadersFromSources).
/// `headers` may already hold `Session-Id` from the prompt cache helper.
pub fn apply_codex_headers(
    headers: &mut HeaderMap,
    auth: &Auth,
    token: &str,
    stream: bool,
    cfg: &Config,
    client: &HeaderMap,
    session_id: Option<&str>,
) {
    set_header(headers, "Content-Type", "application/json");
    if token.trim().is_empty() {
        del_header(headers, "Authorization");
    } else {
        set_header(headers, "Authorization", &format!("Bearer {token}"));
    }
    if let Some(beta) = client.get("x-codex-beta-features").and_then(|v| v.to_str().ok())
        && !beta.is_empty()
    {
        set_header(headers, "X-Codex-Beta-Features", beta);
    }
    for key in [
        "Version",
        "X-Codex-Turn-Metadata",
        "X-Codex-Turn-State",
        "X-Client-Request-Id",
        "X-Codex-Window-Id",
        "Thread-Id",
        "Session-Id",
        "X-Openai-Internal-Codex-Responses-Lite",
    ] {
        ensure_header(headers, Some(client), key, "");
    }
    let (cfg_user_agent, _) = header_defaults(cfg, auth);
    ensure_header_with_config_precedence(headers, client, "User-Agent", &cfg_user_agent, USER_AGENT);
    set_header(headers, "Accept", if stream { "text/event-stream" } else { "application/json" });
    set_header(headers, "Connection", "Keep-Alive");
    apply_originator_and_account(headers, auth, client, false);
    apply_custom_headers(headers, auth, client, session_id);
    apply_cloaking_headers(headers, cfg, auth);
}

/// `Originator` (client value, else `codex-tui` for OAuth) and the OAuth account id header.
fn apply_originator_and_account(headers: &mut HeaderMap, auth: &Auth, client: &HeaderMap, websocket: bool) {
    let is_api_key = auth_uses_api_key(auth);
    let originator = header_value(client, "Originator");
    if !originator.is_empty() {
        set_header(headers, "Originator", &originator);
    } else if !is_api_key {
        set_header(headers, "Originator", ORIGINATOR);
    }
    if is_api_key {
        return;
    }
    if let Some(Value::String(account_id)) = auth.metadata.get("account_id") {
        if websocket {
            let trimmed = account_id.trim();
            if !trimmed.is_empty() {
                set_header(headers, "ChatGPT-Account-ID", trimmed);
            }
        } else {
            set_header(headers, "Chatgpt-Account-Id", account_id);
        }
    }
}

/// Prompt-cache aware session identity headers of a websocket handshake (Go:
/// applyCodexPromptCacheHeadersWithContext, header half): `session_id` and `Conversation_id`.
pub fn websocket_cache_headers(cache_id: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if !cache_id.is_empty() {
        set_header(&mut headers, "session_id", cache_id);
        set_header(&mut headers, "Conversation_id", cache_id);
    }
    headers
}

/// Handshake headers of an upstream Responses websocket (Go: applyCodexWebsocketHeaders).
pub fn apply_websocket_headers(
    headers: &mut HeaderMap,
    auth: &Auth,
    token: &str,
    cfg: &Config,
    native_request: bool,
    client: &HeaderMap,
    session_id: Option<&str>,
) {
    if token.trim().is_empty() {
        del_header(headers, "Authorization");
    } else {
        set_header(headers, "Authorization", &format!("Bearer {token}"));
    }
    let is_api_key = auth_uses_api_key(auth);
    let (cfg_user_agent, cfg_beta) = header_defaults(cfg, auth);
    ensure_header_with_priority(headers, client, "x-codex-beta-features", &cfg_beta, "");
    for key in ["x-codex-turn-state", "x-codex-turn-metadata", "x-client-request-id", "x-responsesapi-include-timing-metrics", "Version"] {
        ensure_header(headers, Some(client), key, "");
    }
    if native_request {
        ensure_header(headers, Some(client), super::headers::RESPONSES_LITE_HEADER, "");
    }
    if is_api_key {
        ensure_header_with_priority(headers, client, "User-Agent", "", "");
    } else {
        ensure_header_with_config_precedence(headers, client, "User-Agent", &cfg_user_agent, USER_AGENT);
    }

    let mut beta = header_value(headers, "OpenAI-Beta");
    if beta.is_empty() {
        beta = header_value(client, "OpenAI-Beta");
    }
    if beta.is_empty() || !beta.contains("responses_websockets=") {
        beta = WEBSOCKET_BETA_HEADER_VALUE.to_string();
    }
    set_header(headers, "OpenAI-Beta", &beta);

    let fallback = if header_value(headers, "User-Agent").contains("Mac OS") { uuid::Uuid::new_v4().to_string() } else { String::new() };
    ensure_session_header(headers, client, &fallback);
    if native_request && is_cloaking_disabled(cfg, auth) {
        del_header(headers, "session_id");
        del_header(headers, "conversation_id");
        for key in ["session-id", "session_id", "conversation_id", "thread-id", "x-codex-routing-hint", "x-codex-window-id"] {
            if let Ok(name) = HeaderName::from_bytes(key.as_bytes()) {
                headers.remove(&name);
                for value in client.get_all(&name) {
                    headers.append(name.clone(), value.clone());
                }
            }
        }
    }
    apply_originator_and_account(headers, auth, client, true);
    apply_custom_headers(headers, auth, client, session_id);
    apply_cloaking_headers(headers, cfg, auth);
}

/// Sets the lower-case `session_id` header from the existing or client session header, else the
/// fallback; the hyphenated `Session-Id` is dropped (Go: ensureCodexWebsocketSessionHeader).
fn ensure_session_header(target: &mut HeaderMap, source: &HeaderMap, fallback: &str) {
    let mut session = session_header_value(target);
    if session.is_empty() {
        session = session_header_value(source);
    }
    if session.is_empty() {
        session = fallback.trim().to_string();
    }
    if !session.is_empty() {
        set_header(target, "session_id", &session);
    }
    del_header(target, "Session-Id");
}

/// Sets the routing hint native Codex attaches to ChatGPT-backend requests: `model=<slug>` plus
/// `;tier=<service_tier>` (Go: applyCodexRoutingHint). API-key credentials are left alone; an
/// operator `header:` rule that resolves to a value wins over the derived hint.
pub fn apply_routing_hint(
    headers: &mut HeaderMap,
    auth: &Auth,
    base_model: &str,
    upstream_body: &[u8],
    client: &HeaderMap,
    session_id: Option<&str>,
) {
    if auth_uses_api_key(auth) {
        return;
    }
    del_header(headers, ROUTING_HINT_HEADER);
    if !auth.attributes.is_empty() {
        let mut resolved = HeaderMap::new();
        apply_custom_headers(&mut resolved, auth, client, session_id);
        let operator = header_value(&resolved, ROUTING_HINT_HEADER);
        if !operator.is_empty() {
            set_header(headers, ROUTING_HINT_HEADER, &operator);
            return;
        }
    }
    let model = base_model.trim();
    if model.is_empty() {
        return;
    }
    let mut hint = format!("model={model}");
    let body = cpa_json::parse(upstream_body);
    if let Some(Value::String(tier)) = body.g("service_tier").v()
        && !tier.trim().is_empty()
    {
        hint.push_str(";tier=");
        hint.push_str(tier.trim());
    }
    set_header(headers, ROUTING_HINT_HEADER, &hint);
}

/// Forces `config.override_header` of the model onto the request (Go: applyModelHeaderOverrides).
pub fn apply_model_header_overrides(headers: &mut HeaderMap, base_model: &str) {
    let Some(overrides) = model_override_headers(base_model, None) else { return };
    for (key, value) in overrides {
        set_header(headers, &key, &value);
    }
    if header_value(headers, "User-Agent").contains("Mac OS") && session_header_value(headers).is_empty() {
        set_header(headers, "Session_id", &uuid::Uuid::new_v4().to_string());
    }
}

/// Header names as written on the wire by Go: canonical `Title-Case`, except names Go sets by
/// direct map assignment (`session_id`, `ChatGPT-Account-ID`).
#[derive(Debug, Clone, Default)]
pub struct WireHeaders(pub Vec<(String, String)>);

impl WireHeaders {
    pub fn from_map(headers: &HeaderMap) -> Self {
        let mut out = Vec::with_capacity(headers.len());
        for (name, value) in headers {
            let Ok(value) = value.to_str() else { continue };
            out.push((wire_name(name.as_str()), value.to_string()));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        WireHeaders(out)
    }
}

fn wire_name(lower: &str) -> String {
    match lower {
        "session_id" => return "session_id".to_string(),
        "chatgpt-account-id" => return "ChatGPT-Account-ID".to_string(),
        _ => {}
    }
    let mut out = String::with_capacity(lower.len());
    let mut upper = true;
    for c in lower.chars() {
        out.push(if upper { c.to_ascii_uppercase() } else { c });
        upper = c == '-';
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth_auth() -> Auth {
        let mut auth = Auth::new("a", "codex");
        auth.metadata.insert("access_token".into(), "tok".into());
        auth.metadata.insert("account_id".into(), " acct-1 ".into());
        auth
    }

    fn api_key_auth() -> Auth {
        let mut auth = Auth::new("k", "codex");
        auth.attributes.insert("api_key".into(), "sk-1".into());
        auth
    }

    #[test]
    fn http_headers_for_oauth_credentials_are_cloaked() {
        let cfg = Config::default();
        let mut headers = HeaderMap::new();
        let mut client = HeaderMap::new();
        set_header(&mut client, "User-Agent", "my-client/1.0");
        set_header(&mut client, "X-Codex-Turn-State", "state");
        apply_codex_headers(&mut headers, &oauth_auth(), "tok", true, &cfg, &client, None);
        assert_eq!(header_value(&headers, "authorization"), "Bearer tok");
        assert_eq!(header_value(&headers, "user-agent"), USER_AGENT);
        assert_eq!(header_value(&headers, "originator"), "codex-tui");
        assert_eq!(header_value(&headers, "x-codex-turn-state"), "state");
        assert_eq!(header_value(&headers, "accept"), "text/event-stream");
        assert_eq!(headers.get("chatgpt-account-id").unwrap(), " acct-1 ");
    }

    #[test]
    fn disabled_cloaking_keeps_client_user_agent_and_api_key_has_no_originator() {
        let mut cfg = Config::default();
        cfg.codex.disable_codex_cloaking = true;
        let mut headers = HeaderMap::new();
        let mut client = HeaderMap::new();
        set_header(&mut client, "User-Agent", "my-client/1.0");
        apply_codex_headers(&mut headers, &api_key_auth(), "sk-1", false, &cfg, &client, None);
        assert_eq!(header_value(&headers, "user-agent"), "my-client/1.0");
        assert!(headers.get("originator").is_none());
        assert!(headers.get("chatgpt-account-id").is_none());
        assert_eq!(header_value(&headers, "accept"), "application/json");
    }

    #[test]
    fn websocket_handshake_headers_use_session_id_and_beta_value() {
        let cfg = Config::default();
        let mut headers = websocket_cache_headers("cache-1");
        apply_websocket_headers(&mut headers, &oauth_auth(), "tok", &cfg, false, &HeaderMap::new(), None);
        let wire = WireHeaders::from_map(&headers);
        let names: Vec<&str> = wire.0.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"session_id") && names.contains(&"ChatGPT-Account-ID") && names.contains(&"Openai-Beta"));
        assert_eq!(header_value(&headers, "openai-beta"), "responses_websockets=2026-02-06");
        assert_eq!(header_value(&headers, "session_id"), "cache-1");
        assert_eq!(header_value(&headers, "conversation_id"), "cache-1");
        assert_eq!(header_value(&headers, "chatgpt-account-id"), "acct-1");
        assert!(headers.get("session-id").is_none());
    }

    #[test]
    fn routing_hint_carries_model_and_tier_for_oauth_only() {
        let mut headers = HeaderMap::new();
        set_header(&mut headers, ROUTING_HINT_HEADER, "model=client");
        apply_routing_hint(&mut headers, &oauth_auth(), " gpt-5 ", br#"{"service_tier":"priority"}"#, &HeaderMap::new(), None);
        assert_eq!(header_value(&headers, ROUTING_HINT_HEADER), "model=gpt-5;tier=priority");
        let mut api = HeaderMap::new();
        apply_routing_hint(&mut api, &api_key_auth(), "gpt-5", b"{}", &HeaderMap::new(), None);
        assert!(api.is_empty());
    }

    fn ws_headers(auth: &Auth, token: &str, cfg: &Config, native: bool, client: &HeaderMap, initial: HeaderMap) -> HeaderMap {
        let mut headers = initial;
        apply_websocket_headers(&mut headers, auth, token, cfg, native, client, None);
        headers
    }

    fn client(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            set_header(&mut h, k, v);
        }
        h
    }

    #[test]
    fn websocket_cloaking_overrides_custom_existing_and_client_identity() {
        let mut cfg = Config::default();
        cfg.codex_header_defaults.user_agent = "config-ua".into();
        for mut auth in [oauth_auth(), api_key_auth()] {
            auth.attributes.insert("header:User-Agent".into(), "custom-ua".into());
            auth.attributes.insert("header:Originator".into(), "custom-origin".into());
            let initial = client(&[("User-Agent", "existing-ua"), ("Originator", "existing-origin")]);
            let headers = ws_headers(&auth, "tok", &cfg, false, &client(&[("User-Agent", "client-ua")]), initial);
            assert_eq!(header_value(&headers, "user-agent"), USER_AGENT);
            assert_eq!(header_value(&headers, "originator"), ORIGINATOR);
        }
    }

    #[test]
    fn websocket_native_requests_forward_client_session_headers_when_cloaking_is_disabled() {
        let mut cfg = Config::default();
        cfg.codex.disable_codex_cloaking = true;
        let c = client(&[
            ("Originator", "Codex Desktop"),
            ("User-Agent", "codex_cli_rs/0.1.0"),
            ("session-id", "legacy-session"),
            ("Thread-Id", "thread-1"),
            ("X-Codex-Routing-Hint", "route-1"),
            ("X-Codex-Window-Id", "window-1"),
        ]);
        let headers = ws_headers(&oauth_auth(), "", &cfg, true, &c, websocket_cache_headers("cache-key"));
        assert_eq!(header_value(&headers, "originator"), "Codex Desktop");
        assert_eq!(header_value(&headers, "user-agent"), "codex_cli_rs/0.1.0");
        assert!(headers.get("session_id").is_none() && headers.get("conversation_id").is_none());
        for (key, want) in [("session-id", "legacy-session"), ("thread-id", "thread-1"), ("x-codex-routing-hint", "route-1"), ("x-codex-window-id", "window-1")] {
            assert_eq!(header_value(&headers, key), want, "{key}");
        }
        // Without client session headers nothing is synthesized from the cache aliases.
        let headers = ws_headers(&oauth_auth(), "", &cfg, true, &HeaderMap::new(), websocket_cache_headers("cache-key"));
        assert!(headers.get("session_id").is_none() && headers.get("session-id").is_none());
    }

    #[test]
    fn websocket_user_agent_precedence_and_api_key_scope() {
        let mut cfg = Config::default();
        cfg.codex.disable_codex_cloaking = true;
        cfg.codex_header_defaults.user_agent = "config-ua".into();
        cfg.codex_header_defaults.beta_features = "config-beta".into();
        let c = client(&[("User-Agent", "client-ua"), ("X-Codex-Beta-Features", "client-beta")]);
        // Config user agent beats the client; the client beta beats the config default.
        let headers = ws_headers(&oauth_auth(), "", &cfg, false, &c, HeaderMap::new());
        assert_eq!(header_value(&headers, "user-agent"), "config-ua");
        assert_eq!(header_value(&headers, "x-codex-beta-features"), "client-beta");
        // Existing headers beat both.
        let existing = client(&[("User-Agent", "existing-ua"), ("X-Codex-Beta-Features", "existing-beta")]);
        let headers = ws_headers(&oauth_auth(), "", &cfg, false, &c, existing);
        assert_eq!((header_value(&headers, "user-agent").as_str(), header_value(&headers, "x-codex-beta-features").as_str()), ("existing-ua", "existing-beta"));
        // API-key credentials ignore the config defaults and get no Originator of their own.
        let headers = ws_headers(&api_key_auth(), "sk-1", &cfg, false, &HeaderMap::new(), HeaderMap::new());
        assert!(headers.get("user-agent").is_none() && headers.get("x-codex-beta-features").is_none() && headers.get("originator").is_none());
        let explicit = client(&[("User-Agent", "api-key-client/1.0"), ("Originator", "explicit-origin")]);
        let headers = ws_headers(&api_key_auth(), "sk-1", &cfg, false, &explicit, HeaderMap::new());
        assert_eq!((header_value(&headers, "user-agent").as_str(), header_value(&headers, "originator").as_str()), ("api-key-client/1.0", "explicit-origin"));
    }

    #[test]
    fn websocket_legacy_underscore_session_header_is_canonicalized() {
        let mut cfg = Config::default();
        cfg.codex.disable_codex_cloaking = true;
        let headers = ws_headers(&oauth_auth(), "", &cfg, false, &client(&[("Session_id", "legacy-underscore-session")]), HeaderMap::new());
        assert_eq!(header_value(&headers, "session_id"), "legacy-underscore-session");
        assert!(headers.get("session-id").is_none());
    }

    #[test]
    fn empty_token_omits_authorization_and_api_key_requests_carry_no_oauth_headers() {
        let cfg = Config::default();
        let mut headers = HeaderMap::new();
        apply_codex_headers(&mut headers, &api_key_auth(), "  ", true, &cfg, &HeaderMap::new(), None);
        assert!(headers.get("authorization").is_none() && headers.get("chatgpt-account-id").is_none());
        let headers = ws_headers(&api_key_auth(), "", &cfg, false, &HeaderMap::new(), HeaderMap::new());
        assert!(headers.get("authorization").is_none() && headers.get("chatgpt-account-id").is_none());
    }
}

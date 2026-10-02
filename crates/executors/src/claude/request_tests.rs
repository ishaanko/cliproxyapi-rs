//! Header and beta assembly tests ported from claude_executor_beta_policy_test.go and
//! claude_executor_test.go. Expectations are the Go assertions.

use cpa_auth::Auth;
use cpa_config::Config;
use http::HeaderMap;
use url::Url;

use super::request::*;

const OAUTH_KEY: &str = "sk-ant-oat-beta-policy";

fn oauth_auth() -> Auth {
    let mut auth = Auth::new("claude-beta-policy", "claude");
    auth.metadata.insert("access_token".into(), OAUTH_KEY.into());
    auth
}

fn api_key_auth(key: &str) -> Auth {
    let mut auth = Auth::new("key-auth", "claude");
    auth.attributes.insert("api_key".into(), key.to_string());
    auth
}

fn incoming(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        set_header(&mut h, k, v);
    }
    h
}

#[allow(clippy::too_many_arguments)]
fn apply(
    auth: &Auth,
    api_key: &str,
    stream: bool,
    extra_betas: &[&str],
    body: &str,
    incoming_headers: &HeaderMap,
    confirmed: bool,
    helper: bool,
    url: &str,
) -> HeaderMap {
    let cfg = Config::default();
    let extra: Vec<String> = extra_betas.iter().map(|s| s.to_string()).collect();
    let url = Url::parse(url).unwrap();
    let mut headers = HeaderMap::new();
    apply_claude_headers_with_native_profile(
        &mut headers,
        &ClaudeHeaderInput {
            auth,
            api_key,
            stream,
            extra_betas: &extra,
            body: body.as_bytes(),
            cfg: &cfg,
            incoming_headers,
            confirmed_claude_code: confirmed,
            helper_profile: helper,
            session_ids: &[],
            url: &url,
            cpa_session_id: None,
        },
    )
    .unwrap();
    headers
}

fn messages(
    auth: &Auth,
    api_key: &str,
    stream: bool,
    extra: &[&str],
    body: &str,
    inc: &HeaderMap,
    confirmed: bool,
) -> HeaderMap {
    apply(auth, api_key, stream, extra, body, inc, confirmed, false, "https://api.anthropic.com/v1/messages")
}

fn beta(h: &HeaderMap) -> String {
    h.get("anthropic-beta").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

#[test]
fn confirmed_client_keeps_oauth_credential_betas() {
    let inc = incoming(&[("anthropic-beta", &format!("{CLAUDE_CODE_BETA},interleaved-thinking-2025-05-14,{CLAUDE_EFFORT_BETA}"))]);
    let h = messages(&oauth_auth(), OAUTH_KEY, false, &[], r#"{"model":"claude-opus-5"}"#, &inc, true);
    let got = beta(&h);
    let parts: Vec<&str> = got.split(',').collect();
    assert!(parts.len() >= 2 && parts[0] == CLAUDE_CODE_BETA && parts[1] == CLAUDE_OAUTH_BETA, "{got}");
    assert_eq!(*parts.last().unwrap(), CLAUDE_EXTENDED_CACHE_TTL_BETA, "{got}");
    assert!(!got.contains("advisor-tool-2026-03-01") && !got.contains(CLAUDE_CACHE_DIAGNOSIS_BETA), "{got}");
    for want in ["interleaved-thinking-2025-05-14", CLAUDE_EFFORT_BETA] {
        assert!(got.contains(want), "{got}");
    }
}

#[test]
fn confirmed_api_key_client_keeps_pure_passthrough() {
    let inc = incoming(&[("anthropic-beta", &format!("{CLAUDE_CODE_BETA},{CLAUDE_EFFORT_BETA}"))]);
    let h = messages(&api_key_auth("key-passthrough"), "key-passthrough", false, &[], r#"{"model":"claude-opus-5"}"#, &inc, true);
    assert_eq!(beta(&h), format!("{CLAUDE_CODE_BETA},{CLAUDE_EFFORT_BETA}"));
}

#[test]
fn unknown_body_beta_preserved_on_anthropic() {
    let h = messages(
        &api_key_auth("key-body-beta"),
        "key-body-beta",
        false,
        &["unknown-body-probe-2099-01-01"],
        r#"{"model":"claude-opus-5"}"#,
        &HeaderMap::new(),
        false,
    );
    assert_eq!(beta(&h), "unknown-body-probe-2099-01-01");
}

#[test]
fn known_body_beta_still_placed_on_anthropic() {
    let h = messages(
        &api_key_auth("key-known-body-beta"),
        "key-known-body-beta",
        false,
        &[CLAUDE_CONTEXT_1M_BETA],
        r#"{"model":"claude-opus-5"}"#,
        &HeaderMap::new(),
        false,
    );
    assert_eq!(beta(&h), CLAUDE_CONTEXT_1M_BETA);
}

#[test]
fn custom_headers_cannot_override_anthropic_identity() {
    let mut auth = api_key_auth("key-custom-headers");
    auth.attributes.insert("header:Anthropic-Beta".into(), "attacker-controlled-2099-01-01".into());
    auth.attributes.insert("header:Accept-Encoding".into(), "identity".into());
    for stream in [false, true] {
        let h = messages(&auth, "key-custom-headers", stream, &[], r#"{"model":"claude-opus-5"}"#, &HeaderMap::new(), false);
        assert_ne!(beta(&h), "attacker-controlled-2099-01-01", "stream={stream}");
        assert_eq!(h.get("accept-encoding").unwrap(), "gzip, deflate, br, zstd", "stream={stream}");
    }
}

#[test]
fn fast_mode_beta_matches_across_stream_modes_and_precedes_oauth_trailer() {
    let auth = api_key_auth("key-fast-parity");
    let body = r#"{"model":"claude-opus-5","speed":"fast"}"#;
    let non_stream = beta(&messages(&auth, "key-fast-parity", false, &[], body, &HeaderMap::new(), false));
    let stream = beta(&messages(&auth, "key-fast-parity", true, &[], body, &HeaderMap::new(), false));
    assert!(non_stream.contains(CLAUDE_FAST_MODE_BETA));
    assert_eq!(non_stream, stream);

    let h = messages(&oauth_auth(), OAUTH_KEY, true, &[], body, &HeaderMap::new(), false);
    let got = beta(&h);
    let parts: Vec<&str> = got.split(',').collect();
    assert_eq!(parts[parts.len() - 1], CLAUDE_EXTENDED_CACHE_TTL_BETA, "{got}");
    assert_eq!(parts[parts.len() - 2], CLAUDE_FAST_MODE_BETA, "{got}");
    assert!(!got.contains(CLAUDE_CACHE_DIAGNOSIS_BETA));
}

#[test]
fn diagnostics_beta_follows_body_in_native_order() {
    for stream in [false, true] {
        let body = r#"{"model":"claude-opus-5","diagnostics":{"previous_message_id":null}}"#;
        let got = beta(&messages(&oauth_auth(), OAUTH_KEY, stream, &[], body, &HeaderMap::new(), false));
        assert!(got.ends_with(&format!("{CLAUDE_EXTENDED_CACHE_TTL_BETA},{CLAUDE_CACHE_DIAGNOSIS_BETA}")), "{got}");
    }
}

#[test]
fn fast_mode_credit_refusal_is_request_scoped_and_body_exact() {
    let bodies = [
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"Usage credits are required for fast mode."}}"#,
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"Fast mode requires usage credits"}}"#,
    ];
    for body in bodies {
        let err = classify_claude_upstream_error_with_cooling(429, &HeaderMap::new(), body.as_bytes(), false);
        assert!(err.is_request_scoped(), "{body}");
        assert_eq!(err.status, 429);
        assert_eq!(err.message, body);
    }
    let real = [
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"Number of requests has exceeded your rate limit."}}"#,
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"This organization has exceeded its usage limit."}}"#,
    ];
    for body in real {
        assert!(!classify_claude_upstream_error_with_cooling(429, &HeaderMap::new(), body.as_bytes(), false).is_request_scoped());
    }
    let other = r#"{"error":{"message":"Usage credits are required for fast mode."}}"#;
    assert!(!classify_claude_upstream_error_with_cooling(500, &HeaderMap::new(), other.as_bytes(), false).is_request_scoped());
}

fn advisor_order_ok(got: &str) -> bool {
    let parts: Vec<&str> = got.split(',').map(str::trim).collect();
    let pos = |name: &str| parts.iter().position(|p| *p == name);
    let adv = pos("advisor-tool-2026-03-01");
    let mid = pos("mid-conversation-system-2026-04-07");
    let tool = pos("advanced-tool-use-2025-11-20");
    adv.is_some() && mid.is_none_or(|m| adv.unwrap() > m) && tool.is_none_or(|t| adv.unwrap() < t)
}

const ADVISOR_BODY: &str = r#"{"model":"claude-opus-5","tools":[{"type":"advisor_20260301","name":"advisor"}]}"#;

#[test]
fn advisor_tool_beta_is_placed_between_mid_conversation_and_advanced_tool_use() {
    let requested = incoming(&[(
        "anthropic-beta",
        "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24",
    )]);
    let confirmed_inc = incoming(&[(
        "anthropic-beta",
        "claude-code-20250219,interleaved-thinking-2025-05-14,mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,effort-2025-11-24",
    )]);
    let key_inc = incoming(&[(
        "anthropic-beta",
        &format!("{CLAUDE_CODE_BETA},mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,{CLAUDE_EFFORT_BETA}"),
    )]);
    let reordered = incoming(&[(
        "anthropic-beta",
        "claude-code-20250219,mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,effort-2025-11-24,advisor-tool-2026-03-01",
    )]);
    for stream in [false, true] {
        assert!(advisor_order_ok(&beta(&messages(&oauth_auth(), OAUTH_KEY, stream, &[], ADVISOR_BODY, &requested, false))));
        assert!(advisor_order_ok(&beta(&messages(&oauth_auth(), OAUTH_KEY, stream, &[], ADVISOR_BODY, &confirmed_inc, true))));
        assert!(advisor_order_ok(&beta(&messages(
            &api_key_auth("key-passthrough"),
            "key-passthrough",
            stream,
            &[],
            ADVISOR_BODY,
            &key_inc,
            false
        ))));
    }
    for confirmed in [false, true] {
        assert!(advisor_order_ok(&beta(&messages(&oauth_auth(), OAUTH_KEY, false, &[], ADVISOR_BODY, &reordered, confirmed))));
    }
}

#[test]
fn advisor_tool_beta_lifted_from_body_betas_for_confirmed_client() {
    let inc = incoming(&[("anthropic-beta", &format!("{CLAUDE_CODE_BETA},interleaved-thinking-2025-05-14,{CLAUDE_EFFORT_BETA}"))]);
    for stream in [false, true] {
        let h = messages(
            &oauth_auth(),
            OAUTH_KEY,
            stream,
            &["advisor-tool-2026-03-01"],
            r#"{"model":"claude-opus-5"}"#,
            &inc,
            true,
        );
        assert!(beta(&h).contains("advisor-tool-2026-03-01"));
    }
}

#[test]
fn advisor_tool_beta_preserved_for_count_tokens() {
    let inc = incoming(&[(
        "anthropic-beta",
        "claude-code-20250219,interleaved-thinking-2025-05-14,context-management-2025-06-27,token-counting-2024-11-01",
    )]);
    let h = apply(
        &oauth_auth(),
        OAUTH_KEY,
        false,
        &[],
        ADVISOR_BODY,
        &inc,
        false,
        false,
        "https://api.anthropic.com/v1/messages/count_tokens",
    );
    assert!(beta(&h).contains("advisor-tool-2026-03-01"));
}

#[test]
fn structured_helper_beta_order_preserved_with_advisor() {
    let helper_beta = format!(
        "{CLAUDE_CODE_BETA},oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,structured-outputs-2025-12-15,cache-diagnosis-2026-04-07"
    );
    let inc = incoming(&[("anthropic-beta", &helper_beta)]);
    let h = apply(
        &oauth_auth(),
        OAUTH_KEY,
        false,
        &[],
        r#"{"model":"claude-haiku-4-5-20251001","tools":[]}"#,
        &inc,
        true,
        true,
        "https://api.anthropic.com/v1/messages",
    );
    assert_eq!(beta(&h), helper_beta);
}

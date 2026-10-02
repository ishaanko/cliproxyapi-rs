//! Request-state tests ported from claude_executor_test.go, claude_executor_diagnostics_test.go
//! and claude_executor_fast_error_test.go: context management ownership, diagnostics injection,
//! continuity commits, fast-mode errors, setup-token detection.

use cpa_auth::Auth;
use cpa_json::J;
use http::HeaderMap;

use super::auth::{is_claude_oauth_scope_403, is_claude_setup_token, should_prepare_request_auth};
use super::body::disable_thinking_if_tool_choice_forced;
use super::cloaking::{
    CLAUDE_CODE_CONTEXT_MANAGEMENT, ClaudeCodeContextManagementState, inject_claude_code_context_management,
    reconcile_claude_code_context_management,
};
use super::diagnostics::{
    claude_message_id_from_sse, commit_claude_continuity_state, inject_claude_diagnostics, observe_claude_stream_line,
};
use super::fast_error::{new_claude_fast_direct_response_error, wrap_claude_fast_request_error};
use cpa_runtime::executor::ExecError;

const CAPTURED: &str = r#"{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#;

#[test]
fn context_management_injected_only_for_enabled_or_adaptive_thinking() {
    for payload in [
        r#"{"model":"claude-opus-5","thinking":{"type":"enabled"}}"#,
        r#"{"model":"claude-opus-5","thinking":{"type":"adaptive"}}"#,
    ] {
        let (got, injected) = inject_claude_code_context_management(payload.as_bytes());
        assert!(injected, "{payload}");
        assert_eq!(cpa_json::parse(&got).g("context_management").raw(), CAPTURED);
    }
    let caller_owned = br#"{"model":"claude-opus-4-6","context_management":{"edits":[]}}"#;
    let (got, injected) = inject_claude_code_context_management(caller_owned);
    assert!(!injected);
    assert_eq!(got, caller_owned);
    for payload in [
        r#"{"model":"claude-opus-5","thinking":{"type":"disabled"}}"#,
        r#"{"model":"claude-opus-4-6"}"#,
        r#"{"model":"claude-opus-5","thinking":{"type":"unexpected"}}"#,
    ] {
        let (got, injected) = inject_claude_code_context_management(payload.as_bytes());
        assert!(!injected && got == payload.as_bytes(), "{payload}");
    }
}

#[test]
fn context_management_never_outlives_eligible_thinking() {
    let cases = [
        (r#"{"model":"claude-opus-5","messages":[]}"#, false),
        (
            r#"{"model":"claude-opus-5","thinking":{"type":"enabled","budget_tokens":1024},"tool_choice":{"type":"any"},"messages":[]}"#,
            false,
        ),
        (r#"{"model":"claude-opus-5","thinking":{"type":"enabled","budget_tokens":1024},"messages":[]}"#, true),
    ];
    for (payload, want_cm) in cases {
        let (body, injected) = inject_claude_code_context_management(payload.as_bytes());
        let state = ClaudeCodeContextManagementState { eligible: true, automatically_injected: injected, ..Default::default() };
        let body = disable_thinking_if_tool_choice_forced(&body);
        let body = reconcile_claude_code_context_management(&body, state);
        assert_eq!(cpa_json::parse(&body).g("context_management").exists(), want_cm, "{payload}");
    }
}

#[test]
fn reconcile_context_management_ownership_table() {
    let with_automatic = |thinking: &str| {
        format!(r#"{{"thinking":{{"type":"{thinking}"}},"context_management":{CLAUDE_CODE_CONTEXT_MANAGEMENT}}}"#)
    };
    let st = |eligible, caller_owned, automatically_injected, payload_rule_touched| ClaudeCodeContextManagementState {
        eligible,
        caller_owned,
        automatically_injected,
        payload_rule_touched,
    };
    let cases: Vec<(&str, String, ClaudeCodeContextManagementState, &str)> = vec![
        ("removes unchanged automatic object when disabled", with_automatic("disabled"), st(true, false, true, false), ""),
        ("preserves rule owned object when disabled", with_automatic("disabled"), st(true, false, true, true), CAPTURED),
        (
            "preserves changed automatic object when disabled",
            r#"{"thinking":{"type":"disabled"},"context_management":{"edits":[{"type":"custom"}]}}"#.to_string(),
            st(true, false, true, false),
            r#"{"edits":[{"type":"custom"}]}"#,
        ),
        ("adds when enabled", r#"{"thinking":{"type":"enabled"}}"#.into(), st(true, false, false, false), CAPTURED),
        ("adds when adaptive", r#"{"thinking":{"type":"adaptive"}}"#.into(), st(true, false, false, false), CAPTURED),
        ("caller ownership prevents addition", r#"{"thinking":{"type":"enabled"}}"#.into(), st(true, true, false, false), ""),
        ("payload rule ownership prevents addition", r#"{"thinking":{"type":"enabled"}}"#.into(), st(true, false, false, true), ""),
        ("ineligible request prevents addition", r#"{"thinking":{"type":"enabled"}}"#.into(), st(false, false, false, false), ""),
        ("omitted thinking prevents addition", "{}".into(), st(true, false, false, false), ""),
        (
            "removes automatic object when thinking was stripped",
            format!(r#"{{"context_management":{CLAUDE_CODE_CONTEXT_MANAGEMENT}}}"#),
            st(true, false, true, false),
            "",
        ),
        (
            "keeps caller object when thinking was stripped",
            format!(r#"{{"context_management":{CLAUDE_CODE_CONTEXT_MANAGEMENT}}}"#),
            st(true, true, false, false),
            CAPTURED,
        ),
        ("unknown thinking prevents addition", r#"{"thinking":{"type":"unexpected"}}"#.into(), st(true, false, false, false), ""),
        ("invalid thinking prevents addition", r#"{"thinking":{"type":123}}"#.into(), st(true, false, false, false), ""),
    ];
    for (name, payload, state, want) in cases {
        let got = reconcile_claude_code_context_management(payload.as_bytes(), state);
        assert_eq!(cpa_json::parse(&got).g("context_management").raw(), want, "{name}");
    }
}

#[test]
fn diagnostics_follow_context_management_and_chain_message_ids() {
    let body = br#"{"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"max_tokens":1,"messages":[]}"#;
    let test_id = uuid::Uuid::new_v4();
    let mut auth = Auth::new(format!("credential-diagnostics-order-{test_id}"), "claude");
    auth.index = String::new();
    let session = format!("session-diagnostics-order-{test_id}");
    let (first, state) = inject_claude_diagnostics(body, &auth, &session);
    let first_text = String::from_utf8(first.clone()).unwrap();
    let want_order = r#""context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"diagnostics":{"previous_message_id":null},"max_tokens""#;
    assert!(first_text.contains(want_order), "{first_text}");

    commit_claude_continuity_state(&state, "msg_01ABCDEF0123456789ABCDEFG", "");
    let (second, _) = inject_claude_diagnostics(body, &auth, &session);
    assert_eq!(cpa_json::parse(&second).g("diagnostics.previous_message_id").str(), "msg_01ABCDEF0123456789ABCDEFG");
}

#[test]
fn message_id_from_sse_commits_only_completed_messages() {
    let complete = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_complete\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    assert_eq!(claude_message_id_from_sse(complete.as_bytes()), "msg_complete");
    let incomplete = complete.replace("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n", "");
    assert_eq!(claude_message_id_from_sse(incomplete.as_bytes()), "");

    let (mut id, mut done) = (String::new(), false);
    observe_claude_stream_line(b"event: message_start", &mut id, &mut done);
    observe_claude_stream_line(b"data: {not json", &mut id, &mut done);
    assert!(id.is_empty() && !done);
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        super::request::set_header(&mut h, k, v);
    }
    h
}

#[test]
fn fast_errors_are_request_scoped_and_never_expose_a_success_status() {
    let upstream = ExecError::new(429, "limit").with_credential_scope();
    let wrapped = wrap_claude_fast_request_error(true, 429, upstream);
    assert!(wrapped.credential_scoped && !wrapped.is_request_scoped(), "credential-scoped cause stays scoped");

    let transport = wrap_claude_fast_request_error(true, 0, ExecError::new(0, "transport unavailable"));
    assert!(transport.is_request_scoped());
    assert_eq!(transport.message, "transport unavailable");

    // A decode failure on a 2xx must not surface the upstream success status.
    let decode = wrap_claude_fast_request_error(true, 200, ExecError::new(0, "decode"));
    assert_eq!(decode.status, 0);
    assert!(decode.is_request_scoped());

    // Without fast mode the error passes through untouched.
    let plain = wrap_claude_fast_request_error(false, 429, ExecError::new(429, "limit"));
    assert!(!plain.is_request_scoped() && plain.status == 429);
}

#[test]
fn fast_direct_response_keeps_status_body_and_drops_representation_headers() {
    const BODY: &str = r#"{"type":"error","error":{"type":"upstream_error","message":"Fast request rejected"}}"#;
    let h = headers(&[("content-type", "application/json"), ("content-encoding", "gzip"), ("content-length", "99")]);
    for status in [400u16, 401, 403, 500, 503] {
        let err = new_claude_fast_direct_response_error(status, &h, BODY.as_bytes());
        assert_eq!(err.status, status);
        assert_eq!(err.body.as_deref(), Some(BODY.as_bytes()));
        assert_eq!(err.message, BODY);
        assert!(err.headers.get("content-encoding").is_none() && err.headers.get("content-length").is_none());
        assert_eq!(err.headers.get("content-type").unwrap(), "application/json");
        assert!(err.is_request_scoped());
    }
    // A unified 5h rejection on 429 is a credential-level limit even in fast mode.
    let limited = headers(&[("anthropic-ratelimit-unified-5h-status", "rejected")]);
    let err = new_claude_fast_direct_response_error(429, &limited, BODY.as_bytes());
    assert!(err.credential_scoped && !err.is_request_scoped());
}

#[test]
fn setup_tokens_and_profile_scope_errors() {
    let mut auth = Auth::new("a", "claude");
    auth.metadata.insert("access_token".into(), "sk-ant-oat01-x".into());
    assert!(!is_claude_setup_token(&auth, "sk-ant-oat01-x"));
    assert!(!is_claude_setup_token(&auth, "plain-api-key"), "only OAuth tokens can be setup tokens");

    for (key, value) in [("skip_account_profile", true), ("is_setup_token", true), ("setup_token", true)] {
        let mut a = auth.clone();
        a.metadata.insert(key.into(), value.into());
        assert!(is_claude_setup_token(&a, "sk-ant-oat01-x"), "{key}");
    }
    let mut a = auth.clone();
    a.attributes.insert("auth_kind".into(), "Setup-Token".into());
    assert!(is_claude_setup_token(&a, "sk-ant-oat01-x"));
    let mut a = auth.clone();
    a.metadata.insert("scope".into(), "user:inference".into());
    assert!(is_claude_setup_token(&a, "sk-ant-oat01-x"));
    a.metadata.insert("scope".into(), "user:profile user:inference".into());
    assert!(!is_claude_setup_token(&a, "sk-ant-oat01-x"));

    assert!(is_claude_oauth_scope_403("status 403: forbidden"));
    assert!(is_claude_oauth_scope_403("permission_error"));
    assert!(!is_claude_oauth_scope_403("connection reset"));
}

#[test]
fn request_auth_preparation_needs_a_device_pool_and_account_uuid_for_oauth_only() {
    let mut oauth = Auth::new("a", "claude");
    oauth.metadata.insert("access_token".into(), "sk-ant-oat01-x".into());
    assert!(should_prepare_request_auth(&oauth), "no device pool yet");

    oauth.metadata.insert("claude_device_ids".into(), serde_json::json!([format!("{:064x}", 3)]));
    assert!(should_prepare_request_auth(&oauth), "no account uuid yet");

    oauth.metadata.insert("account_uuid".into(), "3c9a1f3e-6e2b-4d57-9a53-0a6a4cf1d5aa".into());
    assert!(!should_prepare_request_auth(&oauth));

    let mut api_key = Auth::new("k", "claude");
    api_key.attributes.insert("api_key".into(), "sk-plain".into());
    assert!(!should_prepare_request_auth(&api_key));
}

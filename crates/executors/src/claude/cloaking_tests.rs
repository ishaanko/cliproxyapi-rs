//! Cloaking tests ported from claude_executor_test.go (system relocation, current date, billing
//! fingerprint, wire policy). Expectations are the Go assertions.

use cpa_auth::Auth;
use cpa_config::{CloakConfig, Config};
use cpa_json::J;
use serde_json::Value;

use super::cloaking::*;
use super::policy::resolve_claude_wire_policy;

const DATE: &str = "2026-08-01";

/// Owned elements of the array at `path`.
fn arr(v: &Value, path: &str) -> Vec<Value> {
    v.g(path).array().iter().map(|r| r.value()).collect()
}

fn check(payload: &str, strict: bool) -> Value {
    let out = check_system_instructions_with_signing_mode_at(
        payload.as_bytes(),
        strict,
        false,
        "2.1.280",
        "cli",
        DATE,
        &BillingOptions::default(),
    );
    cpa_json::parse(&out)
}

fn texts(v: &Value, path: &str) -> Vec<String> {
    v.g(path).array().iter().map(|b| b.g("text").str()).collect()
}

fn assert_date_block(block: &Value) {
    assert_eq!(block.g("text").str(), claude_code_current_date_reminder(DATE));
    assert!(!block.g("cache_control").exists());
}

fn assert_ephemeral_user_text(block: &Value, text: &str) {
    assert_eq!(block.g("text").str(), text);
    assert_eq!(block.g("cache_control.type").str(), "ephemeral");
    assert!(!block.g("cache_control.ttl").exists());
}

fn assert_mid_system_message(out: &Value, index: usize, text: &str) {
    let msg = out.g(&format!("messages.{index}")).value();
    assert_eq!(msg.g("role").str(), "system", "{out}");
    let content = arr(&msg, "content");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0].g("text").str(), text);
    assert_eq!(content[0].g("cache_control.type").str(), "ephemeral");
}

#[test]
fn billing_fingerprint_uses_first_user_text() {
    const PROMPT: &str = "CPA_OFFICIAL_BASEURL_CLI_SYSTEM_EMPTY_b82d4e";
    let payload = format!(
        r#"{{"system":"must not seed the build hash","messages":[{{"role":"user","content":[{{"type":"text","text":"<system-reminder>date</system-reminder>"}},{{"type":"text","text":"{PROMPT}"}}]}},{{"role":"assistant","content":"answer"}},{{"role":"user","content":"turn2"}}]}}"#
    );
    assert_eq!(claude_billing_fingerprint_message_text(payload.as_bytes()), PROMPT);
    // Official 2.1.258 capture suffix.
    assert_eq!(compute_fingerprint(PROMPT, "2.1.258"), "1f4");
}

#[test]
fn legacy_system_reminder_models() {
    let cases = [
        ("claude-opus-4-6", true),
        ("claude-opus-4-7", true),
        ("claude-sonnet-5", false),
        ("prefix/claude-sonnet-4-6", true),
        ("claude-3-5-haiku-latest", true),
        ("claude-opus-5", false),
        ("prefix/claude-opus-4-8", false),
        ("claude-fable-5", false),
        ("claude-future-6", false),
        ("", false),
    ];
    for (model, want) in cases {
        let payload = serde_json::json!({ "model": model }).to_string();
        assert_eq!(claude_uses_legacy_system_reminder_bytes(payload.as_bytes()), want, "{model}");
    }
}

#[test]
fn current_date_injection_is_idempotent_and_aligns_first_user_cache() {
    let payload = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
    let first = inject_claude_code_current_date(payload.as_bytes(), DATE);
    assert!(String::from_utf8_lossy(&first).contains("<system-reminder>"));
    assert!(!String::from_utf8_lossy(&first).contains("\\u003csystem-reminder"));
    assert_eq!(first, inject_claude_code_current_date(&first, DATE));
    let v = cpa_json::parse(&first);
    let content = arr(&v, "messages.0.content");
    assert_eq!(content.len(), 2);
    assert_date_block(&content[0]);
    assert_ephemeral_user_text(&content[1], "hello");
}

#[test]
fn current_date_moves_existing_copy_to_first_block() {
    let date_block = build_text_block(&claude_code_current_date_reminder(DATE), false);
    let payload = format!(r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"hello"}},{date_block}]}}]}}"#);
    let out = cpa_json::parse(&inject_claude_code_current_date(payload.as_bytes(), DATE));
    let content = arr(&out, "messages.0.content");
    assert_eq!(content.len(), 2);
    assert_date_block(&content[0]);
    assert_ephemeral_user_text(&content[1], "hello");
}

#[test]
fn current_date_precedes_existing_reminder() {
    let reminder = "<system-reminder>\ncaller instructions\n</system-reminder>";
    let payload = format!(
        r#"{{"messages":[{{"role":"user","content":[{},{{"type":"text","text":"continue","cache_control":{{"type":"ephemeral","ttl":"1h"}}}}]}}]}}"#,
        build_text_block(reminder, false)
    );
    let out = cpa_json::parse(&inject_claude_code_current_date(payload.as_bytes(), DATE));
    let content = arr(&out, "messages.0.content");
    assert_eq!(content.len(), 3);
    assert_date_block(&content[0]);
    assert_eq!(content[1].g("text").str(), reminder);
    assert_ephemeral_user_text(&content[2], "continue");
}

#[test]
fn current_date_follows_leading_tool_results() {
    let payload = r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"},{"type":"text","text":"continue"}]}]}"#;
    let first = inject_claude_code_current_date(payload.as_bytes(), DATE);
    assert_eq!(first, inject_claude_code_current_date(&first, DATE));
    let v = cpa_json::parse(&first);
    let content = arr(&v, "messages.1.content");
    assert_eq!(content.len(), 3);
    assert_eq!(content[0].g("type").str(), "tool_result");
    assert_eq!(content[0].g("tool_use_id").str(), "toolu_1");
    assert_date_block(&content[1]);
    assert_ephemeral_user_text(&content[2], "continue");

    let all = r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{}},{"type":"tool_use","id":"toolu_2","name":"Read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"},{"type":"tool_result","tool_use_id":"toolu_2","content":"ok"}]}]}"#;
    let v = cpa_json::parse(&inject_claude_code_current_date(all.as_bytes(), DATE));
    let content = arr(&v, "messages.1.content");
    assert_eq!(content.len(), 3);
    assert_eq!(content[0].g("tool_use_id").str(), "toolu_1");
    assert_eq!(content[1].g("tool_use_id").str(), "toolu_2");
    assert_date_block(&content[2]);
}

#[test]
fn string_system_becomes_mid_conversation_system_message() {
    let out = check(
        r#"{"model":"claude-opus-5","system":"You are a helpful assistant.","messages":[{"role":"user","content":"hi"}]}"#,
        false,
    );
    let blocks = arr(&out, "system");
    assert_eq!(blocks.len(), 2);
    assert!(blocks[0].g("text").str().contains("cc_entrypoint=cli;"));
    assert_eq!(blocks[1].g("text").str(), CLAUDE_CODE_CLI_IDENTITY);
    assert_eq!(blocks[1].g("cache_control.type").str(), "ephemeral");
    assert!(!blocks[1].g("cache_control.ttl").exists());
    let content = arr(&out, "messages.0.content");
    assert_eq!(content.len(), 2);
    assert_date_block(&content[0]);
    assert_ephemeral_user_text(&content[1], "hi");
    assert_mid_system_message(&out, 1, "You are a helpful assistant.");
}

#[test]
fn future_model_defaults_to_mid_system_and_legacy_uses_reminders() {
    let out = check(r#"{"model":"claude-opus-6","system":"future instructions","messages":[{"role":"user","content":"hi"}]}"#, false);
    assert_eq!(out.g("system.#").int(), 2);
    assert_mid_system_message(&out, 1, "future instructions");

    let out = check(r#"{"model":"claude-opus-4-6","system":"legacy instructions","messages":[{"role":"user","content":"hi"}]}"#, false);
    assert_eq!(out.g("system.#").int(), 2);
    assert_eq!(out.g("messages.#").int(), 1);
    let content = arr(&out, "messages.0.content");
    assert_eq!(content.len(), 3);
    assert_date_block(&content[0]);
    assert_eq!(content[1].g("text").str(), claude_caller_system_reminder("legacy instructions"));
    assert!(!content[1].g("cache_control").exists());
    assert_ephemeral_user_text(&content[2], "hi");
}

#[test]
fn caller_system_blocks_stay_separate() {
    let system = r#""system":[{"type":"text","text":"first guidance","cache_control":{"type":"ephemeral","ttl":"1h"}},{"type":"text","text":"second guidance"}],"messages":[{"role":"user","content":"hi"}]}"#;
    let out = check(&format!(r#"{{"model":"claude-opus-4-6",{system}"#), false);
    let content = arr(&out, "messages.0.content");
    assert_eq!(content.len(), 4);
    assert_date_block(&content[0]);
    for (idx, want) in ["first guidance", "second guidance"].iter().enumerate() {
        assert_eq!(content[idx + 1].g("text").str(), claude_caller_system_reminder(want));
        assert!(!content[idx + 1].g("cache_control").exists());
    }
    assert_ephemeral_user_text(&content[3], "hi");

    let out = check(&format!(r#"{{"model":"claude-opus-5",{system}"#), false);
    assert_eq!(out.g("messages.#").int(), 3);
    assert_mid_system_message(&out, 1, "first guidance");
    assert_mid_system_message(&out, 2, "second guidance");
}

#[test]
fn strict_mode_and_empty_system_add_only_injected_blocks() {
    let out = check(r#"{"system":"You are a helpful assistant.","messages":[{"role":"user","content":"hi"}]}"#, true);
    assert_eq!(out.g("system").array().len(), 2);
    let content = arr(&out, "messages.0.content");
    assert_eq!(content.len(), 2);
    assert_date_block(&content[0]);
    assert_ephemeral_user_text(&content[1], "hi");

    let out = check(r#"{"system":"","messages":[{"role":"user","content":"hi"}]}"#, false);
    assert_eq!(out.g("system").array().len(), 2);
    assert_eq!(out.g("messages.0.content").array().len(), 2);
}

#[test]
fn advisor_history_keeps_caller_system_top_level() {
    let out = check(
        r#"{"model":"claude-opus-5","system":"keep me","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"server_tool_use","id":"srvtoolu_1","name":"advisor","input":{}}]}]}"#,
        false,
    );
    assert_eq!(texts(&out, "system").last().unwrap(), "keep me");
    assert_eq!(out.g("messages.#").int(), 2);
}

#[test]
fn caller_system_blocks_must_be_text() {
    let ok = cpa_json::parse(br#"[{"type":"text","text":"a"},{"type":"text","text":"b"}]"#);
    assert!(validate_claude_caller_system_blocks(&ok).is_ok());
    let bad = cpa_json::parse(br#"[{"type":"text","text":"a"},{"type":"image","source":{}}]"#);
    let err = validate_claude_caller_system_blocks(&bad).unwrap_err();
    assert_eq!(err.status, 400);
    assert!(err.is_request_scoped());
    assert!(err.message.contains("system.1.type: Input should be 'text'"), "{}", err.message);
    assert!(err.message.contains("\"image\""));
}

#[test]
fn wire_policy_cloaks_oauth_and_honors_modes() {
    let oauth_key = "sk-ant-oat01-x";
    let mut oauth = Auth::new("oauth", "claude");
    oauth.metadata.insert("access_token".into(), oauth_key.into());
    let mut apikey = Auth::new("k", "claude");
    apikey.attributes.insert("api_key".into(), "sk-plain".into());

    let cfg = Config::default();
    assert!(resolve_claude_wire_policy(&cfg, &oauth, oauth_key, false).0.cloak);
    assert!(!resolve_claude_wire_policy(&cfg, &oauth, oauth_key, true).0.cloak, "confirmed native client is never cloaked");
    assert!(!resolve_claude_wire_policy(&cfg, &apikey, "sk-plain", false).0.cloak);

    let mut never = Config::default();
    never.disable_claude_cloak_mode = true;
    assert!(!resolve_claude_wire_policy(&never, &oauth, oauth_key, false).0.cloak);

    let mut always = Config::default();
    always.claude_key.push(cpa_config::ClaudeKey {
        api_key: "sk-plain".into(),
        cloak: Some(CloakConfig { mode: "always".into(), strict_mode: true, ..Default::default() }),
        ..Default::default()
    });
    let (policy, settings) = resolve_claude_wire_policy(&always, &apikey, "sk-plain", false);
    assert!(policy.cloak && settings.strict_mode);
    assert!(!resolve_claude_wire_policy(&always, &apikey, "sk-plain", true).0.cloak);
}

// ---------------------------------------------------------------------------- apply_cloaking

use super::helps::ClaudeCtx;

fn apply_cloaking(cfg: &Config, auth: &Auth, payload: &str, api_key: &str, confirmed: bool) -> (Vec<u8>, bool) {
    apply_cloaking_internal(&ClaudeCtx::default(), cfg, auth, payload.as_bytes().to_vec(), api_key, confirmed, true, true).unwrap()
}

fn key_cfg(api_key: &str, cloak: CloakConfig) -> Config {
    Config {
        claude_key: vec![cpa_config::ClaudeKey { api_key: api_key.into(), cloak: Some(cloak), ..Default::default() }],
        ..Default::default()
    }
}

fn key_auth(api_key: &str) -> Auth {
    let mut auth = Auth::new("k", "claude");
    auth.attributes.insert("api_key".into(), api_key.into());
    auth
}

#[test]
fn wire_policy_modes_and_confirmed_clients() {
    let cases = [
        ("unknown auto", false, "auto", true),
        ("unknown always", false, "always", true),
        ("unknown never", false, "never", false),
        ("confirmed auto", true, "auto", false),
        ("confirmed always", true, "always", false),
        ("confirmed never", true, "never", false),
    ];
    for (name, confirmed, mode, want_cloak) in cases {
        let mut auth = Auth::new("a", "claude");
        auth.metadata.insert("cloak_mode".into(), mode.into());
        let (policy, _) = resolve_claude_wire_policy(&Config::default(), &auth, "sk-ant-oat-test", confirmed);
        assert!(policy.oauth, "{name}");
        assert_eq!(policy.confirmed_claude_code, confirmed, "{name}");
        assert_eq!(policy.cloak, want_cloak, "{name}");
    }
}

#[test]
fn configured_strict_mode_and_sensitive_words_apply_when_mode_is_omitted() {
    let cfg = key_cfg("key-123", CloakConfig { strict_mode: true, sensitive_words: vec!["proxy".into()], ..Default::default() });
    let payload = r#"{"system":"proxy rules","messages":[{"role":"user","content":[{"type":"text","text":"proxy access"}]}]}"#;
    let (out, cloaked) = apply_cloaking(&cfg, &key_auth("key-123"), payload, "key-123", false);
    assert!(cloaked);
    let v = cpa_json::parse(&out);
    assert_eq!(v.g("system").array().len(), 2, "strict mode keeps only the injected blocks");
    let content = arr(&v, "messages.0.content");
    assert_eq!(content.len(), 2);
    assert!(content[1].g("text").str().contains('\u{200B}'), "sensitive word obfuscation applies");
}

#[test]
fn opus55_fallback_only_for_unconfirmed_clients() {
    let cfg = key_cfg("sk-ant-oat-opus55-test", CloakConfig::default());
    let auth = key_auth("sk-ant-oat-opus55-test");
    let cases = [
        (r#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"test"}]}"#, false, "claude-opus-4-8"),
        (r#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"test"}]}"#, true, ""),
        (
            r#"{"model":"claude-opus-5-5","fallbacks":[{"model":"claude-sonnet-5"}],"messages":[{"role":"user","content":"test"}]}"#,
            false,
            "claude-sonnet-5",
        ),
        (r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"test"}]}"#, false, ""),
    ];
    for (payload, confirmed, want_model) in cases {
        let (body, cloaked) = apply_cloaking(&cfg, &auth, payload, "sk-ant-oat-opus55-test", confirmed);
        assert_eq!(cloaked, !confirmed);
        let v = cpa_json::parse(&body);
        let fallbacks = arr(&v, "fallbacks");
        if want_model.is_empty() {
            assert!(fallbacks.is_empty(), "{payload}");
        } else {
            assert_eq!(fallbacks.len(), 1);
            assert_eq!(fallbacks[0].g("model").str(), want_model);
        }
        if confirmed {
            assert_eq!(body, payload.as_bytes(), "confirmed native payload must not be cloaked");
        } else {
            assert!(v.g("system.0.text").str().contains("cc_turn_origin=human;"));
        }
    }
}

#[test]
fn opus55_fallback_reconciles_after_model_override() {
    let before = br#"{"model":"claude-opus-5-5","system":[{"type":"text","text":"billing"}]}"#;
    let cloaked = r#"{"model":"claude-opus-5-5","fallbacks":[{"model":"claude-opus-4-8"}],"system":[{"type":"text","text":"billing"}]}"#;
    let state = capture_claude_code_fable_state(before, cloaked.as_bytes(), true);
    let cases = [
        ("switch to Sonnet", r#"{"model":"claude-sonnet-5","fallbacks":[{"model":"claude-opus-4-8"}],"system":[{"type":"text","text":"billing"}]}"#.to_string(), "", false),
        ("switch to Fable", r#"{"model":"claude-fable-5-1","fallbacks":[{"model":"claude-opus-4-8"}],"system":[{"type":"text","text":"billing"}]}"#.to_string(), "claude-opus-5", false),
        ("same Opus", cloaked.to_string(), "claude-opus-4-8", false),
        ("Opus probe", cloaked.to_string(), "", true),
    ];
    for (name, body, want, probe) in cases {
        let got = reconcile_claude_code_fable_model_after_payload(body.into_bytes(), state, false, false, true, probe);
        assert_eq!(cpa_json::parse(&got).g("fallbacks.0.model").str(), want, "{name}");
    }
    let got = reconcile_claude_code_fable_model_after_payload(
        br#"{"model":"claude-opus-5-5","system":[]}"#.to_vec(),
        ClaudeCodeFableState::default(),
        false,
        false,
        true,
        false,
    );
    assert_eq!(cpa_json::parse(&got).g("fallbacks.0.model").str(), "claude-opus-4-8");
}

#[test]
fn fable_cloaking_injects_fallbacks_display_and_reporting_block() {
    let cfg = key_cfg("sk-ant-oat-fable-test", CloakConfig::default());
    let payload = r#"{"model":"claude-fable-5-1","thinking":{"type":"adaptive"},"messages":[{"role":"user","content":"test"}]}"#;
    let (out, cloaked) = apply_cloaking(&cfg, &key_auth("sk-ant-oat-fable-test"), payload, "sk-ant-oat-fable-test", false);
    assert!(cloaked);
    let v = cpa_json::parse(&out);
    let fallbacks = arr(&v, "fallbacks");
    assert_eq!(fallbacks.len(), 1);
    assert_eq!(fallbacks[0].g("model").str(), "claude-opus-5");
    let system = arr(&v, "system");
    assert_eq!(system.len(), 3, "billing, identity, reporting outcomes");
    assert!(system[2].g("text").str().contains("Reporting outcomes"));
    assert!(!system[2].g("cache_control").exists());
    assert_eq!(v.g("thinking.display").str(), "updates");

    let betas = super::request::claude_code_cli_betas(&out, &Default::default(), true);
    assert!(betas.contains("server-side-fallback-2026-06-01") && betas.contains("thinking-display-updates-2026-08-18"), "{betas}");
    assert!(!betas.contains("redact-thinking-2026-02-12"), "{betas}");
}

#[test]
fn non_fable_models_omit_reporting_outcomes_and_fable5_omits_fallbacks() {
    for (key, model) in [("sk-ant-oat-sonnet-test", "claude-sonnet-5"), ("sk-ant-oat-fable5-test", "claude-fable-5")] {
        let cfg = key_cfg(key, CloakConfig::default());
        let payload = format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"test"}}]}}"#);
        let (out, cloaked) = apply_cloaking(&cfg, &key_auth(key), &payload, key, false);
        assert!(cloaked);
        let v = cpa_json::parse(&out);
        let system = arr(&v, "system");
        assert_eq!(system.len(), 2, "{model}");
        assert!(system.iter().all(|b| !b.g("text").str().contains("Reporting outcomes")), "{model}");
        assert!(!v.g("fallbacks").exists(), "{model}");
    }
}

#[test]
fn disabled_or_native_requests_are_left_untouched() {
    let system = "You are a custom assistant for my company.";
    let payload = format!(r#"{{"model":"claude-fable-5-1","system":"{system}","messages":[{{"role":"user","content":"test"}}]}}"#);
    let disabled = Config { disable_claude_cloak_mode: true, ..Default::default() };
    let (out, cloaked) = apply_cloaking(&disabled, &key_auth("sk-ant-oat-fable-test"), &payload, "sk-ant-oat-fable-test", false);
    assert!(!cloaked);
    let v = cpa_json::parse(&out);
    assert_eq!(v.g("system").str(), system);
    assert!(!v.g("fallbacks").exists());

    let cfg = key_cfg("sk-ant-oat-fable-test", CloakConfig::default());
    let (out, cloaked) = apply_cloaking(&cfg, &key_auth("sk-ant-oat-fable-test"), &payload, "sk-ant-oat-fable-test", true);
    assert!(!cloaked);
    assert_eq!(cpa_json::parse(&out).g("system").str(), system);
}

#[test]
fn cloaking_rejects_non_text_caller_system_blocks_unless_strict() {
    let payload = r#"{"model":"claude-opus-5","system":[{"type":"image","source":{}}],"messages":[{"role":"user","content":"hi"}]}"#;
    let mut auth = Auth::new("a", "claude");
    auth.metadata.insert("access_token".into(), "sk-ant-oat-x".into());
    let err = apply_cloaking_internal(&ClaudeCtx::default(), &Config::default(), &auth, payload.as_bytes().to_vec(), "sk-ant-oat-x", false, false, false)
        .unwrap_err();
    assert_eq!(err.status, 400);
    assert!(err.is_request_scoped());

    let strict = key_cfg("sk-ant-oat-x", CloakConfig { strict_mode: true, ..Default::default() });
    let (out, cloaked) = apply_cloaking(&strict, &key_auth("sk-ant-oat-x"), payload, "sk-ant-oat-x", false);
    assert!(cloaked);
    assert_eq!(cpa_json::parse(&out).g("system").array().len(), 2);
}

/// Calendar date of the currentDate reminder in the first user message.
fn cloak_date(payload: &[u8]) -> String {
    let root = cpa_json::parse(payload);
    let content = root.g("messages.0.content");
    let texts: Vec<String> =
        if content.is_array() { content.array().iter().map(|b| b.g("text").str()).collect() } else { vec![content.str()] };
    for text in texts {
        if let Some(rest) = text.split("Today's date is ").nth(1) {
            return rest.chars().take(10).collect();
        }
    }
    panic!("no currentDate reminder in first user message: {}", String::from_utf8_lossy(payload));
}

/// The cloaked currentDate reminder stays identical for one session even when the candidate date
/// flips between two requests (a rewrite would invalidate the prompt-cache prefix).
#[test]
fn cloaked_date_reminder_is_pinned_to_the_session_across_midnight() {
    use super::helps::ClaudeCtx;
    use super::helps::diagnostics::pin_claude_session_date;
    use chrono::{Days, NaiveDate};

    let mut auth = Auth::new("test-date-pin-cred", "claude");
    auth.attributes.insert("timezone".into(), "Asia/Shanghai".into());
    let cfg = Config::default();
    let headers = http::HeaderMap::new();
    let ctx = ClaudeCtx { session_id: "sess-midnight".into(), ..Default::default() };
    let checked = |payload: &str| {
        let tags = resolve_claude_continuity_tags(&ctx, &cfg, &auth, &headers, payload.as_bytes(), true, "", "").expect("continuity");
        let out = check_system_instructions_with_signing_mode_at(
            payload.as_bytes(),
            false,
            false,
            "2.1.280",
            "cli",
            &tags.ctx.pinned_date,
            &BillingOptions { prev_req: &tags.prev_req, prompt_id: &tags.prompt_id, ..Default::default() },
        );
        (tags.ctx, out)
    };

    let (c1, out1) = checked(r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"turn one"}]}"#);
    assert!(!c1.pinned_date.is_empty(), "first request must establish a pinned session date");
    let date1 = cloak_date(&out1);
    assert_eq!(date1, c1.pinned_date);

    // The next calendar day as a later request's candidate: the session pin must win.
    let anchor = NaiveDate::parse_from_str(&c1.pinned_date, "%Y-%m-%d").expect("anchor date");
    let next_day = anchor.checked_add_days(Days::new(1)).expect("next day").format("%Y-%m-%d").to_string();
    assert_eq!(pin_claude_session_date(&c1.key, &next_day), c1.pinned_date);

    let (c2, out2) = checked(
        r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"turn one"},{"role":"assistant","content":"ok"},{"role":"user","content":"turn two"}]}"#,
    );
    assert_eq!(c2.pinned_date, c1.pinned_date);
    assert_eq!(cloak_date(&out2), date1, "date reminder changed within one session");
}

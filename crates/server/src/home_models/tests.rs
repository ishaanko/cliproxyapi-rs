//! Ports of the Go Home model tests (internal/api/server_test.go, server_grok_models_test.go).

use std::sync::Arc;

use async_trait::async_trait;
use cpa_auth::Auth;
use cpa_config::OAuthModelSetting;
use cpa_runtime::executor::{ExecError, Executor, Options, Request, Response, StreamResult};

use super::*;

fn entry(id: &str) -> HomeModelEntry {
    HomeModelEntry { id: id.into(), ..Default::default() }
}

fn entry_with(id: &str, providers: &[&str]) -> HomeModelEntry {
    HomeModelEntry { providers: providers.iter().map(|p| p.to_string()).collect(), ..entry(id) }
}

fn by_id(entries: Vec<HomeModelEntry>) -> HashMap<String, HomeModelEntry> {
    entries.into_iter().map(|e| (e.id.clone(), e)).collect()
}

#[test]
fn claude_model_includes_anthropic_schema_fields() {
    let full = format_claude_model(&HomeModelEntry {
        created: 1_771_372_800,
        owned_by: "anthropic".into(),
        display_name: "Claude 4.6 Sonnet".into(),
        context_length: 200_000,
        max_completion_tokens: 64_000,
        ..entry("claude-sonnet-4-6")
    });
    assert_eq!(full["created_at"], "2026-02-18T00:00:00Z");
    assert_eq!(full["type"], "model");
    assert_eq!(full["display_name"], "Claude 4.6 Sonnet");
    assert_eq!(full["max_input_tokens"], 200_000);
    assert_eq!(full["max_tokens"], 64_000);

    let defaults = format_claude_model(&entry("claude-no-limits"));
    assert_eq!(defaults["display_name"], "claude-no-limits");
    assert_eq!(defaults["max_input_tokens"], DEFAULT_CLAUDE_MAX_INPUT_TOKENS);
    assert_eq!(defaults["max_tokens"], DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS);
    assert!(!defaults.contains_key("created_at"));

    let custom = format_claude_model(&HomeModelEntry { display_name: "GPT-4o".into(), ..entry("gpt-4o") });
    assert_eq!((custom["id"].as_str(), custom["display_name"].as_str()), (Some("gpt-4o"), Some("GPT-4o")));
}

#[test]
fn decode_keeps_token_metadata() {
    let entries = by_id(
        decode_home_models(
            br#"{
            "claude": [{"id": "claude-sonnet-4-6", "created": 1771372800, "owned_by": "anthropic",
                        "context_length": 200000, "max_completion_tokens": 64000}],
            "gemini": [{"name": "models/gemini-3-pro", "inputTokenLimit": 1048576, "outputTokenLimit": 65536,
                        "thinking": {"min": 128, "max": 65535, "dynamic_allowed": true,
                                     "levels": ["low", "medium", "high"]}}]
        }"#,
        )
        .expect("decode"),
    );
    let claude = &entries["claude-sonnet-4-6"];
    assert_eq!((claude.context_length, claude.max_completion_tokens), (200_000, 64_000));
    let gemini = &entries["gemini-3-pro"];
    assert_eq!((gemini.context_length, gemini.max_completion_tokens), (1_048_576, 65_536));
    let levels = gemini.thinking.as_ref().map(|t| t.levels.clone());
    assert_eq!(levels, Some(vec!["low".to_string(), "medium".into(), "high".into()]));

    let formatted = format_codex_model(gemini);
    assert_eq!(formatted["context_length"], 1_048_576);
    assert_eq!(formatted["thinking"]["levels"], json!(["low", "medium", "high"]));
}

#[test]
fn decode_merges_sections_and_reports_bad_payloads() {
    let entries = decode_home_models(br#"{"B":[{"id":"m"}],"a":[{"id":" m "},{"id":"a-only"},{}, null]}"#).expect("decode");
    let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["a-only", "m"]);
    assert_eq!(entries[1].providers, ["b", "a"]);
    assert_eq!(entries[1].native_capability_routes.len(), 2);

    assert_eq!(decode_home_models(b"").expect_err("empty"), "home models payload is empty");
    assert_eq!(decode_home_models(b"{}").expect_err("no sections"), "home models payload has no sections");
    assert_eq!(decode_home_models(b"null").expect_err("null"), "home models payload has no sections");
    assert_eq!(decode_home_models(br#"{"x":[{}]}"#).expect_err("no models"), "home models payload contains no models");
    assert!(decode_home_models(b"[]").expect_err("array").starts_with("parse home models payload:"));
    assert!(decode_home_models(br#"{"x":"y"}"#).expect_err("string section").starts_with("parse home models payload:"));
}

#[test]
fn max_context_length_is_carried_to_the_codex_model() {
    let entries =
        decode_home_models(br#"{"codex":[{"id":"gpt-6-sol","context_length":272000,"max_context_length":524288}]}"#).expect("decode");
    assert_eq!(entries[0].max_context_length, 524_288);
    assert_eq!(format_codex_model(&entries[0])["max_context_length"], 524_288);
}

#[test]
fn oauth_settings_apply_per_provider_channel() {
    let setting = |n: i64| vec![OAuthModelSetting { name: "shared-model".into(), max_context_length: n, ..Default::default() }];
    let mut cfg = Config::default();
    cfg.oauth_settings.insert("codex".into(), setting(524_288));
    cfg.oauth_settings.insert("claude".into(), setting(200_000));
    let max = |providers: &[&str]| {
        format_codex_model_with_settings(&entry_with("shared-model", providers), &cfg).get("max_context_length").cloned()
    };
    assert_eq!(max(&["codex"]), Some(json!(524_288)));
    assert_eq!(max(&["claude"]), Some(json!(200_000)));
    assert_eq!(max(&["vertex"]), None);
    assert_eq!(max(&["claude", "codex"]), Some(json!(524_288)));
}

#[test]
fn auth_status_and_error_message() {
    let cases: [(&str, Option<u16>); 5] = [
        (r#"{"error":{"type":"no_credentials","message":"Missing API key"}}"#, Some(401)),
        (r#"{"error":{"type":"invalid_credential","message":"Invalid API key"}}"#, Some(401)),
        (r#"{"error":{"type":"internal_error","message":"boom"}}"#, Some(502)),
        (r#"{"openai":[{"id":"gpt-5.5"}]}"#, None),
        ("{}", None),
    ];
    for (raw, want) in cases {
        assert_eq!(models_auth_status(raw.as_bytes()), want, "{raw}");
    }
    assert_eq!(
        models_error_message(br#"{"error":{"type":"invalid_credential","message":"Invalid API key"}}"#),
        "Invalid API key"
    );
    assert_eq!(models_error_message(br#"{"openai":[]}"#), "home models request failed");
}

fn codex_catalog(entries: &[HomeModelEntry], client_version: &str) -> HashMap<String, Entry> {
    let models: Vec<Entry> = entries.iter().map(format_codex_model).collect();
    let search = web_search_capability_for_model(entries);
    let ctx = CatalogContext {
        providers_for_model: None,
        web_search_capability: &search,
        apply_patch_capability: None,
        optimize_multi_agent_v2: false,
        client_version,
    };
    build_models(&models, &ctx)
        .into_iter()
        .map(|m| (m.get("slug").and_then(Value::as_str).unwrap_or_default().to_string(), m))
        .collect()
}

#[test]
fn web_search_capability_needs_every_route_and_known_providers() {
    let entries = decode_home_models(
        br#"{
        "codex":[{"id":"home-codex","native_capabilities":{"web_search":true}},
                 {"id":"home-unknown"},
                 {"id":"home-duplicate","native_capabilities":{"web_search":true}},
                 {"id":"home-duplicate","native_capabilities":{"web_search":false}}],
        "xai":[{"id":"home-xai","native_capabilities":{"web_search":true}}],
        "claude":[{"id":"gpt-5.5","native_capabilities":{"web_search":true}}],
        "gemini":[{"id":"home-gemini","native_capabilities":{"web_search":true}}],
        "custom":[{"id":"home-custom","native_capabilities":{"web_search":true}}]
    }"#,
    )
    .expect("decode");
    let catalog = codex_catalog(&entries, "cpa");
    for id in ["home-codex", "home-xai", "gpt-5.5"] {
        assert_eq!(catalog[id]["cpa_capabilities"]["web_search"], true, "{id}");
    }
    assert_eq!(catalog["gpt-5.5"]["supports_search_tool"], true, "legacy Home supports_search_tool changed");
    for id in ["home-gemini", "home-duplicate"] {
        assert_eq!(catalog[id]["cpa_capabilities"]["web_search"], false, "{id}");
    }
    for id in ["home-unknown", "home-custom"] {
        assert!(!catalog[id].contains_key("cpa_capabilities"), "{id}");
    }
    // Without the `cpa` client version the capability block is never emitted.
    assert!(!codex_catalog(&entries, "0.153.4")["home-codex"].contains_key("cpa_capabilities"));
}

#[test]
fn devin_models_get_the_display_suffix() {
    let entries = [
        HomeModelEntry { display_name: "SWE-2".into(), ..entry_with("devin/swe-2", &["devin"]) },
        HomeModelEntry { display_name: "Home Regular".into(), ..entry_with("home-regular", &["openai"]) },
    ];
    let catalog = codex_catalog(&entries, "0.153.4");
    assert_eq!(catalog["devin/swe-2"]["display_name"], "SWE-2 (Devin)");
    assert_eq!(catalog["home-regular"]["display_name"], "Home Regular");
}

/// Executor that only answers `supports_apply_patch`.
struct PatchExecutor {
    provider: &'static str,
    supports: bool,
}

#[async_trait]
impl Executor for PatchExecutor {
    fn identifier(&self) -> &str {
        self.provider
    }

    async fn execute(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
        Err(ExecError::new(500, "unused"))
    }

    async fn execute_stream(&self, _: &Auth, _: Request, _: Options) -> Result<StreamResult, ExecError> {
        Err(ExecError::new(500, "unused"))
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        Ok(auth.clone())
    }

    async fn count_tokens(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
        Err(ExecError::new(500, "unused"))
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        self.supports
    }
}

#[test]
fn apply_patch_capability_follows_home_routing_only() {
    let mut entries = decode_home_models(
        br#"{
        "codex":[{"id":"home-patch-native"},{"id":"home-patch-mixed"},{"id":"home-patch-partial"},{"id":"home-patch-blank"}],
        "home-custom":[{"id":"home-patch-synthetic"},{"id":"home-patch-mixed"}],
        "remote":[{"id":"home-patch-remote"},{"id":"home-patch-partial"}],
        "plugin":[{"id":"home-patch-plugin"}],
        "disabled":[{"id":"home-patch-disabled"}],
        "":[{"id":"home-patch-blank"}]
    }"#,
    )
    .expect("decode");
    entries.push(entry("home-patch-no-routes"));
    let manager = Manager::default();
    for (provider, supports) in
        [("codex", true), ("home-custom", true), ("remote", false), ("plugin", false), ("disabled", false)]
    {
        manager.register_executor(Arc::new(PatchExecutor { provider, supports }));
    }
    let lookup = apply_patch_capability_for_model(&entries, &manager);
    for (id, want) in [
        ("home-patch-native", true),
        (" home-patch-synthetic ", true),
        ("home-patch-mixed", true),
        ("home-patch-remote", false),
        ("home-patch-plugin", false),
        ("home-patch-partial", false),
        ("home-patch-disabled", false),
        ("home-patch-no-routes", false),
        ("home-patch-blank", false),
        ("team/home-patch-native", false),
        ("home-patch-native(high)", false),
        ("missing", false),
    ] {
        assert_eq!(lookup(id), want, "{id:?}");
    }
    // Duplicate entries never overwrite an unknown routing candidate.
    let duplicates = [entry_with("duplicate", &["remote"]), entry_with("duplicate", &["codex"])];
    assert!(!apply_patch_capability_for_model(&duplicates, &manager)("duplicate"));
    assert!(!apply_patch_capability_for_model(&[], &manager)("home-patch-native"));
}

#[test]
fn grok_adapter_omits_reasoning() {
    let entries = [
        HomeModelEntry { display_name: "Home Model".into(), context_length: 1234, ..entry("home-model") },
        HomeModelEntry { display_name: "No Context Model".into(), ..entry("home-model-without-context") },
    ];
    let response = grok_response(&entries);
    let data = response["data"].as_array().expect("data");
    assert_eq!(data.len(), 2);
    assert_eq!(
        (data[0]["id"].as_str(), data[0]["name"].as_str(), data[0]["context_window"].as_i64()),
        (Some("home-model"), Some("Home Model"), Some(1234))
    );
    assert!(data[1].get("context_window").is_none());
    assert!(data.iter().all(|m| m.get("reasoning_efforts").is_none() && m["api_backend"] == "responses"));
}

#[test]
fn gemini_lookup_accepts_prefixed_and_bare_ids() {
    let e = entry("gemini-3-pro");
    for action in ["gemini-3-pro", "models/gemini-3-pro"] {
        assert!(gemini_model_matches(&e, action), "{action}");
    }
    assert!(!gemini_model_matches(&e, ""));
    assert!(!gemini_model_matches(&e, "gemini-3"));
    assert_eq!(format_gemini_model(&e)["name"], "models/gemini-3-pro");
    assert_eq!(format_gemini_model(&entry("models/x"))["name"], "models/x");
}

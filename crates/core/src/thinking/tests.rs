//! Ports of the Go thinking tests (internal/thinking/*_test.go) and a replay of the
//! test/thinking_conversion_test.go matrix against recorded Go outputs.

use cpa_json::{J, Value};
use parking_lot::Mutex;

use super::*;
use crate::registry::{
    ModelInfo, ThinkingSupport, get_kimi_models, global_registry, lookup_model_info,
};

fn val(body: &[u8]) -> Value {
    cpa_json::parse(body)
}

/// gjson `GetBytes(body, path).String()`.
fn gs(body: &[u8], path: &str) -> String {
    val(body).g(path).str()
}

/// gjson `GetBytes(body, path).Exists()`.
fn gx(body: &[u8], path: &str) -> bool {
    val(body).g(path).exists()
}

fn s(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

fn support(min: i64, max: i64, zero: bool, dynamic: bool, levels: &[&str]) -> ThinkingSupport {
    ThinkingSupport {
        min,
        max,
        zero_allowed: zero,
        dynamic_allowed: dynamic,
        levels: levels.iter().map(|l| (*l).to_owned()).collect(),
    }
}

fn info(id: &str, ty: &str, thinking: Option<ThinkingSupport>) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        r#type: ty.into(),
        thinking,
        ..Default::default()
    }
}

/// Serializes tests that touch the global registry and unregisters their client on drop.
static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

struct Registered {
    client: String,
    _guard: parking_lot::MutexGuard<'static, ()>,
}

impl Registered {
    fn new(client: &str, provider: &str, models: &[ModelInfo]) -> Self {
        let guard = REGISTRY_LOCK.lock();
        global_registry().register_client(client, provider, models);
        Self {
            client: client.into(),
            _guard: guard,
        }
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        global_registry().unregister_client(&self.client);
    }
}

// ---------------------------------------------------------------- suffix / conversions

#[test]
fn parse_suffix_splits_on_last_open_paren() {
    let r = parse_suffix("claude-sonnet-4-5(16384)");
    assert_eq!(
        (r.model_name.as_str(), r.has_suffix, r.raw_suffix.as_str()),
        ("claude-sonnet-4-5", true, "16384")
    );
    let r = parse_suffix("gemini-2.5-pro");
    assert_eq!(
        (r.model_name.as_str(), r.has_suffix, r.raw_suffix.as_str()),
        ("gemini-2.5-pro", false, "")
    );
    // No closing paren at the end: not a suffix.
    assert!(!parse_suffix("m(high)x").has_suffix);
    // Empty raw suffix is still a suffix.
    let r = parse_suffix("m()");
    assert_eq!(
        (r.model_name.as_str(), r.has_suffix, r.raw_suffix.as_str()),
        ("m", true, "")
    );
    // Only the last parenthesis group counts.
    let r = parse_suffix("a(b)(high)");
    assert_eq!(
        (r.model_name.as_str(), r.raw_suffix.as_str()),
        ("a(b)", "high")
    );
}

#[test]
fn suffix_value_parsing() {
    assert_eq!(parse_numeric_suffix("08192"), Some(8192));
    assert_eq!(parse_numeric_suffix("0"), Some(0));
    assert_eq!(parse_numeric_suffix("-1"), None);
    assert_eq!(parse_numeric_suffix("9223372036854775808"), None);
    assert_eq!(parse_numeric_suffix(""), None);
    assert_eq!(parse_special_suffix("NONE"), Some(ThinkingMode::None));
    assert_eq!(parse_special_suffix("-1"), Some(ThinkingMode::Auto));
    assert_eq!(parse_special_suffix("high"), None);
    assert_eq!(parse_level_suffix("HIGH"), Some("high"));
    assert_eq!(parse_level_suffix("none"), None);
    assert_eq!(parse_level_suffix("ultra"), None);
}

#[test]
fn level_budget_conversions() {
    assert_eq!(convert_level_to_budget("HIGH"), Some(24576));
    assert_eq!(convert_level_to_budget("max"), Some(128000));
    assert_eq!(convert_level_to_budget("ultra"), None);
    assert_eq!(convert_budget_to_level(-2), None);
    assert_eq!(convert_budget_to_level(-1), Some("auto"));
    assert_eq!(convert_budget_to_level(0), Some("none"));
    assert_eq!(convert_budget_to_level(512), Some("minimal"));
    assert_eq!(convert_budget_to_level(513), Some("low"));
    assert_eq!(convert_budget_to_level(8192), Some("medium"));
    assert_eq!(convert_budget_to_level(24576), Some("high"));
    assert_eq!(convert_budget_to_level(24577), Some("xhigh"));
    assert_eq!(map_to_claude_effort(" XHigh ", true), Some("max"));
    assert_eq!(map_to_claude_effort("xhigh", false), Some("high"));
    assert_eq!(map_to_claude_effort("minimal", false), Some("low"));
    assert_eq!(map_to_claude_effort("", false), None);
    assert_eq!(map_to_claude_effort("ultra", false), None);
}

// ---------------------------------------------------------------- summary (summary_test.go)

#[test]
fn extract_summary_config_table() {
    use SummaryMode::{Disabled as D, Enabled as E, Unspecified as U};
    #[rustfmt::skip]
    let tests: &[(&str, &str, &str, SummaryMode, &str)] = &[
        ("chat effort enables", "openai", r#"{"reasoning_effort":"high"}"#, E, "auto"),
        ("chat none disables", "openai", r#"{"reasoning_effort":"none"}"#, D, ""),
        ("chat missing unspecified", "openai", r#"{}"#, U, ""),
        ("chat null effort unspecified", "openai", r#"{"reasoning_effort":null}"#, U, ""),
        ("chat non-string effort unspecified", "openai", r#"{"reasoning_effort":17}"#, U, ""),
        ("chat google extension false overrides effort", "openai", r#"{"reasoning_effort":"high","extra_body":{"google":{"thinking_config":{"include_thoughts":false}}}}"#, D, ""),
        ("chat google extension true", "openai", r#"{"extra_body":{"google":{"thinking_config":{"include_thoughts":true}}}}"#, E, "auto"),
        ("chat exclude disables", "openai", r#"{"reasoning_effort":"high","reasoning":{"exclude":true}}"#, D, ""),
        ("chat exclude false enables", "openai", r#"{"reasoning":{"effort":"high","exclude":false}}"#, E, "auto"),
        ("chat legacy include_reasoning false disables", "openai", r#"{"reasoning_effort":"high","include_reasoning":false}"#, D, ""),
        ("chat legacy include_reasoning true enables", "openai", r#"{"include_reasoning":true}"#, E, "auto"),
        ("chat reasoning enabled false disables", "openai", r#"{"reasoning":{"enabled":false}}"#, D, ""),
        ("chat reasoning enabled true enables", "openai", r#"{"reasoning":{"enabled":true}}"#, E, "auto"),
        ("chat exclude wins over include_reasoning", "openai", r#"{"reasoning":{"exclude":true},"include_reasoning":true}"#, D, ""),
        ("chat non-boolean include_reasoning unspecified", "openai", r#"{"include_reasoning":"false"}"#, U, ""),
        ("responses effort alone unspecified", "openai-response", r#"{"reasoning":{"effort":"high"}}"#, U, ""),
        ("responses summary auto", "openai-response", r#"{"reasoning":{"effort":"high","summary":"auto"}}"#, E, "auto"),
        ("responses summary concise", "openai-response", r#"{"reasoning":{"summary":"concise"}}"#, E, "concise"),
        ("responses summary null", "openai-response", r#"{"reasoning":{"summary":null}}"#, D, ""),
        ("responses boolean summary invalid", "openai-response", r#"{"reasoning":{"summary":true}}"#, U, ""),
        ("responses deprecated generate summary", "openai-response", r#"{"reasoning":{"generate_summary":"detailed"}}"#, E, "detailed"),
        ("claude summarized", "claude", r#"{"thinking":{"type":"adaptive","display":"summarized"}}"#, E, "auto"),
        ("claude omitted", "claude", r#"{"thinking":{"type":"enabled","budget_tokens":2048,"display":"omitted"}}"#, D, ""),
        ("claude display without type is invalid", "claude", r#"{"thinking":{"display":"summarized"}}"#, U, ""),
        ("claude display with auto type is invalid", "claude", r#"{"thinking":{"type":"auto","display":"summarized"}}"#, U, ""),
        // Runs before ApplyThinking fills budget_tokens, so an absent budget is not inactive thinking.
        ("claude enabled display without budget is valid", "claude", r#"{"thinking":{"type":"enabled","display":"summarized"}}"#, E, "auto"),
        ("claude enabled display with zero budget is invalid", "claude", r#"{"thinking":{"type":"enabled","budget_tokens":0,"display":"summarized"}}"#, U, ""),
        ("claude auto compatibility budget summarized", "claude", r#"{"thinking":{"type":"enabled","budget_tokens":-1,"display":"summarized"}}"#, E, "auto"),
        ("claude auto compatibility budget omitted", "claude", r#"{"thinking":{"type":"enabled","budget_tokens":-1,"display":"omitted"}}"#, D, ""),
        ("gemini include true", "gemini", r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":true}}}"#, E, "auto"),
        ("gemini include false", "gemini", r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":false}}}"#, D, ""),
        ("antigravity include true", "antigravity", r#"{"request":{"generationConfig":{"thinkingConfig":{"includeThoughts":true}}}}"#, E, "auto"),
        ("interactions auto", "interactions", r#"{"generation_config":{"thinking_summaries":"auto"}}"#, E, "auto"),
        ("interactions none", "interactions", r#"{"generation_config":{"thinking_summaries":"none"}}"#, D, ""),
        ("interactions nested snake include false", "interactions", r#"{"generation_config":{"thinking_config":{"include_thoughts":false}}}"#, D, ""),
        ("interactions nested camel include true", "interactions", r#"{"generation_config":{"thinking_config":{"includeThoughts":true}}}"#, E, "auto"),
        ("interactions camel config snake include true", "interactions", r#"{"generation_config":{"thinkingConfig":{"include_thoughts":true}}}"#, E, "auto"),
        ("interactions camel config camel include false", "interactions", r#"{"generation_config":{"thinkingConfig":{"includeThoughts":false}}}"#, D, ""),
        ("interactions enum wins over compatibility reasoning", "interactions", r#"{"generation_config":{"thinking_summaries":"none"},"reasoning":{"summary":"auto"}}"#, D, ""),
        ("interactions compatibility reasoning auto", "interactions", r#"{"reasoning":{"summary":"auto"}}"#, E, "auto"),
        ("interactions compatibility reasoning none", "interactions", r#"{"reasoning":{"summary":"none"}}"#, D, ""),
        ("interactions enum wins over include alias", "interactions", r#"{"generation_config":{"thinking_summaries":"none","thinking_config":{"include_thoughts":true}}}"#, D, ""),
        ("interactions string include alias is invalid", "interactions", r#"{"generation_config":{"thinking_config":{"include_thoughts":"false"}}}"#, U, ""),
        ("interactions detailed is invalid", "interactions", r#"{"generation_config":{"thinking_summaries":"detailed"}}"#, U, ""),
        ("interactions boolean is invalid", "interactions", r#"{"generation_config":{"thinking_summaries":true}}"#, U, ""),
        ("gemini string bool is invalid", "gemini", r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":"true"}}}"#, U, ""),
    ];
    for (name, format, body, mode, detail) in tests {
        let got = extract_summary_config(body.as_bytes(), format);
        assert!(
            got.mode == *mode && got.detail == *detail,
            "{name}: got {got:?}, want {mode:?} {detail:?}"
        );
    }
}

#[test]
fn explicit_summary_config_does_not_use_chat_effort() {
    let got = extract_explicit_summary_config(br#"{"reasoning_effort":"high"}"#, "openai");
    assert_eq!(got.mode, SummaryMode::Unspecified);
    let got = extract_explicit_summary_config(
        br#"{"reasoning_effort":"high","reasoning":{"exclude":true}}"#,
        "openai",
    );
    assert_eq!(got.mode, SummaryMode::Disabled);
}

#[test]
fn translated_summary_ignores_chat_effort_only_for_claude() {
    let body = br#"{"reasoning_effort":"high"}"#;
    assert_eq!(
        extract_translated_summary_config(body, "openai", "claude").mode,
        SummaryMode::Unspecified
    );
    assert_eq!(
        extract_translated_summary_config(body, "openai", "gemini").mode,
        SummaryMode::Enabled
    );
}

fn cfg(mode: SummaryMode, detail: &str) -> SummaryConfig {
    SummaryConfig {
        mode,
        detail: detail.into(),
    }
}

#[test]
fn apply_summary_config_table() {
    use SummaryMode::{Disabled as D, Enabled as E};
    #[rustfmt::skip]
    let tests: &[(&str, &str, &str, SummaryMode, &str, &str, &str)] = &[
        // (name, format, body, mode, detail, path, want)
        ("chat enabled invents no effort", "openai", "", E, "", "reasoning_effort", ""),
        ("chat enabled preserves active effort", "openai", r#"{"reasoning_effort":"high"}"#, E, "", "reasoning_effort", "high"),
        ("chat enabled preserves disabled effort", "openai", r#"{"reasoning_effort":"none"}"#, E, "", "reasoning_effort", "none"),
        // Chat cannot express "reason but hide", so disabling must not become effort "none".
        ("chat disabled preserves requested effort", "openai", r#"{"reasoning_effort":"high"}"#, D, "", "reasoning_effort", "high"),
        ("chat disabled sets openrouter exclude when present", "openai", r#"{"reasoning":{"effort":"high","exclude":false}}"#, D, "", "reasoning.exclude", "true"),
        ("chat enabled clears openrouter exclude when present", "openai", r#"{"reasoning":{"effort":"high","exclude":true}}"#, E, "", "reasoning.exclude", "false"),
        ("chat disabled updates legacy include_reasoning when present", "openai", r#"{"reasoning_effort":"high","include_reasoning":true}"#, D, "", "include_reasoning", "false"),
        ("chat disabled invents no openrouter field", "openai", r#"{"reasoning_effort":"high"}"#, D, "", "reasoning", ""),
        ("claude enabled", "claude", r#"{"thinking":{"type":"adaptive"}}"#, E, "", "thinking.display", "summarized"),
        ("claude disabled", "claude", r#"{"thinking":{"type":"enabled","budget_tokens":2048}}"#, D, "", "thinking.display", "omitted"),
        ("gemini enabled", "gemini", "", E, "", "generationConfig.thinkingConfig.includeThoughts", "true"),
        ("gemini disabled", "gemini", "", D, "", "generationConfig.thinkingConfig.includeThoughts", "false"),
        ("antigravity enabled", "antigravity", "", E, "", "request.generationConfig.thinkingConfig.includeThoughts", "true"),
        ("interactions detail collapses to auto", "interactions", "", E, "detailed", "generation_config.thinking_summaries", "auto"),
        ("interactions disabled", "interactions", "", D, "", "generation_config.thinking_summaries", "none"),
        ("responses concise", "openai-response", "", E, "concise", "reasoning.summary", "concise"),
    ];
    for (name, format, body, mode, detail, path, want) in tests {
        let body = if body.is_empty() { "{}" } else { body };
        let out = apply_summary_config(body.as_bytes().to_vec(), format, &cfg(*mode, detail));
        assert_eq!(gs(&out, path), *want, "{name}: body={}", s(&out));
    }
}

#[test]
fn apply_summary_config_openai_chat_provider_dialects() {
    use SummaryMode::{Disabled as D, Enabled as E};
    #[rustfmt::skip]
    let tests: &[(&str, &str, &str, SummaryMode, &str, bool, &str)] = &[
        // (name, provider, body, mode, wantExclude, wantExisting, wantEffort)
        ("OpenAI does not invent visibility", "openai", "{}", E, "", false, ""),
        ("OpenRouter enables visibility", "openrouter", "{}", E, "false", true, ""),
        ("OpenRouter disables visibility", "prod-openrouter", "{}", D, "true", true, ""),
        ("DeepSeek preserves documented effort", "deepseek", r#"{"reasoning_effort":"high"}"#, D, "", false, "high"),
        ("Kimi preserves documented K3 effort", "kimi", r#"{"reasoning_effort":"max"}"#, E, "", false, "max"),
        ("Moonshot does not invent visibility", "moonshot", r#"{"thinking":{"type":"enabled"}}"#, E, "", false, ""),
        ("generic provider updates existing OpenRouter field", "openai-compatibility", r#"{"reasoning":{"exclude":false}}"#, D, "true", true, ""),
    ];
    for (name, provider, body, mode, want_exclude, want_existing, want_effort) in tests {
        let out = summary::apply_summary_config_for_provider(
            body.as_bytes().to_vec(),
            "openai",
            "model",
            provider,
            None,
            &cfg(*mode, ""),
        );
        let exclude = val(&out).g("reasoning.exclude").str();
        assert_eq!(
            gx(&out, "reasoning.exclude"),
            *want_existing,
            "{name}: {}",
            s(&out)
        );
        if *want_existing {
            assert_eq!(exclude, *want_exclude, "{name}: {}", s(&out));
        }
        if want_effort.is_empty() {
            assert!(
                !gx(&out, "reasoning_effort"),
                "{name} invented reasoning_effort: {}",
                s(&out)
            );
        } else {
            assert_eq!(
                gs(&out, "reasoning_effort"),
                *want_effort,
                "{name}: {}",
                s(&out)
            );
        }
    }
}

#[test]
fn apply_summary_config_normalizes_target_aliases() {
    let tests = [
        (
            "gemini",
            r#"{"generationConfig":{"thinkingConfig":{"include_thoughts":true}}}"#,
            "generationConfig.thinkingConfig.includeThoughts",
            "generationConfig.thinkingConfig.include_thoughts",
        ),
        (
            "antigravity",
            r#"{"request":{"generationConfig":{"thinkingConfig":{"include_thoughts":true}}}}"#,
            "request.generationConfig.thinkingConfig.includeThoughts",
            "request.generationConfig.thinkingConfig.include_thoughts",
        ),
        (
            "interactions",
            r#"{"generation_config":{"thinkingSummaries":"auto"}}"#,
            "generation_config.thinking_summaries",
            "generation_config.thinkingSummaries",
        ),
    ];
    for (format, body, canonical, alias) in tests {
        let out = apply_summary_config(
            body.as_bytes().to_vec(),
            format,
            &cfg(SummaryMode::Enabled, ""),
        );
        assert!(
            gx(&out, canonical),
            "{format} missing canonical field: {}",
            s(&out)
        );
        assert!(
            !gx(&out, alias),
            "{format} retained alias {alias}: {}",
            s(&out)
        );
    }
}

/// Anthropic requires thinking.type and rejects display on a disabled block, so display must never
/// be written unless thinking is already active; the body must come back byte-identical.
#[test]
fn apply_summary_config_claude_display_requires_active_thinking() {
    let bodies = [
        "{}",
        r#"{"messages":[{"role":"user","content":"hi"}]}"#,
        r#"{"thinking":{"type":"disabled"}}"#,
    ];
    for mode in [SummaryMode::Enabled, SummaryMode::Disabled] {
        for body in bodies {
            let out = apply_summary_config(body.as_bytes().to_vec(), "claude", &cfg(mode, ""));
            assert!(
                !gx(&out, "thinking.display"),
                "{mode:?} wrote display without active thinking: {}",
                s(&out)
            );
            assert_eq!(s(&out), body, "{mode:?} changed body");
        }
    }
}

#[test]
fn apply_summary_config_for_model_claude_enabled_summary_uses_valid_thinking_mode() {
    let tests = [
        (
            "adaptive model",
            "claude-opus-5",
            r#"{"model":"claude-opus-5","max_tokens":32000}"#,
            "adaptive",
            0,
        ),
        (
            "manual model",
            "claude-haiku-4-5-20251001",
            r#"{"model":"claude-haiku-4-5-20251001","max_tokens":32000}"#,
            "enabled",
            1024,
        ),
    ];
    for (name, model, body, want_type, want_budget) in tests {
        let out = apply_summary_config_for_model(
            body.as_bytes().to_vec(),
            "claude",
            model,
            &cfg(SummaryMode::Enabled, ""),
        );
        assert_eq!(gs(&out, "thinking.type"), want_type, "{name}: {}", s(&out));
        assert_eq!(
            gs(&out, "thinking.display"),
            "summarized",
            "{name}: {}",
            s(&out)
        );
        if want_budget > 0 {
            assert_eq!(
                val(&out).g("thinking.budget_tokens").int(),
                want_budget,
                "{name}: {}",
                s(&out)
            );
        }
    }
}

/// Disabling summaries must not add a Claude thinking block: absence preserves each model's default.
#[test]
fn apply_summary_config_for_model_claude_disabled_summary_does_not_enable_thinking() {
    for model in ["claude-opus-5", "claude-haiku-4-5-20251001"] {
        let body = format!(r#"{{"model":"{model}","max_tokens":32000}}"#);
        let out = apply_summary_config_for_model(
            body.into_bytes(),
            "claude",
            model,
            &cfg(SummaryMode::Disabled, ""),
        );
        assert!(
            !gx(&out, "thinking"),
            "model {model} gained thinking for a disabled summary: {}",
            s(&out)
        );
    }
}

#[test]
fn apply_summary_config_responses_cases() {
    let out = apply_summary_config(
        br#"{"reasoning":{"generate_summary":"detailed"}}"#.to_vec(),
        "openai-response",
        &cfg(SummaryMode::Enabled, "detailed"),
    );
    assert_eq!(gs(&out, "reasoning.summary"), "detailed");
    assert!(
        !gx(&out, "reasoning.generate_summary"),
        "deprecated generate_summary remained: {}",
        s(&out)
    );

    let out = apply_summary_config(
        br#"{"reasoning":{"effort":"high","summary":"auto"}}"#.to_vec(),
        "openai-response",
        &cfg(SummaryMode::Disabled, ""),
    );
    assert!(!gx(&out, "reasoning.summary"), "{}", s(&out));
    assert_eq!(gs(&out, "reasoning.effort"), "high");

    let out = apply_summary_config(
        br#"{"model":"gpt-5.4","reasoning":{"summary":"auto"}}"#.to_vec(),
        "openai-response",
        &cfg(SummaryMode::Disabled, ""),
    );
    assert!(
        !gx(&out, "reasoning"),
        "empty reasoning object left behind: {}",
        s(&out)
    );
}

#[test]
fn apply_summary_config_unspecified_leaves_body_unchanged() {
    let body = br#"{"thinking":{"type":"adaptive"}}"#;
    assert_eq!(
        apply_summary_config(body.to_vec(), "claude", &SummaryConfig::default()),
        body
    );
}

// ---------------------------------------------------------------- codex usage (apply_codex_usage_test.go)

#[test]
fn extract_codex_reasoning_effort_with_configuration_update() {
    #[rustfmt::skip]
    let tests: &[(&str, &str, &str, &str, &str, &str)] = &[
        // (name, provider, model, body, wantRequestEffort, wantTranslatedEffort)
        ("codex extracts configuration_update effort over top-level", "codex", "gpt-6-astra",
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#, "low", "low"),
        ("openai-response extracts configuration_update effort over top-level", "openai-response", "gpt-6-astra",
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#, "low", "low"),
        ("codex picks latest configuration_update when multiple are present", "codex", "gpt-6-astra",
            r#"{
				"model":"gpt-6-astra",
				"reasoning":{"effort":"xhigh","summary":"auto"},
				"input":[
					{"type":"configuration_update","reasoning":{"effort":"low"}},
					{"role":"user","content":"second turn"},
					{"type":"configuration_update","reasoning":{"effort":"medium"}}
				]
			}"#, "medium", "medium"),
        ("codex falls back to top-level when configuration_update has no reasoning effort", "codex", "gpt-6-astra",
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","tools":[]}]}"#, "xhigh", "xhigh"),
        ("codex handles configuration_update effort none", "codex", "gpt-6-astra",
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"none"}}]}"#, "none", "none"),
        ("codex handles configuration_update effort auto", "codex", "gpt-6-astra",
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"auto"}}]}"#, "auto", "auto"),
        ("trailing configuration_update without reasoning effort preserves earlier effort", "codex", "gpt-6-astra",
            r#"{
				"model":"gpt-6-astra",
				"reasoning":{"effort":"xhigh"},
				"input":[
					{"type":"configuration_update","reasoning":{"effort":"low"}},
					{"type":"configuration_update","tools":[]}
				]
			}"#, "low", "low"),
        ("invalid json returns empty effort", "codex", "gpt-6-astra",
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":"#, "", ""),
        ("codex falls back to top-level when input has no configuration_update", "codex", "gpt-6-astra",
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"role":"user","content":"hello"}]}"#, "xhigh", "xhigh"),
        ("source configuration_update takes precedence over suffix for request effort", "codex", "gpt-6-astra(high)",
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#, "low", "low"),
        ("suffix takes precedence over top-level without updates", "openai-response", "gpt-6-astra(high)",
            r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","tools":[]}]}"#, "high", "xhigh"),
    ];
    for (name, provider, model, body, want_request, want_translated) in tests {
        assert_eq!(
            extract_reasoning_effort(body.as_bytes(), provider, model),
            *want_request,
            "{name}"
        );
        assert_eq!(
            extract_translated_reasoning_effort(body.as_bytes(), provider),
            *want_translated,
            "{name}"
        );
    }
}

#[test]
fn extract_codex_reasoning_effort_configuration_update_target_routing() {
    const SOURCE: &str = r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"medium"}},{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":null}},{"role":"user","content":"ok"}]}"#;
    for supported in [true, false] {
        let mut model = info(
            "opaque-route",
            "codex",
            Some(support(0, 0, false, false, &["low", "high", "xhigh"])),
        );
        model.support_configuration_update = supported;
        let body = apply_thinking_with_model_info_and_summary(
            SOURCE.as_bytes(),
            SOURCE.as_bytes(),
            "opaque-route(high)",
            "openai-response",
            "codex",
            "codex",
            Some(&model),
            &SummaryConfig::default(),
            false,
        )
        .expect("apply");
        assert_eq!(
            extract_reasoning_effort(SOURCE.as_bytes(), "openai-response", "opaque-route(high)"),
            "low"
        );
        let want = if supported { "low" } else { "high" };
        assert_eq!(
            extract_translated_reasoning_effort(&body, "codex"),
            want,
            "body={}",
            s(&body)
        );
        assert_eq!(gs(&body, "reasoning.effort"), "high", "body={}", s(&body));
        let has_update = gs(&body, "input.0.type") == "configuration_update";
        assert_eq!(has_update, supported, "body={}", s(&body));
    }
}

#[test]
fn apply_configuration_update_routing() {
    const MODEL: &str = "configured-responses";
    struct Case {
        name: &'static str,
        body: &'static str,
        format: &'static str,
        suffix: &'static str,
        supported: bool,
        no_thinking: bool,
        skip_effort: bool,
        want_effort: &'static str,
        want_input: &'static str,
        want_same: bool,
    }
    let base = Case {
        name: "",
        body: "",
        format: "",
        suffix: "",
        supported: false,
        no_thinking: false,
        skip_effort: false,
        want_effort: "",
        want_input: "",
        want_same: false,
    };
    let tests = [
        Case {
            name: "supported native request preserves baseline and updates byte for byte",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            want_effort: "xhigh",
            want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
            want_same: true,
            ..base
        },
        Case {
            name: "supported no-effort request remains unchanged",
            body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]}"#,
            supported: true,
            want_input: r#"[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]"#,
            want_same: true,
            ..base
        },
        Case {
            name: "supported native request without thinking metadata preserves baseline summary and updates",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            no_thinking: true,
            want_effort: "xhigh",
            want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
            want_same: true,
            ..base
        },
        Case {
            name: "supported native suffix still strips effort without thinking metadata",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            no_thinking: true,
            suffix: "high",
            want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
            ..base
        },
        Case {
            name: "supported suffix only rewrites the top-level effort",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto","other":7},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            suffix: "high",
            want_effort: "high",
            want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
            ..base
        },
        Case {
            name: "supported suffix keeps effective input effort for current turn",
            body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            suffix: "high",
            want_effort: "high",
            want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
            ..base
        },
        Case {
            name: "supported invalid suffix leaves native payload untouched",
            body: r#"{"reasoning":{"generate_summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            suffix: "invalid",
            want_input: r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#,
            want_same: true,
            ..base
        },
        Case {
            name: "unsupported latest nonempty update wins and other input order stays unchanged",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto","other":7},"input":[{"role":"user","content":"first"},{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":"  "}},{"role":"assistant","content":"reply"},{"type":"configuration_update","reasoning":{"effort":"medium"}},{"type":"configuration_update","tools":[]},{"role":"user","content":"last"}]}"#,
            want_effort: "medium",
            want_input: r#"[{"role":"user","content":"first"},{"role":"assistant","content":"reply"},{"role":"user","content":"last"}]"#,
            ..base
        },
        Case {
            name: "unsupported ignores empty and nonstring efforts",
            body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":42}},{"type":"configuration_update","reasoning":{"effort":null}},{"type":"configuration_update","reasoning":{"effort":"  "}},{"role":"user","content":"ok"}]}"#,
            want_effort: "low",
            want_input: r#"[{"role":"user","content":"ok"}]"#,
            ..base
        },
        Case {
            name: "unsupported suffix takes precedence and still removes updates",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            suffix: "high",
            want_effort: "high",
            want_input: r#"[{"role":"user","content":"ok"}]"#,
            ..base
        },
        Case {
            name: "unsupported without top-level reasoning promotes the last update",
            body: r#"{"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            want_effort: "low",
            want_input: r#"[{"role":"user","content":"ok"}]"#,
            ..base
        },
        Case {
            name: "unsupported removes updates with no effort without inventing one",
            body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]}"#,
            want_input: r#"[{"role":"user","content":"ok"}]"#,
            ..base
        },
        Case {
            name: "unsupported without thinking support still removes updates",
            body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]}"#,
            no_thinking: true,
            want_input: r#"[{"role":"user","content":"ok"}]"#,
            ..base
        },
        Case {
            name: "unsupported without thinking support strips effort but keeps summary",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto","other":7},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            no_thinking: true,
            want_input: r#"[{"role":"user","content":"ok"}]"#,
            ..base
        },
        Case {
            name: "unsupported openai-response alias removes updates",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            format: "openai-response",
            want_effort: "low",
            want_input: r#"[{"role":"user","content":"ok"}]"#,
            ..base
        },
        Case {
            name: "invalid JSON is untouched",
            body: r#"{"reasoning":{"effort":"xhigh"},"input":["#,
            want_same: true,
            skip_effort: true,
            ..base
        },
        Case {
            name: "nonarray input is untouched",
            body: r#"{"reasoning":{"summary":"auto"},"input":{"type":"configuration_update","reasoning":{"effort":"low"}}}"#,
            want_input: r#"{"type":"configuration_update","reasoning":{"effort":"low"}}"#,
            want_same: true,
            ..base
        },
    ];
    for tc in tests {
        let format = if tc.format.is_empty() {
            "codex"
        } else {
            tc.format
        };
        let thinking_support = if tc.no_thinking {
            None
        } else {
            Some(support(
                0,
                0,
                false,
                false,
                &["low", "medium", "high", "xhigh"],
            ))
        };
        let mut model = info(MODEL, "codex", thinking_support);
        model.support_configuration_update = tc.supported;
        let suffix = if tc.suffix.is_empty() {
            String::new()
        } else {
            format!("({})", tc.suffix)
        };
        let body = tc.body.as_bytes();
        let applied = apply_thinking_with_model_info(
            body,
            body,
            &format!("{MODEL}{suffix}"),
            format,
            format,
            "codex",
            Some(&model),
        )
        .unwrap_or_else(|e| panic!("{}: {e}", tc.name));
        let name = tc.name;
        if tc.want_same {
            assert_eq!(s(&applied), tc.body, "{name}: body changed");
        }
        if !tc.skip_effort {
            assert_eq!(
                gs(&applied, "reasoning.effort"),
                tc.want_effort,
                "{name}: body={}",
                s(&applied)
            );
        }
        if !tc.want_input.is_empty() {
            assert_eq!(
                val(&applied).g("input").raw(),
                tc.want_input,
                "{name}: body={}",
                s(&applied)
            );
        }
        if gx(body, "reasoning.summary") {
            assert_eq!(
                gs(&applied, "reasoning.summary"),
                "auto",
                "{name}: body={}",
                s(&applied)
            );
        }
        if gx(body, "reasoning.other") {
            assert_eq!(
                val(&applied).g("reasoning.other").int(),
                7,
                "{name}: {}",
                s(&applied)
            );
        }
        if tc.supported && !tc.suffix.is_empty() {
            assert_eq!(
                extract_translated_reasoning_effort(&applied, format),
                "low",
                "{name}: body={}",
                s(&applied)
            );
        }
    }
}

/// Native Responses bodies stay byte-identical and usage reporting sees the effective effort.
/// (The Go test also asserts debug log fields; logging is not ported.)
#[test]
fn apply_thinking_native_responses_effective_effort() {
    #[rustfmt::skip]
    let tests: &[(&str, &str, &str)] = &[
        ("update overrides top-level baseline", r#"{"model":"gpt-6-sol","reasoning":{"effort":"high","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"xhigh"}},{"role":"user","content":"ok"}]}"#, "xhigh"),
        ("latest update wins", r#"{"model":"gpt-6-sol","reasoning":{"effort":"high"},"input":[{"type":"configuration_update","reasoning":{"effort":"xhigh"}},{"type":"configuration_update","reasoning":{"effort":"max"}}]}"#, "max"),
        ("trailing empty and nonstring updates keep last effective effort", r#"{"model":"gpt-6-sol","reasoning":{"effort":"high"},"input":[{"type":"configuration_update","reasoning":{"effort":"xhigh"}},{"type":"configuration_update","reasoning":{"effort":null}},{"type":"configuration_update","reasoning":{"effort":42}},{"type":"configuration_update","reasoning":{"effort":"  "}}]}"#, "xhigh"),
        ("top-level fallback without update", r#"{"model":"gpt-6-sol","reasoning":{"effort":"high"},"input":[{"role":"user","content":"ok"}]}"#, "high"),
        ("update without top-level effort", r#"{"model":"gpt-6-sol","input":[{"type":"configuration_update","reasoning":{"effort":"xhigh"}}]}"#, "xhigh"),
        ("update disables thinking", r#"{"model":"gpt-6-sol","reasoning":{"effort":"high"},"input":[{"type":"configuration_update","reasoning":{"effort":"none"}}]}"#, "none"),
    ];
    for (name, body, want_effort) in tests {
        for bound in [false, true] {
            let body = body.as_bytes();
            let applied = if bound {
                let model =
                    lookup_model_info("gpt-6-sol", Some("codex")).expect("gpt-6-sol in catalog");
                assert!(
                    model.support_configuration_update,
                    "gpt-6-sol must support configuration updates"
                );
                apply_thinking_with_model_info(
                    body,
                    body,
                    "gpt-6-sol",
                    "codex",
                    "codex",
                    "codex",
                    Some(&model),
                )
            } else {
                apply_thinking(body, "gpt-6-sol", "codex", "codex", "codex")
            }
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                applied, body,
                "{name} (bound={bound}): native Responses body changed"
            );
            assert_eq!(
                extract_translated_reasoning_effort(&applied, "codex"),
                *want_effort,
                "{name}"
            );
        }
    }
}

#[test]
fn apply_thinking_preserves_codex_top_level_reasoning_effort_baseline() {
    // Preserving the request-level baseline sent upstream keeps the prompt prefix cacheable.
    let body = br#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#;
    let applied = apply_thinking(body, "gpt-6-astra", "codex", "codex", "codex").expect("apply");
    assert_eq!(gs(&applied, "reasoning.effort"), "xhigh");
    assert_eq!(gs(&applied, "reasoning.summary"), "auto");
    assert_eq!(gs(&applied, "input.0.reasoning.effort"), "low");
    assert_eq!(
        extract_translated_reasoning_effort(&applied, "codex"),
        "low"
    );
}

// ---------------------------------------------------------------- configured api key (apply_configured_api_key_test.go)

#[test]
fn model_info_maps_cross_family_high_intent() {
    let tests = [
        (
            "xhigh stays xhigh",
            "xhigh",
            vec!["high", "max", "xhigh"],
            "xhigh",
        ),
        ("xhigh prefers max", "xhigh", vec!["high", "max"], "max"),
        ("xhigh falls back to high", "xhigh", vec!["high"], "high"),
        ("max stays max", "max", vec!["high", "xhigh", "max"], "max"),
        ("max prefers xhigh", "max", vec!["high", "xhigh"], "xhigh"),
        ("max falls back to high", "max", vec!["high"], "high"),
    ];
    for (name, source, supported, want) in tests {
        let model = info(
            "claude-upstream",
            "claude",
            Some(support(0, 0, false, false, &supported)),
        );
        let body = br#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}}"#;
        let src = format!(r#"{{"reasoning_effort":"{source}"}}"#);
        let out = apply_thinking_with_model_info(
            body,
            src.as_bytes(),
            "claude-upstream",
            "openai",
            "claude",
            "claude",
            Some(&model),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            gs(&out, "output_config.effort"),
            want,
            "{name}: body={}",
            s(&out)
        );
    }
}

#[test]
fn model_info_maps_openai_compatibility_high_intent() {
    let model = info(
        "compat-upstream",
        "openai-compatibility",
        Some(support(0, 0, false, false, &["high", "max"])),
    );
    let out = apply_thinking_with_model_info(
        br#"{"reasoning_effort":"high"}"#,
        br#"{"reasoning_effort":"xhigh"}"#,
        "compat-upstream",
        "openai",
        "openai",
        "compat-provider",
        Some(&model),
    )
    .expect("apply");
    assert_eq!(gs(&out, "reasoning_effort"), "max", "body={}", s(&out));
}

#[test]
fn model_info_maps_responses_to_codex_high_intent() {
    let model = info(
        "codex-upstream",
        "codex",
        Some(support(0, 0, false, false, &["high", "xhigh"])),
    );
    let out = apply_thinking_with_model_info(
        br#"{"reasoning":{"effort":"high"}}"#,
        br#"{"reasoning":{"effort":"max"}}"#,
        "codex-upstream",
        "openai-response",
        "codex",
        "codex",
        Some(&model),
    )
    .expect("apply");
    assert_eq!(gs(&out, "reasoning.effort"), "xhigh", "body={}", s(&out));
}

#[test]
fn model_info_keeps_same_family_validation_strict() {
    let model = info(
        "openai-upstream",
        "openai",
        Some(support(0, 0, false, false, &["low", "medium", "high"])),
    );
    let body = br#"{"reasoning_effort":"xhigh"}"#;
    let err = apply_thinking_with_model_info(
        body,
        body,
        "openai-upstream",
        "openai",
        "openai",
        "openai",
        Some(&model),
    )
    .expect_err("unsupported xhigh must error");
    assert_eq!(err.code, ErrorCode::LevelNotSupported);
    assert_eq!(
        err.message,
        r#"level "xhigh" not supported, valid levels: low, medium, high"#
    );
    assert_eq!(err.status_code(), 400);
}

#[test]
fn model_info_applies_enabled_summary_only_claude_visibility() {
    let model = info(
        "private-claude",
        "claude",
        Some(support(0, 0, false, false, &["high"])),
    );
    let out = apply_thinking_with_model_info(
        br#"{"model":"private-claude","max_tokens":32000}"#,
        br#"{"reasoning":{"summary":"auto"}}"#,
        "private-claude",
        "openai-response",
        "claude",
        "claude",
        Some(&model),
    )
    .expect("apply");
    assert_eq!(gs(&out, "thinking.type"), "adaptive", "body={}", s(&out));
    assert_eq!(
        gs(&out, "thinking.display"),
        "summarized",
        "body={}",
        s(&out)
    );
}

#[test]
fn model_info_and_summary_drops_inferred_claude_mode_when_summary_removed() {
    let model = info(
        "private-manual-claude",
        "claude",
        Some(support(1024, 16000, false, false, &[])),
    );
    let out = apply_thinking_with_model_info_and_summary(
        br#"{"model":"private-manual-claude","max_tokens":32000,"thinking":{"type":"adaptive"}}"#,
        br#"{"reasoning":{"summary":"auto"}}"#,
        "private-manual-claude",
        "openai-response",
        "claude",
        "claude",
        Some(&model),
        &SummaryConfig::default(),
        false,
    )
    .expect("apply");
    assert!(
        !gx(&out, "thinking"),
        "removed summary retained globally inferred adaptive thinking: {}",
        s(&out)
    );
}

#[test]
fn model_info_does_not_activate_claude_for_disabled_summary() {
    let model = info(
        "private-claude",
        "claude",
        Some(support(0, 0, false, false, &["high"])),
    );
    let out = apply_thinking_with_model_info(
        br#"{"model":"private-claude","max_tokens":32000}"#,
        br#"{"reasoning":{"summary":null}}"#,
        "private-claude",
        "openai-response",
        "claude",
        "claude",
        Some(&model),
    )
    .expect("apply");
    assert!(
        !gx(&out, "thinking"),
        "disabled summary activated Claude thinking: {}",
        s(&out)
    );
}

#[test]
fn model_info_summary_only_does_not_invent_openai_effort() {
    let model = info(
        "private-openai",
        "openai",
        Some(support(0, 0, false, false, &["high", "max"])),
    );
    let out = apply_thinking_with_model_info(
        br#"{"model":"private-openai","messages":[{"role":"user","content":"hi"}]}"#,
        br#"{"model":"private-openai","reasoning":{"summary":"auto"},"input":"hi"}"#,
        "private-openai",
        "openai-response",
        "openai",
        "openai",
        Some(&model),
    )
    .expect("apply");
    assert!(
        !gx(&out, "reasoning_effort"),
        "summary-only request invented reasoning_effort: {}",
        s(&out)
    );
}

#[test]
fn with_summary_keeps_openai_chat_suffix_none() {
    let out = apply_thinking_with_summary(
        br#"{"model":"private-openai","messages":[{"role":"user","content":"hi"}]}"#,
        "private-openai(none)",
        "openai-response",
        "openai",
        "openai",
        &cfg(SummaryMode::Enabled, "auto"),
    )
    .expect("apply");
    assert_eq!(gs(&out, "reasoning_effort"), "none", "body={}", s(&out));
}

#[test]
fn model_info_uses_openrouter_visibility() {
    let model = info(
        "openrouter-model",
        "openai-compatibility",
        Some(support(0, 0, false, false, &["high", "max"])),
    );
    let out = apply_thinking_with_model_info(
        br#"{"model":"openrouter-model","messages":[{"role":"user","content":"hi"}]}"#,
        br#"{"model":"openrouter-model","reasoning":{"summary":"auto"},"input":"hi"}"#,
        "openrouter-model",
        "openai-response",
        "openai",
        "openrouter",
        Some(&model),
    )
    .expect("apply");
    let parsed = val(&out);
    let exclude = parsed.g("reasoning.exclude");
    assert!(
        exclude.exists() && !exclude.bool(),
        "OpenRouter summary visibility not enabled: {}",
        s(&out)
    );
    assert!(
        !gx(&out, "reasoning_effort"),
        "invented reasoning_effort: {}",
        s(&out)
    );
}

#[test]
fn apply_configuration_update_cross_protocol() {
    const SOURCE: &str = r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"},{"type":"configuration_update","reasoning":{"effort":"high"}}]}"#;
    #[rustfmt::skip]
    #[allow(clippy::type_complexity)]
    let tests: &[(&str, &str, &str, &str, &str, bool, &str, &str)] = &[
        // (name, body, model, format, typeName, supported, wantPath, want)
        ("unsupported Responses source becomes OpenAI Chat effort", r#"{"messages":[{"role":"user","content":"ok"}],"reasoning_effort":"medium","other":true}"#, "private-chat", "openai", "openai", false, "reasoning_effort", "high"),
        ("supported updates cannot be sent to OpenAI Chat", r#"{"messages":[{"role":"user","content":"ok"}],"reasoning_effort":"medium","other":true}"#, "private-chat", "openai", "openai", true, "reasoning_effort", "high"),
        ("Responses source becomes Claude effort", r#"{"max_tokens":4096,"thinking":{"type":"adaptive"},"output_config":{"effort":"medium"},"other":true}"#, "private-claude", "claude", "claude", false, "output_config.effort", "high"),
        ("model suffix overrides source updates", r#"{"messages":[{"role":"user","content":"ok"}],"reasoning_effort":"medium","other":true}"#, "private-chat(low)", "openai", "openai", false, "reasoning_effort", "low"),
    ];
    for (name, body, model, format, type_name, supported, want_path, want) in tests {
        let mut m = info(
            "private",
            type_name,
            Some(support(
                0,
                0,
                false,
                false,
                &["low", "medium", "high", "xhigh"],
            )),
        );
        m.support_configuration_update = *supported;
        let out = apply_thinking_with_model_info(
            body.as_bytes(),
            SOURCE.as_bytes(),
            model,
            "openai-response",
            format,
            format,
            Some(&m),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(gs(&out, want_path), *want, "{name}: body={}", s(&out));
        assert!(
            val(&out).g("other").bool(),
            "{name}: non-thinking field lost: {}",
            s(&out)
        );
    }
}

#[test]
fn apply_configuration_update_source_entry() {
    const SOURCE: &str = r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#;
    const TARGET: &str = r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"medium"}},{"role":"user","content":"ok"}]}"#;
    #[rustfmt::skip]
    #[allow(clippy::type_complexity)]
    let tests: &[(&str, &str, &str, &str, &str, &str, &str, bool)] = &[
        // (name, model, body, source, format, want, wantInput, wantSame)
        ("registry capability preserves native Responses without suffix", "gpt-6-astra", SOURCE, SOURCE, "", "xhigh", r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#, true),
        ("unknown gpt-6 model defaults to unsupported", "gpt-6-unknown-routed", SOURCE, SOURCE, "", "low", r#"[{"role":"user","content":"ok"}]"#, false),
        ("source effort controls translated target", "gpt-6-unknown-routed", TARGET, SOURCE, "", "low", r#"[{"role":"user","content":"ok"}]"#, false),
        ("unknown model source update applies to OpenAI Chat", "gpt-6-unknown-routed", r#"{"reasoning_effort":"xhigh","messages":[{"role":"user","content":"ok"}]}"#, SOURCE, "openai", "low", "", false),
        ("nonarray input remains unchanged", "gpt-6-unknown-routed", r#"{"reasoning":{"summary":"auto"},"input":{"type":"configuration_update"}}"#, SOURCE, "", "low", r#"{"type":"configuration_update"}"#, false),
    ];
    for (name, model, body, source, format, want, want_input, want_same) in tests {
        let format = if format.is_empty() { "codex" } else { format };
        let summary = extract_summary_config(source.as_bytes(), "openai-response");
        let out = apply_thinking_with_source_and_summary(
            body.as_bytes(),
            source.as_bytes(),
            model,
            "openai-response",
            format,
            format,
            &summary,
            false,
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        let path = if format == "openai" {
            "reasoning_effort"
        } else {
            "reasoning.effort"
        };
        assert_eq!(gs(&out, path), *want, "{name}: body={}", s(&out));
        if !want_input.is_empty() {
            assert_eq!(
                val(&out).g("input").raw(),
                *want_input,
                "{name}: body={}",
                s(&out)
            );
        }
        if *want_same {
            assert_eq!(s(&out), *body, "{name}: native body changed");
        }
    }
}

#[test]
fn apply_configuration_update_invalid_target() {
    const INVALID_TARGET: &str = r#"{"reasoning":{"effort":"xhigh"},"input":["#;
    const UPDATE_SOURCE: &str = r#"{"reasoning":{"effort":"medium"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#;
    #[rustfmt::skip]
    let tests: &[(&str, &str, &str, bool, bool, &str)] = &[
        // (name, model, source, bound, normalized, want)
        ("bound model does not rebuild invalid target from source update", "private-codex", UPDATE_SOURCE, true, false, INVALID_TARGET),
        ("unbound model does not rebuild invalid target from source update", "gpt-6-unknown-routed", UPDATE_SOURCE, false, false, INVALID_TARGET),
        ("normalized updates cannot rebuild invalid target", "private-codex", UPDATE_SOURCE, true, true, INVALID_TARGET),
        ("suffix alone still rebuilds invalid target", "private-codex(high)", "", true, false, r#"{"reasoning":{"effort":"high"}}"#),
        ("suffix takes priority over source update for invalid target", "private-codex(high)", UPDATE_SOURCE, true, true, r#"{"reasoning":{"effort":"high"}}"#),
    ];
    let model = info(
        "private-codex",
        "codex",
        Some(support(
            0,
            0,
            false,
            false,
            &["low", "medium", "high", "xhigh"],
        )),
    );
    for (name, m, source, bound, normalized, want) in tests {
        let body = INVALID_TARGET.as_bytes();
        let out = if *bound {
            apply_thinking_with_model_info_and_summary(
                body,
                source.as_bytes(),
                m,
                "codex",
                "codex",
                "codex",
                Some(&model),
                &SummaryConfig::default(),
                *normalized,
            )
        } else {
            apply_thinking_with_source_and_summary(
                body,
                source.as_bytes(),
                m,
                "codex",
                "codex",
                "codex",
                &SummaryConfig::default(),
                *normalized,
            )
        }
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(s(&out), *want, "{name}");
    }
}

#[test]
fn apply_configuration_update_bound_model_without_thinking() {
    const BODY: &str = r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#;
    let summary = extract_summary_config(BODY.as_bytes(), "codex");
    let out = apply_thinking_with_model_info_and_summary(
        BODY.as_bytes(),
        BODY.as_bytes(),
        "gpt-6-astra",
        "codex",
        "codex",
        "codex",
        None,
        &summary,
        false,
    )
    .expect("apply");
    assert_eq!(
        gs(&out, "reasoning.effort"),
        "low",
        "unresolved binding used static support: {}",
        s(&out)
    );
    assert_eq!(
        val(&out).g("input").array().len(),
        1,
        "unresolved binding retained an update: {}",
        s(&out)
    );

    let mut model = ModelInfo {
        id: "custom".into(),
        user_defined: true,
        ..Default::default()
    };
    let out = apply_thinking_with_model_info_and_summary(
        BODY.as_bytes(),
        BODY.as_bytes(),
        "custom",
        "codex",
        "codex",
        "codex",
        Some(&model),
        &summary,
        false,
    )
    .expect("apply user-defined");
    assert_eq!(
        gs(&out, "reasoning.effort"),
        "low",
        "user-defined model lost source effort: {}",
        s(&out)
    );
    assert_eq!(
        val(&out).g("input").array().len(),
        1,
        "user-defined model retained an update: {}",
        s(&out)
    );

    model.support_configuration_update = true;
    for (name, m, want) in [
        ("native no suffix", "custom", "xhigh"),
        ("native suffix", "custom(high)", "high"),
    ] {
        let out = apply_thinking_with_model_info_and_summary(
            BODY.as_bytes(),
            BODY.as_bytes(),
            m,
            "codex",
            "codex",
            "codex",
            Some(&model),
            &summary,
            false,
        )
        .expect("apply native");
        assert_eq!(gs(&out, "reasoning.effort"), want, "{name}: {}", s(&out));
        assert_eq!(
            gs(&out, "input.0.reasoning.effort"),
            "low",
            "{name}: input update effort changed: {}",
            s(&out)
        );
        assert_eq!(gs(&out, "reasoning.summary"), "auto", "{name}: {}", s(&out));
    }
}

#[test]
fn model_info_uses_original_responses_effort() {
    let model = info(
        "claude-upstream",
        "claude",
        Some(support(0, 0, false, false, &["high", "max"])),
    );
    let out = apply_thinking_with_model_info(
        br#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}}"#,
        br#"{"reasoning":{"effort":"xhigh"}}"#,
        "claude-upstream",
        "openai-response",
        "claude",
        "claude",
        Some(&model),
    )
    .expect("apply");
    assert_eq!(gs(&out, "output_config.effort"), "max", "body={}", s(&out));
}

// ---------------------------------------------------------------- claude enabled effort (claude_enabled_effort_test.go)

#[test]
fn claude_enabled_with_output_config_effort() {
    let tests = [
        (
            "explicit output_config effort is preserved without budget",
            r#"{"model":"custom-openai","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled"},"output_config":{"effort":"high"}}"#,
            "high",
        ),
        (
            "legacy budget remains authoritative when both are present",
            r#"{"model":"custom-openai","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":8192},"output_config":{"effort":"high"}}"#,
            "medium",
        ),
        (
            "enabled without budget or effort keeps auto default",
            r#"{"model":"custom-openai","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled"}}"#,
            "auto",
        ),
        (
            "enabled with empty effort string falls back to auto",
            r#"{"model":"custom-openai","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled"},"output_config":{"effort":""}}"#,
            "auto",
        ),
        (
            "enabled with whitespace-only effort falls back to auto",
            r#"{"model":"custom-openai","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled"},"output_config":{"effort":"   "}}"#,
            "auto",
        ),
        (
            "enabled with non-string effort falls back to auto",
            r#"{"model":"custom-openai","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled"},"output_config":{"effort":123}}"#,
            "auto",
        ),
    ];
    for (name, body, want) in tests {
        let out = apply_thinking(
            body.as_bytes(),
            "custom-openai",
            "claude",
            "openai",
            "openai",
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            gs(&out, "reasoning_effort"),
            want,
            "{name}: body={}",
            s(&out)
        );
    }
}

/// Go chains `openaiclaude.ConvertClaudeRequestToOpenAI` (translator crate, not reachable from
/// core) into `ApplyThinkingWithSourceAndSummary`; the translated body below is that function's
/// recorded output for `raw_claude`.
#[test]
fn claude_to_openai_translation_and_thinking_chained() {
    let raw_claude = br#"{"model":"custom-openai","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled"},"output_config":{"effort":"high"}}"#;
    let translated = br#"{"model":"custom-openai","messages":[{"role":"user","content":"hi"}],"stream":false,"reasoning_effort":"high"}"#;
    assert_eq!(gs(translated, "reasoning_effort"), "high");
    let out = apply_thinking_with_source_and_summary(
        translated,
        raw_claude,
        "custom-openai",
        "claude",
        "openai",
        "openai",
        &SummaryConfig::default(),
        false,
    )
    .expect("apply");
    assert_eq!(gs(&out, "reasoning_effort"), "high", "body={}", s(&out));
}

// ---------------------------------------------------------------- kimi (kimi_max_clamp_repro_test.go)

/// K2.8 models list "max" among their levels, so effort=max is preserved.
#[test]
fn kimi_k28_claude_messages_max_preserves_max() {
    let _reg = Registered::new("test-kimi-k28-max", "kimi", &get_kimi_models());
    let body = br#"{"model":"kimi-k2.8","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"max"}}"#;
    let out = apply_thinking(body, "kimi-k2.8", "claude", "claude", "claude").expect("apply");
    assert_eq!(gs(&out, "output_config.effort"), "max", "body={}", s(&out));
}

/// Claude Code -> Kimi /v1/messages with effort=max on K2.5 clamps to high.
#[test]
fn kimi_claude_messages_max_clamps_to_high() {
    let _reg = Registered::new("test-kimi-max-clamp", "kimi", &get_kimi_models());
    let body = br#"{"model":"kimi-k2.5","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"max"}}"#;
    let out = apply_thinking(body, "kimi-k2.5", "claude", "claude", "claude").expect("apply");
    assert_eq!(gs(&out, "thinking.type"), "adaptive", "body={}", s(&out));
    assert_eq!(gs(&out, "output_config.effort"), "high", "body={}", s(&out));
}

// ---------------------------------------------------------------- thinking_conversion_test.go matrix

/// The model definitions registered by the Go matrix test (getTestModels).
fn matrix_models() -> Vec<ModelInfo> {
    let m = |id: &str, ty: &str, t: Option<ThinkingSupport>| info(id, ty, t);
    let claude = |id: &str, max_completion: i64, t: ThinkingSupport| {
        let mut model = info(id, "claude", Some(t));
        model.max_completion_tokens = max_completion;
        model
    };
    vec![
        m(
            "level-model",
            "openai",
            Some(support(
                0,
                0,
                false,
                false,
                &["minimal", "low", "medium", "high"],
            )),
        ),
        m(
            "level-subset-model",
            "gemini",
            Some(support(0, 0, false, false, &["low", "high"])),
        ),
        m(
            "gemini-budget-model",
            "gemini",
            Some(support(128, 20000, false, true, &[])),
        ),
        m(
            "gemini-mixed-model",
            "gemini",
            Some(support(128, 32768, false, true, &["low", "high"])),
        ),
        m(
            "gemini-toggle-mixed-model",
            "gemini",
            Some(support(128, 32768, true, true, &["low", "high"])),
        ),
        m(
            "claude-budget-model",
            "claude",
            Some(support(1024, 128000, true, false, &[])),
        ),
        claude(
            "claude-opus-4-6-model",
            128000,
            support(1024, 128000, true, false, &["low", "medium", "high", "max"]),
        ),
        claude(
            "claude-sonnet-4-6-model",
            64000,
            support(1024, 128000, true, false, &["low", "medium", "high"]),
        ),
        m(
            "antigravity-budget-model",
            "antigravity",
            Some(support(128, 20000, true, true, &[])),
        ),
        m(
            "kimi-toggle-thinking-model",
            "kimi",
            Some(support(0, 0, true, false, &["low", "medium", "high"])),
        ),
        m(
            "kimi-tiered-thinking-model",
            "kimi",
            Some(support(0, 0, false, false, &["low", "medium", "high"])),
        ),
        m(
            "xai-level-model",
            "xai",
            Some(support(
                0,
                0,
                true,
                false,
                &["none", "low", "medium", "high"],
            )),
        ),
        m("no-thinking-model", "openai", None),
        ModelInfo {
            user_defined: true,
            ..m("user-defined-model", "openai", None)
        },
    ]
}

/// Replays every case of the Go matrix (285 cases across suffix, body, provider-target,
/// interactions and Claude adaptive tests) through `apply_thinking`. Each record holds the body the
/// Go translators produced for the case plus Go's exact `ApplyThinking` result, so this checks the
/// full output (including key order), not just the fields the Go assertions inspect.
#[test]
fn thinking_conversion_matrix_matches_go() {
    let _reg = Registered::new("thinking-matrix", "test", &matrix_models());
    let data = include_str!("testdata/conversion_matrix.jsonl");
    let mut failures = Vec::new();
    let mut count = 0;
    for line in data.lines().filter(|l| !l.trim().is_empty()) {
        count += 1;
        let rec: Value = serde_json::from_str(line).expect("fixture line");
        let field = |k: &str| rec.g(k).str();
        let (name, from, to, model, input) = (
            field("name"),
            field("from"),
            field("to"),
            field("model"),
            field("input"),
        );
        let got = apply_thinking(input.as_bytes(), &model, &from, &to, &to);
        if rec.g("err").bool() {
            match got {
                Err(e) => {
                    if e.message != field("errMsg") {
                        failures.push(format!(
                            "{name}: error message {:?}, want {:?}",
                            e.message,
                            field("errMsg")
                        ));
                    }
                }
                Ok(out) => failures.push(format!("{name}: expected error, got body {}", s(&out))),
            }
            continue;
        }
        match got {
            Err(e) => failures.push(format!("{name}: unexpected error {e}")),
            Ok(out) => {
                // Go keeps untouched bytes verbatim; compare compact forms with key order.
                let want = cpa_json::to_string(&cpa_json::parse(field("output").as_bytes()));
                let have = cpa_json::to_string(&cpa_json::parse(&out));
                if want != have {
                    failures.push(format!("{name}:\n  want {want}\n  got  {have}"));
                }
            }
        }
    }
    assert_eq!(count, 285, "fixture case count");
    assert!(
        failures.is_empty(),
        "{} of {count} cases differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ---------------------------------------------------------------- Go differential replay

/// Normal form for comparing bodies: compact JSON (key order kept) when valid, else the raw text.
fn norm(body: &[u8]) -> String {
    if cpa_json::valid(body) {
        cpa_json::to_string(&cpa_json::parse(body))
    } else {
        s(body)
    }
}

fn model_from_spec(spec: &Value) -> ModelInfo {
    let mut m = ModelInfo {
        id: spec.g("id").str(),
        r#type: spec.g("type").str(),
        max_completion_tokens: spec.g("maxCompl").int(),
        support_configuration_update: spec.g("cfgUpdate").bool(),
        user_defined: spec.g("userDefined").bool(),
        ..Default::default()
    };
    if spec.g("hasThinking").bool() {
        m.thinking = Some(ThinkingSupport {
            min: spec.g("min").int(),
            max: spec.g("max").int(),
            zero_allowed: spec.g("zero").bool(),
            dynamic_allowed: spec.g("dyn").bool(),
            levels: spec.g("levels").array().iter().map(|l| l.str()).collect(),
        });
    }
    m
}

fn summary_mode(n: i64) -> SummaryMode {
    match n {
        1 => SummaryMode::Disabled,
        2 => SummaryMode::Enabled,
        _ => SummaryMode::Unspecified,
    }
}

fn mode_num(m: SummaryMode) -> i64 {
    match m {
        SummaryMode::Unspecified => 0,
        SummaryMode::Disabled => 1,
        SummaryMode::Enabled => 2,
    }
}

fn thinking_mode_num(m: ThinkingMode) -> i64 {
    match m {
        ThinkingMode::Budget => 0,
        ThinkingMode::Level => 1,
        ThinkingMode::None => 2,
        ThinkingMode::Auto => 3,
    }
}

/// Differential replay against a JSONL file recorded from the Go implementation by
/// `testdata/oracle_gen.go.txt` (about 176k cases: `apply`, `applyInfo`, summary extraction and
/// application, effort labels, strip and `validate` records). Run with
/// `THINKING_ORACLE=/tmp/oracle_wide.jsonl cargo test -p cpa-core thinking_oracle -- --ignored`.
#[test]
#[ignore = "needs THINKING_ORACLE=<jsonl recorded from Go>"]
fn thinking_oracle_file_matches_go() {
    let Ok(path) = std::env::var("THINKING_ORACLE") else {
        return;
    };
    let _reg = Registered::new("thinking-oracle", "test", &matrix_models());
    let data = std::fs::read_to_string(path).expect("read oracle file");
    let mut failures: Vec<String> = Vec::new();
    let mut count = 0usize;
    let mut fail = |line: &str, got: String| failures.push(format!("{line}\n  got {got}"));
    for line in data.lines().filter(|l| !l.trim().is_empty()) {
        // Known divergence: gjson `Int()` wraps integers beyond i64 while `cpa_json` saturates.
        if line.contains("99999999999999999999") {
            continue;
        }
        count += 1;
        let rec: Value = serde_json::from_str(line).expect("oracle line");
        let f = |k: &str| rec.g(k).str();
        let want_out = f("output");
        let want_err = rec.g("err").bool();
        match f("kind").as_str() {
            "apply" => {
                let got = apply_thinking(
                    f("input").as_bytes(),
                    &f("model"),
                    &f("from"),
                    &f("to"),
                    &f("providerKey"),
                );
                match (&got, want_err) {
                    // Go's Kimi applier returns plain (non-ThinkingError) errors: no errCode recorded.
                    (Err(e), true)
                        if e.message == f("errMsg")
                            && (e.code.as_str() == f("errCode")
                                || (e.code == ErrorCode::ApplyFailed
                                    && f("errCode").is_empty())) => {}
                    (Ok(out), false) if norm(out) == norm(want_out.as_bytes()) => {}
                    _ => fail(line, format!("{got:?}")),
                }
            }
            "applyInfo" => {
                let info_v = rec.g("info");
                let model = info_v.is_object().then(|| model_from_spec(&info_v.value()));
                let summary = cfg(
                    summary_mode(rec.g("summaryMode").int()),
                    &f("summaryDetail"),
                );
                let (body, source, m, from, to, pk) = (
                    f("input"),
                    f("source"),
                    f("model"),
                    f("from"),
                    f("to"),
                    f("providerKey"),
                );
                let got = if rec.g("explicitSummary").bool() {
                    apply_thinking_with_model_info_and_summary(
                        body.as_bytes(),
                        source.as_bytes(),
                        &m,
                        &from,
                        &to,
                        &pk,
                        model.as_ref(),
                        &summary,
                        rec.g("normalized").bool(),
                    )
                } else {
                    apply_thinking_with_model_info(
                        body.as_bytes(),
                        source.as_bytes(),
                        &m,
                        &from,
                        &to,
                        &pk,
                        model.as_ref(),
                    )
                };
                match (&got, want_err) {
                    (Err(e), true) if e.message == f("errMsg") => {}
                    (Ok(out), false) if norm(out) == norm(want_out.as_bytes()) => {}
                    _ => fail(line, format!("{got:?}")),
                }
            }
            "summaryExtract" => {
                let (body, format) = (f("input"), f("format"));
                let c = extract_summary_config(body.as_bytes(), &format);
                let e = extract_explicit_summary_config(body.as_bytes(), &format);
                let t = extract_translated_summary_config(body.as_bytes(), &format, "claude");
                let ok = mode_num(c.mode) == rec.g("mode").int()
                    && c.detail == f("detail")
                    && mode_num(e.mode) == rec.g("explicitMode").int()
                    && e.detail == f("explicitDetail")
                    && mode_num(t.mode) == rec.g("translatedMode").int()
                    && t.detail == f("translatedDetail");
                if !ok {
                    fail(line, format!("{c:?} {e:?} {t:?}"));
                }
            }
            "summaryApply" => {
                let detail = f("detail");
                let c = cfg(
                    summary_mode(if rec.g("mode").int() == 1 { 1 } else { 2 }),
                    &detail,
                );
                let got = apply_summary_config_for_model(
                    f("input").into_bytes(),
                    &f("format"),
                    &f("model"),
                    &c,
                );
                if norm(&got) != norm(want_out.as_bytes()) {
                    fail(line, s(&got));
                }
            }
            "summaryToClaude" => {
                let got = apply_translated_summary_to_claude(
                    br#"{"max_tokens":32000}"#,
                    f("source").as_bytes(),
                    &f("format"),
                    "claude-opus-5",
                );
                if norm(&got) != norm(want_out.as_bytes()) {
                    fail(line, s(&got));
                }
            }
            "effort" => {
                let a =
                    extract_reasoning_effort(f("input").as_bytes(), &f("provider"), &f("model"));
                let b = extract_translated_reasoning_effort(f("input").as_bytes(), &f("provider"));
                if a != f("effort") || b != f("translated") {
                    fail(line, format!("{a:?} {b:?}"));
                }
            }
            "strip" => {
                let got = strip_thinking_config(
                    f("input").as_bytes(),
                    f("provider").trim().to_lowercase().as_str(),
                );
                if norm(&got) != norm(want_out.as_bytes()) {
                    fail(line, s(&got));
                }
            }
            "validate" => {
                let model = model_from_spec(&rec.g("info").value());
                let config = ThinkingConfig {
                    mode: match rec.g("mode").int() {
                        1 => ThinkingMode::Level,
                        2 => ThinkingMode::None,
                        3 => ThinkingMode::Auto,
                        _ => ThinkingMode::Budget,
                    },
                    budget: rec.g("budget").int(),
                    level: f("level"),
                };
                let got = validate_config(
                    &config,
                    Some(&model),
                    &f("from"),
                    &f("to"),
                    rec.g("fromSuffix").bool(),
                );
                match (&got, want_err) {
                    (Err(e), true) if e.message == f("errMsg") => {}
                    (Ok(c), false)
                        if thinking_mode_num(c.mode) == rec.g("outMode").int()
                            && c.budget == rec.g("outBudget").int()
                            && c.level == f("outLevel") => {}
                    _ => fail(line, format!("{got:?}")),
                }
            }
            other => panic!("unknown record kind {other}"),
        }
    }
    if let Ok(dump) = std::env::var("THINKING_ORACLE_DUMP") {
        std::fs::write(dump, failures.join("\n")).expect("write failures");
    }
    assert!(
        failures.is_empty(),
        "{} of {count} oracle cases differ (first 15):\n{}",
        failures.len(),
        failures
            .iter()
            .take(15)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

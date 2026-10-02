//! Reading the canonical [`ThinkingConfig`] out of provider-shaped request bodies, and the
//! reasoning-effort labels used for usage logging (Go: apply.go extract* helpers).

use cpa_json::{J, Value};

use super::configuration_update::{configuration_update_config, is_responses_format};
use super::convert::convert_budget_to_level;
use super::json::parse_valid;
use super::suffix::{parse_level_suffix, parse_numeric_suffix, parse_special_suffix};
use super::types::{SuffixResult, ThinkingConfig, ThinkingMode};
use super::{level, parse_suffix};

/// Extracts `provider`'s thinking config from a request body (`provider` must already be
/// normalized). Empty or invalid bodies and unknown providers yield the empty config.
pub(crate) fn extract_thinking_config(body: &[u8], provider: &str) -> ThinkingConfig {
    match parse_valid(body) {
        Some(v) => thinking_config_of(&v, provider),
        None => ThinkingConfig::default(),
    }
}

fn thinking_config_of(v: &Value, provider: &str) -> ThinkingConfig {
    match provider {
        "claude" => extract_claude_config(v),
        "gemini" | "antigravity" => extract_gemini_config(v, provider),
        "interactions" => extract_interactions_config(v),
        "openai" => extract_openai_config(v),
        "codex" | "xai" => extract_codex_config(v),
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => extract_kimi_config(v),
        _ => ThinkingConfig::default(),
    }
}

/// Source-side extraction: `openai-response` bodies use the Codex `reasoning.effort` shape.
pub(crate) fn extract_source_thinking_config(body: &[u8], provider: &str) -> ThinkingConfig {
    let provider = provider.trim().to_lowercase();
    if provider == "openai-response" {
        return parse_valid(body)
            .map(|v| extract_codex_config(&v))
            .unwrap_or_default();
    }
    extract_thinking_config(body, &provider)
}

/// Canonical config from a lowercased effort string: none, auto, else a level.
fn config_from_effort(value: String) -> ThinkingConfig {
    match value.as_str() {
        level::NONE => ThinkingConfig::none(),
        level::AUTO => ThinkingConfig::auto(),
        _ => ThinkingConfig::level(value),
    }
}

/// Canonical config from an integer budget: 0 none, -1 auto, else a budget.
fn config_from_budget(value: i64) -> ThinkingConfig {
    match value {
        0 => ThinkingConfig::none(),
        -1 => ThinkingConfig::auto(),
        _ => ThinkingConfig::budget(value),
    }
}

/// Claude: `thinking.type` ("enabled", "adaptive", "auto", "disabled"), `thinking.budget_tokens`
/// (-1 auto, 0 disabled, >0 budget) and `output_config.effort`.
///
/// `disabled` wins over budget_tokens. Adaptive/auto only counts when `output_config.effort` is a
/// string (otherwise upstream defaults apply). When type is `enabled`, budget_tokens wins over
/// effort; with neither, Auto means "enabled with default budget".
fn extract_claude_config(v: &Value) -> ThinkingConfig {
    let thinking_type = v.g("thinking.type").str();
    if thinking_type == "disabled" {
        return ThinkingConfig::none();
    }
    if thinking_type == "adaptive" || thinking_type == "auto" {
        if let Some(effort) = v.g("output_config.effort").as_str() {
            let value = effort.trim().to_lowercase();
            if value.is_empty() {
                return ThinkingConfig::default();
            }
            return config_from_effort(value);
        }
        return ThinkingConfig::default();
    }

    let budget = v.g("thinking.budget_tokens");
    if budget.exists() {
        return config_from_budget(budget.int());
    }

    if thinking_type == "enabled" {
        if let Some(effort) = v.g("output_config.effort").as_str() {
            let value = effort.trim().to_lowercase();
            if !value.is_empty() {
                return config_from_effort(value);
            }
        }
        return ThinkingConfig::auto();
    }

    ThinkingConfig::default()
}

/// Gemini/Antigravity: `thinkingLevel` (Gemini 3) takes precedence over `thinkingBudget`
/// (Gemini 2.5); snake_case spellings are accepted too. Antigravity paths are `request.` prefixed.
fn extract_gemini_config(v: &Value, provider: &str) -> ThinkingConfig {
    let prefix = if provider == "antigravity" {
        "request.generationConfig.thinkingConfig"
    } else {
        "generationConfig.thinkingConfig"
    };

    let mut lvl = v.g(&format!("{prefix}.thinkingLevel"));
    if !lvl.exists() {
        lvl = v.g(&format!("{prefix}.thinking_level"));
    }
    if lvl.exists() {
        // Not lowercased or trimmed, unlike the interactions variant.
        return config_from_effort(lvl.str());
    }

    let mut budget = v.g(&format!("{prefix}.thinkingBudget"));
    if !budget.exists() {
        budget = v.g(&format!("{prefix}.thinking_budget"));
    }
    if budget.exists() {
        return config_from_budget(budget.int());
    }

    ThinkingConfig::default()
}

fn extract_interactions_config(v: &Value) -> ThinkingConfig {
    for path in [
        "generation_config.thinking_level",
        "generation_config.thinkingLevel",
        "generation_config.thinking_config.thinking_level",
        "generation_config.thinking_config.thinkingLevel",
        "generation_config.thinkingConfig.thinking_level",
        "generation_config.thinkingConfig.thinkingLevel",
    ] {
        let lvl = v.g(path);
        if !lvl.exists() {
            continue;
        }
        return config_from_effort(lvl.str().trim().to_lowercase());
    }

    for path in [
        "generation_config.thinking_budget",
        "generation_config.thinkingBudget",
        "generation_config.thinking_config.thinking_budget",
        "generation_config.thinking_config.thinkingBudget",
        "generation_config.thinkingConfig.thinking_budget",
        "generation_config.thinkingConfig.thinkingBudget",
    ] {
        let budget = v.g(path);
        if !budget.exists() {
            continue;
        }
        return config_from_budget(budget.int());
    }

    ThinkingConfig::default()
}

/// OpenAI Chat: `reasoning_effort`; `none` is ModeNone, anything else a level as written (not
/// lowercased).
fn extract_openai_config(v: &Value) -> ThinkingConfig {
    let effort = v.g("reasoning_effort");
    if effort.exists() {
        let value = effort.str();
        if value == level::NONE {
            return ThinkingConfig::none();
        }
        return ThinkingConfig::level(value);
    }
    ThinkingConfig::default()
}

/// Kimi's native `thinking` object, with `reasoning_effort` as a legacy fallback. Native fields
/// win; `thinking.type="enabled"` without an explicit effort means "use the upstream default" and
/// returns the empty config so the request is preserved instead of becoming ModeAuto.
fn extract_kimi_config(v: &Value) -> ThinkingConfig {
    let thinking_type = v.g("thinking.type");
    if thinking_type.exists() {
        match thinking_type.str().trim().to_lowercase().as_str() {
            "disabled" => return ThinkingConfig::none(),
            "enabled" if !v.g("thinking.effort").exists() => return ThinkingConfig::default(),
            _ => {}
        }
    }

    let effort = v.g("thinking.effort");
    if effort.exists() {
        let value = effort.str().trim().to_lowercase();
        if value.is_empty() {
            return ThinkingConfig::default();
        }
        return config_from_effort(value);
    }

    // An explicit native thinking object without an effort is left for Kimi to interpret and must
    // not be overridden by the legacy field.
    if thinking_type.exists() {
        return ThinkingConfig::default();
    }

    extract_openai_config(v)
}

/// Codex/Responses: `reasoning.effort`.
pub(crate) fn extract_codex_config(v: &Value) -> ThinkingConfig {
    let effort = v.g("reasoning.effort");
    if effort.exists() {
        let value = effort.str();
        if value == level::NONE {
            return ThinkingConfig::none();
        }
        return ThinkingConfig::level(value);
    }
    ThinkingConfig::default()
}

/// The last effective Responses update, falling back to the top-level effort when no update
/// applies (used for usage reporting and source-intent resolution).
pub(crate) fn extract_codex_usage_config(body: &[u8]) -> ThinkingConfig {
    let Some(v) = parse_valid(body) else {
        return ThinkingConfig::default();
    };
    let update = configuration_update_config(&v);
    if update.has_config() {
        return update;
    }
    extract_codex_config(&v)
}

/// Raw parse of a suffix's content into a config: special values, then levels, then numbers
/// (0 is None). Unknown content yields the empty config (no config).
pub(crate) fn parse_suffix_to_config(raw_suffix: &str) -> ThinkingConfig {
    if let Some(mode) = parse_special_suffix(raw_suffix) {
        match mode {
            ThinkingMode::None => return ThinkingConfig::none(),
            ThinkingMode::Auto => return ThinkingConfig::auto(),
            _ => {}
        }
    }
    if let Some(lvl) = parse_level_suffix(raw_suffix) {
        return ThinkingConfig::level(lvl);
    }
    if let Some(budget) = parse_numeric_suffix(raw_suffix) {
        return if budget == 0 {
            ThinkingConfig::none()
        } else {
            ThinkingConfig::budget(budget)
        };
    }
    ThinkingConfig::default()
}

/// The source request's thinking setting as a canonical `reasoning_effort` label (`none`, `auto`,
/// a level, or a budget-derived level) for usage logging. Responses updates take precedence over a
/// suffix because they describe the source turn's intent; otherwise a valid suffix overrides the
/// top-level setting.
pub fn extract_reasoning_effort(body: &[u8], provider: &str, model: &str) -> String {
    let provider = provider.trim().to_lowercase();
    if is_responses_format(&provider) {
        let effort = reasoning_effort_from_config(
            &super::configuration_update::extract_configuration_update_config(body),
        );
        if !effort.is_empty() {
            return effort;
        }
    }
    let effort = reasoning_effort_from_suffix(&parse_suffix(model));
    if !effort.is_empty() {
        return effort;
    }

    let mut config = extract_thinking_config_for_usage(body, &provider);
    if !config.has_config() && matches!(provider.as_str(), "openai-response" | "openai") {
        config = extract_codex_usage_config(body);
    }
    reasoning_effort_from_config(&config)
}

/// The final provider payload's thinking setting as a canonical `reasoning_effort` label.
pub fn extract_translated_reasoning_effort(body: &[u8], provider: &str) -> String {
    let provider = provider.trim().to_lowercase();
    let mut config = extract_thinking_config_for_usage(body, &provider);
    if !config.has_config() && matches!(provider.as_str(), "openai" | "openai-response") {
        config = extract_codex_usage_config(body);
        if !config.has_config() {
            config = parse_valid(body)
                .map(|v| extract_openai_config(&v))
                .unwrap_or_default();
        }
    }
    reasoning_effort_from_config(&config)
}

fn extract_thinking_config_for_usage(body: &[u8], provider: &str) -> ThinkingConfig {
    match provider.trim().to_lowercase().as_str() {
        "codex" | "xai" | "openai-response" => extract_codex_usage_config(body),
        _ => extract_thinking_config(body, provider),
    }
}

fn reasoning_effort_from_suffix(suffix: &SuffixResult) -> String {
    if !suffix.has_suffix {
        return String::new();
    }
    reasoning_effort_from_config(&parse_suffix_to_config(&suffix.raw_suffix))
}

fn reasoning_effort_from_config(config: &ThinkingConfig) -> String {
    if !config.has_config() {
        return String::new();
    }
    match config.mode {
        ThinkingMode::None => level::NONE.to_owned(),
        ThinkingMode::Auto => level::AUTO.to_owned(),
        ThinkingMode::Level => config.level.trim().to_lowercase(),
        ThinkingMode::Budget => convert_budget_to_level(config.budget)
            .unwrap_or_default()
            .to_owned(),
    }
}

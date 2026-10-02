//! Native Google Interactions thinking applier (Go: internal/thinking/provider/interactions).
//!
//! Writes `generation_config.thinking_level` and `generation_config.thinking_summaries`. Unlike
//! the other appliers it has no user-defined or no-thinking-support shortcut.

use cpa_json::{J, Kind, Value};

use super::super::convert::{convert_budget_to_level, eq_fold};
use super::super::json::parse_or_empty_object;
use super::super::types::{ProviderApplier, ThinkingConfig, ThinkingError, ThinkingMode, level};
use crate::registry::ModelInfo;

/// Applies thinking to Interactions request bodies via `generation_config`.
#[derive(Debug, Default, Clone, Copy)]
pub struct InteractionsApplier;

impl ProviderApplier for InteractionsApplier {
    fn apply(
        &self,
        body: &[u8],
        config: &ThinkingConfig,
        model_info: Option<&ModelInfo>,
    ) -> Result<Vec<u8>, ThinkingError> {
        let original = parse_or_empty_object(body);
        let result = strip_interactions_thinking_fields(&original);
        let out = match config.mode {
            ThinkingMode::Level => apply_level(result, &original, &config.level, model_info),
            ThinkingMode::Budget => apply_budget(result, &original, config.budget, model_info),
            ThinkingMode::Auto => set_thinking_summaries(result, &original),
            ThinkingMode::None => apply_none(result, &original, config, model_info),
        };
        Ok(cpa_json::to_vec(&out))
    }
}

fn apply_budget(
    result: Value,
    original: &Value,
    budget: i64,
    model_info: Option<&ModelInfo>,
) -> Value {
    match convert_budget_to_level(budget) {
        // Amount and summary visibility are independent. Interactions has no wire-level "none"
        // thinking level, so keep only explicit summary intent and let the model use its default.
        None | Some(level::NONE) | Some(level::AUTO) => set_thinking_summaries(result, original),
        Some(lvl) => apply_level(result, original, lvl, model_info),
    }
}

fn apply_level(
    mut result: Value,
    original: &Value,
    lvl: &str,
    model_info: Option<&ModelInfo>,
) -> Value {
    let lvl = normalize_interactions_level(lvl, model_info);
    if !lvl.is_empty() {
        cpa_json::set(&mut result, "generation_config.thinking_level", lvl);
    }
    set_thinking_summaries(result, original)
}

fn apply_none(
    result: Value,
    original: &Value,
    config: &ThinkingConfig,
    model_info: Option<&ModelInfo>,
) -> Value {
    if !config.level.is_empty() {
        return apply_level(result, original, &config.level, model_info);
    }
    if config.budget > 0 {
        return apply_budget(result, original, config.budget, model_info);
    }
    // Amount fully disabled: restoring thinking_summaries alone could make a default-on model
    // reason and return a summary despite the explicit none override.
    result
}

/// Removes every thinking-related key (snake/camel, `generation_config` and `generationConfig`).
fn strip_interactions_thinking_fields(body: &Value) -> Value {
    let mut result = body.clone();
    for path in [
        "generation_config.thinking_level",
        "generation_config.thinkingLevel",
        "generation_config.thinking_budget",
        "generation_config.thinkingBudget",
        "generation_config.thinking_summaries",
        "generation_config.thinkingSummaries",
        "generation_config.thinking_config",
        "generation_config.thinkingConfig",
        "generationConfig.thinkingLevel",
        "generationConfig.thinking_level",
        "generationConfig.thinkingBudget",
        "generationConfig.thinking_budget",
        "generationConfig.thinkingSummaries",
        "generationConfig.thinking_summaries",
        "generationConfig.thinkingConfig",
    ] {
        cpa_json::delete(&mut result, path);
    }
    result
}

/// Re-sets `generation_config.thinking_summaries` from the original body's explicit selector, else
/// derived from its `include_thoughts` boolean.
fn set_thinking_summaries(mut result: Value, original: &Value) -> Value {
    if let Some(value) = original_thinking_summaries(original) {
        cpa_json::set(&mut result, "generation_config.thinking_summaries", value);
        return result;
    }
    if let Some(include) = original_include_thoughts(original) {
        cpa_json::set(
            &mut result,
            "generation_config.thinking_summaries",
            if include { "auto" } else { "none" },
        );
    }
    result
}

fn original_thinking_summaries(body: &Value) -> Option<&'static str> {
    for path in [
        "generation_config.thinking_summaries",
        "generation_config.thinkingSummaries",
    ] {
        let Some(value) = body.g(path).as_str().map(|s| s.trim().to_lowercase()) else {
            continue;
        };
        match value.as_str() {
            "auto" => return Some("auto"),
            "none" => return Some("none"),
            _ => {}
        }
    }
    None
}

fn original_include_thoughts(body: &Value) -> Option<bool> {
    for path in [
        "generation_config.thinking_config.include_thoughts",
        "generation_config.thinking_config.includeThoughts",
        "generation_config.thinkingConfig.include_thoughts",
        "generation_config.thinkingConfig.includeThoughts",
    ] {
        match body.g(path).kind() {
            Kind::True => return Some(true),
            Kind::False => return Some(false),
            _ => {}
        }
    }
    None
}

/// Normalizes a level to the model's own spelling (case-insensitive), falling back to the model's
/// last level; without model levels `max`/`xhigh` become `high`. `none`/`auto` yield "".
fn normalize_interactions_level(lvl: &str, model_info: Option<&ModelInfo>) -> String {
    let lvl = lvl.trim().to_lowercase();
    if lvl.is_empty() || lvl == level::NONE || lvl == level::AUTO {
        return String::new();
    }
    if let Some(support) = model_info.and_then(|m| m.thinking.as_ref())
        && let Some(last) = support.levels.last()
    {
        let chosen = support
            .levels
            .iter()
            .find(|c| eq_fold(c, &lvl))
            .unwrap_or(last);
        return chosen.to_lowercase();
    }
    match lvl.as_str() {
        level::MAX | level::XHIGH => level::HIGH.to_owned(),
        _ => lvl,
    }
}

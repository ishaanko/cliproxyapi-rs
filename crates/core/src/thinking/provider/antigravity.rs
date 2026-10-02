//! Antigravity thinking applier (Go: internal/thinking/provider/antigravity).
//!
//! Same wire shape as Gemini under `request.generationConfig.thinkingConfig`, plus extra
//! normalization for Claude models served through Antigravity: thinking budget < max tokens, and
//! the whole `thinkingConfig` dropped when the budget falls below the model minimum.

use cpa_json::{J, Value};

use super::super::apply::is_user_defined_model;
use super::super::json::{parse_or_empty_object, set, to_bytes};
use super::super::types::{ProviderApplier, ThinkingConfig, ThinkingError, ThinkingMode};
use super::restore_include_thoughts;
use crate::registry::ModelInfo;

const PREFIX: &str = "request.generationConfig.thinkingConfig";

/// Sentinel budget meaning "thinkingConfig was removed entirely".
const BUDGET_REMOVED: i64 = -2;

/// Applies thinking to Antigravity request bodies.
#[derive(Debug, Default, Clone, Copy)]
pub struct AntigravityApplier;

impl ProviderApplier for AntigravityApplier {
    fn apply(
        &self,
        body: &[u8],
        config: &ThinkingConfig,
        model_info: Option<&ModelInfo>,
    ) -> Result<Vec<u8>, ThinkingError> {
        if is_user_defined_model(model_info) {
            return Ok(apply_compatible(body, config, model_info));
        }
        let Some(info) = model_info else {
            return Ok(body.to_vec());
        };
        let Some(support) = &info.thinking else {
            return Ok(body.to_vec());
        };

        let v = parse_or_empty_object(body);
        let is_claude = info.id.to_lowercase().contains("claude");

        // Auto and Budget always use the budget format.
        if matches!(config.mode, ThinkingMode::Auto | ThinkingMode::Budget) {
            return Ok(apply_budget_format(&v, config, model_info, is_claude));
        }
        // Level and None pick by model capability.
        if !support.levels.is_empty() {
            return Ok(apply_level_format(&v, config));
        }
        Ok(apply_budget_format(&v, config, model_info, is_claude))
    }
}

/// User-defined models: same split as the Gemini compat path.
fn apply_compatible(
    body: &[u8],
    config: &ThinkingConfig,
    model_info: Option<&ModelInfo>,
) -> Vec<u8> {
    let v = parse_or_empty_object(body);
    let is_claude = model_info.is_some_and(|m| m.id.to_lowercase().contains("claude"));

    match config.mode {
        ThinkingMode::Auto => apply_budget_format(&v, config, model_info, is_claude),
        ThinkingMode::Level => apply_level_format(&v, config),
        ThinkingMode::None if !config.level.is_empty() => apply_level_format(&v, config),
        _ => apply_budget_format(&v, config, model_info, is_claude),
    }
}

/// Writes `thinkingLevel`; None with no budget or level removes the whole `thinkingConfig`.
fn apply_level_format(original: &Value, config: &ThinkingConfig) -> Vec<u8> {
    let mut result = original.clone();
    for key in [
        "thinkingBudget",
        "thinking_budget",
        "thinking_level",
        "includeThoughts",
        "include_thoughts",
    ] {
        cpa_json::delete(&mut result, &format!("{PREFIX}.{key}"));
    }

    if config.mode == ThinkingMode::None {
        if config.budget == 0 && config.level.is_empty() {
            // Amount fully disabled: visibility is irrelevant and restoring includeThoughts alone
            // would recreate thinkingConfig and let a default-on model think again.
            cpa_json::delete(&mut result, PREFIX);
            return to_bytes(&result);
        }
        if !config.level.is_empty() {
            set(
                &mut result,
                &format!("{PREFIX}.thinkingLevel"),
                config.level.as_str(),
            );
        }
        restore_include_thoughts(&mut result, original, PREFIX);
        return to_bytes(&result);
    }

    set(
        &mut result,
        &format!("{PREFIX}.thinkingLevel"),
        config.level.as_str(),
    );
    restore_include_thoughts(&mut result, original, PREFIX);
    to_bytes(&result)
}

fn apply_budget_format(
    original: &Value,
    config: &ThinkingConfig,
    model_info: Option<&ModelInfo>,
    is_claude: bool,
) -> Vec<u8> {
    let mut result = original.clone();
    for key in [
        "thinkingLevel",
        "thinking_level",
        "thinking_budget",
        "includeThoughts",
        "include_thoughts",
    ] {
        cpa_json::delete(&mut result, &format!("{PREFIX}.{key}"));
    }

    let mut budget = config.budget;
    // Claude-specific constraints first, to get the final budget value.
    if is_claude && let Some(info) = model_info {
        budget = normalize_claude_budget(budget, &mut result, info);
        // The amount was removed entirely; summary visibility is independent, so keep an explicit
        // includeThoughts control if present.
        if budget == BUDGET_REMOVED {
            restore_include_thoughts(&mut result, original, PREFIX);
            return to_bytes(&result);
        }
    }

    set(&mut result, &format!("{PREFIX}.thinkingBudget"), budget);
    restore_include_thoughts(&mut result, original, PREFIX);
    to_bytes(&result)
}

/// Claude constraints on the budget: below max tokens (budget >= max becomes max-1), and the whole
/// `thinkingConfig` removed (returns [`BUDGET_REMOVED`]) when it falls under the model minimum. A
/// model-default max is written back as `maxOutputTokens`.
fn normalize_claude_budget(mut budget: i64, payload: &mut Value, info: &ModelInfo) -> i64 {
    let (effective_max, set_default_max) = effective_max_tokens(payload, info);
    if effective_max > 0 && budget >= effective_max {
        budget = effective_max - 1;
    }

    let min_budget = info.thinking.as_ref().map_or(0, |t| t.min);
    if min_budget > 0 && budget >= 0 && budget < min_budget {
        cpa_json::delete(payload, PREFIX);
        return BUDGET_REMOVED;
    }

    if set_default_max && effective_max > 0 {
        set(
            payload,
            "request.generationConfig.maxOutputTokens",
            effective_max,
        );
    }
    budget
}

/// Request `maxOutputTokens` when positive, else the model default (second value true = from
/// model, so it should be written back).
fn effective_max_tokens(payload: &Value, info: &ModelInfo) -> (i64, bool) {
    let max_tok = payload.g("request.generationConfig.maxOutputTokens");
    if max_tok.exists() && max_tok.int() > 0 {
        return (max_tok.int(), false);
    }
    if info.max_completion_tokens > 0 {
        return (info.max_completion_tokens, true);
    }
    (0, false)
}

//! Gemini thinking applier (Go: internal/thinking/provider/gemini).
//!
//! Gemini 2.5 uses `thinkingBudget` (numeric); Gemini 3.x uses `thinkingLevel`
//! (minimal/low/medium/high). The output format follows the mode and the model's `levels`:
//! Auto always writes `thinkingBudget=-1`, models with levels use `thinkingLevel`, the rest use
//! `thinkingBudget`.

use cpa_json::Value;

use super::super::apply::is_user_defined_model;
use super::super::json::{parse_or_empty_object, set, to_bytes};
use super::super::types::{ProviderApplier, ThinkingConfig, ThinkingError, ThinkingMode};
use super::restore_include_thoughts;
use crate::registry::ModelInfo;

const PREFIX: &str = "generationConfig.thinkingConfig";

/// Applies thinking to Gemini request bodies under `generationConfig.thinkingConfig`.
#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiApplier;

impl ProviderApplier for GeminiApplier {
    fn apply(
        &self,
        body: &[u8],
        config: &ThinkingConfig,
        model_info: Option<&ModelInfo>,
    ) -> Result<Vec<u8>, ThinkingError> {
        if is_user_defined_model(model_info) {
            return Ok(apply_compatible(body, config));
        }
        let Some(support) = model_info.and_then(|m| m.thinking.as_ref()) else {
            return Ok(body.to_vec());
        };

        let v = parse_or_empty_object(body);
        // Level uses the level format; None picks by model capability; Budget/Auto use budgets.
        Ok(match config.mode {
            ThinkingMode::Level => apply_level_format(&v, config),
            ThinkingMode::None if !support.levels.is_empty() => apply_level_format(&v, config),
            _ => apply_budget_format(&v, config),
        })
    }
}

/// User-defined models: Auto and budgets use the budget format, levels (and None carrying a level)
/// the level format.
fn apply_compatible(body: &[u8], config: &ThinkingConfig) -> Vec<u8> {
    let v = parse_or_empty_object(body);
    match config.mode {
        ThinkingMode::Auto => apply_budget_format(&v, config),
        ThinkingMode::Level => apply_level_format(&v, config),
        ThinkingMode::None if !config.level.is_empty() => apply_level_format(&v, config),
        _ => apply_budget_format(&v, config),
    }
}

/// Writes `thinkingLevel`. None with no budget or level removes the whole `thinkingConfig`: with
/// the amount fully disabled, visibility is irrelevant, and restoring `includeThoughts` alone would
/// recreate the config and let a default-on model think again.
fn apply_level_format(original: &Value, config: &ThinkingConfig) -> Vec<u8> {
    let mut result = original.clone();
    // Remove conflicting fields so thinkingLevel and thinkingBudget never coexist, and normalize
    // the includeThoughts field name (restored below from the original body).
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

    // Only Level reaches here; budget conversion is the caller's job.
    set(
        &mut result,
        &format!("{PREFIX}.thinkingLevel"),
        config.level.as_str(),
    );
    restore_include_thoughts(&mut result, original, PREFIX);
    to_bytes(&result)
}

fn apply_budget_format(original: &Value, config: &ThinkingConfig) -> Vec<u8> {
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
    set(
        &mut result,
        &format!("{PREFIX}.thinkingBudget"),
        config.budget,
    );
    restore_include_thoughts(&mut result, original, PREFIX);
    to_bytes(&result)
}

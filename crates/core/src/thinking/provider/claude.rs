//! Claude thinking applier (Go: internal/thinking/provider/claude).
//!
//! Two control styles: manual (`thinking.type="enabled"` + `budget_tokens`) and adaptive
//! (`thinking.type="adaptive"` + `output_config.effort`, models that advertise levels).

use cpa_json::{J, Value};

use super::super::apply::is_user_defined_model;
use super::super::convert::convert_level_to_budget;
use super::super::json::{
    body_or_empty_object, delete_if_empty_object, parse_or_empty_object, set, to_bytes,
};
use super::super::types::{ProviderApplier, ThinkingConfig, ThinkingError, ThinkingMode};
use crate::registry::ModelInfo;

/// Applies thinking to Claude request bodies. Expects a config pre-validated by `validate_config`
/// (mode conversion, budget clamping, ZeroAllowed).
///
/// Output when enabled: `{"thinking":{"type":"enabled","budget_tokens":N}}`; adaptive:
/// `{"thinking":{"type":"adaptive"},"output_config":{"effort":"high"}}`; disabled:
/// `{"thinking":{"type":"disabled"}}`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeApplier;

impl ProviderApplier for ClaudeApplier {
    fn apply(
        &self,
        body: &[u8],
        config: &ThinkingConfig,
        model_info: Option<&ModelInfo>,
    ) -> Result<Vec<u8>, ThinkingError> {
        if is_user_defined_model(model_info) {
            return Ok(apply_compatible_claude(body, config));
        }
        let Some(info) = model_info else {
            return Ok(body.to_vec());
        };
        let Some(support) = &info.thinking else {
            return Ok(body.to_vec());
        };

        let mut v = parse_or_empty_object(body);
        let supports_adaptive = !support.levels.is_empty();

        match config.mode {
            ThinkingMode::None => {
                set_disabled(&mut v, true);
                Ok(to_bytes(&v))
            }
            ThinkingMode::Level => {
                // Adaptive effort is only valid when the model advertises discrete levels.
                if supports_adaptive && !config.level.is_empty() {
                    set_adaptive(&mut v, Some(&config.level));
                    return Ok(to_bytes(&v));
                }
                // Non-adaptive Claude models: convert the level to budget_tokens.
                match convert_level_to_budget(&config.level) {
                    Some(budget) => Ok(apply_budget(v, budget, info)),
                    None => Ok(body_or_empty_object(body)),
                }
            }
            ThinkingMode::Budget => Ok(apply_budget(v, config.budget, info)),
            ThinkingMode::Auto => {
                if supports_adaptive {
                    // Adaptive without explicit effort lets upstream pick its default.
                    set_adaptive(&mut v, None);
                } else {
                    // Legacy fallback: enable thinking without budget_tokens.
                    set(&mut v, "thinking.type", "enabled");
                    cpa_json::delete(&mut v, "thinking.budget_tokens");
                    clear_effort(&mut v);
                }
                Ok(to_bytes(&v))
            }
        }
    }
}

/// Budget mode: the budget was pre-validated, 0 means disabled.
fn apply_budget(mut v: Value, budget: i64, info: &ModelInfo) -> Vec<u8> {
    if budget == 0 {
        set_disabled(&mut v, false);
        return to_bytes(&v);
    }
    set(&mut v, "thinking.type", "enabled");
    set(&mut v, "thinking.budget_tokens", budget);
    clear_effort(&mut v);
    // Anthropic requires max_tokens > budget_tokens.
    normalize_claude_budget(&mut v, budget, info);
    to_bytes(&v)
}

/// `thinking.type="disabled"` without budget or effort. `drop_display` also removes
/// `thinking.display`, which only applies to an active thinking block.
fn set_disabled(v: &mut Value, drop_display: bool) {
    set(v, "thinking.type", "disabled");
    cpa_json::delete(v, "thinking.budget_tokens");
    if drop_display {
        cpa_json::delete(v, "thinking.display");
    }
    clear_effort(v);
}

/// `thinking.type="adaptive"`, without budget; `effort` sets `output_config.effort`, `None`
/// removes it (and an emptied `output_config`).
fn set_adaptive(v: &mut Value, effort: Option<&str>) {
    set(v, "thinking.type", "adaptive");
    cpa_json::delete(v, "thinking.budget_tokens");
    match effort {
        Some(effort) => set(v, "output_config.effort", effort),
        None => clear_effort(v),
    }
}

/// Deletes `output_config.effort` and an emptied `output_config`.
fn clear_effort(v: &mut Value) {
    cpa_json::delete(v, "output_config.effort");
    delete_if_empty_object(v, "output_config");
}

/// Ensures max_tokens > budget_tokens: the effective max is the request's `max_tokens`, else the
/// model default (written back into the request). A budget >= max becomes max-1, and if that falls
/// below the model minimum the budget is left unchanged.
fn normalize_claude_budget(v: &mut Value, budget_tokens: i64, info: &ModelInfo) {
    if budget_tokens <= 0 {
        return;
    }

    let (effective_max, set_default_max) = effective_max_tokens(v, info);
    if set_default_max && effective_max > 0 {
        set(v, "max_tokens", effective_max);
    }

    let mut adjusted = budget_tokens;
    if effective_max > 0 && adjusted >= effective_max {
        adjusted = effective_max - 1;
    }

    let min_budget = info.thinking.as_ref().map_or(0, |t| t.min);
    if min_budget > 0 && adjusted > 0 && adjusted < min_budget {
        return;
    }

    if adjusted != budget_tokens {
        set(v, "thinking.budget_tokens", adjusted);
    }
}

/// Request `max_tokens` when positive, else the model default (second value true = from model).
fn effective_max_tokens(v: &Value, info: &ModelInfo) -> (i64, bool) {
    let max_tok = v.g("max_tokens");
    if max_tok.exists() && max_tok.int() > 0 {
        return (max_tok.int(), false);
    }
    if info.max_completion_tokens > 0 {
        return (info.max_completion_tokens, true);
    }
    (0, false)
}

/// User-defined models: no model capabilities, so Level always means adaptive effort and Budget
/// is written as-is; upstream validates.
fn apply_compatible_claude(body: &[u8], config: &ThinkingConfig) -> Vec<u8> {
    let mut v = parse_or_empty_object(body);
    match config.mode {
        ThinkingMode::None => set_disabled(&mut v, true),
        ThinkingMode::Auto => {
            set(&mut v, "thinking.type", "enabled");
            cpa_json::delete(&mut v, "thinking.budget_tokens");
            clear_effort(&mut v);
        }
        ThinkingMode::Level => {
            if config.level.is_empty() {
                return body_or_empty_object(body);
            }
            set_adaptive(&mut v, Some(&config.level));
        }
        ThinkingMode::Budget => {
            set(&mut v, "thinking.type", "enabled");
            set(&mut v, "thinking.budget_tokens", config.budget);
            clear_effort(&mut v);
        }
    }
    to_bytes(&v)
}

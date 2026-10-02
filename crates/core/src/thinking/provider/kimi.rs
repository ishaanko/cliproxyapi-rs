//! Kimi (Moonshot) thinking applier (Go: internal/thinking/provider/kimi).
//!
//! Kimi uses a native `thinking` object for enabled and disabled thinking. Top-level
//! `reasoning_effort` is only a legacy input of the extraction layer and is removed from the
//! final payload.

use super::super::apply::is_user_defined_model;
use super::super::convert::convert_budget_to_level;
use super::super::json::{body_or_empty_object, parse_or_empty_object, to_bytes, try_set};
use super::super::types::{
    ErrorCode, ProviderApplier, ThinkingConfig, ThinkingError, ThinkingMode, level,
};
use crate::registry::ModelInfo;

/// Applies thinking to Kimi request bodies: enabled thinking is `thinking.type="enabled"` plus
/// `thinking.effort`, disabled is `thinking.type="disabled"`. Registered under `kimi`, `kimi-ai`,
/// `kimi.ai` and `kimi.com`.
#[derive(Debug, Default, Clone, Copy)]
pub struct KimiApplier;

impl ProviderApplier for KimiApplier {
    fn apply(
        &self,
        body: &[u8],
        config: &ThinkingConfig,
        model_info: Option<&ModelInfo>,
    ) -> Result<Vec<u8>, ThinkingError> {
        if is_user_defined_model(model_info) {
            return apply_compatible_kimi(body, config);
        }
        if model_info.is_none_or(|m| m.thinking.is_none()) {
            return Ok(body.to_vec());
        }

        let effort: String = match config.mode {
            ThinkingMode::Level => {
                if config.level.is_empty() {
                    return Ok(body_or_empty_object(body));
                }
                config.level.clone()
            }
            // Respect the clamped fallback level for models that cannot disable thinking; Kimi
            // otherwise requires an explicit disabled thinking object.
            ThinkingMode::None => {
                if !config.level.is_empty() && config.level != level::NONE {
                    config.level.clone()
                } else {
                    return apply_disabled_thinking(body);
                }
            }
            ThinkingMode::Budget => match convert_budget_to_level(config.budget) {
                Some(l) => l.to_owned(),
                None => return Ok(body_or_empty_object(body)),
            },
            ThinkingMode::Auto => level::AUTO.to_owned(),
        };
        apply_enabled_thinking(body, &effort)
    }
}

/// User-defined Kimi models.
fn apply_compatible_kimi(body: &[u8], config: &ThinkingConfig) -> Result<Vec<u8>, ThinkingError> {
    let effort: String = match config.mode {
        ThinkingMode::Level => {
            if config.level.is_empty() {
                return Ok(body_or_empty_object(body));
            }
            config.level.clone()
        }
        ThinkingMode::None => {
            if config.level.is_empty() || config.level == level::NONE {
                return apply_disabled_thinking(body);
            }
            config.level.clone()
        }
        ThinkingMode::Auto => level::AUTO.to_owned(),
        ThinkingMode::Budget => match convert_budget_to_level(config.budget) {
            Some(l) => l.to_owned(),
            None => return Ok(body_or_empty_object(body)),
        },
    };
    apply_enabled_thinking(body, &effort)
}

/// The Go applier surfaces sjson failures (a named key set into an array) as plain errors.
fn set_failed(field: &str, key: String) -> ThinkingError {
    ThinkingError::new(
        ErrorCode::ApplyFailed,
        format!(
            "kimi thinking: failed to set {field}: cannot set array element for non-numeric key '{key}'"
        ),
    )
}

fn apply_enabled_thinking(body: &[u8], effort: &str) -> Result<Vec<u8>, ThinkingError> {
    let mut v = parse_or_empty_object(body);
    cpa_json::delete(&mut v, "reasoning_effort");
    try_set(&mut v, "thinking.type", "enabled").map_err(|k| set_failed("thinking.type", k))?;
    try_set(&mut v, "thinking.effort", effort).map_err(|k| set_failed("thinking.effort", k))?;
    Ok(to_bytes(&v))
}

/// Replaces the `thinking` object with `{"type":"disabled"}` and drops the legacy effort.
fn apply_disabled_thinking(body: &[u8]) -> Result<Vec<u8>, ThinkingError> {
    let mut v = parse_or_empty_object(body);
    cpa_json::delete(&mut v, "thinking");
    cpa_json::delete(&mut v, "reasoning_effort");
    try_set(&mut v, "thinking.type", "disabled").map_err(|k| set_failed("thinking.type", k))?;
    Ok(to_bytes(&v))
}

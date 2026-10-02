//! OpenAI Chat thinking applier (Go: internal/thinking/provider/openai).
//!
//! Level-only: writes `reasoning_effort` (low/medium/high/xhigh, some models also `none`). The
//! Responses-style `reasoning.effort` writer is [`CodexApplier`](super::CodexApplier).

use super::super::apply::is_user_defined_model;
use super::super::convert::{convert_budget_to_level, has_level};
use super::super::json::{body_or_empty_object, parse_or_empty_object};
use super::super::types::{ProviderApplier, ThinkingConfig, ThinkingError, ThinkingMode, level};
use crate::registry::ModelInfo;

/// Applies thinking to OpenAI Chat request bodies as `reasoning_effort`.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAIApplier;

impl ProviderApplier for OpenAIApplier {
    fn apply(
        &self,
        body: &[u8],
        config: &ThinkingConfig,
        model_info: Option<&ModelInfo>,
    ) -> Result<Vec<u8>, ThinkingError> {
        apply_effort(body, config, model_info, "reasoning_effort")
    }
}

/// Shared by the OpenAI (`reasoning_effort`) and Codex/xAI (`reasoning.effort`) appliers, which
/// differ only in the written path.
pub(super) fn apply_effort(
    body: &[u8],
    config: &ThinkingConfig,
    model_info: Option<&ModelInfo>,
    path: &str,
) -> Result<Vec<u8>, ThinkingError> {
    if is_user_defined_model(model_info) {
        return Ok(apply_compatible_effort(body, config, path));
    }
    let Some(support) = model_info.and_then(|m| m.thinking.as_ref()) else {
        return Ok(body.to_vec());
    };

    // Only Level and None are handled; other modes pass through unchanged.
    if !matches!(config.mode, ThinkingMode::Level | ThinkingMode::None) {
        return Ok(body.to_vec());
    }

    let mut v = parse_or_empty_object(body);
    if config.mode == ThinkingMode::Level {
        cpa_json::set(&mut v, path, config.level.as_str());
        return Ok(cpa_json::to_vec(&v));
    }

    let mut effort = "";
    if config.budget == 0 && (support.zero_allowed || has_level(&support.levels, level::NONE)) {
        effort = level::NONE;
    }
    if effort.is_empty() && !config.level.is_empty() {
        effort = &config.level;
    }
    if effort.is_empty() && !support.levels.is_empty() {
        effort = &support.levels[0];
    }
    if effort.is_empty() {
        return Ok(body_or_empty_object(body));
    }

    cpa_json::set(&mut v, path, effort);
    Ok(cpa_json::to_vec(&v))
}

/// User-defined models: Level as given, None as `none` (or the carried level), Auto as `auto`,
/// Budget through the threshold mapping.
fn apply_compatible_effort(body: &[u8], config: &ThinkingConfig, path: &str) -> Vec<u8> {
    let effort: String = match config.mode {
        ThinkingMode::Level => {
            if config.level.is_empty() {
                return body_or_empty_object(body);
            }
            config.level.clone()
        }
        ThinkingMode::None => {
            if config.level.is_empty() {
                level::NONE.to_owned()
            } else {
                config.level.clone()
            }
        }
        ThinkingMode::Auto => level::AUTO.to_owned(),
        ThinkingMode::Budget => match convert_budget_to_level(config.budget) {
            Some(l) => l.to_owned(),
            None => return body_or_empty_object(body),
        },
    };

    let mut v = parse_or_empty_object(body);
    cpa_json::set(&mut v, path, effort);
    cpa_json::to_vec(&v)
}

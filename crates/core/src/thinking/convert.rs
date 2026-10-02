//! Level/budget conversions and model capability detection (Go: internal/thinking/convert.go).

use super::types::level;
use crate::registry::ModelInfo;

/// Go `strings.EqualFold` for level names.
pub(crate) fn eq_fold(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// Converts a level to a budget (case-insensitive): none 0, auto -1, minimal 512, low 1024,
/// medium 8192, high 24576, xhigh 32768, max 128000 (large; per-model clamping narrows it for
/// budget-only providers).
pub fn convert_level_to_budget(lvl: &str) -> Option<i64> {
    match lvl.to_lowercase().as_str() {
        "none" => Some(0),
        "auto" => Some(-1),
        "minimal" => Some(512),
        "low" => Some(1024),
        "medium" => Some(8192),
        "high" => Some(24576),
        "xhigh" => Some(32768),
        "max" => Some(128000),
        _ => None,
    }
}

/// Upper bound of the `minimal` level (1-512).
pub const THRESHOLD_MINIMAL: i64 = 512;
/// Upper bound of the `low` level (513-1024).
pub const THRESHOLD_LOW: i64 = 1024;
/// Upper bound of the `medium` level (1025-8192).
pub const THRESHOLD_MEDIUM: i64 = 8192;
/// Upper bound of the `high` level (8193-24576).
pub const THRESHOLD_HIGH: i64 = 24576;

/// Converts a budget to the nearest level: -1 auto, 0 none, then threshold ranges; `None` for
/// invalid negatives (< -1).
pub fn convert_budget_to_level(budget: i64) -> Option<&'static str> {
    match budget {
        b if b < -1 => None,
        -1 => Some(level::AUTO),
        0 => Some(level::NONE),
        b if b <= THRESHOLD_MINIMAL => Some(level::MINIMAL),
        b if b <= THRESHOLD_LOW => Some(level::LOW),
        b if b <= THRESHOLD_MEDIUM => Some(level::MEDIUM),
        b if b <= THRESHOLD_HIGH => Some(level::HIGH),
        _ => Some(level::XHIGH),
    }
}

/// Whether `target` is in `levels` (case-insensitive, entries trimmed).
pub fn has_level(levels: &[String], target: &str) -> bool {
    levels.iter().any(|l| eq_fold(l.trim(), target))
}

/// Maps a generic level to a Claude adaptive effort (low/medium/high/max). `supports_max` says
/// whether the target model accepts `max`. `None` for empty or unknown levels.
pub fn map_to_claude_effort(lvl: &str, supports_max: bool) -> Option<&'static str> {
    match lvl.trim().to_lowercase().as_str() {
        "minimal" | "low" => Some(level::LOW),
        "medium" => Some(level::MEDIUM),
        "high" => Some(level::HIGH),
        "xhigh" | "max" => Some(if supports_max {
            level::MAX
        } else {
            level::HIGH
        }),
        "auto" => Some(level::HIGH),
        _ => None,
    }
}

/// Thinking format support of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCapability {
    /// No model info (passthrough behavior, internal use).
    Unknown,
    /// Model does not support thinking.
    None,
    /// Numeric budgets only (Claude, Gemini 2.5).
    BudgetOnly,
    /// Discrete levels only (OpenAI, Codex, Kimi).
    LevelOnly,
    /// Both budgets and levels (Gemini 3).
    Hybrid,
}

/// Classifies a model by its `ThinkingSupport`.
pub fn detect_model_capability(model_info: Option<&ModelInfo>) -> ModelCapability {
    let Some(info) = model_info else {
        return ModelCapability::Unknown;
    };
    let Some(support) = &info.thinking else {
        return ModelCapability::None;
    };
    let has_budget = support.min > 0 || support.max > 0;
    let has_levels = !support.levels.is_empty();
    match (has_budget, has_levels) {
        (true, true) => ModelCapability::Hybrid,
        (true, false) => ModelCapability::BudgetOnly,
        (false, true) => ModelCapability::LevelOnly,
        (false, false) => ModelCapability::None,
    }
}

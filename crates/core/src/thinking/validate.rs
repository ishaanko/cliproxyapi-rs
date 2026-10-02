//! Validation and clamping of a canonical config against model capabilities
//! (Go: internal/thinking/validate.go).

use tracing::{debug, warn};

use super::convert::{
    ModelCapability, convert_budget_to_level, convert_level_to_budget, detect_model_capability,
    eq_fold,
};
use super::types::{ErrorCode, ThinkingConfig, ThinkingError, ThinkingMode, level};
use crate::registry::{ModelInfo, ThinkingSupport};

/// Canonical ordering of levels, lowest to highest.
const STANDARD_LEVEL_ORDER: [&str; 6] = [
    level::MINIMAL,
    level::LOW,
    level::MEDIUM,
    level::HIGH,
    level::XHIGH,
    level::MAX,
];

/// Go `%q` for the (lowercased) level names that end up in error messages.
fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Validates and normalizes `config` against `model_info`:
/// - models without thinking support only accept `None`;
/// - budget-only models convert levels to budgets, level-only models convert budgets to (clamped)
///   levels, hybrid models keep the original form;
/// - levels must be in the model's list (cross-family or model-family-mismatch paths clamp
///   instead of failing);
/// - budgets are range-checked when the config came from the request body in the same provider
///   family, otherwise clamped (suffix configs always clamp).
pub fn validate_config(
    config: &ThinkingConfig,
    model_info: Option<&ModelInfo>,
    from_format: &str,
    to_format: &str,
    from_suffix: bool,
) -> Result<ThinkingConfig, ThinkingError> {
    let mut config = config.clone();
    let from_format = from_format.trim().to_lowercase();
    let to_format = to_format.trim().to_lowercase();
    let from_format = from_format.as_str();
    let to_format = to_format.as_str();

    let mut model = "unknown";
    let mut support: Option<&ThinkingSupport> = None;
    if let Some(info) = model_info {
        if !info.id.is_empty() {
            model = info.id.as_str();
        }
        support = info.thinking.as_ref();
    }

    let Some(support) = support else {
        if config.mode != ThinkingMode::None {
            return Err(ThinkingError::with_model(
                ErrorCode::ThinkingNotSupported,
                "thinking not supported for this model",
                model,
            ));
        }
        return Ok(config);
    };

    // Cross-family conversions (openai->gemini, claude->gemini, ...) onto a level-capable model
    // clamp unsupported levels instead of failing; same-family conversions validate strictly.
    // `model_family_mismatch` covers providers that reuse another protocol on the wire (Kimi
    // serving Claude-compatible /v1/messages): from/to look like "claude" but the model is not.
    let capability = detect_model_capability(model_info);
    let to_has_level_support = matches!(
        capability,
        ModelCapability::LevelOnly | ModelCapability::Hybrid
    );
    let mut model_family_mismatch = false;
    if let Some(info) = model_info {
        let model_type = info.r#type.trim().to_lowercase();
        if !model_type.is_empty()
            && ((!from_format.is_empty() && !is_same_provider_family(from_format, &model_type))
                || (!to_format.is_empty() && !is_same_provider_family(to_format, &model_type)))
        {
            model_family_mismatch = true;
        }
    }
    let allow_clamp_unsupported = to_has_level_support
        && (!is_same_provider_family(from_format, to_format) || model_family_mismatch);

    // Strict budget range checks apply only to body configs, with a known source in the same
    // provider family. Cross-family or suffix configs clamp to improve interoperability.
    let strict_budget = !from_suffix
        && !from_format.is_empty()
        && is_same_provider_family(from_format, to_format)
        && !model_family_mismatch;
    let mut budget_derived_from_level = false;

    match capability {
        ModelCapability::BudgetOnly => {
            if config.mode == ThinkingMode::Level && config.level != level::AUTO {
                let Some(budget) = convert_level_to_budget(&config.level) else {
                    return Err(ThinkingError::new(
                        ErrorCode::UnknownLevel,
                        format!("unknown level: {}", config.level),
                    ));
                };
                config.mode = ThinkingMode::Budget;
                config.budget = budget;
                config.level.clear();
                budget_derived_from_level = true;
            }
        }
        ModelCapability::LevelOnly => {
            if config.mode == ThinkingMode::Budget {
                let Some(lvl) = convert_budget_to_level(config.budget) else {
                    return Err(ThinkingError::new(
                        ErrorCode::UnknownLevel,
                        format!(
                            "budget {} cannot be converted to a valid level",
                            config.budget
                        ),
                    ));
                };
                // Clamp the derived standard level to the nearest supported one; none/auto are
                // preserved.
                config.mode = ThinkingMode::Level;
                config.level = clamp_level(lvl, model_info, to_format);
                config.budget = 0;
            }
        }
        ModelCapability::Hybrid | ModelCapability::None | ModelCapability::Unknown => {}
    }

    if config.mode == ThinkingMode::Level && config.level == level::NONE {
        config.mode = ThinkingMode::None;
        config.budget = 0;
        config.level.clear();
    }
    if config.mode == ThinkingMode::Level && config.level == level::AUTO {
        config.mode = ThinkingMode::Auto;
        config.budget = -1;
        config.level.clear();
    }
    if config.mode == ThinkingMode::Budget && config.budget == 0 {
        config.mode = ThinkingMode::None;
        config.level.clear();
    }

    if !support.levels.is_empty()
        && config.mode == ThinkingMode::Level
        && !is_level_supported(&config.level, &support.levels)
    {
        if allow_clamp_unsupported {
            config.level = clamp_level(&config.level, model_info, to_format);
        }
        if !is_level_supported(&config.level, &support.levels) {
            // The user explicitly asked for an unsupported level.
            let valid_levels = normalize_levels(&support.levels);
            let message = format!(
                "level {} not supported, valid levels: {}",
                go_quote(&config.level.to_lowercase()),
                valid_levels.join(", ")
            );
            return Err(ThinkingError::new(ErrorCode::LevelNotSupported, message));
        }
    }

    if strict_budget && config.mode == ThinkingMode::Budget && !budget_derived_from_level {
        let (min, max) = (support.min, support.max);
        if (min != 0 || max != 0)
            && (config.budget < min
                || config.budget > max
                || (config.budget == 0 && !support.zero_allowed))
        {
            return Err(ThinkingError::new(
                ErrorCode::BudgetOutOfRange,
                format!("budget {} out of range [{},{}]", config.budget, min, max),
            ));
        }
    }

    // Auto becomes a mid-range value when the model has no dynamic thinking.
    if config.mode == ThinkingMode::Auto && !support.dynamic_allowed {
        config = convert_auto_to_mid_range(config, support, to_format, model);
        // The canonical mid-range level may be missing from a discrete subset (Levels=[low,
        // high]); clamp it like a budget-derived level so providers never see an unsupported value.
        if config.mode == ThinkingMode::Level
            && !support.levels.is_empty()
            && !is_level_supported(&config.level, &support.levels)
        {
            config.level = clamp_level(&config.level, model_info, to_format);
        }
    }

    if config.mode == ThinkingMode::None && to_format == "claude" {
        // Claude disables via thinking.type="disabled"; keep Budget=0 so the applier omits
        // budget_tokens.
        config.budget = 0;
        config.level.clear();
    } else {
        if matches!(
            config.mode,
            ThinkingMode::Budget | ThinkingMode::Auto | ThinkingMode::None
        ) {
            config.budget = clamp_budget(config.budget, model_info, to_format);
        }

        // None on a model that cannot be disabled falls back to its lowest level. Budget-capable
        // models reach this with Budget > 0; level-only models need the capability flags because
        // their Min/Max range is zero.
        let cannot_disable_level_model =
            !support.zero_allowed && !is_level_supported(level::NONE, &support.levels);
        if config.mode == ThinkingMode::None
            && !support.levels.is_empty()
            && (config.budget > 0 || cannot_disable_level_model)
        {
            config.level = support.levels[0].clone();
        }
    }

    Ok(config)
}

/// Converts Auto to a fixed value when the model has no dynamic thinking: level-only models use
/// `medium`; budget models use `(min+max)/2` (None if that is <= 0 and zero is allowed, else Min).
fn convert_auto_to_mid_range(
    mut config: ThinkingConfig,
    support: &ThinkingSupport,
    provider: &str,
    model: &str,
) -> ThinkingConfig {
    if !support.levels.is_empty() && support.min == 0 && support.max == 0 {
        config.mode = ThinkingMode::Level;
        config.level = level::MEDIUM.to_owned();
        config.budget = 0;
        debug!(
            provider,
            model,
            original_mode = "auto",
            clamped_to = level::MEDIUM,
            "thinking: mode converted, dynamic not allowed, using medium level |"
        );
        return config;
    }

    let mid = (support.min + support.max) / 2;
    if mid <= 0 && support.zero_allowed {
        config.mode = ThinkingMode::None;
        config.budget = 0;
    } else if mid <= 0 {
        config.mode = ThinkingMode::Budget;
        config.budget = support.min;
    } else {
        config.mode = ThinkingMode::Budget;
        config.budget = mid;
    }
    debug!(
        provider,
        model,
        original_mode = "auto",
        clamped_to = config.budget,
        "thinking: mode converted, dynamic not allowed |"
    );
    config
}

/// Clamps `lvl` to the nearest level the model supports; ties prefer the lower level.
pub(crate) fn clamp_level(lvl: &str, model_info: Option<&ModelInfo>, provider: &str) -> String {
    let mut model = "unknown";
    let mut supported: &[String] = &[];
    if let Some(info) = model_info {
        if !info.id.is_empty() {
            model = info.id.as_str();
        }
        if let Some(t) = &info.thinking {
            supported = &t.levels;
        }
    }

    if supported.is_empty() || is_level_supported(lvl, supported) {
        return lvl.to_owned();
    }
    let Some(pos) = level_index(lvl) else {
        return lvl.to_owned();
    };

    let mut best: Option<(usize, usize)> = None; // (index, distance)
    for s in supported {
        if let Some(idx) = level_index(s.trim()) {
            let dist = pos.abs_diff(idx);
            if best.is_none_or(|(best_idx, best_dist)| {
                dist < best_dist || (dist == best_dist && idx < best_idx)
            }) {
                best = Some((idx, dist));
            }
        }
    }

    if let Some((idx, _)) = best {
        let clamped = STANDARD_LEVEL_ORDER[idx];
        debug!(
            provider,
            model,
            original_value = lvl,
            clamped_to = clamped,
            "thinking: level clamped |"
        );
        return clamped.to_owned();
    }
    lvl.to_owned()
}

/// Clamps a budget to the model's range. -1 passes through; 0 maps to Min unless zero is allowed;
/// level-only models (Min == Max == 0) are unchanged.
pub(crate) fn clamp_budget(value: i64, model_info: Option<&ModelInfo>, provider: &str) -> i64 {
    let mut model = "unknown";
    let mut support: Option<&ThinkingSupport> = None;
    if let Some(info) = model_info {
        if !info.id.is_empty() {
            model = info.id.as_str();
        }
        support = info.thinking.as_ref();
    }
    let Some(support) = support else {
        return value;
    };

    if value == -1 {
        return value;
    }

    let (min, max) = (support.min, support.max);
    if value == 0 && !support.zero_allowed {
        warn!(
            provider,
            model,
            original_value = value,
            clamped_to = min,
            min,
            max,
            "thinking: budget zero not allowed |"
        );
        return min;
    }

    if min == 0 && max == 0 {
        return value;
    }

    if value < min {
        if value == 0 && support.zero_allowed {
            return 0;
        }
        log_clamp(provider, model, value, min, min, max);
        return min;
    }
    if value > max {
        log_clamp(provider, model, value, max, min, max);
        return max;
    }
    value
}

/// Whether `lvl` is in `supported` (case-insensitive, entries trimmed).
pub(crate) fn is_level_supported(lvl: &str, supported: &[String]) -> bool {
    supported.iter().any(|s| eq_fold(lvl, s.trim()))
}

fn level_index(lvl: &str) -> Option<usize> {
    STANDARD_LEVEL_ORDER.iter().position(|l| eq_fold(lvl, l))
}

fn normalize_levels(levels: &[String]) -> Vec<String> {
    levels.iter().map(|l| l.trim().to_lowercase()).collect()
}

/// Providers whose models take token budgets (some are hybrid and also take levels).
pub(crate) fn is_budget_capable_provider(provider: &str) -> bool {
    matches!(provider, "gemini" | "antigravity" | "claude")
}

fn is_gemini_family(provider: &str) -> bool {
    matches!(provider, "gemini" | "antigravity")
}

fn is_openai_family(provider: &str) -> bool {
    matches!(provider, "openai" | "openai-response" | "codex")
}

/// Equal names, or both in the gemini family, or both in the openai family.
pub(crate) fn is_same_provider_family(from: &str, to: &str) -> bool {
    from == to
        || (is_gemini_family(from) && is_gemini_family(to))
        || (is_openai_family(from) && is_openai_family(to))
}

fn log_clamp(provider: &str, model: &str, original: i64, clamped_to: i64, min: i64, max: i64) {
    debug!(
        provider,
        model,
        original_value = original,
        min,
        max,
        clamped_to,
        "thinking: budget clamped |"
    );
}

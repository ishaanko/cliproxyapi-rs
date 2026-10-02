//! Model-name thinking suffixes: `model(value)` (Go: internal/thinking/suffix.go).

use super::types::{SuffixResult, ThinkingMode, level};

/// Splits `model-name(value)` at the last `(`; the name must end with `)`. Content is not
/// validated (`m()` has a suffix with an empty raw value).
pub fn parse_suffix(model: &str) -> SuffixResult {
    let no_suffix = || SuffixResult {
        model_name: model.to_owned(),
        has_suffix: false,
        raw_suffix: String::new(),
    };
    let Some(last_open) = model.rfind('(') else {
        return no_suffix();
    };
    if !model.ends_with(')') {
        return no_suffix();
    }
    SuffixResult {
        model_name: model[..last_open].to_owned(),
        has_suffix: true,
        raw_suffix: model[last_open + 1..model.len() - 1].to_owned(),
    }
}

/// Parses a raw suffix as a non-negative integer budget (leading zeros fine, overflow and
/// negatives rejected; `-1` is handled by [`parse_special_suffix`]).
pub fn parse_numeric_suffix(raw: &str) -> Option<i64> {
    if raw.is_empty() {
        return None;
    }
    // Rust's integer parser accepts the same optional sign as Go's strconv.Atoi.
    let value: i64 = raw.parse().ok()?;
    (value >= 0).then_some(value)
}

/// `none` -> [`ThinkingMode::None`], `auto` / `-1` -> [`ThinkingMode::Auto`] (case-insensitive).
pub fn parse_special_suffix(raw: &str) -> Option<ThinkingMode> {
    match raw.to_lowercase().as_str() {
        "none" => Some(ThinkingMode::None),
        "auto" | "-1" => Some(ThinkingMode::Auto),
        _ => None,
    }
}

/// `minimal|low|medium|high|xhigh|max` (case-insensitive). `none` and `auto` are special values.
pub fn parse_level_suffix(raw: &str) -> Option<&'static str> {
    match raw.to_lowercase().as_str() {
        "minimal" => Some(level::MINIMAL),
        "low" => Some(level::LOW),
        "medium" => Some(level::MEDIUM),
        "high" => Some(level::HIGH),
        "xhigh" => Some(level::XHIGH),
        "max" => Some(level::MAX),
        _ => None,
    }
}

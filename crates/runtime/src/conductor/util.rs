//! Small helpers shared across the conductor: model-suffix parsing, string folding, JSON text
//! helpers and time arithmetic with the "unset = None" convention.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::time::Duration;

/// Result of splitting a thinking suffix off a model name (Go: thinking.ParseSuffix).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuffixResult {
    pub model_name: String,
    pub has_suffix: bool,
    pub raw_suffix: String,
}

/// `model(value)` -> name + raw suffix. Uses the LAST `(` and requires the string to end with `)`.
pub fn parse_suffix(model: &str) -> SuffixResult {
    let Some(last_open) = model.rfind('(') else {
        return SuffixResult {
            model_name: model.to_string(),
            has_suffix: false,
            raw_suffix: String::new(),
        };
    };
    if !model.ends_with(')') {
        return SuffixResult {
            model_name: model.to_string(),
            has_suffix: false,
            raw_suffix: String::new(),
        };
    }
    SuffixResult {
        model_name: model[..last_open].to_string(),
        has_suffix: true,
        raw_suffix: model[last_open + 1..model.len() - 1].to_string(),
    }
}

/// Model key used for per-model state: trimmed name without thinking suffix (Go: canonicalModelKey).
pub fn canonical_model_key(model: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        return String::new();
    }
    let name = parse_suffix(model).model_name;
    let name = name.trim();
    if name.is_empty() {
        model.to_string()
    } else {
        name.to_string()
    }
}

/// [`canonical_model_key`] without allocating (selection scans call it per credential).
pub fn canonical_model_key_ref(model: &str) -> &str {
    let model = model.trim();
    if model.is_empty() {
        return "";
    }
    let name = match model.rfind('(') {
        Some(open) if model.ends_with(')') => model[..open].trim(),
        _ => model.trim(),
    };
    if name.is_empty() { model } else { name }
}

/// Go `strings.EqualFold` for the strings we compare (model names, keys, URLs).
pub fn eq_fold(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

/// `time.Time` helpers over `Option<DateTime>` where `None` is the zero time.
pub fn after(t: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    t.is_some_and(|t| t > now)
}

/// Longest span we do arithmetic with (100 years); upstream hints and metadata can be absurd.
const MAX_SPAN_DAYS: i64 = 36_500;

/// `std::time::Duration` as a bounded chrono duration (never fails, never overflows later math).
pub fn to_chrono(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d)
        .unwrap_or(chrono::Duration::days(MAX_SPAN_DAYS))
        .min(chrono::Duration::days(MAX_SPAN_DAYS))
}

/// `t + d`, saturating at the maximum representable time instead of panicking.
pub fn add_chrono(t: DateTime<Utc>, d: chrono::Duration) -> DateTime<Utc> {
    t.checked_add_signed(d)
        .unwrap_or(if d > chrono::Duration::zero() {
            DateTime::<Utc>::MAX_UTC
        } else {
            DateTime::<Utc>::MIN_UTC
        })
}

pub fn add_duration(now: DateTime<Utc>, d: Duration) -> DateTime<Utc> {
    add_chrono(now, to_chrono(d))
}

pub fn chrono_to_std(d: chrono::Duration) -> Duration {
    d.to_std().unwrap_or(Duration::ZERO)
}

/// Metadata string value, trimmed. Accepts strings only (Go also accepted `[]byte`).
pub fn meta_string(meta: &crate::executor::Metadata, key: &str) -> String {
    meta.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Strips `prefix` from a model, returning the model unchanged when it does not start with it.
pub fn rewrite_model_for_prefix(model: &str, prefix: &str) -> String {
    rewrite_model_for_prefix_ref(model, prefix).to_string()
}

/// [`rewrite_model_for_prefix`] as a borrowed slice.
pub fn rewrite_model_for_prefix_ref<'a>(model: &'a str, prefix: &str) -> &'a str {
    let prefix = prefix.trim();
    if model.is_empty() || prefix.is_empty() {
        return model;
    }
    model
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(model)
}

/// Trims, drops empties and removes duplicates, keeping first occurrences (Go: dedupeStrings).
pub fn dedupe_strings(values: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(values.len());
    for v in values {
        let v = v.trim().to_string();
        if v.is_empty() || out.contains(&v) {
            continue;
        }
        out.push(v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_durations_do_not_overflow_time_math() {
        let now = chrono::Utc::now();
        assert!(add_duration(now, std::time::Duration::MAX) >= now);
    }

    #[test]
    fn suffix_uses_last_paren_and_requires_closing() {
        let r = parse_suffix("claude-x(8192)");
        assert_eq!(
            (r.model_name.as_str(), r.raw_suffix.as_str(), r.has_suffix),
            ("claude-x", "8192", true)
        );
        assert!(!parse_suffix("a(b)c").has_suffix);
        assert_eq!(canonical_model_key(" m(high) "), "m");
        assert_eq!(canonical_model_key("(x)"), "(x)");
    }
}

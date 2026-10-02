//! Chat model uid resolution (Go: helps/devin_models.go).
//!
//! Maps a requested model plus thinking level/budget to the upstream `chat_model_uid`, using the
//! Devin catalog (`cpa_core::registry`) for the effort levels each model supports.

use cpa_core::registry::lookup_devin_model;
use cpa_core::thinking::parse_suffix;

/// Recognized model uid suffixes; a model already ending in one is sent as is.
const KNOWN_SUFFIXES: &[&str] = &[
    "-none",
    "-low",
    "-medium",
    "-high",
    "-xhigh",
    "-max",
    "-fast",
    "-slow",
    "-priority",
    "-low-priority",
    "-medium-priority",
    "-high-priority",
    "-xhigh-priority",
    "-max-priority",
    "-low-fast",
    "-medium-fast",
    "-high-fast",
    "-xhigh-fast",
    "-max-fast",
    "-none-fast",
    "-thinking-1m",
    "-thinking",
    "-max-1m",
    "-none-1m",
    "_none",
    "_minimal",
    "_low",
    "_medium",
    "_high",
    "_xhigh",
    "_max",
    "_thinking",
];

/// Private upstream aliases that cannot be inferred from the catalog.
const SPECIAL_ALIASES: &[(&str, &str)] = &[
    ("claude-haiku-4-5", "MODEL_PRIVATE_11"),
    ("gpt-4-1", "MODEL_CHAT_GPT_4_1_2025_04_14"),
];

const STANDARD_LEVEL_ORDER: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

/// Whether `model` already ends with a known effort suffix.
pub fn has_effort_suffix(model: &str) -> bool {
    let lower = model.trim().to_lowercase();
    KNOWN_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

/// Canonical effort for a loose level string or a token budget; "" when neither applies.
pub fn normalize_thinking_level(level: &str, budget_tokens: i64) -> String {
    let normalized = level.trim().to_lowercase();
    match normalized.as_str() {
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "fast" => return normalized,
        "none" | "off" | "disabled" => return "none".into(),
        "auto" | "adaptive" => return "high".into(),
        _ => {}
    }
    if budget_tokens > 0 {
        return match budget_tokens {
            ..=4096 => "low",
            4097..=16384 => "medium",
            16385..=32768 => "high",
            _ => "max",
        }
        .into();
    }
    String::new()
}

/// Resolves `raw_model` into the upstream `chat_model_uid`. Empty input gives `swe-2-high`.
pub fn resolve_chat_model_uid(raw_model: &str, thinking_level: &str, budget_tokens: i64) -> String {
    let model = raw_model.trim();
    if model.is_empty() {
        return "swe-2-high".into();
    }

    // 1. Strip the devin/ prefix (case-insensitive).
    let clean = match model.get(..6) {
        Some(p) if p.eq_ignore_ascii_case("devin/") => &model[6..],
        _ => model,
    };

    // 2. An exact effort suffix is used directly.
    if has_effort_suffix(clean) {
        return clean.to_string();
    }

    // 3. A parenthesis or colon suffix overrides the thinking level.
    let parsed = parse_suffix(clean);
    let mut base_model = parsed.model_name.trim().to_string();
    let mut level = thinking_level.to_string();
    if parsed.has_suffix {
        level = parsed.raw_suffix.clone();
    } else if let Some(colon) = clean.rfind(':') {
        base_model = clean[..colon].trim().to_string();
        level = clean[colon + 1..].trim().to_string();
    }

    // 4. Normalize the requested effort.
    let effort = normalize_thinking_level(&level, budget_tokens);
    let lower_base = base_model.to_lowercase();
    let mut canonical = lower_base.replace('.', "-");

    // 5. Special private aliases and models with fixed uid families.
    if let Some((_, alias)) = SPECIAL_ALIASES.iter().find(|(k, _)| *k == canonical) {
        return (*alias).into();
    }
    if canonical == "claude-sonnet-4-5" || canonical.contains("sonnet-4-5") {
        return if !effort.is_empty() && effort != "none" {
            "MODEL_PRIVATE_3"
        } else {
            "MODEL_PRIVATE_2"
        }
        .into();
    }
    if canonical == "gemini-3-flash" {
        canonical = "gemini-3-8-flash".into();
    }
    match canonical.replace('-', "_").as_str() {
        "model_gpt_5_2" => {
            let eff = clamp_effort(&effort, &["none", "low", "medium", "high", "xhigh"], "low");
            return format!("MODEL_GPT_5_2_{}", eff.to_uppercase());
        }
        "model_google_gemini_3_0_flash" => {
            let eff = clamp_effort(&effort, &["minimal", "low", "medium", "high"], "high");
            return format!("MODEL_GOOGLE_GEMINI_3_0_FLASH_{}", eff.to_uppercase());
        }
        "model_claude_4_5_opus" => {
            return if !effort.is_empty() && effort != "none" {
                "MODEL_CLAUDE_4_5_OPUS_THINKING"
            } else {
                "MODEL_CLAUDE_4_5_OPUS"
            }
            .into();
        }
        _ => {}
    }

    // 6. Catalog lookup for the supported effort levels.
    let mut info = lookup_devin_model(&canonical);
    if info.is_none() && canonical != lower_base {
        info = lookup_devin_model(&lower_base);
    }
    let allowed: Vec<String> = info
        .and_then(|m| m.thinking)
        .map(|t| t.levels)
        .filter(|l| !l.is_empty())
        .unwrap_or_default();

    // 7. Base models that stay bare unless a specific variant is requested.
    let thinking_on = !effort.is_empty() && effort != "none";
    match canonical.as_str() {
        "swe-1-7" => {
            return if effort == "medium" {
                "swe-1-7-medium"
            } else {
                "swe-1-7"
            }
            .into();
        }
        "swe-1-6" => {
            return if effort == "fast" {
                "swe-1-6-fast"
            } else {
                "swe-1-6"
            }
            .into();
        }
        "glm-5-2" => {
            return match effort.as_str() {
                "none" => "glm-5-2-none",
                "max" => "glm-5-2-max",
                _ => "glm-5-2",
            }
            .into();
        }
        "glm-5-2-1m" => {
            return match effort.as_str() {
                "none" => "glm-5-2-none-1m",
                "max" => "glm-5-2-max-1m",
                _ => "glm-5-2-1m",
            }
            .into();
        }
        "claude-opus-4-6" | "claude-sonnet-4-6" => {
            return if thinking_on {
                format!("{canonical}-thinking")
            } else {
                canonical
            };
        }
        "claude-opus-4-6-1m" => {
            return if thinking_on {
                "claude-opus-4-6-thinking-1m".into()
            } else {
                canonical
            };
        }
        "claude-sonnet-4-6-1m" => {
            return if thinking_on {
                "claude-sonnet-4-6-thinking-1m".into()
            } else {
                canonical
            };
        }
        _ => {}
    }

    // 8. No catalog levels: a bare model.
    if allowed.is_empty() {
        return canonical;
    }

    // 9. Default effort, clamped to the supported levels.
    let allowed_refs: Vec<&str> = allowed.iter().map(String::as_str).collect();
    let default_effort = select_default_effort(&canonical, &allowed_refs);
    let clamped = clamp_effort(&effort, &allowed_refs, &default_effort);
    format!("{canonical}-{clamped}")
}

fn select_default_effort(base_model: &str, levels: &[&str]) -> String {
    if base_model.contains("swe-2") {
        return "high".into();
    }
    let has = |l: &str| levels.contains(&l);
    let (none, low, medium, high) = (has("none"), has("low"), has("medium"), has("high"));
    // GPT-5.x families in the Devin CLI support none and low and default to low.
    if none && low && base_model.starts_with("gpt-5") {
        return "low".into();
    }
    const PREFER_HIGH: &[&str] = &["gemini", "grok", "glm", "deepseek", "kimi", "nemotron"];
    if high && PREFER_HIGH.iter().any(|f| base_model.contains(f)) {
        return "high".into();
    }
    if medium {
        return "medium".into();
    }
    if high {
        return "high".into();
    }
    if low {
        return "low".into();
    }
    levels.first().copied().unwrap_or_default().into()
}

fn level_index(level: &str) -> Option<usize> {
    let lower = level.trim().to_lowercase();
    STANDARD_LEVEL_ORDER.iter().position(|l| *l == lower)
}

/// The supported level closest to `requested` (ties prefer the higher effort); `default` when
/// nothing was requested or the request is unrecognized.
fn clamp_effort(requested: &str, allowed: &[&str], default: &str) -> String {
    if requested.is_empty() {
        return default.to_string();
    }
    let req_lower = requested.trim().to_lowercase();
    if let Some(a) = allowed
        .iter()
        .find(|a| a.trim().to_lowercase() == req_lower)
    {
        return (*a).to_string();
    }
    if req_lower == "none" {
        return default.to_string();
    }
    let Some(req_idx) = level_index(&req_lower) else {
        return default.to_string();
    };
    let mut best = default.to_string();
    let mut best_dist = 999usize;
    let mut best_idx: Option<usize> = None;
    for a in allowed {
        let Some(a_idx) = level_index(a) else {
            continue;
        };
        let dist = req_idx.abs_diff(a_idx);
        if dist < best_dist {
            best_dist = dist;
            best = (*a).to_string();
            best_idx = Some(a_idx);
        } else if dist == best_dist && best_idx.is_none_or(|b| a_idx > b) {
            best = (*a).to_string();
            best_idx = Some(a_idx);
        }
    }
    best
}

#[cfg(test)]
mod tests;

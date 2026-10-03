//! The two `internal/thinking` helpers the HTTP layer needs: `ParseSuffix` (model names like
//! `gpt-5.2(high)`) and `ExtractReasoningEffort` (the `reasoning_effort` execution metadata used
//! for usage records). The full thinking port lives with the translators; this is the slice the
//! handlers call before execution.

use cpa_json::J;
use serde_json::Value;

/// `thinking.SuffixResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suffix {
    pub model_name: String,
    pub has_suffix: bool,
    pub raw_suffix: String,
}

/// `thinking.ParseSuffix`: splits a trailing `(value)` off a model name.
pub fn parse_suffix(model: &str) -> Suffix {
    let plain = || Suffix {
        model_name: model.to_string(),
        has_suffix: false,
        raw_suffix: String::new(),
    };
    let Some(last_open) = model.rfind('(') else {
        return plain();
    };
    if !model.ends_with(')') {
        return plain();
    }
    Suffix {
        model_name: model[..last_open].to_string(),
        has_suffix: true,
        raw_suffix: model[last_open + 1..model.len() - 1].to_string(),
    }
}

/// A parsed thinking setting (`ThinkingConfig`); `None` means "no config".
#[derive(Debug, Clone, PartialEq, Eq)]
enum Config {
    None,
    Auto,
    Level(String),
    Budget(i64),
}

fn level_or_none(value: &str) -> Config {
    if value == "none" {
        Config::None
    } else {
        Config::Level(value.to_string())
    }
}

fn budget_config(value: i64) -> Config {
    match value {
        0 => Config::None,
        -1 => Config::Auto,
        v => Config::Budget(v),
    }
}

/// `ConvertBudgetToLevel`.
fn budget_to_level(budget: i64) -> Option<&'static str> {
    match budget {
        i64::MIN..=-2 => None,
        -1 => Some("auto"),
        0 => Some("none"),
        1..=512 => Some("minimal"),
        513..=1024 => Some("low"),
        1025..=8192 => Some("medium"),
        8193..=24576 => Some("high"),
        _ => Some("xhigh"),
    }
}

/// `reasoningEffortFromConfig`. A level that is empty counts as "no config".
fn effort_from_config(config: &Config) -> String {
    match config {
        Config::None => "none".into(),
        Config::Auto => "auto".into(),
        Config::Level(level) => level.trim().to_lowercase(),
        Config::Budget(budget) => budget_to_level(*budget).unwrap_or("").into(),
    }
}

fn effort_from_suffix(model: &str) -> String {
    let suffix = parse_suffix(model);
    if !suffix.has_suffix {
        return String::new();
    }
    let raw = suffix.raw_suffix.as_str();
    let lower = raw.to_lowercase();
    let config = match lower.as_str() {
        "none" => Config::None,
        "auto" | "-1" => Config::Auto,
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => Config::Level(lower.clone()),
        _ => match raw.parse::<i64>() {
            Ok(0) => Config::None,
            Ok(n) if n > 0 => Config::Budget(n),
            _ => return String::new(),
        },
    };
    effort_from_config(&config)
}

fn string_at(root: &Value, path: &str) -> Option<String> {
    let res = root.g(path);
    res.exists().then(|| res.str())
}

fn claude_config(root: &Value) -> Option<Config> {
    let thinking_type = root.g("thinking.type").str();
    let effort_cfg = |value: &str| match value {
        "" => None,
        "none" => Some(Config::None),
        "auto" => Some(Config::Auto),
        other => Some(Config::Level(other.to_string())),
    };
    let output_effort = || {
        let node = root.g("output_config.effort");
        (node.exists() && node.is_string()).then(|| node.str().trim().to_lowercase())
    };
    if thinking_type == "disabled" {
        return Some(Config::None);
    }
    if thinking_type == "adaptive" || thinking_type == "auto" {
        return output_effort().and_then(|v| effort_cfg(&v));
    }
    let budget = root.g("thinking.budget_tokens");
    if budget.exists() {
        return Some(budget_config(budget.int()));
    }
    if thinking_type == "enabled" {
        if let Some(value) = output_effort()
            && let Some(cfg) = effort_cfg(&value)
        {
            return Some(cfg);
        }
        return Some(Config::Auto);
    }
    None
}

fn gemini_config(root: &Value, provider: &str) -> Option<Config> {
    let prefix = if provider == "antigravity" {
        "request.generationConfig.thinkingConfig"
    } else {
        "generationConfig.thinkingConfig"
    };
    let level = string_at(root, &format!("{prefix}.thinkingLevel"))
        .or_else(|| string_at(root, &format!("{prefix}.thinking_level")));
    if let Some(value) = level {
        return Some(match value.as_str() {
            "none" => Config::None,
            "auto" => Config::Auto,
            other => Config::Level(other.to_string()),
        });
    }
    let budget = {
        let a = root.g(&format!("{prefix}.thinkingBudget"));
        if a.exists() { a } else { root.g(&format!("{prefix}.thinking_budget")) }
    };
    budget.exists().then(|| budget_config(budget.int()))
}

fn interactions_config(root: &Value) -> Option<Config> {
    for path in [
        "generation_config.thinking_level",
        "generation_config.thinkingLevel",
        "generation_config.thinking_config.thinking_level",
        "generation_config.thinking_config.thinkingLevel",
        "generation_config.thinkingConfig.thinking_level",
        "generation_config.thinkingConfig.thinkingLevel",
    ] {
        if let Some(value) = string_at(root, path) {
            let value = value.trim().to_lowercase();
            return Some(match value.as_str() {
                "none" => Config::None,
                "auto" => Config::Auto,
                other => Config::Level(other.to_string()),
            });
        }
    }
    for path in [
        "generation_config.thinking_budget",
        "generation_config.thinkingBudget",
        "generation_config.thinking_config.thinking_budget",
        "generation_config.thinking_config.thinkingBudget",
        "generation_config.thinkingConfig.thinking_budget",
        "generation_config.thinkingConfig.thinkingBudget",
    ] {
        let node = root.g(path);
        if node.exists() {
            return Some(budget_config(node.int()));
        }
    }
    None
}

fn openai_config(root: &Value) -> Option<Config> {
    string_at(root, "reasoning_effort").map(|v| level_or_none(&v))
}

fn codex_config(root: &Value) -> Option<Config> {
    string_at(root, "reasoning.effort").map(|v| level_or_none(&v))
}

/// `extractConfigurationUpdateConfig`: the last non-empty Responses effort update.
fn configuration_update_config(root: &Value) -> Option<Config> {
    let input = root.g("input");
    if !input.is_array() {
        return None;
    }
    let mut effort = String::new();
    for item in input.array() {
        if item.g("type").str() == "configuration_update" {
            let value = item.g("reasoning.effort");
            if value.is_string() {
                let normalized = value.str().trim().to_lowercase();
                if !normalized.is_empty() {
                    effort = normalized;
                }
            }
        }
    }
    match effort.as_str() {
        "" => None,
        "none" => Some(Config::None),
        "auto" => Some(Config::Auto),
        other => Some(Config::Level(other.to_string())),
    }
}

fn kimi_config(root: &Value) -> Option<Config> {
    let thinking_type = root.g("thinking.type");
    if thinking_type.exists() {
        match thinking_type.str().trim().to_lowercase().as_str() {
            "disabled" => return Some(Config::None),
            "enabled" if !root.g("thinking.effort").exists() => return None,
            _ => {}
        }
    }
    if root.g("thinking.effort").exists() {
        let value = root.g("thinking.effort").str().trim().to_lowercase();
        return match value.as_str() {
            "" => None,
            "none" => Some(Config::None),
            "auto" => Some(Config::Auto),
            other => Some(Config::Level(other.to_string())),
        };
    }
    if thinking_type.exists() {
        return None;
    }
    openai_config(root)
}

fn thinking_config(root: &Value, provider: &str) -> Option<Config> {
    match provider {
        "claude" => claude_config(root),
        "gemini" | "antigravity" => gemini_config(root, provider),
        "interactions" => interactions_config(root),
        "openai" => openai_config(root),
        "codex" | "xai" => codex_config(root),
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => kimi_config(root),
        _ => None,
    }
}

fn codex_usage_config(root: &Value) -> Option<Config> {
    configuration_update_config(root).or_else(|| codex_config(root))
}

fn usable(config: Option<Config>) -> Option<Config> {
    config.filter(|c| !effort_from_config(c).is_empty() || *c == Config::None)
}

/// The body parsed once, `None` unless it is valid JSON (metadata extraction treats anything else
/// as "no settings").
pub fn parse_if_valid(body: &[u8]) -> Option<Value> {
    (!body.is_empty() && cpa_json::valid(body)).then(|| cpa_json::parse(body))
}

/// `thinking.ExtractReasoningEffort`: canonical `reasoning_effort` label of the source request
/// (empty when the request carries no thinking setting).
/// `root` is the parsed body when it is valid JSON (see [`parse_if_valid`]).
pub fn extract_reasoning_effort(root: Option<&Value>, provider: &str, model: &str) -> String {
    let provider = provider.trim().to_lowercase();
    let responses = provider == "codex" || provider == "openai-response";
    if responses
        && let Some(root) = root
    {
        let effort = configuration_update_config(root).map(|c| effort_from_config(&c)).unwrap_or_default();
        if !effort.is_empty() {
            return effort;
        }
    }
    let from_suffix = effort_from_suffix(model);
    if !from_suffix.is_empty() {
        return from_suffix;
    }
    let Some(root) = root else {
        return String::new();
    };
    let mut config = match provider.as_str() {
        "codex" | "xai" | "openai-response" => codex_usage_config(&root),
        other => thinking_config(&root, other),
    };
    config = usable(config);
    if config.is_none() && (provider == "openai-response" || provider == "openai") {
        config = usable(codex_usage_config(&root));
    }
    config.map(|c| effort_from_config(&c)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_split() {
        let s = parse_suffix("gpt-5.2(high)");
        assert_eq!((s.model_name.as_str(), s.raw_suffix.as_str(), s.has_suffix), ("gpt-5.2", "high", true));
        assert!(!parse_suffix("plain").has_suffix);
        assert!(!parse_suffix("a(b)c").has_suffix);
    }

    #[test]
    fn effort_sources() {
        let body = br#"{"reasoning_effort":"high"}"#;
        let effort = |body: &[u8], provider: &str, model: &str| extract_reasoning_effort(parse_if_valid(body).as_ref(), provider, model);
        assert_eq!(effort(body, "openai", "m"), "high");
        assert_eq!(effort(body, "openai", "m(8192)"), "medium");
        let claude = br#"{"thinking":{"type":"enabled","budget_tokens":600}}"#;
        assert_eq!(effort(claude, "claude", "m"), "low");
        let resp = br#"{"reasoning":{"effort":"low"}}"#;
        assert_eq!(effort(resp, "openai-response", "m"), "low");
        assert_eq!(effort(b"{}", "openai", "m"), "");
    }
}

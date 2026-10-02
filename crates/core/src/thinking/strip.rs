//! Removal of thinking fields for models without thinking support (Go: internal/thinking/strip.go).

use super::json::{delete_if_empty_object, parse_valid};

/// Removes the thinking configuration fields of `provider` from the body. Empty or invalid JSON
/// and unknown providers return the body unchanged.
pub fn strip_thinking_config(body: &[u8], provider: &str) -> Vec<u8> {
    let Some(mut v) = parse_valid(body) else {
        return body.to_vec();
    };

    let paths: &[&str] = match provider {
        "claude" => &["thinking", "output_config.effort"],
        "gemini" => &["generationConfig.thinkingConfig"],
        "antigravity" => &["request.generationConfig.thinkingConfig"],
        "interactions" => &[
            "generation_config.thinking_level",
            "generation_config.thinkingLevel",
            "generation_config.thinking_budget",
            "generation_config.thinkingBudget",
            "generation_config.thinking_summaries",
            "generation_config.thinkingSummaries",
            "generation_config.thinking_config",
            "generation_config.thinkingConfig",
        ],
        "openai" => &["reasoning_effort", "reasoning"],
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => &["reasoning_effort", "thinking"],
        "codex" | "xai" => &["reasoning"],
        _ => return body.to_vec(),
    };

    for path in paths {
        cpa_json::delete(&mut v, path);
    }
    // Avoid leaving an empty output_config when effort was its only field.
    if provider == "claude" {
        delete_if_empty_object(&mut v, "output_config");
    }
    cpa_json::to_vec(&v)
}

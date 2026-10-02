//! Provider-neutral reasoning-summary visibility intent (Go: internal/thinking/summary.go).
//!
//! Summary visibility is orthogonal to thinking effort: it is extracted from the source request,
//! survives effort rewriting, and is written back in the target protocol.

use cpa_json::{J, Kind, Value};

use super::json::{delete_if_empty_object, parse_valid, set, to_bytes};
use super::parse_suffix;
use crate::registry::{ModelInfo, lookup_model_info};

/// Whether the client explicitly asked for reasoning summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SummaryMode {
    #[default]
    Unspecified,
    Disabled,
    Enabled,
}

/// Reasoning-summary visibility intent. `detail` preserves protocols that distinguish `auto`,
/// `concise` and `detailed` summaries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryConfig {
    pub mode: SummaryMode,
    pub detail: String,
}

impl SummaryConfig {
    fn enabled(detail: &str) -> Self {
        Self {
            mode: SummaryMode::Enabled,
            detail: detail.to_owned(),
        }
    }

    fn disabled() -> Self {
        Self {
            mode: SummaryMode::Disabled,
            detail: String::new(),
        }
    }
}

/// Reads protocol-specific summary visibility intent.
///
/// OpenAI Chat is the one protocol where effort implies summaries: chat completions has no
/// summary field of its own and clients sending `reasoning_effort` have always received reasoning
/// summaries, so a non-none effort counts as an explicit request. Every other protocol carries a
/// dedicated field, so effort alone means nothing.
pub fn extract_summary_config(body: &[u8], format: &str) -> SummaryConfig {
    let normalized = format.trim().to_lowercase();
    // Check the format first so unsupported targets skip parsing the whole body.
    if !summary_format_supported(&normalized) {
        return SummaryConfig::default();
    }
    match parse_valid(body) {
        Some(v) => summary_config_of(&v, &normalized),
        None => SummaryConfig::default(),
    }
}

/// [`extract_summary_config`] over an already parsed body and a normalized, supported format.
fn summary_config_of(v: &Value, normalized: &str) -> SummaryConfig {
    match normalized {
        "openai" => {
            if let Some(config) = extract_openai_explicit_summary_config(v) {
                return config;
            }
            if let Some(effort) = v.g("reasoning_effort").as_str() {
                let value = effort.trim().to_lowercase();
                if value.is_empty() {
                    return SummaryConfig::default();
                }
                if value == "none" {
                    return SummaryConfig::disabled();
                }
                return SummaryConfig::enabled("auto");
            }
        }
        "openai-response" | "codex" => {
            if let Some(config) = responses_summary_config(v, "reasoning.summary") {
                return config;
            }
            if let Some(config) = responses_summary_config(v, "reasoning.generate_summary") {
                return config;
            }
        }
        "claude" => {
            // Anthropic only accepts display alongside active adaptive/manual thinking.
            if !claude_thinking_accepts_display(v) {
                return SummaryConfig::default();
            }
            if let Some(config) = claude_summary_config(v, "thinking.display") {
                return config;
            }
        }
        "gemini" => {
            if let Some(config) = first_summary_bool_config(
                v,
                &[
                    "generationConfig.thinkingConfig.includeThoughts",
                    "generationConfig.thinkingConfig.include_thoughts",
                    "generation_config.thinking_config.include_thoughts",
                    "generation_config.thinking_config.includeThoughts",
                ],
            ) {
                return config;
            }
        }
        "antigravity" => {
            if let Some(config) = first_summary_bool_config(
                v,
                &[
                    "request.generationConfig.thinkingConfig.includeThoughts",
                    "request.generationConfig.thinkingConfig.include_thoughts",
                    "request.generationConfig.thinking_config.includeThoughts",
                    "request.generationConfig.thinking_config.include_thoughts",
                ],
            ) {
                return config;
            }
        }
        "interactions" => {
            for path in [
                "generation_config.thinking_summaries",
                "generation_config.thinkingSummaries",
            ] {
                if let Some(config) = interactions_summary_config(v, path) {
                    return config;
                }
            }
            // Existing Interactions translators accept the OpenAI-style top-level compatibility
            // object. The official generation_config selector stays authoritative when both exist.
            if let Some(config) = interactions_summary_config(v, "reasoning.summary") {
                return config;
            }
            if let Some(config) = first_summary_bool_config(
                v,
                &[
                    "generation_config.thinking_config.include_thoughts",
                    "generation_config.thinking_config.includeThoughts",
                    "generation_config.thinkingConfig.include_thoughts",
                    "generation_config.thinkingConfig.includeThoughts",
                ],
            ) {
                return config;
            }
        }
        _ => {}
    }
    SummaryConfig::default()
}

/// Reads only explicit visibility controls. Unlike [`extract_summary_config`], OpenAI Chat
/// `reasoning_effort` is not a summary proxy; executors use this to tell whether a request
/// normalizer retained or removed the translated target field.
pub fn extract_explicit_summary_config(body: &[u8], format: &str) -> SummaryConfig {
    let normalized = format.trim().to_lowercase();
    if normalized != "openai" {
        return extract_summary_config(body, &normalized);
    }
    parse_valid(body)
        .and_then(|v| extract_openai_explicit_summary_config(&v))
        .unwrap_or_default()
}

/// Reads source visibility intent using the source/target protocol pair. OpenAI Chat
/// `reasoning_effort` controls depth, not Claude display visibility, so it is ignored only for
/// Chat-to-Claude.
pub fn extract_translated_summary_config(
    body: &[u8],
    source_format: &str,
    target_format: &str,
) -> SummaryConfig {
    let source = source_format.trim().to_lowercase();
    let target = target_format.trim().to_lowercase();
    if target == "claude" && source == "openai" {
        return extract_explicit_summary_config(body, &source);
    }
    extract_summary_config(body, &source)
}

/// Copies an explicit source visibility choice onto a Claude body. Chat `reasoning_effort` is not
/// a visibility field, so it stays unspecified and the Claude cloak can apply its default display.
pub fn apply_translated_summary_to_claude(
    out: &[u8],
    source: &[u8],
    source_format: &str,
    model: &str,
) -> Vec<u8> {
    let config = extract_translated_summary_config(source, source_format, "claude");
    if config.mode == SummaryMode::Unspecified {
        return out.to_vec();
    }
    apply_summary_config_for_model(out.to_vec(), "claude", model, &config)
}

/// Writes canonical summary intent in the target protocol.
pub fn apply_summary_config(body: Vec<u8>, format: &str, config: &SummaryConfig) -> Vec<u8> {
    apply_summary_config_for_model(body, format, "", config)
}

/// Writes canonical summary intent in the target protocol, using the target model's capabilities
/// when a valid target request must activate thinking before it can request summaries.
pub fn apply_summary_config_for_model(
    body: Vec<u8>,
    format: &str,
    model: &str,
    config: &SummaryConfig,
) -> Vec<u8> {
    apply_summary_config_for_provider(body, format, model, "", None, config)
}

/// Like [`apply_summary_config_for_model`] with the resolved model definition (an API-key model
/// whose capability is not globally visible) and the execution provider identity, used by Chat
/// dialects whose visibility controls are not part of the OpenAI wire format.
pub(crate) fn apply_summary_config_for_provider(
    body: Vec<u8>,
    format: &str,
    model: &str,
    provider: &str,
    model_info: Option<&ModelInfo>,
    config: &SummaryConfig,
) -> Vec<u8> {
    let normalized = format.trim().to_lowercase();
    if config.mode == SummaryMode::Unspecified || !summary_format_supported(&normalized) {
        return body;
    }
    let Some(mut v) = parse_valid(&body) else {
        return body;
    };

    let enabled = config.mode == SummaryMode::Enabled;
    // Branches that may leave the body untouched report false so the original bytes are kept.
    let changed = match normalized.as_str() {
        "openai" => apply_openai_chat_summary_config(&mut v, provider, enabled),
        "claude" => {
            // Anthropic documents display as invalid with thinking.type=disabled and requires it
            // alongside adaptive or enabled thinking. Model defaults differ (Opus 5 / Sonnet 5
            // default to adaptive, Fable/Mythos 5 are always on, the 4.x models default to off, the
            // newest default display to omitted), so keeping a missing thinking block absent
            // preserves each model's default. Only an enabled summary may activate a valid target
            // thinking mode so summarized text can be returned; a disabled summary only adds
            // `omitted` to an already-active mode.
            //
            // https://platform.claude.com/docs/en/build-with-claude/thinking
            // https://platform.claude.com/docs/en/build-with-claude/thinking-troubleshooting#supported-models
            let mut activated = false;
            if enabled && !v.g("thinking.type").exists() {
                activated = enable_claude_thinking_for_summary(&mut v, model, model_info);
            }
            if claude_thinking_accepts_display(&v) {
                set(
                    &mut v,
                    "thinking.display",
                    if enabled { "summarized" } else { "omitted" },
                );
                true
            } else {
                activated
            }
        }
        "gemini" => {
            set(
                &mut v,
                "generationConfig.thinkingConfig.includeThoughts",
                enabled,
            );
            for path in [
                "generationConfig.thinkingConfig.include_thoughts",
                "generation_config.thinking_config.include_thoughts",
                "generation_config.thinking_config.includeThoughts",
            ] {
                cpa_json::delete(&mut v, path);
            }
            true
        }
        "antigravity" => {
            set(
                &mut v,
                "request.generationConfig.thinkingConfig.includeThoughts",
                enabled,
            );
            for path in [
                "request.generationConfig.thinkingConfig.include_thoughts",
                "request.generationConfig.thinking_config.include_thoughts",
                "request.generationConfig.thinking_config.includeThoughts",
            ] {
                cpa_json::delete(&mut v, path);
            }
            true
        }
        "interactions" => {
            // Google Interactions only accepts auto or none; OpenAI's concise and detailed
            // selectors collapse to the enabled value.
            set(
                &mut v,
                "generation_config.thinking_summaries",
                if enabled { "auto" } else { "none" },
            );
            cpa_json::delete(&mut v, "generation_config.thinkingSummaries");
            true
        }
        "openai-response" | "codex" => {
            if enabled {
                set(
                    &mut v,
                    "reasoning.summary",
                    normalized_summary_detail(&config.detail),
                );
                cpa_json::delete(&mut v, "reasoning.generate_summary");
            } else {
                // Omitting the field is the documented way to disable summaries; an explicit null
                // is not accepted by every Responses-compatible backend.
                cpa_json::delete(&mut v, "reasoning.summary");
                cpa_json::delete(&mut v, "reasoning.generate_summary");
                delete_if_empty_object(&mut v, "reasoning");
            }
            true
        }
        _ => false,
    };
    if changed { to_bytes(&v) } else { body }
}

/// Protocols whose summary visibility this module can read or write.
fn summary_format_supported(format: &str) -> bool {
    matches!(
        format,
        "openai"
            | "openai-response"
            | "codex"
            | "claude"
            | "gemini"
            | "antigravity"
            | "interactions"
    )
}

/// Whether the body carries an active Claude thinking block that can hold a display field.
fn claude_thinking_accepts_display(v: &Value) -> bool {
    match v.g("thinking.type").str().trim().to_lowercase().as_str() {
        "adaptive" => true,
        "enabled" => {
            // This runs before ApplyThinking normalizes the request, so a missing budget_tokens is
            // an unfinished body rather than inactive thinking. -1 is CPA's compatibility
            // representation for auto thinking.
            let budget = v.g("thinking.budget_tokens");
            if !budget.is_number() {
                return true;
            }
            let value = budget.int();
            value == -1 || value > 0
        }
        _ => false,
    }
}

/// Writes only documented Chat visibility controls.
///
/// OpenAI Chat Completions exposes `reasoning_effort` but no summary or visibility parameter;
/// DeepSeek and Kimi Chat likewise document no independent hide/show switch. Summary intent must
/// never invent or overwrite thinking effort for those dialects. OpenRouter is the exception:
/// `reasoning.exclude` is its "reason but hide" control and `include_reasoning` its deprecated
/// inverse alias. Unknown OpenAI-compatible providers are handled conservatively by updating
/// those fields only when the payload already carries them.
fn apply_openai_chat_summary_config(v: &mut Value, provider: &str, enabled: bool) -> bool {
    let mut changed = false;
    if is_open_router_provider(provider) || v.g("reasoning.exclude").is_bool() {
        set(v, "reasoning.exclude", !enabled);
        changed = true;
    }
    if v.g("include_reasoning").is_bool() {
        set(v, "include_reasoning", enabled);
        changed = true;
    }
    changed
}

fn is_open_router_provider(provider: &str) -> bool {
    let provider = provider.trim().to_lowercase();
    provider == "openrouter"
        || provider
            .split(['-', '_', '/', '.', ':'])
            .any(|part| part == "openrouter")
}

fn extract_openai_explicit_summary_config(v: &Value) -> Option<SummaryConfig> {
    // Google's documented Chat Completions extension is the authoritative explicit visibility
    // control when present, ahead of CPA compatibility aliases and the reasoning_effort fallback.
    for path in [
        "extra_body.google.thinking_config.include_thoughts",
        "extra_body.google.thinking_config.includeThoughts",
        "extra_body.google.thinkingConfig.include_thoughts",
        "extra_body.google.thinkingConfig.includeThoughts",
        "extra_body.extra_body.google.thinking_config.include_thoughts",
        "extra_body.extra_body.google.thinking_config.includeThoughts",
        "google.thinking_config.include_thoughts",
        "google.thinking_config.includeThoughts",
        "thinking.includeThoughts",
        "thinking.include_thoughts",
        "reasoning.includeThoughts",
        "reasoning.include_thoughts",
        "generationConfig.thinkingConfig.includeThoughts",
        "generationConfig.thinkingConfig.include_thoughts",
        "generation_config.thinking_config.include_thoughts",
        "generation_config.thinking_config.includeThoughts",
    ] {
        if let Some(config) = summary_bool_config(v, path) {
            return Some(config);
        }
    }

    for path in ["reasoning.summary", "reasoning.generate_summary"] {
        if let Some(config) = responses_summary_config(v, path) {
            return Some(config);
        }
    }

    // reasoning.exclude is OpenRouter's "reason but hide" bit, not an OpenAI wire field;
    // include_reasoning is its legacy alias (include_reasoning:false == reasoning.exclude:true).
    // Only real JSON booleans count.
    let exclude = v.g("reasoning.exclude");
    if exclude.is_bool() {
        return Some(if exclude.bool() {
            SummaryConfig::disabled()
        } else {
            SummaryConfig::enabled("auto")
        });
    }
    let include = v.g("include_reasoning");
    if include.is_bool() {
        return Some(if include.bool() {
            SummaryConfig::enabled("auto")
        } else {
            SummaryConfig::disabled()
        });
    }
    // OpenRouter's reasoning.enabled turns reasoning on "with no exclusions", so it also decides
    // visibility when no dedicated bit was sent.
    let enabled = v.g("reasoning.enabled");
    if enabled.is_bool() {
        return Some(if enabled.bool() {
            SummaryConfig::enabled("auto")
        } else {
            SummaryConfig::disabled()
        });
    }
    None
}

fn first_summary_bool_config(v: &Value, paths: &[&str]) -> Option<SummaryConfig> {
    paths.iter().find_map(|path| summary_bool_config(v, path))
}

fn summary_bool_config(v: &Value, path: &str) -> Option<SummaryConfig> {
    match v.g(path).kind() {
        Kind::True => Some(SummaryConfig::enabled("auto")),
        Kind::False => Some(SummaryConfig::disabled()),
        _ => None,
    }
}

fn responses_summary_config(v: &Value, path: &str) -> Option<SummaryConfig> {
    let value = v.g(path);
    if !value.exists() {
        return None;
    }
    if value.is_null() {
        return Some(SummaryConfig::disabled());
    }
    let raw = value.as_str()?.trim().to_lowercase();
    match raw.as_str() {
        "auto" | "concise" | "detailed" => Some(SummaryConfig::enabled(&raw)),
        // Compatibility with clients that expose a none enum; the OpenAI wire representation
        // disables summaries by omitting the field.
        "none" => Some(SummaryConfig::disabled()),
        _ => None,
    }
}

fn claude_summary_config(v: &Value, path: &str) -> Option<SummaryConfig> {
    match v.g(path).as_str()?.trim().to_lowercase().as_str() {
        "summarized" => Some(SummaryConfig::enabled("auto")),
        "omitted" => Some(SummaryConfig::disabled()),
        _ => None,
    }
}

fn interactions_summary_config(v: &Value, path: &str) -> Option<SummaryConfig> {
    match v.g(path).as_str()?.trim().to_lowercase().as_str() {
        "auto" => Some(SummaryConfig::enabled("auto")),
        "none" => Some(SummaryConfig::disabled()),
        _ => None,
    }
}

/// Removes a globally inferred adaptive mode when the selected API-key model supports only manual
/// extended thinking. The exact model-aware summary pass can then activate enabled thinking with a
/// valid budget, or leave thinking absent when max_tokens cannot accommodate it.
pub(crate) fn strip_inferred_claude_summary_activation(
    body: Vec<u8>,
    model_info: Option<&ModelInfo>,
) -> Vec<u8> {
    let Some(support) = model_info.and_then(|m| m.thinking.as_ref()) else {
        return body;
    };
    if !support.levels.is_empty() || support.min <= 0 {
        return body;
    }
    let Some(mut v) = parse_valid(&body) else {
        return body;
    };
    if !v
        .g("thinking.type")
        .str()
        .trim()
        .eq_ignore_ascii_case("adaptive")
    {
        return body;
    }

    for path in [
        "thinking.type",
        "thinking.budget_tokens",
        "thinking.display",
        "output_config.effort",
    ] {
        cpa_json::delete(&mut v, path);
    }
    for path in ["thinking", "output_config"] {
        delete_if_empty_object(&mut v, path);
    }
    to_bytes(&v)
}

/// Activates valid Claude thinking so an enabled summary can be returned: adaptive for models with
/// levels, else a manual budget of the model minimum (only when max_tokens leaves room). Returns
/// whether the body was modified.
fn enable_claude_thinking_for_summary(
    v: &mut Value,
    model: &str,
    resolved: Option<&ModelInfo>,
) -> bool {
    let looked_up;
    let model_info = match resolved {
        Some(info) => Some(info),
        None => {
            let mut base_model = parse_suffix(model).model_name;
            if base_model.is_empty() {
                base_model = parse_suffix(&v.g("model").str()).model_name;
            }
            looked_up = lookup_model_info(&base_model, Some("claude"));
            looked_up.as_ref()
        }
    };
    let Some(support) = model_info.and_then(|m| m.thinking.as_ref()) else {
        return false;
    };

    if !support.levels.is_empty() {
        set(v, "thinking.type", "adaptive");
        cpa_json::delete(v, "thinking.budget_tokens");
        return true;
    }

    let budget = support.min;
    if budget <= 0 {
        return false;
    }
    let max_tokens = v.g("max_tokens");
    if max_tokens.exists() && max_tokens.int() <= budget {
        return false;
    }
    set(v, "thinking.type", "enabled");
    set(v, "thinking.budget_tokens", budget);
    true
}

fn normalized_summary_detail(detail: &str) -> &'static str {
    match detail.trim().to_lowercase().as_str() {
        "concise" => "concise",
        "detailed" => "detailed",
        _ => "auto",
    }
}

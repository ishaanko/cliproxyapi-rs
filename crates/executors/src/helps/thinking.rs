//! Glue between executors and the thinking pipeline (Go: helps/thinking.go, model_capabilities.go,
//! thinking_providers.go).
//!
//! The provider appliers (claude, gemini, antigravity, openai, codex, xai, kimi, interactions)
//! are built into `cpa_core::thinking`, so there is nothing to register here (Go registers them
//! from `thinking_providers.go` imports). Executors call [`apply_request_thinking`] after
//! translating the request.

use cpa_core::thinking::{
    SummaryConfig, SummaryMode, ThinkingError, apply_summary_config_for_model, apply_thinking_with_model_info_and_summary,
    apply_thinking_with_source_and_summary, apply_thinking_with_summary, extract_explicit_summary_config,
    extract_summary_config, extract_translated_summary_config,
};
use cpa_runtime::conductor::resolved_model_info;
use cpa_json::lazy::Doc;
use cpa_runtime::executor::{Options, Request};
use cpa_translator::Format;

/// Applies thinking configuration to the translated `body` while preserving reasoning-summary
/// visibility from the client payload (Go: ApplyThinkingWithSourcePayload).
///
/// `current_source_payload` is the payload that was translated, `original_source_payload` keeps
/// intent that an earlier interceptor removed.
pub fn apply_thinking_with_source_payload(
    body: &[u8],
    current_source_payload: &[u8],
    original_source_payload: &[u8],
    model: &str,
    from_format: &str,
    to_format: &str,
    provider_key: &str,
) -> Result<Vec<u8>, ThinkingError> {
    let summary = translated_request_summary_config(
        body,
        current_source_payload,
        original_source_payload,
        model,
        from_format,
        to_format,
    );
    apply_thinking_with_summary(body, model, from_format, to_format, provider_key, &summary)
}

/// Top-level keys the summary readers of `cpa_core::thinking` start their paths at, per protocol.
/// `None` for protocols without summary support (the readers return the default for those).
fn summary_roots(format: &str) -> Option<&'static [&'static str]> {
    Some(match format {
        "openai" => &["extra_body", "google", "thinking", "reasoning", "generationConfig", "generation_config", "include_reasoning", "reasoning_effort"],
        "openai-response" | "codex" => &["reasoning"],
        "claude" => &["thinking"],
        "gemini" => &["generationConfig", "generation_config"],
        "antigravity" => &["request"],
        "interactions" => &["generation_config", "reasoning"],
        _ => return None,
    })
}

/// False when `body` is a well-formed object without any key the summary readers of `format`
/// could look at, so reading it would return the default without needing a full parse.
fn may_carry_summary(body: &[u8], format: &str) -> bool {
    let Some(roots) = summary_roots(format) else {
        return true;
    };
    match Doc::lazy(body) {
        Some(doc) => roots.iter().any(|k| doc.has(k)),
        None => true,
    }
}

fn extract_summary(body: &[u8], format: &str) -> SummaryConfig {
    if may_carry_summary(body, format) { extract_summary_config(body, format) } else { SummaryConfig::default() }
}

fn extract_explicit_summary(body: &[u8], format: &str) -> SummaryConfig {
    if may_carry_summary(body, format) { extract_explicit_summary_config(body, format) } else { SummaryConfig::default() }
}

fn extract_translated_summary(body: &[u8], from: &str, to: &str) -> SummaryConfig {
    if may_carry_summary(body, from) { extract_translated_summary_config(body, from, to) } else { SummaryConfig::default() }
}

/// Summary visibility to carry into the target payload. The translated target body wins so a
/// request normalizer can remove or rewrite a canonical summary field; the original source is
/// consulted only when the payload that was translated no longer carries the inbound intent, or
/// when the target could not represent it until the model-aware pass (notably Claude).
pub fn translated_request_summary_config(
    body: &[u8],
    current_source_payload: &[u8],
    original_source_payload: &[u8],
    model: &str,
    from_format: &str,
    to_format: &str,
) -> SummaryConfig {
    let from_format = from_format.trim().to_lowercase();
    let to_format = to_format.trim().to_lowercase();

    let target_summary = if from_format == to_format {
        extract_summary(body, &to_format)
    } else {
        extract_explicit_summary(body, &to_format)
    };
    if target_summary.mode != SummaryMode::Unspecified {
        return target_summary;
    }

    // Each extraction parses a whole payload, so the original is read only when it can matter
    // (nothing in the translated source) and not at all when it is the same payload.
    let current = extract_translated_summary(current_source_payload, &from_format, &to_format);
    if current.mode == SummaryMode::Unspecified {
        if current_source_payload == original_source_payload {
            return current;
        }
        return extract_translated_summary(original_source_payload, &from_format, &to_format);
    }

    let has_transformer = match (Format::parse(&from_format), Format::parse(&to_format)) {
        (Some(from), Some(to)) => cpa_translator::global().has_request_transformer(from, to),
        _ => false,
    };
    if !has_transformer {
        // A missing translation must remain source-shaped. Same-format requests were handled
        // above, including explicit native aliases.
        return SummaryConfig::default();
    }

    let candidate = apply_summary_config_for_model(body.to_vec(), &to_format, model, &current);
    if extract_explicit_summary_config(&candidate, &to_format).mode != SummaryMode::Unspecified {
        // Registry translation applied this field before plugin normalization. If it is absent
        // now but can be represented on the normalized body, the normalizer deliberately removed
        // it and must remain authoritative.
        return SummaryConfig::default();
    }

    // Some intents cannot be represented until the final model-aware pass (for example Claude
    // display is invalid on disabled thinking, but a suffix can activate adaptive thinking).
    current
}

/// Applies thinking to a translated request, using the model capabilities the auth manager bound
/// to this execution attempt when present, else the registry lookup (Go: ApplyRequestThinking).
/// `normalized_updates_changed` says a plugin normalizer rewrote `configuration_update` items.
pub fn apply_request_thinking(
    body: &[u8],
    req: &Request,
    opts: &Options,
    from_format: &str,
    to_format: &str,
    provider: &str,
    normalized_updates_changed: bool,
) -> Result<Vec<u8>, ThinkingError> {
    let original_source: &[u8] = if opts.original_request.is_empty() { &req.payload } else { &opts.original_request };
    let source: &[u8] = if req.payload.is_empty() { &opts.original_request } else { &req.payload };
    let summary = translated_request_summary_config(body, &req.payload, original_source, &req.model, from_format, to_format);
    match resolved_model_info(req) {
        Some(resolved) => apply_thinking_with_model_info_and_summary(
            body,
            source,
            &req.model,
            from_format,
            to_format,
            provider,
            Some(&resolved.info),
            &summary,
            normalized_updates_changed,
        ),
        None => apply_thinking_with_source_and_summary(
            body,
            source,
            &req.model,
            from_format,
            to_format,
            provider,
            &summary,
            normalized_updates_changed,
        ),
    }
}

/// Whether the selected API-key model enables compatibility handling for Claude thinking blocks
/// (Go: APIKeyModelIsCompat).
pub fn api_key_model_is_compat(req: &Request) -> bool {
    resolved_model_info(req).is_some_and(|r| r.is_compat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_follows_target_then_source() {
        // An explicit target field wins over the source.
        let body = br#"{"reasoning":{"summary":"concise"}}"#;
        let summary = translated_request_summary_config(body, b"{}", b"{}", "gpt-5", "openai-response", "codex");
        assert_eq!(summary.mode, SummaryMode::Enabled);
        // With nothing on either side the intent stays unspecified.
        let none = translated_request_summary_config(b"{}", b"{}", b"{}", "gpt-5", "openai-response", "codex");
        assert_eq!(none.mode, SummaryMode::Unspecified);
    }

    #[test]
    fn unknown_formats_do_not_panic() {
        let out = apply_thinking_with_source_payload(b"{}", b"{}", b"{}", "m", "nope", "nada", "nada");
        assert_eq!(out.unwrap(), b"{}".to_vec());
    }
}

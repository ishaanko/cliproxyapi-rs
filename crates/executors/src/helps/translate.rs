//! Client request translation with the Codex client compatibility stages (Go:
//! helps/codex_multi_agent_v2.go `TranslateRequest*WithCodexMultiAgentV2` and
//! `TranslateRequest*WithAPIKeyModelCompatibility*`, which executors share).
//!
//! Stages, in order: integer tool schemas for Codex clients, the orphan delegation rewrite and
//! the multi-agent v2 input rewrite for Responses sources aimed at a non-Codex target, then the
//! translator itself (the compat Claude converters when the model is flagged for compatibility).
//! Plugin request normalizers do not exist in this port.

use cpa_config::Config;
use cpa_core::thinking::{apply_summary_config_for_model, extract_translated_summary_config};
use cpa_translator::{Ctx, Format, RequestEnvelope, translate_request_envelope};
use http::HeaderMap;

use super::codex_tool_integers::normalize_codex_tool_integer_types;
use crate::codex::multi_agent_v2::{rewrite_input, rewrite_orphan_delegation_input_for_config};

/// What a request translation depends on besides the payload itself.
pub struct RequestTranslation<'a> {
    /// Downstream request headers (Codex client detection, subagent marker).
    pub headers: &'a HeaderMap,
    pub cfg: Option<&'a Config>,
    /// Identifier of the executor the request is for; Codex targets skip the tool integer fix.
    pub target_executor: &'a str,
    pub from: Format,
    pub to: Format,
    /// The API-key model is flagged for compatibility (compat Claude converters, forced
    /// agent_message rewrite).
    pub is_compat: bool,
    pub ctx: Ctx,
    /// Model, stream flag and model info handed to the translator; `body` is replaced per call.
    pub envelope: RequestEnvelope,
}

impl<'a> RequestTranslation<'a> {
    pub fn new(headers: &'a HeaderMap, cfg: Option<&'a Config>, from: Format, to: Format, model: &str, stream: bool) -> Self {
        RequestTranslation {
            headers,
            cfg,
            target_executor: "",
            from,
            to,
            is_compat: false,
            ctx: Ctx::default(),
            envelope: RequestEnvelope { model: model.to_string(), stream, ..Default::default() },
        }
    }

    pub fn compat(mut self, is_compat: bool) -> Self {
        self.is_compat = is_compat;
        self
    }

    pub fn target_executor(mut self, executor: &'a str) -> Self {
        self.target_executor = executor;
        self
    }
}

/// Go: isCodexTargetExecutor.
fn is_codex_target_executor(executor: &str) -> bool {
    matches!(executor.trim().to_lowercase().as_str(), "codex" | "codex-websockets" | "codex_websockets")
}

type CompatConverter = fn(&str, &[u8], bool) -> Vec<u8>;

/// The compat-aware converter for a client/target pair (Go's switch in
/// TranslateRequestWithAPIKeyModelCompatibilityForExecutor).
fn compat_converter(from: Format, to: Format) -> Option<CompatConverter> {
    use cpa_translator::{claude, codex, gemini, interactions, openai};
    match (from, to) {
        (Format::Claude, Format::Codex) => Some(codex::claude::convert_claude_request_to_codex_with_compat),
        (Format::Claude, Format::Gemini) => Some(gemini::claude::convert_claude_request_to_gemini_with_compat),
        (Format::Claude, Format::Interactions) => Some(interactions::claude::convert_claude_request_to_interactions_with_compat),
        (Format::Claude, Format::OpenAI) => Some(openai::claude::convert_claude_request_to_openai_with_compat),
        (Format::OpenAI, Format::Claude) => Some(claude::openai::chat_completions::convert_openai_request_to_claude_with_compat),
        (Format::OpenAIResponse, Format::Claude) => Some(claude::openai::responses::convert_openai_responses_request_to_claude_with_compat),
        _ => None,
    }
}

/// Orphan delegation rewrite, then the multi-agent v2 input rewrite for non-Codex targets.
/// `compat_input` makes the latter unconditional (it also strips author/recipient/passthrough
/// metadata from every input item, which strict upstreams reject).
fn rewrite_responses_input(t: &RequestTranslation, body: Vec<u8>, compat_input: bool) -> Vec<u8> {
    if t.from != Format::OpenAIResponse {
        return body;
    }
    let body = rewrite_orphan_delegation_input_for_config(t.headers, &body, t.cfg);
    if t.to == Format::Codex || t.to == Format::OpenAIResponse {
        return body;
    }
    rewrite_input(t.headers, &body, t.cfg, compat_input)
}

/// Translates one payload; the flag reports that a plugin normalizer rewrote
/// `configuration_update` items (taken from the translator on the non-compat path only, as in Go).
pub fn translate_request(t: &RequestTranslation, payload: &[u8]) -> (Vec<u8>, bool) {
    let mut body = if is_codex_target_executor(t.target_executor) {
        payload.to_vec()
    } else {
        normalize_codex_tool_integer_types(payload, Some(t.headers))
    };
    let compat_path = t.is_compat && !(t.to == Format::Codex && t.from != Format::Claude);
    if compat_path {
        body = rewrite_responses_input(t, body, true);
        if let Some(convert) = compat_converter(t.from, t.to) {
            let translated = convert(&t.envelope.model, &body, t.envelope.stream);
            let summary = extract_translated_summary_config(&body, t.from.as_str(), t.to.as_str());
            return (apply_summary_config_for_model(translated, t.to.as_str(), &t.envelope.model, &summary), false);
        }
    }
    body = rewrite_responses_input(t, body, false);
    let env = translate_request_envelope(&t.ctx, t.from, t.to, RequestEnvelope { body, ..t.envelope.clone() });
    (env.body, !compat_path && env.configuration_updates_changed)
}

/// Translates the payload-config baseline and the working payload; identical inputs translate
/// once. The flag belongs to the working payload.
pub fn translate_request_pair(t: &RequestTranslation, original: &[u8], working: &[u8]) -> (Vec<u8>, Vec<u8>, bool) {
    let (original_translated, changed) = translate_request(t, original);
    if original == working {
        let copy = original_translated.clone();
        return (original_translated, copy, changed);
    }
    let (working_translated, changed) = translate_request(t, working);
    (original_translated, working_translated, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: &str = r#"{"input":[{"type":"message","role":"user","author":"a","recipient":"b","internal_chat_message_metadata_passthrough":{"x":1},"content":"hi"}]}"#;

    fn stripped(t: &RequestTranslation) -> bool {
        let out = String::from_utf8(rewrite_responses_input(t, INPUT.as_bytes().to_vec(), t.is_compat)).unwrap();
        !out.contains("author") && !out.contains("recipient") && !out.contains("passthrough")
    }

    #[test]
    fn compat_strips_codex_item_metadata_only_for_non_codex_targets() {
        let headers = HeaderMap::new();
        let to = |to, compat| RequestTranslation::new(&headers, None, Format::OpenAIResponse, to, "m", false).compat(compat);
        assert!(stripped(&to(Format::Claude, true)));
        assert!(!stripped(&to(Format::Claude, false)));
        assert!(!stripped(&to(Format::Codex, true)));
        assert!(!stripped(&to(Format::OpenAIResponse, true)));
    }
}

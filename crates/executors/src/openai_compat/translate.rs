//! Request translation entry points shared by the OpenAI-compatible and xAI executors (Go:
//! helps/codex_multi_agent_v2.go `TranslateRequest*WithAPIKeyModelCompatibility*` and
//! helps/claude_code_session.go `ClaudeCodePromptCache`).
//!
//! The Codex multi-agent v2 / orphan delegation rewrites (config `optimize-multi-agent-v2`) are
//! not ported; every other step matches Go.

use cpa_translator::{Ctx, Format, RequestEnvelope};
use http::HeaderMap;
use uuid::Uuid;

use crate::helps::codex_tool_integers::{is_codex_user_agent, normalize_codex_tool_integer_types};

/// Translates `payload` from the client format to the upstream one. With `is_compat` the
/// compatibility-aware Claude translators keep thinking blocks with unusable signatures. Returns
/// the body plus whether a normalizer rewrote `configuration_update` items.
pub fn translate_request(
    headers: &HeaderMap,
    from: Format,
    to: Format,
    model: &str,
    payload: &[u8],
    stream: bool,
    is_compat: bool,
) -> (Vec<u8>, bool) {
    let mut body = payload.to_vec();
    if is_codex_user_agent(Some(headers)) {
        body = normalize_codex_tool_integer_types(&body, Some(headers));
    }
    if is_compat && from == Format::Claude {
        let compat: Option<fn(&str, &[u8], bool) -> Vec<u8>> = match to {
            Format::Codex => Some(cpa_translator::codex::claude::convert_claude_request_to_codex_with_compat),
            Format::Gemini => Some(cpa_translator::gemini::claude::convert_claude_request_to_gemini_with_compat),
            Format::Interactions => {
                Some(cpa_translator::interactions::claude::convert_claude_request_to_interactions_with_compat)
            }
            Format::OpenAI => Some(cpa_translator::openai::claude::convert_claude_request_to_openai_with_compat),
            _ => None,
        };
        if let Some(convert) = compat {
            let translated = convert(model, &body, stream);
            let summary = cpa_core::thinking::extract_translated_summary_config(&body, from.as_str(), to.as_str());
            let translated = cpa_core::thinking::apply_summary_config_for_model(translated, to.as_str(), model, &summary);
            return (translated, false);
        }
    }
    let env = RequestEnvelope { model: model.to_string(), stream, body, ..Default::default() };
    let out = cpa_translator::translate_request_envelope(&Ctx::default(), from, to, env);
    (out.body, out.configuration_updates_changed)
}

/// Translates the pristine client payload and the working one (Go:
/// TranslateRequestPairWithAPIKeyModelCompatibilityAndUpdateIntent). The update-intent flag
/// belongs to the working payload.
#[allow(clippy::too_many_arguments)]
pub fn translate_request_pair(
    headers: &HeaderMap,
    from: Format,
    to: Format,
    model: &str,
    original: &[u8],
    request: &[u8],
    stream: bool,
    is_compat: bool,
) -> (Vec<u8>, Vec<u8>, bool) {
    let (original_translated, changed) = translate_request(headers, from, to, model, original, stream, is_compat);
    if original == request {
        let working = original_translated.clone();
        return (original_translated, working, changed);
    }
    let (working, changed) = translate_request(headers, from, to, model, request, stream, is_compat);
    (original_translated, working, changed)
}

const CLAUDE_CODE_SESSION_HEADER: &str = "x-claude-code-session-id";
const CLAUDE_CODE_AGENT_HEADER: &str = "x-claude-code-agent-id";

fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn claude_code_session_id_from_payload(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let parsed = cpa_json::parse(payload);
    let user_id = cpa_json::J::g(&parsed, "metadata.user_id").str();
    if user_id.is_empty() {
        return String::new();
    }
    // Go: `_session_([a-f0-9-]+)$`.
    if let Some(idx) = user_id.rfind("_session_") {
        let tail = &user_id[idx + "_session_".len()..];
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-') {
            return tail.to_string();
        }
    }
    if user_id.starts_with('{') {
        return cpa_json::J::g(&cpa_json::parse(user_id.as_bytes()), "session_id").str().trim().to_string();
    }
    String::new()
}

/// Go: ClaudeCodeExecutionScope, `claude:<session>:agent:<agent>`, `None` without a session.
pub fn claude_code_execution_scope(payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let mut session = header_value(headers, CLAUDE_CODE_SESSION_HEADER);
    if session.is_empty() {
        session = claude_code_session_id_from_payload(payload);
    }
    if session.is_empty() {
        return None;
    }
    let mut agent = header_value(headers, CLAUDE_CODE_AGENT_HEADER);
    if agent.is_empty() {
        agent = "main".into();
    }
    Some(format!("claude:{session}:agent:{agent}"))
}

/// Name-based (SHA-1, OID namespace) UUID, Go: `uuid.NewSHA1(uuid.NameSpaceOID, data)`.
pub fn uuid_sha1_oid(data: &[u8]) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, data).to_string()
}

/// Go: ClaudeCodePromptCache, the deterministic `prompt_cache_key` for one Claude Code agent.
pub fn claude_code_prompt_cache_id(model: &str, payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let model = model.trim();
    let scope = claude_code_execution_scope(payload, headers)?;
    if model.is_empty() {
        return None;
    }
    let identity = ["cli-proxy-api:codex:claude-code", model, &scope].join("\x00");
    Some(uuid_sha1_oid(identity.as_bytes()))
}

/// Go: helps.EndApplyPatchStream, usable from spawned tasks. The helps version holds `&Param`
/// across an await, which is not `Send` because `Param` is not `Sync`; this one decides before
/// awaiting. Sends the finalize frames, then the gateway error when the stream failed; returns
/// true when the caller must stop (failure or the client went away).
pub async fn end_apply_patch_stream(
    param: &mut cpa_translator::Param,
    reporter: &crate::helps::usage::UsageReporter,
    out: &crate::helps::apply_patch::ChunkSender,
    gateway_err: cpa_runtime::executor::ExecError,
) -> bool {
    use crate::helps::apply_patch::{apply_patch_translation_error, finalize_apply_patch_stream, record_apply_patch_stream_failure};
    let chunks = finalize_apply_patch_stream(param);
    record_apply_patch_stream_failure(param, reporter, &gateway_err);
    let failed = apply_patch_translation_error(param).is_some();
    for chunk in chunks {
        if out.send(Ok(bytes::Bytes::from(chunk))).await.is_err() {
            return true;
        }
    }
    if failed {
        let _ = out.send(Err(gateway_err)).await;
    }
    failed
}

/// `UsageReporter::observe_body_stream` for an owned reporter: the helps version borrows the
/// reporter in its return type, which cannot move into a spawned task. The first non-empty
/// chunk marks TTFT (`packet_only` marks only the first-packet fallback).
pub fn observe_body<S, E>(
    reporter: crate::helps::usage::UsageReporter,
    stream: S,
    packet_only: bool,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, E>>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>>,
{
    use futures_util::StreamExt;
    reporter.start_response_ttft();
    let mut marked = false;
    stream.inspect(move |item| {
        if !marked && item.as_ref().is_ok_and(|b| !b.is_empty()) {
            marked = true;
            if packet_only {
                reporter.record_first_packet();
            } else {
                reporter.mark_first_response_byte();
            }
        }
    })
}

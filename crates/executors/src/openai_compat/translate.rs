//! Claude Code session scope and prompt cache id derivation shared by the OpenAI-compatible and
//! xAI executors (Go: helps/claude_code_session.go `ClaudeCodePromptCache`).

use http::HeaderMap;
use uuid::Uuid;


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

/// Go: helps.StopApplyPatchStream, usable from spawned tasks (see [`end_apply_patch_stream`]).
/// Propagates a retained failure after its one translated frame; true when the stream failed.
pub async fn stop_apply_patch_stream(
    param: &mut cpa_translator::Param,
    reporter: &crate::helps::usage::UsageReporter,
    out: &crate::helps::apply_patch::ChunkSender,
    gateway_err: cpa_runtime::executor::ExecError,
) -> bool {
    use crate::helps::apply_patch::record_apply_patch_stream_failure;
    if !record_apply_patch_stream_failure(param, reporter, &gateway_err) {
        return false;
    }
    let _ = out.send(Err(gateway_err)).await;
    true
}

/// Metadata key naming a handler-level source type (`openai-image`, `openai-video`) that the
/// translator `Format` enum cannot express. Go carries these as `SourceFormat` strings; callers
/// that route image or video requests set this key.
pub const META_HANDLER_TYPE: &str = "handler_type";

/// Go: `opts.SourceFormat.String()` including the handler-level types.
pub fn source_handler_type(opts: &cpa_runtime::executor::Options) -> String {
    match opts.metadata.get(META_HANDLER_TYPE).and_then(serde_json::Value::as_str) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => opts.source_format.as_str().to_string(),
    }
}

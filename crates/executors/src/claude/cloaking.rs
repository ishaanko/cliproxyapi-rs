//! Cloaking: system prompt injection, billing header, current-date reminder, context management
//! and the fake `metadata.user_id` (Go: claude_executor_cloaking.go up to `withEphemeralCacheControl`;
//! the cache_control half of that file is in `cache_control`).
//!
//! JSON bodies are `&[u8]` in and `Vec<u8>` out like Go's `[]byte`; edits go through `cpa_json`.

use std::collections::HashMap;

use cpa_auth::Auth;
use cpa_config::Config;
use cpa_json::J;
use cpa_runtime::executor::{ErrorCode, ExecError};
use http::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::cache_control::strip_claude_cache_control_ttl;
use crate::helps::cloak_obfuscate::{build_sensitive_word_matcher, obfuscate_sensitive_words};
use super::helps::client_detection::{ClaudeCodeRequestDetection, detect_claude_code_request};
use super::helps::credential_identity::*;
use super::helps::device_profile::default_claude_version;
use super::helps::diagnostics::*;
use super::helps::{ClaudeContinuityContext, ClaudeCtx};
use super::policy::resolve_claude_wire_policy;
use super::request::{
    apply_claude_cloak_thinking_display, claude_model_uses_progress_display, is_claude_fable51_model,
    is_claude_opus55_model,
};
use crate::helps::status::status_err;

pub const FINGERPRINT_SALT: &str = "59cf53e54c78";
pub const CLAUDE_CODE_CLI_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
pub const CLAUDE_CODE_FABLE_REPORTING_OUTCOMES: &str = "# Reporting outcomes\n\nReport what actually happened, not what you intended. When you say something is done, sent, saved, fixed, or verified, that claim must rest on a result you observed in this session \u{2014} tool output, the file as it now reads, the page as it now loads \u{2014} not on what the step should have produced. If you did not check, say you did not check. If any step failed, was skipped, or came back different from what you expected, say so in the first sentence of your report, before anything else, even when the rest of the work succeeded. Never quietly work around a failure in a way that makes it look resolved; a problem the user can see is recoverable, one your summary hides is not. When you stop before the task is complete, your first line says so plainly and names what is left. Do not describe partial work as done, and do not let a summary read as more certain than the evidence behind it.";

/// Go: `claudeCodeContextManagement`.
pub const CLAUDE_CODE_CONTEXT_MANAGEMENT: &str = r#"{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#;

/// Resolves the incoming client headers and runs native client detection
/// (Go: detectIncomingClaudeCodeRequest). The gin-context merge of Go is not needed: the incoming
/// headers already are the request's headers.
pub fn detect_incoming_claude_code_request(
    incoming: &HeaderMap,
    payload: &[u8],
    count_tokens: bool,
    cfg: &Config,
) -> (HeaderMap, ClaudeCodeRequestDetection) {
    let resolved = incoming.clone();
    let detection = detect_claude_code_request(&resolved, payload, count_tokens, Some(cfg));
    (resolved, detection)
}

/// `X-CPA-Claude-Workload` of the request (Go: getWorkloadFromContext).
pub fn get_workload(incoming: Option<&HeaderMap>) -> String {
    incoming
        .and_then(|h| h.get("x-cpa-claude-workload"))
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// Generates and injects a fake `metadata.user_id` unless a valid one exists (Go: injectFakeUserID).
/// With `use_cache` false a new device id is generated for every call. In Home mode the cached
/// ids come from Home KV and a Home failure fails the request.
pub fn inject_fake_user_id(payload: &[u8], api_key: &str, use_cache: bool) -> Result<Vec<u8>, ExecError> {
    use crate::helps::id_cache::{
        cached_session_id_required_blocking, cached_user_id_required_blocking, generate_fake_user_id_with_session_id,
    };
    let generate = || {
        let id = if use_cache {
            cached_user_id_required_blocking(api_key)
        } else {
            cached_session_id_required_blocking(api_key).map(|s| generate_fake_user_id_with_session_id(&s))
        };
        id.map_err(|e| crate::helps::home_kv::exec_error(&e))
    };
    let mut root = cpa_json::parse(payload);
    if !root.g("metadata").exists() {
        cpa_json::set(&mut root, "metadata.user_id", generate()?);
        return Ok(cpa_json::to_vec(&root));
    }
    let existing = root.g("metadata.user_id").str();
    if existing.is_empty() || !crate::helps::id_cache::is_valid_user_id(&existing) {
        cpa_json::set(&mut root, "metadata.user_id", generate()?);
        return Ok(cpa_json::to_vec(&root));
    }
    Ok(payload.to_vec())
}

/// The 3-char build fingerprint embedded in `cc_version` (Go: computeFingerprint):
/// `SHA256(salt + text[4] + text[7] + text[20] + version)[:3]` over UTF-16 code units.
pub fn compute_fingerprint(message_text: &str, version: &str) -> String {
    let units: Vec<u16> = message_text.encode_utf16().collect();
    let mut sampled = [0u16; 3];
    for (slot, idx) in sampled.iter_mut().zip([4usize, 7, 20]) {
        *slot = units.get(idx).copied().unwrap_or(u16::from(b'0'));
    }
    let input = format!("{FINGERPRINT_SALT}{}{version}", String::from_utf16_lossy(&sampled));
    hex::encode(Sha256::digest(input.as_bytes()))[..3].to_string()
}

/// Optional pieces of the billing header beyond version and entrypoint.
#[derive(Debug, Clone, Default)]
pub struct BillingOptions<'a> {
    pub workload: &'a str,
    pub is_subagent: bool,
    pub prev_req: &'a str,
    pub prompt_id: &'a str,
    /// Only `"human"` is emitted.
    pub turn_origin: &'a str,
}

/// The `x-anthropic-billing-header` text block Claude Code prepends to its system prompt
/// (Go: generateBillingHeader). `cch` is present only on signed paths.
pub fn generate_billing_header(
    cch_signing: bool,
    version: &str,
    message_text: &str,
    entrypoint: &str,
    opts: &BillingOptions<'_>,
) -> String {
    let entrypoint = if entrypoint.is_empty() { "cli" } else { entrypoint };
    let build_hash = compute_fingerprint(message_text, version);
    let mut b = String::new();
    b.push_str("x-anthropic-billing-header: cc_version=");
    b.push_str(version);
    b.push('.');
    b.push_str(&build_hash);
    b.push_str("; cc_entrypoint=");
    b.push_str(entrypoint);
    b.push(';');
    if cch_signing {
        b.push_str(" cch=00000;");
    }
    if !opts.workload.is_empty() {
        b.push_str(" cc_workload=");
        b.push_str(opts.workload);
        b.push(';');
    }
    if opts.is_subagent {
        b.push_str(" cc_is_subagent=true;");
    }
    if cch_signing {
        if !opts.prev_req.is_empty() {
            b.push_str(" cc_prev_req=");
            b.push_str(opts.prev_req);
            b.push(';');
        }
        if !opts.prompt_id.is_empty() {
            b.push_str(" cc_prompt_id=");
            b.push_str(opts.prompt_id);
            b.push(';');
        }
        if opts.turn_origin == "human" {
            b.push_str(" cc_turn_origin=human;");
        }
    }
    b
}

/// Result of [`resolve_claude_continuity_tags`].
pub struct ContinuityTags {
    pub prev_req: String,
    pub prompt_id: String,
    pub ctx: ClaudeContinuityContext,
}

/// Resolves `cc_prev_req` / `cc_prompt_id` and the continuity context for a cloaked request
/// (Go: resolveClaudeContinuityTags). `None` when there is no session or credential identity.
pub fn resolve_claude_continuity_tags(
    ctx: &ClaudeCtx,
    cfg: &Config,
    auth: &Auth,
    incoming_headers: &HeaderMap,
    payload: &[u8],
    confirmed_claude_code: bool,
    existing_prev_req: &str,
    existing_prompt_id: &str,
) -> Option<ContinuityTags> {
    let has_execution_metadata = ctx.execution_metadata;
    let mut session_id = ctx.session_id.clone();
    if session_id.is_empty() {
        session_id =
            claude_agent_session_uuid_for_request(incoming_headers, payload, payload, confirmed_claude_code, &[]);
    }
    if session_id.is_empty() {
        return None;
    }
    let cred_identity = super::diagnostics::claude_diagnostics_credential_identity(auth);
    let is_new_turn = is_claude_new_prompt_turn(payload);
    let begun = begin_claude_continuity(&cred_identity, &session_id, is_new_turn, existing_prompt_id);
    let (continuity_key, seq, prev_msg_id, stored_prev_req, stored_prompt_id) =
        (begun.key, begun.sequence, begun.previous_message_id, begun.previous_request_id, begun.prompt_id);

    // The currentDate reminder is pinned to the session on its first request so a local-midnight
    // flip between requests cannot rewrite it and invalidate the prompt-cache prefix.
    let pinned_date = pin_claude_session_date(&continuity_key, &super::tz::claude_code_current_date(cfg, auth));

    let prompt_id = if !existing_prompt_id.is_empty() {
        existing_prompt_id.to_string()
    } else if !prev_msg_id.is_empty() && !stored_prompt_id.is_empty() && (has_execution_metadata || !is_new_turn) {
        stored_prompt_id.clone()
    } else if !has_execution_metadata {
        claude_deterministic_prompt_id(&format!("cpa:prompt:{}", claude_billing_fingerprint_message_text(payload)))
    } else {
        stored_prompt_id.clone()
    };

    let prev_req = if (has_execution_metadata || !existing_prev_req.is_empty()) && !stored_prev_req.is_empty() {
        stored_prev_req.clone()
    } else {
        existing_prev_req.to_string()
    };

    let mut c_ctx = ClaudeContinuityContext {
        key: continuity_key,
        sequence: seq,
        prompt_id: prompt_id.clone(),
        pinned_date,
        initialized: true,
        ..Default::default()
    };
    if has_execution_metadata || !existing_prev_req.is_empty() {
        c_ctx.previous_message_id = prev_msg_id;
        c_ctx.previous_request_id = stored_prev_req;
    }
    Some(ContinuityTags { prev_req, prompt_id, ctx: c_ctx })
}

/// The first user text used for the build fingerprint, skipping the injected date and context
/// reminders (Go: claudeBillingFingerprintMessageText).
pub fn claude_billing_fingerprint_message_text(payload: &[u8]) -> String {
    let root = crate::helps::parse_cache::parse(payload);
    billing_fingerprint_message_text(&root)
}

fn billing_fingerprint_message_text(root: &Value) -> String {
    let Some(idx) = first_claude_user_message_index_value(root) else {
        return String::new();
    };
    let content = root.g(&format!("messages.{idx}.content"));
    if content.is_string() {
        return content.str();
    }
    if content.is_array() {
        for part in content.array() {
            if part.g("type").str() == "text" {
                let text = part.g("text").str();
                if !is_claude_code_current_date_reminder(&text) && !is_claude_code_context_reminder(&text) {
                    return text;
                }
            }
        }
    }
    String::new()
}

/// Billing header for a signed body that lacks one (Go: claudeCCHFallbackBillingHeader).
pub fn claude_cch_fallback_billing_header(
    ctx: &ClaudeCtx,
    cfg: &Config,
    payload: &[u8],
    entrypoint: &str,
) -> String {
    let is_probe_or_helper = is_claude_probe_or_helper_request(payload);
    let (mut prev_req, mut prompt_id) = extract_claude_billing_tags(payload);
    if !is_probe_or_helper && let Some(continuity) = &ctx.continuity {
        let continuity = continuity.lock();
        if prev_req.is_empty() {
            prev_req = continuity.previous_request_id.clone();
        }
        if prompt_id.is_empty() {
            prompt_id = continuity.prompt_id.clone();
        }
    }
    let incoming = ctx.incoming_headers.clone().unwrap_or_default();
    let is_subagent = is_claude_subagent_request(&incoming, payload);
    generate_billing_header(
        true,
        &default_claude_version(Some(cfg)),
        &claude_billing_fingerprint_message_text(payload),
        entrypoint,
        &BillingOptions {
            workload: &get_workload(ctx.incoming_headers.as_ref()),
            is_subagent,
            prev_req: &prev_req,
            prompt_id: &prompt_id,
            turn_origin: "",
        },
    )
}

/// Keeps the top-level system in Claude Code's minimal CLI shape and relocates caller system
/// blocks (Go: checkSystemInstructionsWithSigningModeAt). `now_date` is the `YYYY-MM-DD` used by
/// the current-date reminder.
#[allow(clippy::too_many_arguments)]
pub fn check_system_instructions_with_signing_mode_at(
    payload: &[u8],
    strict_mode: bool,
    cch_signing: bool,
    version: &str,
    entrypoint: &str,
    now_date: &str,
    billing: &BillingOptions<'_>,
) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let system = root.g("system").value();
    let message_text = billing_fingerprint_message_text(&root);

    let billing_text = generate_billing_header(cch_signing, version, &message_text, entrypoint, billing);
    let mut system_blocks = vec![build_text_block(&billing_text, false), build_text_block(CLAUDE_CODE_CLI_IDENTITY, true)];
    let model = root.g("model").str().trim().to_lowercase();
    if is_claude_fable51_model(&model) && !is_claude_probe_or_helper_request(payload) {
        system_blocks.push(build_text_block(CLAUDE_CODE_FABLE_REPORTING_OUTCOMES, false));
    }
    cpa_json::set(&mut root, "system", Value::Array(system_blocks.clone()));
    if strict_mode {
        return inject_claude_code_current_date(&cpa_json::to_vec(&root), now_date);
    }

    let forwarded = collect_forwarded_claude_system_prompt_blocks(&system);
    if forwarded.is_empty() {
        return inject_claude_code_current_date(&cpa_json::to_vec(&root), now_date);
    }
    if claude_history_has_advisor_call_or_result(&root) {
        for block in &forwarded {
            system_blocks.push(build_text_block(block, false));
        }
        cpa_json::set(&mut root, "system", Value::Array(system_blocks));
        return inject_claude_code_current_date(&cpa_json::to_vec(&root), now_date);
    }
    let bytes = cpa_json::to_vec(&root);
    let bytes = if claude_uses_legacy_system_reminder(&root) {
        prepend_claude_system_reminders_to_first_user_message(&bytes, &forwarded)
    } else {
        // Unknown and future model IDs optimistically use the mid-conversation system role.
        insert_claude_mid_conversation_system_messages(&bytes, &forwarded)
    };
    inject_claude_code_current_date(&bytes, now_date)
}

/// Relocates caller system blocks into messages for a cloaked count_tokens request, which carries
/// only model, messages and tools (Go: relocateClaudeSystemPromptForCountTokens).
pub fn relocate_claude_system_prompt_for_count_tokens(payload: &[u8], strict_mode: bool) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let system = root.g("system");
    if !system.exists() {
        return payload.to_vec();
    }
    let system = system.value();
    // Strict mode drops caller prompts on the Messages path, so it must not reintroduce them here.
    let forwarded = if strict_mode { Vec::new() } else { collect_forwarded_claude_system_prompt_blocks(&system) };
    if forwarded.is_empty() {
        cpa_json::delete(&mut root, "system");
        return cpa_json::to_vec(&root);
    }
    if claude_history_has_advisor_call_or_result(&root) {
        // Keep them in top-level system so counting stays aligned with the Messages path.
        let blocks: Vec<Value> = forwarded.iter().map(|b| build_text_block(b, false)).collect();
        cpa_json::set(&mut root, "system", Value::Array(blocks));
        return cpa_json::to_vec(&root);
    }
    cpa_json::delete(&mut root, "system");
    let bytes = cpa_json::to_vec(&root);
    if claude_uses_legacy_system_reminder(&root) {
        return prepend_claude_system_reminders_to_first_user_message(&bytes, &forwarded);
    }
    insert_claude_mid_conversation_system_messages(&bytes, &forwarded)
}

/// Official model IDs and aliases that reject a mid-conversation `role=system` message
/// (Go: claudeLegacySystemReminderModels).
const CLAUDE_LEGACY_SYSTEM_REMINDER_MODELS: &[&str] = &[
    "claude-3-5-haiku-20241022",
    "claude-3-5-haiku-latest",
    "claude-3-7-sonnet-20250219",
    "claude-3-7-sonnet-latest",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "claude-opus-4",
    "claude-opus-4-20250514",
    "claude-opus-4-1",
    "claude-opus-4-1-20250805",
    "claude-opus-4-5",
    "claude-opus-4-5-20251101",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-sonnet-4",
    "claude-sonnet-4-20250514",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-6",
];

/// Whether the body's model predates mid-conversation system turns (Go: claudeUsesLegacySystemReminder).
pub fn claude_uses_legacy_system_reminder(root: &Value) -> bool {
    let mut model = root.g("model").str().trim().to_lowercase();
    if let Some(slash) = model.rfind('/') {
        model = model[slash + 1..].to_string();
    }
    !model.is_empty() && CLAUDE_LEGACY_SYSTEM_REMINDER_MODELS.contains(&model.as_str())
}

/// [`claude_uses_legacy_system_reminder`] over body bytes.
pub fn claude_uses_legacy_system_reminder_bytes(payload: &[u8]) -> bool {
    claude_uses_legacy_system_reminder(&crate::helps::parse_cache::parse(payload))
}

/// A caller system block Claude cannot carry in any system slot; request scoped
/// (Go: newClaudeCallerSystemBlockError).
pub fn new_claude_caller_system_block_error(index: usize, block_type: &str) -> ExecError {
    let block_type = if block_type.is_empty() { "unknown" } else { block_type };
    status_err(
        400,
        format!(
            "invalid_request_error: system.{index}.type: Input should be 'text'. System instructions support text only, but this block has type {block_type:?}. Move non-text content into a user message."
        ),
    )
    .with_code(ErrorCode::RequestScoped)
}

/// A mid-conversation system turn addressed to a model that cannot carry it; request scoped
/// (Go: newClaudeMidSystemMessageModelError).
pub fn new_claude_mid_system_message_model_error(model: &str) -> ExecError {
    let model = if model.is_empty() { "unknown" } else { model };
    status_err(
        400,
        format!(
            "invalid_request_error: role 'system' is not supported on this model. Model {model:?} predates mid-conversation system turns, so system instructions must stay in the top-level system field for it."
        ),
    )
    .with_code(ErrorCode::RequestScoped)
}

/// Rejects a first-party legacy-model request that carries a mid-conversation `role=system` turn
/// (Go: validateClaudeMidSystemMessageModel).
pub fn validate_claude_mid_system_message_model(
    payload: &[u8],
    confirmed_claude_code: bool,
    first_party_anthropic: bool,
) -> Result<(), ExecError> {
    if confirmed_claude_code || !first_party_anthropic {
        return Ok(());
    }
    let root = crate::helps::parse_cache::parse(payload);
    if !claude_uses_legacy_system_reminder(&root) || !super::body::claude_payload_has_mid_system_message(payload) {
        return Ok(());
    }
    Err(new_claude_mid_system_message_model_error(&root.g("model").str()))
}

/// Rejects caller system content that cannot keep its operator authority
/// (Go: validateClaudeCallerSystemBlocks).
pub fn validate_claude_caller_system_blocks(system: &Value) -> Result<(), ExecError> {
    let Value::Array(parts) = system else {
        // A string system prompt is text by definition.
        return Ok(());
    };
    let mut index = 0;
    for part in parts {
        let part_type = part.g("type").str();
        let part_type = part_type.trim();
        if part_type != "text" {
            return Err(new_claude_caller_system_block_error(index, part_type));
        }
        index += 1;
    }
    Ok(())
}

/// Caller system texts that survive relocation (Go: collectForwardedClaudeSystemPromptBlocks).
pub fn collect_forwarded_claude_system_prompt_blocks(system: &Value) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut append_text = |text: String| {
        if text.trim().is_empty()
            || cpa_core::util::is_claude_code_attribution_system_text(&text)
            || text == CLAUDE_CODE_CLI_IDENTITY
        {
            return;
        }
        blocks.push(text);
    };
    match system {
        Value::Array(parts) => {
            for part in parts {
                if part.g("type").str() == "text" {
                    append_text(part.g("text").str());
                }
            }
        }
        Value::String(s) => append_text(s.clone()),
        _ => {}
    }
    blocks
}

/// A text block with JSON.stringify-compatible HTML characters (Go: buildTextBlock); `ephemeral`
/// attaches `cache_control:{"type":"ephemeral"}`.
pub fn build_text_block(text: &str, ephemeral: bool) -> Value {
    if ephemeral {
        json!({"type": "text", "text": text, "cache_control": {"type": "ephemeral"}})
    } else {
        json!({"type": "text", "text": text})
    }
}

/// Go: prependClaudeSystemRemindersToFirstUserMessage.
pub fn prepend_claude_system_reminders_to_first_user_message(payload: &[u8], texts: &[String]) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let Some(first_user_idx) = first_claude_user_message_index_value(&root) else {
        return payload.to_vec();
    };
    if texts.is_empty() {
        return payload.to_vec();
    }
    let reminder_texts: Vec<String> = texts.iter().map(|t| claude_caller_system_reminder(t)).collect();
    let content_path = format!("messages.{first_user_idx}.content");
    let content = root.g(&content_path).value();
    match content {
        Value::Array(blocks) => {
            let mut existing: HashMap<String, usize> = HashMap::with_capacity(blocks.len());
            for block in &blocks {
                if block.g("type").str() == "text" {
                    *existing.entry(block.g("text").str()).or_insert(0) += 1;
                }
            }
            let mut reminder_blocks = Vec::with_capacity(reminder_texts.len());
            for reminder_text in &reminder_texts {
                if let Some(count) = existing.get_mut(reminder_text)
                    && *count > 0
                {
                    *count -= 1;
                    continue;
                }
                reminder_blocks.push(build_text_block(reminder_text, false));
            }
            if reminder_blocks.is_empty() {
                return payload.to_vec();
            }
            let mut insert_at = 0;
            while insert_at < blocks.len() && blocks[insert_at].g("type").str() == "tool_result" {
                insert_at += 1;
            }
            let mut raw_blocks = Vec::with_capacity(blocks.len() + reminder_blocks.len());
            for (idx, block) in blocks.into_iter().enumerate() {
                if idx == insert_at {
                    raw_blocks.append(&mut reminder_blocks);
                }
                raw_blocks.push(block);
            }
            raw_blocks.append(&mut reminder_blocks);
            cpa_json::set(&mut root, &content_path, Value::Array(raw_blocks));
        }
        Value::String(s) => {
            let mut raw_blocks: Vec<Value> = reminder_texts.iter().map(|t| build_text_block(t, false)).collect();
            raw_blocks.push(build_text_block(&s, false));
            cpa_json::set(&mut root, &content_path, Value::Array(raw_blocks));
        }
        _ => {}
    }
    cpa_json::to_vec(&root)
}

/// Go: claudeCallerSystemReminder.
pub fn claude_caller_system_reminder(text: &str) -> String {
    let mut reminder = String::from("<system-reminder>\n");
    reminder.push_str(text);
    if !text.ends_with('\n') {
        reminder.push('\n');
    }
    reminder.push_str("</system-reminder>");
    reminder
}

/// Whether messages contain an advisor call or result; Anthropic binds the encrypted advisor
/// result to the conversation layout, so no system splice may shift message indices
/// (Go: claudeHistoryHasAdvisorCallOrResult).
pub fn claude_history_has_advisor_call_or_result(root: &Value) -> bool {
    let messages = root.g("messages");
    if !messages.is_array() {
        return false;
    }
    for msg in messages.array() {
        let content = msg.g("content");
        if content.is_array() {
            for block in content.array() {
                match block.g("type").str().as_str() {
                    "advisor_tool_result" | "advisor_redacted_result" => return true,
                    "server_tool_use" => {
                        if block.g("name").str() == "advisor" {
                            return true;
                        }
                    }
                    "tool_result" => {
                        let inner = block.g("content");
                        if inner.is_array() {
                            if inner.array().iter().any(|b| b.g("type").str() == "advisor_redacted_result") {
                                return true;
                            }
                        } else if inner.is_object() && inner.g("type").str() == "advisor_redacted_result" {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        } else if content.is_object() {
            let block_type = content.g("type").str();
            if block_type == "advisor_tool_result" || block_type == "advisor_redacted_result" {
                return true;
            }
        }
    }
    false
}

/// Go: insertClaudeMidConversationSystemMessages.
pub fn insert_claude_mid_conversation_system_messages(payload: &[u8], texts: &[String]) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let Some(first_user_idx) = first_claude_user_message_index_value(&root) else {
        return payload.to_vec();
    };
    if texts.is_empty() {
        return payload.to_vec();
    }
    let Value::Array(message_blocks) = root.g("messages").value() else {
        return payload.to_vec();
    };
    let mut insert_at = first_user_idx + 1;
    while insert_at < message_blocks.len() && message_blocks[insert_at].g("role").str() == "user" {
        insert_at += 1;
    }
    if message_blocks.len() - insert_at >= texts.len() {
        let matches = texts.iter().enumerate().all(|(idx, text)| {
            let message = &message_blocks[insert_at + idx];
            message.g("role").str() == "system" && claude_message_content_text(&message.g("content").value()) == *text
        });
        if matches {
            return payload.to_vec();
        }
    }
    let system_messages: Vec<Value> = texts
        .iter()
        .map(|text| json!({"role": "system", "content": [build_text_block(text, true)]}))
        .collect();
    let mut raw_messages = Vec::with_capacity(message_blocks.len() + system_messages.len());
    let mut pending = Some(system_messages);
    for (idx, message) in message_blocks.into_iter().enumerate() {
        if idx == insert_at && let Some(mut inserted) = pending.take() {
            raw_messages.append(&mut inserted);
        }
        raw_messages.push(message);
    }
    if let Some(mut inserted) = pending.take() {
        raw_messages.append(&mut inserted);
    }
    cpa_json::set(&mut root, "messages", Value::Array(raw_messages));
    cpa_json::to_vec(&root)
}

/// Text of a message content: a string, or the `\n\n`-joined text blocks (Go: claudeMessageContentText).
pub fn claude_message_content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.g("type").str() == "text")
            .map(|b| b.g("text").str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Identifies only the `role=system` turns CPA itself inserted while cloaking, so a legacy-model
/// payload rewrite can undo exactly those (Go: claudeCodeSystemPlacementState).
#[derive(Debug, Clone, Default)]
pub struct ClaudeCodeSystemPlacementState {
    insert_at: usize,
    inserted_raw: Vec<String>,
    texts: Vec<String>,
}

/// Records CPA's modern-model system placement right after cloaking
/// (Go: captureClaudeCodeSystemPlacement).
pub fn capture_claude_code_system_placement(before: &[u8], after: &[u8], cloaked: bool) -> ClaudeCodeSystemPlacementState {
    let before_root = crate::helps::parse_cache::parse(before);
    if !cloaked || claude_uses_legacy_system_reminder(&before_root) {
        return ClaudeCodeSystemPlacementState::default();
    }
    let texts = collect_forwarded_claude_system_prompt_blocks(&before_root.g("system").value());
    if texts.is_empty() {
        return ClaudeCodeSystemPlacementState::default();
    }
    let after_root = crate::helps::parse_cache::parse(after);
    let before_res = before_root.g("messages");
    let after_res = after_root.g("messages");
    let before_messages = before_res.array();
    let after_messages = after_res.array();
    if after_messages.len() != before_messages.len() + texts.len() {
        return ClaudeCodeSystemPlacementState::default();
    }
    let Some(first_user_idx) = first_claude_user_message_index_value(&before_root) else {
        return ClaudeCodeSystemPlacementState::default();
    };
    let mut insert_at = first_user_idx + 1;
    while insert_at < before_messages.len() && before_messages[insert_at].g("role").str() == "user" {
        insert_at += 1;
    }
    if insert_at + texts.len() > after_messages.len() {
        return ClaudeCodeSystemPlacementState::default();
    }
    let mut inserted_raw = Vec::with_capacity(texts.len());
    for (idx, text) in texts.iter().enumerate() {
        let message = &after_messages[insert_at + idx];
        if message.g("role").str() != "system" || claude_message_content_text(&message.g("content").value()) != *text {
            return ClaudeCodeSystemPlacementState::default();
        }
        inserted_raw.push(message.raw());
    }
    ClaudeCodeSystemPlacementState { insert_at, inserted_raw, texts }
}

/// Repairs a stale placement decision when payload rules change the model from modern to legacy
/// (Go: reconcileClaudeCodeSystemPlacementAfterPayload). Fails closed when rules also changed
/// the tracked messages.
pub fn reconcile_claude_code_system_placement_after_payload(
    payload: &[u8],
    state: &ClaudeCodeSystemPlacementState,
) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    if state.inserted_raw.is_empty() || !claude_uses_legacy_system_reminder(&root) {
        return payload.to_vec();
    }
    let messages_res = root.g("messages");
    let messages = messages_res.array();
    if state.insert_at + state.inserted_raw.len() > messages.len() {
        return payload.to_vec();
    }
    for (idx, raw) in state.inserted_raw.iter().enumerate() {
        if messages[state.insert_at + idx].raw() != *raw {
            return payload.to_vec();
        }
    }
    let kept: Vec<Value> = messages
        .iter()
        .enumerate()
        .filter(|(idx, _)| *idx < state.insert_at || *idx >= state.insert_at + state.inserted_raw.len())
        .map(|(_, m)| m.value())
        .collect();
    drop(messages);
    drop(messages_res);
    cpa_json::set(&mut root, "messages", Value::Array(kept));
    prepend_claude_system_reminders_to_first_user_message(&cpa_json::to_vec(&root), &state.texts)
}

/// Model-specific additions made while cloaking (Go: claudeCodeFableState).
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeCodeFableState {
    pub injected_fallbacks: bool,
    pub injected_display: bool,
    pub injected_reporting: bool,
}

fn strip_zwsp(s: &str) -> String {
    s.replace('\u{200B}', "")
}

fn has_fable_reporting_block(root: &Value) -> bool {
    let system = root.g("system");
    if !system.is_array() {
        let s = strip_zwsp(&system.str());
        return s == CLAUDE_CODE_FABLE_REPORTING_OUTCOMES || s.contains(CLAUDE_CODE_FABLE_REPORTING_OUTCOMES);
    }
    system
        .array()
        .iter()
        .any(|blk| strip_zwsp(&blk.g("text").str()) == CLAUDE_CODE_FABLE_REPORTING_OUTCOMES)
}

/// Go: captureClaudeCodeFableState.
pub fn capture_claude_code_fable_state(before: &[u8], after: &[u8], cloaked: bool) -> ClaudeCodeFableState {
    if !cloaked || before.is_empty() || after.is_empty() {
        return ClaudeCodeFableState::default();
    }
    let (b, a) = (crate::helps::parse_cache::parse(before), crate::helps::parse_cache::parse(after));
    ClaudeCodeFableState {
        injected_fallbacks: !b.g("fallbacks").exists() && a.g("fallbacks").exists(),
        injected_display: !b.g("thinking.display").exists() && a.g("thinking.display").exists(),
        injected_reporting: !has_fable_reporting_block(&b) && has_fable_reporting_block(&a),
    }
}

fn remove_fable_reporting_block(root: &mut Value) {
    let system = root.g("system");
    if system.is_array() {
        let mut removed = false;
        let mut blocks = Vec::new();
        for blk in system.array() {
            if strip_zwsp(&blk.g("text").str()) == CLAUDE_CODE_FABLE_REPORTING_OUTCOMES {
                removed = true;
                continue;
            }
            blocks.push(blk.value());
        }
        drop(system);
        if removed {
            cpa_json::set(root, "system", Value::Array(blocks));
        }
    } else if strip_zwsp(&system.str()) == CLAUDE_CODE_FABLE_REPORTING_OUTCOMES {
        drop(system);
        cpa_json::delete(root, "system");
    }
}

/// Reconciles model-specific additions (Opus fallback, `thinking.display=updates`, the reporting
/// block) when payload rules rewrite the model between Fable 5.1 and other models
/// (Go: reconcileClaudeCodeFableModelAfterPayload).
pub fn reconcile_claude_code_fable_model_after_payload(
    body: Vec<u8>,
    fable_state: ClaudeCodeFableState,
    payload_touched_fallbacks: bool,
    payload_touched_display: bool,
    cloaked: bool,
    is_probe_or_helper: bool,
) -> Vec<u8> {
    if !cloaked || body.is_empty() {
        return body;
    }
    let mut root = cpa_json::parse(&body);

    // Probes and helpers never carry Fable additions.
    if is_probe_or_helper {
        if fable_state.injected_fallbacks && !payload_touched_fallbacks {
            cpa_json::delete(&mut root, "fallbacks");
        }
        if fable_state.injected_display && !payload_touched_display {
            cpa_json::delete(&mut root, "thinking.display");
        }
        if fable_state.injected_reporting {
            remove_fable_reporting_block(&mut root);
        }
        return cpa_json::to_vec(&root);
    }
    let current_model = root.g("model").str().trim().to_lowercase();
    // Never override a caller or payload-rule fallback; only replace one inserted by the cloak.
    if fable_state.injected_fallbacks && !payload_touched_fallbacks {
        let want_fallback = if is_claude_fable51_model(&current_model) {
            "claude-opus-5"
        } else if is_claude_opus55_model(&current_model) {
            "claude-opus-4-8"
        } else {
            ""
        };
        if root.g("fallbacks.0.model").str() != want_fallback {
            cpa_json::delete(&mut root, "fallbacks");
        }
    }

    if is_claude_fable51_model(&current_model) {
        if !root.g("fallbacks").exists() && !payload_touched_fallbacks {
            cpa_json::set(&mut root, "fallbacks", json!([{"model": "claude-opus-5"}]));
        }
        if root.g("thinking").exists() {
            let thinking_type = root.g("thinking.type").str();
            if thinking_type == "adaptive" && !root.g("thinking.display").exists() && !payload_touched_display {
                cpa_json::set(&mut root, "thinking.display", "updates");
            } else if thinking_type != "adaptive" && fable_state.injected_display && !payload_touched_display {
                cpa_json::delete(&mut root, "thinking.display");
            }
        } else if fable_state.injected_display && !payload_touched_display {
            cpa_json::delete(&mut root, "thinking.display");
        }
        if !has_fable_reporting_block(&root) {
            let system = root.g("system");
            let reporting = build_text_block(CLAUDE_CODE_FABLE_REPORTING_OUTCOMES, false);
            let new_system = if system.is_array() {
                let mut blocks: Vec<Value> = system.array().iter().map(|b| b.value()).collect();
                blocks.push(reporting);
                Some(blocks)
            } else if system.is_string() {
                Some(vec![build_text_block(&system.str(), false), reporting])
            } else if !system.exists() {
                Some(vec![reporting])
            } else {
                None
            };
            drop(system);
            if let Some(blocks) = new_system {
                cpa_json::set(&mut root, "system", Value::Array(blocks));
            }
        }
        return apply_claude_cloak_thinking_display(cpa_json::to_vec(&root), payload_touched_display);
    }

    if is_claude_opus55_model(&current_model) && !root.g("fallbacks").exists() && !payload_touched_fallbacks {
        cpa_json::set(&mut root, "fallbacks", json!([{"model": "claude-opus-4-8"}]));
    }
    let thinking_type = root.g("thinking.type").str().trim().to_lowercase();
    let thinking_active = thinking_type == "adaptive" || thinking_type == "enabled";
    // Payload rules can disable thinking without touching display; remove only CPA's own value.
    if fable_state.injected_display
        && !payload_touched_display
        && (!thinking_active || !claude_model_uses_progress_display(&current_model))
    {
        cpa_json::delete(&mut root, "thinking.display");
    }
    let mut out = cpa_json::to_vec(&root);
    out = apply_claude_cloak_thinking_display(out, payload_touched_display);
    if fable_state.injected_reporting {
        let mut root = cpa_json::parse(&out);
        remove_fable_reporting_block(&mut root);
        out = cpa_json::to_vec(&root);
    }
    out
}

/// Index of the first `role=user` message (Go: firstClaudeUserMessageIndex; `-1` becomes `None`).
pub fn first_claude_user_message_index(payload: &[u8]) -> Option<usize> {
    first_claude_user_message_index_value(&crate::helps::parse_cache::parse(payload))
}

fn first_claude_user_message_index_value(root: &Value) -> Option<usize> {
    let messages = root.g("messages");
    if !messages.is_array() {
        return None;
    }
    messages.array().iter().position(|msg| msg.g("role").str() == "user")
}

/// Go: isClaudeCodeContextReminder.
pub fn is_claude_code_context_reminder(text: &str) -> bool {
    text.starts_with("<system-reminder>") && text.contains("</system-reminder>")
}

/// Go: isClaudeCodeCurrentDateReminder.
pub fn is_claude_code_current_date_reminder(text: &str) -> bool {
    text.starts_with(
        "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is ",
    )
}

/// The current-date system reminder (Go: claudeCodeCurrentDateReminder).
pub fn claude_code_current_date_reminder(date: &str) -> String {
    format!(
        "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is {date}.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n\n"
    )
}

/// Stamps `{"type":"ephemeral"}` onto a content block (Go: withEphemeralCacheControl).
fn with_ephemeral_cache_control(mut block: Value) -> Value {
    cpa_json::set(&mut block, "cache_control", json!({"type": "ephemeral"}));
    block
}

/// Injects the leading current-date reminder into the first user message and marks the first real
/// user text block for caching (Go: injectClaudeCodeCurrentDate).
pub fn inject_claude_code_current_date(payload: &[u8], date: &str) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let Some(first_user_idx) = first_claude_user_message_index_value(&root) else {
        return payload.to_vec();
    };
    let content_path = format!("messages.{first_user_idx}.content");
    let content = root.g(&content_path).value();
    let date_block = build_text_block(&claude_code_current_date_reminder(date), false);

    if let Value::String(s) = &content {
        let user_block = build_text_block(s, true);
        cpa_json::set(&mut root, &content_path, Value::Array(vec![date_block, user_block]));
        return cpa_json::to_vec(&root);
    }
    let Value::Array(blocks) = content else {
        return payload.to_vec();
    };

    let mut raw_blocks = Vec::with_capacity(blocks.len() + 1);
    let mut actual_text_cached = false;
    for block in blocks {
        if block.g("type").str() == "text" {
            let text = block.g("text").str();
            if is_claude_code_current_date_reminder(&text) {
                continue;
            }
            if !actual_text_cached && !is_claude_code_context_reminder(&text) {
                raw_blocks.push(with_ephemeral_cache_control(block));
                actual_text_cached = true;
                continue;
            }
        }
        raw_blocks.push(block);
    }
    // The user message after an assistant tool_use turn must lead with its tool_result blocks,
    // so the reminder goes after them.
    let mut insert_at = 0;
    while insert_at < raw_blocks.len() && raw_blocks[insert_at].g("type").str() == "tool_result" {
        insert_at += 1;
    }
    raw_blocks.insert(insert_at, date_block);
    cpa_json::set(&mut root, &content_path, Value::Array(raw_blocks));
    cpa_json::to_vec(&root)
}

/// Whether `thinking` allows the clear_thinking strategy (Go: claudeThinkingAcceptsClearThinking).
fn claude_thinking_accepts_clear_thinking(root: &Value) -> bool {
    matches!(root.g("thinking.type").str().as_str(), "enabled" | "adaptive")
}

/// Supplies `context_management` when the caller omitted it (Go: injectClaudeCodeContextManagement).
pub fn inject_claude_code_context_management(payload: &[u8]) -> (Vec<u8>, bool) {
    let mut root = cpa_json::parse(payload);
    if root.g("context_management").exists() || !claude_thinking_accepts_clear_thinking(&root) {
        return (payload.to_vec(), false);
    }
    cpa_json::set(&mut root, "context_management", cpa_json::parse_str(CLAUDE_CODE_CONTEXT_MANAGEMENT));
    (cpa_json::to_vec(&root), true)
}

/// Go: claudeCodeContextManagementState.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeCodeContextManagementState {
    pub eligible: bool,
    pub caller_owned: bool,
    pub automatically_injected: bool,
    pub payload_rule_touched: bool,
}

/// Resolves automatic `context_management` ownership after payload rules and forced tool-choice
/// processing (Go: reconcileClaudeCodeContextManagement).
pub fn reconcile_claude_code_context_management(payload: &[u8], state: ClaudeCodeContextManagementState) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let context_management = root.g("context_management");
    if !claude_thinking_accepts_clear_thinking(&root) {
        if state.caller_owned || !state.automatically_injected || state.payload_rule_touched {
            return payload.to_vec();
        }
        if context_management.raw() != CLAUDE_CODE_CONTEXT_MANAGEMENT {
            return payload.to_vec();
        }
        drop(context_management);
        cpa_json::delete(&mut root, "context_management");
        return cpa_json::to_vec(&root);
    }
    if !state.eligible || state.caller_owned || state.payload_rule_touched || context_management.exists() {
        return payload.to_vec();
    }
    drop(context_management);
    cpa_json::set(&mut root, "context_management", cpa_json::parse_str(CLAUDE_CODE_CONTEXT_MANAGEMENT));
    cpa_json::to_vec(&root)
}

/// Applies the shared Messages wire policy: system prompt cloak, fallbacks, display, ttl strip and
/// identity (Go: applyCloakingInternal). Returns the body and whether cloaking ran.
#[allow(clippy::too_many_arguments)]
pub fn apply_cloaking_internal(
    ctx: &ClaudeCtx,
    cfg: &Config,
    auth: &Auth,
    payload: Vec<u8>,
    api_key: &str,
    confirmed_claude_code: bool,
    cch_signing: bool,
    obfuscate_sensitive_words_flag: bool,
) -> Result<(Vec<u8>, bool), ExecError> {
    let (policy, settings) = resolve_claude_wire_policy(cfg, auth, api_key, confirmed_claude_code);
    if !policy.cloak {
        return Ok((payload, false));
    }
    // Strict mode drops caller system prompts entirely, so an unusable block cannot lose information.
    if !settings.strict_mode {
        validate_claude_caller_system_blocks(&crate::helps::parse_cache::parse(&payload).g("system").value())?;
    }

    let billing_version = default_claude_version(Some(cfg));
    let workload = get_workload(ctx.incoming_headers.as_ref());

    let is_probe_or_helper = is_claude_probe_or_helper_request(&payload);
    let mut is_subagent = false;
    let mut prev_req = String::new();
    let mut prompt_id = String::new();
    let mut pinned_date = String::new();
    let mut incoming_headers = HeaderMap::new();
    if !is_probe_or_helper {
        incoming_headers = ctx.incoming_headers.clone().unwrap_or_default();
        is_subagent = is_claude_subagent_request(&incoming_headers, &payload);
        let (existing_prev_req, existing_prompt_id) = extract_claude_billing_tags(&payload);
        if let Some(tags) = resolve_claude_continuity_tags(
            ctx,
            cfg,
            auth,
            &incoming_headers,
            &payload,
            confirmed_claude_code,
            &existing_prev_req,
            &existing_prompt_id,
        ) {
            prev_req = tags.prev_req;
            prompt_id = tags.prompt_id;
            pinned_date = tags.ctx.pinned_date.clone();
            if let Some(continuity) = &ctx.continuity {
                *continuity.lock() = tags.ctx;
            }
        }
    }

    let turn_origin = if !is_probe_or_helper && !is_subagent { "human" } else { "" };
    // Without continuity state (probe/helper traffic, or no session identity) fall back to the
    // per-request date, which is the pre-pinning behaviour.
    let now_date = if pinned_date.is_empty() { super::tz::claude_code_current_date(cfg, auth) } else { pinned_date };
    let mut payload = check_system_instructions_with_signing_mode_at(
        &payload,
        settings.strict_mode,
        cch_signing,
        &billing_version,
        "cli",
        &now_date,
        &BillingOptions {
            workload: &workload,
            is_subagent,
            prev_req: &prev_req,
            prompt_id: &prompt_id,
            turn_origin,
        },
    );

    // Native Claude Code attaches model fallbacks to Opus 5.5 and Fable 5.1 requests.
    let mut root = cpa_json::parse(&payload);
    let model = root.g("model").str().trim().to_lowercase();
    let mut changed = false;
    if is_claude_opus55_model(&model) && !is_probe_or_helper && !root.g("fallbacks").exists() {
        cpa_json::set(&mut root, "fallbacks", json!([{"model": "claude-opus-4-8"}]));
        changed = true;
    }
    if is_claude_fable51_model(&model) && !is_probe_or_helper && !root.g("fallbacks").exists() {
        cpa_json::set(&mut root, "fallbacks", json!([{"model": "claude-opus-5"}]));
        changed = true;
    }
    if changed {
        payload = cpa_json::to_vec(&root);
    }
    if !is_probe_or_helper {
        payload = apply_claude_cloak_thinking_display(payload, false);
    }

    // Probes never use 1h cache in native Claude Code; subagents keep a caller-requested 1h ttl.
    if is_probe_or_helper || (is_subagent && !claude_subagent_requests_1h(&incoming_headers, &payload)) {
        payload = strip_claude_cache_control_ttl(&payload);
    }

    // CLI-profile identity is applied later through ApplyClaudeCredentialMetadata; other cloaking
    // keeps the legacy per-request fake user_id.
    if !policy.profile_claude_code_cli {
        payload = inject_fake_user_id(&payload, api_key, settings.cache_user_id)?;
    }

    if obfuscate_sensitive_words_flag && !settings.sensitive_words.is_empty() {
        let matcher = build_sensitive_word_matcher(&settings.sensitive_words);
        payload = obfuscate_sensitive_words(&payload, matcher.as_ref());
    }
    Ok((payload, true))
}

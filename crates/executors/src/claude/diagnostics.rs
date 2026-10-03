//! Request diagnostics / continuity wiring for the executor (Go: claude_executor_diagnostics.go).
//! The state store itself is `helps::diagnostics`.

use cpa_auth::Auth;
use cpa_json::J;
use serde_json::{Map, Value, json};

use super::helps::credential_identity::claude_credential_account_uuid;
use super::helps::diagnostics::{begin_claude_continuity, commit_claude_continuity};

/// What a request must commit once the upstream message id is known
/// (Go: claudeDiagnosticsRequestState).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeDiagnosticsRequestState {
    pub key: String,
    pub sequence: u64,
    pub prompt_id: String,
}

/// Injects `diagnostics` for a request that has no continuity context yet (Go: injectClaudeDiagnostics).
pub fn inject_claude_diagnostics(
    body: &[u8],
    auth: &Auth,
    session_id: &str,
) -> (Vec<u8>, ClaudeDiagnosticsRequestState) {
    let begun = begin_claude_continuity(&claude_diagnostics_credential_identity(auth), session_id, false, "");
    inject_claude_diagnostics_with_state(body, &begun.key, begun.sequence, &begun.previous_message_id, &begun.prompt_id)
}

/// Writes `diagnostics:{"previous_message_id":...}` right after `context_management` when present
/// (Go: injectClaudeDiagnosticsWithState).
pub fn inject_claude_diagnostics_with_state(
    body: &[u8],
    key: &str,
    sequence: u64,
    previous_message_id: &str,
    prompt_id: &str,
) -> (Vec<u8>, ClaudeDiagnosticsRequestState) {
    if key.is_empty() {
        return (body.to_vec(), ClaudeDiagnosticsRequestState::default());
    }
    let state = ClaudeDiagnosticsRequestState { key: key.to_string(), sequence, prompt_id: prompt_id.to_string() };
    let value = if previous_message_id.is_empty() {
        json!({"previous_message_id": null})
    } else {
        json!({"previous_message_id": previous_message_id})
    };
    let mut root = cpa_json::parse(body);
    if root.g("diagnostics").exists() {
        cpa_json::set(&mut root, "diagnostics", value);
        return (cpa_json::to_vec(&root), state);
    }
    if let Value::Object(map) = &mut root
        && map.contains_key("context_management")
    {
        let mut rebuilt = Map::with_capacity(map.len() + 1);
        for (k, v) in std::mem::take(map) {
            let after_context_management = k == "context_management";
            rebuilt.insert(k, v);
            if after_context_management {
                rebuilt.insert("diagnostics".to_string(), value.clone());
            }
        }
        *map = rebuilt;
        return (cpa_json::to_vec(&root), state);
    }
    if !cpa_json::set(&mut root, "diagnostics", value) {
        return (body.to_vec(), ClaudeDiagnosticsRequestState::default());
    }
    (cpa_json::to_vec(&root), state)
}

/// Records the finished request's message id for the next `previous_message_id`
/// (Go: commitClaudeContinuity).
pub fn commit_claude_continuity_state(state: &ClaudeDiagnosticsRequestState, message_id: &str, request_id: &str) {
    commit_claude_continuity(&state.key, state.sequence, message_id, request_id, &state.prompt_id);
}

/// Stable credential key for the continuity store (Go: claudeDiagnosticsCredentialIdentity).
pub fn claude_diagnostics_credential_identity(auth: &Auth) -> String {
    let id = auth.id.trim();
    if !id.is_empty() {
        return format!("id:{id}");
    }
    let index = auth.index.trim();
    if !index.is_empty() {
        return format!("index:{index}");
    }
    let device_ids = cpa_auth::claude::normalize_device_id_pool(auth.metadata.get(cpa_auth::claude::DEVICE_IDS_METADATA_KEY));
    if let Some(first) = device_ids.first() {
        return format!("device:{first}");
    }
    let account_uuid = claude_credential_account_uuid(auth);
    if !account_uuid.is_empty() {
        return format!("account:{account_uuid}");
    }
    String::new()
}

/// Go: claudeMessageIDFromResponse.
pub fn claude_message_id_from_response(data: &[u8]) -> String {
    cpa_json::parse(data).g("id").str().trim().to_string()
}

/// Tracks the upstream message id and completion across stream lines (Go: observeClaudeStreamLine).
pub fn observe_claude_stream_line(line: &[u8], message_id: &mut String, completed: &mut bool) {
    let line = crate::helps::text::trim_space(line);
    if !line.starts_with(b"data:") {
        return;
    }
    let payload = crate::helps::text::trim_space(&line[5..]);
    // Only message_start / message_stop matter; other events (the bulk of a stream) are decided
    // from the top-level `type` without validating or parsing the whole frame.
    if let Some(doc) = cpa_json::lazy::Doc::lazy(payload)
        && !matches!(doc.g("type").str().as_str(), "message_start" | "message_stop")
    {
        return;
    }
    if !cpa_json::valid(payload) {
        return;
    }
    let root = cpa_json::parse(payload);
    match root.g("type").str().as_str() {
        "message_start" => {
            let id = root.g("message.id").str();
            let id = id.trim();
            if !id.is_empty() {
                *message_id = id.to_string();
            }
        }
        "message_stop" => *completed = true,
        _ => {}
    }
}

/// Message id of a complete SSE body, only once `message_stop` was seen (Go: claudeMessageIDFromSSE).
pub fn claude_message_id_from_sse(data: &[u8]) -> String {
    let mut message_id = String::new();
    let mut completed = false;
    for line in data.split(|b| *b == b'\n') {
        observe_claude_stream_line(line, &mut message_id, &mut completed);
    }
    if completed { message_id } else { String::new() }
}

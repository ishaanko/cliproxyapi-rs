//! Codex `apply_patch` custom-tool bridging for upstreams without custom tools (Go:
//! common/apply_patch_events.go, apply_patch_input.go, apply_patch_responses.go).
//!
//! Responses `custom` tools named `apply_patch` are declared upstream as function tools with a
//! single `input` string argument; these helpers decode the streamed arguments back into custom
//! tool call events and rewrite requests/responses accordingly. Errors are `String` messages with
//! Go's text.

mod input;
mod responses;

pub use input::ApplyPatchInputDecoder;
pub use responses::{ApplyPatchResponsesBridge, normalize_apply_patch_responses_request};

use cpa_json::Value;

/// Owns the input decoder and identity of one tool call.
#[derive(Debug, Default)]
pub struct ApplyPatchCallState {
    pub item_id: String,
    pub call_id: String,
    pub name: String,
    pub namespace: String,
    pub output_index: i64,
    pub decoder: ApplyPatchInputDecoder,
}

impl ApplyPatchCallState {
    /// Decodes the next arguments fragment; returns the newly decoded input text.
    pub fn push_arguments(&mut self, fragment: &str) -> Result<String, String> {
        self.decoder.push(fragment)
    }

    /// Validates the complete arguments; returns the unsent suffix and the full decoded input.
    pub fn finish_arguments(&mut self, arguments: &str) -> Result<(String, String), String> {
        let tail = self.decoder.finish(arguments)?;
        Ok((tail, self.decoder.input().to_string()))
    }
}

fn event_identity(payload: &mut Value, s: &ApplyPatchCallState, sequence: i64) {
    cpa_json::set(payload, "item_id", s.item_id.as_str());
    cpa_json::set(payload, "call_id", s.call_id.as_str());
    cpa_json::set(payload, "output_index", s.output_index);
    cpa_json::set(payload, "sequence_number", sequence);
}

/// A Responses custom-tool input delta event payload (no SSE framing).
pub fn apply_patch_input_delta(s: &ApplyPatchCallState, delta: &str, sequence: i64) -> Vec<u8> {
    let mut payload = cpa_json::parse_str(
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"","call_id":"","output_index":0,"sequence_number":0,"delta":""}"#,
    );
    event_identity(&mut payload, s, sequence);
    cpa_json::set(&mut payload, "delta", delta);
    cpa_json::to_vec(&payload)
}

/// A Responses custom-tool input completion event payload (no SSE framing).
pub fn apply_patch_input_done(s: &ApplyPatchCallState, input: &str, sequence: i64) -> Vec<u8> {
    let mut payload = cpa_json::parse_str(
        r#"{"type":"response.custom_tool_call_input.done","item_id":"","call_id":"","output_index":0,"sequence_number":0,"input":""}"#,
    );
    event_identity(&mut payload, s, sequence);
    cpa_json::set(&mut payload, "input", input);
    cpa_json::to_vec(&payload)
}

/// A terminal Responses failure that does not expose upstream arguments.
pub fn apply_patch_failure(response_id: &str, sequence: i64) -> Vec<u8> {
    let mut payload = cpa_json::parse_str(
        r#"{"type":"response.failed","sequence_number":0,"response":{"id":"","object":"response","status":"failed","error":{"type":"server_error","code":"invalid_tool_arguments","message":"Invalid apply_patch tool arguments received from upstream.","param":null}}}"#,
    );
    cpa_json::set(&mut payload, "response.id", response_id);
    cpa_json::set(&mut payload, "sequence_number", sequence);
    cpa_json::to_vec(&payload)
}

/// Retains a conversion error for the caller (Go's embedded `ApplyPatchErrorState`; also the
/// `ToolInputError()` contract the translator registry checks).
#[derive(Debug, Default, Clone)]
pub struct ApplyPatchErrorState {
    err: Option<String>,
}

impl ApplyPatchErrorState {
    /// Stores the original conversion error for executor handling.
    pub fn set_tool_input_error(&mut self, err: impl Into<String>) {
        self.err = Some(err.into());
    }

    /// The original conversion error, if any.
    pub fn tool_input_error(&self) -> Option<&str> {
        self.err.as_deref()
    }
}

//! Codex terminal-event handling: upstream failure classification, completed-output patching and
//! bootstrap-buffering predicates (Go: codex_executor_terminal.go, helps/codex_terminal_incomplete.go).
//!
//! Events are handed around as parsed `serde_json::Value`s (one parse per upstream frame); the raw
//! bytes of output items are kept when they feed a rebuilt `response.output` array.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ErrorCode, ExecError};

/// Message of the 408 raised when a stream ends without a terminal event.
pub const INCOMPLETE_STREAM_MESSAGE: &str =
    "stream error: stream disconnected before completion: stream closed before response.completed";

/// Message of the 502 raised for an empty `response.incomplete`.
pub const EMPTY_INCOMPLETE_STREAM_MESSAGE: &str =
    "stream error: upstream terminated with incomplete empty response (0 tokens)";

/// Frames a bootstrap may hold back (Go: codexBootstrapMaxBufferedFrames).
pub const BOOTSTRAP_MAX_BUFFERED_FRAMES: usize = 48;
/// Bytes a bootstrap may retain (Go: codexBootstrapMaxBufferedBytes).
pub const BOOTSTRAP_MAX_BUFFERED_BYTES: usize = 1 << 20;

/// `http.StatusText`.
pub fn status_text(status: u16) -> &'static str {
    http::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("")
}

/// Go `statusErr{code, msg}` carrying the upstream body for relay.
pub fn status_error(status: u16, msg: impl Into<String>) -> ExecError {
    let msg = msg.into();
    let msg = if msg.is_empty() { format!("status {status}") } else { msg };
    let mut err = ExecError::new(status, msg.clone());
    err.body = Some(Bytes::from(msg.into_bytes()));
    err
}

/// 408 for a stream that ended before `response.completed`; never cools the credential.
pub fn new_incomplete_stream_error() -> ExecError {
    status_error(408, INCOMPLETE_STREAM_MESSAGE).with_code(ErrorCode::RequestScoped)
}

/// 502 for `response.incomplete` with no output at all; never cools the credential.
pub fn new_empty_incomplete_stream_error() -> ExecError {
    status_error(502, EMPTY_INCOMPLETE_STREAM_MESSAGE).with_code(ErrorCode::RequestScoped)
}

// ---------------------------------------------------------------- output items

/// Collected `response.output_item.done` items of one response, by `output_index` with a list for
/// items that carry none.
#[derive(Debug, Default)]
pub struct OutputItems {
    pub by_index: BTreeMap<i64, Vec<u8>>,
    pub fallback: Vec<Vec<u8>>,
}

impl OutputItems {
    pub fn len(&self) -> usize {
        self.by_index.len() + self.fallback.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Records the item of a `response.output_item.done` event (Go: collectCodexOutputItemDone).
    /// `raw` is the event as received so the item keeps its original bytes.
    pub fn collect(&mut self, event: &impl J, raw: &[u8]) {
        let item = event.g("item");
        if !matches!(item.v(), Some(Value::Object(_) | Value::Array(_))) {
            return;
        }
        let item_raw = cpa_json::raw_at(raw, "item").map(|s| s.as_bytes().to_vec()).unwrap_or_else(|| item.raw().into_bytes());
        let index = event.g("output_index");
        if index.exists() {
            self.by_index.insert(index.int(), item_raw);
        } else {
            self.fallback.push(item_raw);
        }
    }
}

/// Rebuilds `response.output` of a completed event from the collected items, or hydrates missing
/// ids of the items it already has (Go: patchCodexCompletedOutput).
pub fn patch_completed_output(event: &[u8], items: &OutputItems) -> Vec<u8> {
    let mut ev = cpa_json::parse(event);
    let output = ev.g("response.output");
    let output_len = output.array().len();
    if output.is_array() && output_len > 0 {
        return hydrate_completed_output_item_ids(event, &mut ev, items);
    }
    if items.is_empty() {
        return event.to_vec();
    }
    let mut all: Vec<&[u8]> = items.by_index.values().map(Vec::as_slice).collect();
    all.extend(items.fallback.iter().map(Vec::as_slice));
    let array = crate::helps::payload::join_raw_json_array(&all);
    if cpa_json::set_raw(&mut ev, "response.output", &String::from_utf8_lossy(&array)).is_err() {
        return event.to_vec();
    }
    cpa_json::to_vec(&ev)
}

fn hydrate_completed_output_item_ids(original: &[u8], ev: &mut Value, items: &OutputItems) -> Vec<u8> {
    let count = ev.g("response.output").array().len();
    let mut changed = false;
    for index in 0..count {
        let id = ev.g(&format!("response.output.{index}.id"));
        let has_id = id.exists() && !id.is_null() && (!id.is_string() || !id.str().trim().is_empty());
        if has_id {
            continue;
        }
        let Some(done_item) = items.by_index.get(&(index as i64)) else { continue };
        let done = cpa_json::parse(done_item);
        let done_id = done.g("id");
        if !done_id.is_string() || done_id.str().trim().is_empty() {
            continue;
        }
        if cpa_json::set(ev, &format!("response.output.{index}.id"), done_id.str()) {
            changed = true;
        }
    }
    if changed { cpa_json::to_vec(ev) } else { original.to_vec() }
}

// ---------------------------------------------------------------- deltas and empty incomplete

/// Whether an event carries non-blank generated content (Go: HasMeaningfulCodexOutputDelta).
pub fn has_meaningful_output_delta(event: &impl J) -> bool {
    match event.g("type").str().as_str() {
        "response.output_text.delta"
        | "response.reasoning_text.delta"
        | "response.reasoning_summary_text.delta"
        | "response.function_call_arguments.delta" => {
            let delta = event.g("delta");
            delta.exists() && !delta.str().trim().is_empty()
        }
        _ => false,
    }
}

/// Whether `response.incomplete` is a silent upstream abort: explicitly zero output tokens and no
/// output content at all (Go: IsCodexTerminalEmptyIncomplete).
pub fn is_terminal_empty_incomplete(event: &impl J, output_items: usize, saw_output_delta: bool) -> bool {
    if event.g("type").str() != "response.incomplete" {
        return false;
    }
    if saw_output_delta || output_items > 0 {
        return false;
    }
    let output = event.g("response.output");
    if output.is_array() && !output.array().is_empty() {
        return false;
    }
    let tokens = event.g("response.usage.output_tokens");
    let Some(Value::Number(n)) = tokens.v() else { return false };
    n.to_string().trim() == "0"
}

// ---------------------------------------------------------------- terminal failures

/// `{"error":{...}}` body of a terminal `error` / `response.failed` event, `None` for other
/// events (Go: codexTerminalFailureBody).
pub fn terminal_failure_body(event: &impl J) -> Option<Vec<u8>> {
    let body = match event.g("type").str().as_str() {
        "error" => terminal_error_body(event, "error").or_else(|| terminal_top_level_error_body(event)),
        "response.failed" => terminal_error_body(event, "response.error").or_else(|| terminal_error_body(event, "error")),
        _ => return None,
    };
    let mut body = match body {
        Some(b) => b,
        None => cpa_json::parse_str(r#"{"error":{"message":"upstream stream failed without error details"}}"#),
    };
    let seq = event.g("sequence_number");
    if seq.exists() {
        cpa_json::set(&mut body, "sequence_number", seq.int());
    }
    Some(cpa_json::to_vec(&body))
}

fn terminal_error_body(event: &impl J, path: &str) -> Option<Value> {
    let error = event.g(path);
    if !error.exists() {
        return None;
    }
    let mut body = cpa_json::parse_str(r#"{"error":{}}"#);
    if matches!(error.v(), Some(Value::Object(_) | Value::Array(_))) {
        cpa_json::set(&mut body, "error", error.value());
    } else {
        let message = error.str();
        if !message.trim().is_empty() {
            cpa_json::set(&mut body, "error.message", message.trim());
        }
    }
    let has_message = |b: &Value| !b.g("error.message").str().trim().is_empty();
    if !has_message(&body) {
        let message = event.g("response.error.message").str();
        if !message.trim().is_empty() {
            cpa_json::set(&mut body, "error.message", message.trim());
        }
    }
    if !has_message(&body) {
        let code = body.g("error.code").str();
        if !code.trim().is_empty() {
            cpa_json::set(&mut body, "error.message", code.trim());
        }
    }
    if !has_message(&body) {
        let error_type = body.g("error.type").str();
        if !error_type.trim().is_empty() {
            cpa_json::set(&mut body, "error.message", error_type.trim());
        }
    }
    Some(body)
}

fn terminal_top_level_error_body(event: &impl J) -> Option<Value> {
    let field = |name: &str| event.g(name).str().trim().to_string();
    let (message, code, error_type, param) = (field("message"), field("code"), field("error_type"), field("param"));
    if message.is_empty() && code.is_empty() && error_type.is_empty() && param.is_empty() {
        return None;
    }
    let mut body = cpa_json::parse_str(r#"{"error":{}}"#);
    if !message.is_empty() {
        cpa_json::set(&mut body, "error.message", message.as_str());
    }
    if !code.is_empty() {
        cpa_json::set(&mut body, "error.code", code.as_str());
    }
    if !error_type.is_empty() {
        cpa_json::set(&mut body, "error.type", error_type.as_str());
    }
    if !param.is_empty() {
        cpa_json::set(&mut body, "error.param", param.as_str());
    }
    if body.g("error.message").str().trim().is_empty() {
        if !code.is_empty() {
            cpa_json::set(&mut body, "error.message", code.as_str());
        } else if !error_type.is_empty() {
            cpa_json::set(&mut body, "error.message", error_type.as_str());
        }
    }
    Some(body)
}

/// A terminal failure event as an error plus the failure body it was built from (Go:
/// codexTerminalFailureErrWithCooling). Context-length, usage-limit, capacity and invalid-signature
/// bodies are classified as 400 first (usage-limit and capacity then become 429).
pub fn terminal_failure_err(event: &impl J, model_level_cooling: bool) -> Option<(ExecError, Vec<u8>)> {
    let body = terminal_failure_body(event)?;
    if terminal_stream_err_should_handle(&body) {
        return Some((new_status_err_with_cooling(400, &body, model_level_cooling), body));
    }
    let status = terminal_failure_status(&body);
    Some((new_status_err_with_cooling(status, &body, model_level_cooling), body))
}

fn terminal_failure_status(body: &[u8]) -> u16 {
    let parsed = cpa_json::parse(body);
    for path in ["error.status_code", "error.status"] {
        let status = parsed.g(path).int();
        if (400..=599).contains(&status) {
            return status as u16;
        }
    }
    let error_type = parsed.g("error.type").str().trim().to_lowercase();
    let error_code = parsed.g("error.code").str().trim().to_lowercase();
    match () {
        _ if error_code == "cyber_policy" => 400,
        _ if error_type == "not_found_error" || error_code == "not_found" || error_code == "model_not_found" => 404,
        _ if error_type == "authentication_error" || error_code == "invalid_api_key" || error_code == "unauthorized" => 401,
        _ if error_type == "permission_error" || error_code == "forbidden" || error_code == "permission_denied" => 403,
        _ if error_type == "rate_limit_error" || error_code == "rate_limit_exceeded" => 429,
        _ if error_type == "invalid_request_error" || error_type == "bad_request_error" => 400,
        _ => 502,
    }
}

fn terminal_stream_err_should_handle(body: &[u8]) -> bool {
    if terminal_error_is_context_length(body) || is_usage_limit_error(body) || is_model_capacity_error(body) {
        return true;
    }
    matches!(status_error_classification(400, body), Some((code, _)) if code == "thinking_signature_invalid")
}

fn terminal_error_is_context_length(body: &[u8]) -> bool {
    let parsed = cpa_json::parse(body);
    let code = parsed.g("error.code").str().trim().to_lowercase();
    let message = parsed.g("error.message").str().trim().to_lowercase();
    code == "context_length_exceeded"
        || code == "context_too_large"
        || message.contains("context window")
        || message.contains("context length")
        || message.contains("too many tokens")
}

// ---------------------------------------------------------------- status errors

/// Builds the executor error for an upstream status and body (Go: newCodexStatusErrWithCooling):
/// usage-limit and capacity bodies force 429, usage limits cool the whole credential unless model
/// cooling is on, and known failure shapes are rewritten to a normalized `{"error":{...}}` body.
pub fn new_status_err_with_cooling(status: u16, body: &[u8], model_level_cooling: bool) -> ExecError {
    let usage_limit = is_usage_limit_error(body);
    let mut code = status;
    if is_model_capacity_error(body) || usage_limit {
        code = 429;
    }
    let body = classify_status_error(code, body);
    let mut err = status_error(code, String::from_utf8_lossy(&body).into_owned());
    err.credential_scoped = usage_limit && !model_level_cooling;
    if let Some(retry) = parse_retry_after(code, &body, SystemTime::now()) {
        err.retry_after = Some(retry);
    }
    err
}

/// [`new_status_err_with_cooling`] without model-level cooling (Go: newCodexStatusErr).
pub fn new_status_err(status: u16, body: &[u8]) -> ExecError {
    new_status_err_with_cooling(status, body, false)
}

/// Rewrites recognised failure shapes to `{"error":{"message","type","code"}}`.
pub fn classify_status_error(status: u16, body: &[u8]) -> Vec<u8> {
    let Some((code, error_type)) = status_error_classification(status, body) else {
        return body.to_vec();
    };
    let parsed = cpa_json::parse(body);
    let mut message = parsed.g("error.message").str();
    if message.is_empty() {
        message = parsed.g("message").str();
    }
    if message.is_empty() {
        message = String::from_utf8_lossy(body).trim().to_string();
    }
    if message.is_empty() {
        message = status_text(status).to_string();
    }
    let mut out = cpa_json::parse_str(r#"{"error":{}}"#);
    cpa_json::set(&mut out, "error.message", message);
    cpa_json::set(&mut out, "error.type", error_type);
    cpa_json::set(&mut out, "error.code", code);
    cpa_json::to_vec(&out)
}

/// `(code, type)` of a recognised failure (Go: codexStatusErrorClassification).
pub fn status_error_classification(status: u16, body: &[u8]) -> Option<(&'static str, &'static str)> {
    let parsed = cpa_json::parse(body);
    let mut error_message = parsed.g("error.message").str().trim().to_lowercase();
    if error_message.is_empty() {
        error_message = parsed.g("message").str().trim().to_lowercase();
    }
    let lower = String::from_utf8_lossy(body).trim().to_lowercase();
    let upstream_code = parsed.g("error.code").str().trim().to_lowercase();
    let upstream_type = parsed.g("error.type").str().trim().to_lowercase();
    let is_invalid_request = upstream_type.is_empty() || upstream_type == "invalid_request_error";

    if status == 413
        || upstream_code == "context_length_exceeded"
        || upstream_code == "context_too_large"
        || (is_invalid_request
            && (error_message.contains("context length")
                || error_message.contains("context_length")
                || error_message.contains("maximum context")
                || error_message.contains("too many tokens")))
    {
        return Some(("context_too_large", "invalid_request_error"));
    }
    if lower.contains("invalid signature in thinking block") || lower.contains("invalid_encrypted_content") {
        return Some(("thinking_signature_invalid", "invalid_request_error"));
    }
    if upstream_code == "previous_response_not_found"
        || lower.contains("previous_response_not_found")
        || (lower.contains("previous_response_id") && lower.contains("not found"))
    {
        return Some(("previous_response_not_found", "invalid_request_error"));
    }
    if status == 401
        || upstream_type == "authentication_error"
        || upstream_code == "invalid_api_key"
        || lower.contains("invalid or expired token")
        || lower.contains("refresh_token_reused")
    {
        return Some(("auth_unavailable", "authentication_error"));
    }
    None
}

/// Model-at-capacity bodies (Go: isCodexModelCapacityError).
pub fn is_model_capacity_error(body: &[u8]) -> bool {
    if body.is_empty() {
        return false;
    }
    let parsed = cpa_json::parse(body);
    let candidates = [parsed.g("error.message").str(), parsed.g("message").str(), String::from_utf8_lossy(body).into_owned()];
    candidates.iter().any(|c| {
        let lower = c.trim().to_lowercase();
        !lower.is_empty()
            && (lower.contains("model is at capacity")
                || lower.contains("model_at_capacity")
                || lower.contains("model_is_at_capacity")
                || (lower.contains("model") && lower.contains("at capacity")))
    })
}

/// Quota exhaustion: `error.type` or top-level `type` is `usage_limit_reached` (Go:
/// isCodexUsageLimitError). Per-minute rate limits are deliberately excluded.
pub fn is_usage_limit_error(body: &[u8]) -> bool {
    if body.is_empty() {
        return false;
    }
    let parsed = cpa_json::parse(body);
    [parsed.g("error.type").str(), parsed.g("type").str()]
        .iter()
        .any(|c| c.trim().eq_ignore_ascii_case("usage_limit_reached"))
}

/// Cooldown for usage-limit 429s from `resets_at` / `resets_in_seconds` (Go: parseCodexRetryAfter).
pub fn parse_retry_after(status: u16, body: &[u8], now: SystemTime) -> Option<Duration> {
    if status != 429 || body.is_empty() {
        return None;
    }
    let parsed = cpa_json::parse(body);
    let error = parsed.g("error").value();
    for quota in [&error, &parsed] {
        if !quota.g("type").str().trim().eq_ignore_ascii_case("usage_limit_reached") {
            continue;
        }
        let resets_at = quota.g("resets_at").int();
        if resets_at > 0 {
            let at = UNIX_EPOCH + Duration::from_secs(resets_at as u64);
            if let Ok(delta) = at.duration_since(now)
                && !delta.is_zero()
            {
                return Some(delta);
            }
        }
        let in_seconds = quota.g("resets_in_seconds").int();
        if in_seconds > 0 {
            return Some(Duration::from_secs(in_seconds as u64));
        }
    }
    None
}

// ---------------------------------------------------------------- bootstrap buffering

/// 503 for a buffered overload rejection so the conductor fails over (Go:
/// newCodexBootstrapOverloadErr). The status is kept out of the shared mapping on purpose.
pub fn new_bootstrap_overload_err(body: &[u8]) -> ExecError {
    new_status_err(503, body)
}

/// Transient capacity rejections another credential may serve (Go: isCodexOverloadBootstrapFailure).
pub fn is_overload_bootstrap_failure(body: &[u8]) -> bool {
    if is_model_capacity_error(body) {
        return true;
    }
    let parsed = cpa_json::parse(body);
    let error_type = parsed.g("error.type").str().trim().to_lowercase();
    let error_code = parsed.g("error.code").str().trim().to_lowercase();
    let mut message = parsed.g("error.message").str().trim().to_lowercase();
    if message.is_empty() {
        message = parsed.g("message").str().trim().to_lowercase();
    }
    error_type == "service_unavailable_error"
        || error_code == "server_is_overloaded"
        || error_type == "rate_limit_error"
        || error_code == "rate_limit_exceeded"
        || ((error_type == "server_error" || error_code == "server_error") && message.contains("you can retry your request"))
}

/// Whether a frame may be held back before the downstream headers are committed: nothing
/// observable has happened yet (Go: isCodexBootstrapBufferableEvent). The list is closed on purpose.
pub fn is_bootstrap_bufferable_event(event_type: &str, payload: &[u8], event: &impl J) -> bool {
    if payload.iter().all(u8::is_ascii_whitespace) {
        return true;
    }
    match event_type {
        "response.created" | "response.in_progress" | "codex.rate_limits" | "codex.response.metadata" | "keepalive" => true,
        "response.output_item.added" => is_bufferable_output_item(event),
        "response.content_part.added" | "response.reasoning_summary_part.added" => is_empty_part(event),
        _ => false,
    }
}

fn is_bufferable_output_item(event: &impl J) -> bool {
    let item = event.g("item");
    match item.g("type").str().as_str() {
        "message" => is_empty_content_list(&item.g("content")),
        "reasoning" => {
            item.g("encrypted_content").str().is_empty()
                && is_empty_content_list(&item.g("summary"))
                && is_empty_content_list(&item.g("content"))
        }
        "function_call" => item.g("arguments").str().is_empty(),
        "custom_tool_call" => item.g("input").str().is_empty(),
        _ => false,
    }
}

fn is_empty_content_list(list: &cpa_json::Res<'_>) -> bool {
    list.array().iter().all(|entry| match entry.g("type").str().as_str() {
        "output_text" | "summary_text" | "text" | "reasoning_text" => entry.g("text").str().is_empty(),
        "refusal" => entry.g("refusal").str().is_empty(),
        _ => false,
    })
}

fn is_empty_part(event: &impl J) -> bool {
    let part = event.g("part");
    match part.g("type").str().as_str() {
        "output_text" | "summary_text" | "text" | "reasoning_text" => part.g("text").str().is_empty(),
        "refusal" => part.g("refusal").str().is_empty(),
        _ => false,
    }
}

/// `response.done` becomes `response.completed` (Go: normalizeCodexWebsocketCompletion).
pub fn normalize_completion(payload: &[u8]) -> Vec<u8> {
    let mut ev = cpa_json::parse(payload);
    if ev.g("type").str().trim() == "response.done" && cpa_json::set(&mut ev, "type", "response.completed") {
        return cpa_json::to_vec(&ev);
    }
    payload.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str) -> Value {
        cpa_json::parse(s.as_bytes())
    }

    #[test]
    fn usage_limit_forces_429_with_credential_scope_and_reset() {
        let body = br#"{"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":120}}"#;
        let err = new_status_err_with_cooling(400, body, false);
        assert_eq!(err.status, 429);
        assert!(err.credential_scoped);
        assert_eq!(err.retry_after, Some(Duration::from_secs(120)));
        assert!(!new_status_err_with_cooling(429, body, true).credential_scoped);
    }

    #[test]
    fn classifies_context_and_signature_failures() {
        let out = classify_status_error(400, br#"{"error":{"message":"maximum context length exceeded"}}"#);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"error":{"message":"maximum context length exceeded","type":"invalid_request_error","code":"context_too_large"}}"#
        );
        assert_eq!(status_error_classification(400, b"invalid_encrypted_content").unwrap().0, "thinking_signature_invalid");
        assert_eq!(status_error_classification(401, b"{}").unwrap().1, "authentication_error");
        assert!(status_error_classification(500, b"boom").is_none());
    }

    #[test]
    fn terminal_failure_maps_status_from_type_and_code() {
        let (err, _) = terminal_failure_err(&ev(r#"{"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"slow"}}}"#), false).unwrap();
        assert_eq!(err.status, 429);
        let (err, body) = terminal_failure_err(&ev(r#"{"type":"error","message":"nope","sequence_number":3}"#), false).unwrap();
        assert_eq!(err.status, 502);
        assert_eq!(String::from_utf8(body).unwrap(), r#"{"error":{"message":"nope"},"sequence_number":3}"#);
        let (err, _) = terminal_failure_err(&ev(r#"{"type":"error","error":{"code":"context_length_exceeded","message":"too long"}}"#), false).unwrap();
        assert_eq!(err.status, 400);
        assert!(terminal_failure_err(&ev(r#"{"type":"response.completed"}"#), false).is_none());
    }

    #[test]
    fn empty_incomplete_requires_literal_zero_tokens() {
        let e = ev(r#"{"type":"response.incomplete","response":{"output":[],"usage":{"output_tokens":0}}}"#);
        assert!(is_terminal_empty_incomplete(&e, 0, false));
        assert!(!is_terminal_empty_incomplete(&e, 1, false));
        assert!(!is_terminal_empty_incomplete(&e, 0, true));
        let float = ev(r#"{"type":"response.incomplete","response":{"usage":{"output_tokens":0.0}}}"#);
        assert!(!is_terminal_empty_incomplete(&float, 0, false));
    }

    #[test]
    fn completed_output_is_rebuilt_in_index_order() {
        let mut items = OutputItems::default();
        let done_b = br#"{"type":"response.output_item.done","output_index":1,"item":{"id":"b","type":"message"}}"#;
        let done_a = br#"{"type":"response.output_item.done","output_index":0,"item":{"id":"a","type":"reasoning"}}"#;
        items.collect(&cpa_json::parse(done_b), done_b);
        items.collect(&cpa_json::parse(done_a), done_a);
        let out = patch_completed_output(br#"{"type":"response.completed","response":{"output":[]}}"#, &items);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"type":"response.completed","response":{"output":[{"id":"a","type":"reasoning"},{"id":"b","type":"message"}]}}"#
        );
        let hydrated = patch_completed_output(br#"{"type":"response.completed","response":{"output":[{"id":"","type":"reasoning"},{"id":"b"}]}}"#, &items);
        assert!(String::from_utf8(hydrated).unwrap().contains(r#""id":"a""#));
    }

    #[test]
    fn bootstrap_bufferable_list_is_closed() {
        let created = ev(r#"{"type":"response.created"}"#);
        assert!(is_bootstrap_bufferable_event("response.created", b"{}", &created));
        let item = ev(r#"{"type":"response.output_item.added","item":{"type":"message","content":[]}}"#);
        assert!(is_bootstrap_bufferable_event("response.output_item.added", b"{}", &item));
        let search = ev(r#"{"type":"response.output_item.added","item":{"type":"web_search_call"}}"#);
        assert!(!is_bootstrap_bufferable_event("response.output_item.added", b"{}", &search));
        assert!(!is_bootstrap_bufferable_event("response.output_text.delta", b"{}", &created));
        assert!(is_bootstrap_bufferable_event("", b"  ", &created));
    }

    #[test]
    fn overload_failures_are_recognised() {
        assert!(is_overload_bootstrap_failure(br#"{"error":{"code":"server_is_overloaded"}}"#));
        assert!(is_overload_bootstrap_failure(br#"{"error":{"type":"server_error","message":"You can retry your request"}}"#));
        assert!(!is_overload_bootstrap_failure(br#"{"error":{"type":"invalid_request_error"}}"#));
    }
}

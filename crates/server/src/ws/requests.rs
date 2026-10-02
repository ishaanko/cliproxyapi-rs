//! Request normalization for the Responses websocket (Go:
//! openai_responses_websocket_requests.go / _prewarm.go): turns `response.create` /
//! `response.append` frames into full HTTP-style Responses requests, merging the previous
//! request and response output when the client sends only a delta.

use cpa_json::J;
use serde_json::Value;

use crate::error::ErrorMessage;

pub const WS_REQUEST_TYPE_CREATE: &str = "response.create";
pub const WS_REQUEST_TYPE_APPEND: &str = "response.append";

const CODEX_LOCAL_COMPACTION_SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// `gjson Result.String()` of a metadata value, trimmed.
fn meta_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(other) => other.to_string().trim().to_string(),
    }
}

fn status_error(status: u16, message: impl Into<String>) -> ErrorMessage {
    ErrorMessage::new(status, message)
}

fn bad_request(message: &str) -> ErrorMessage {
    status_error(400, message)
}

/// `responsesWebsocketPreviousResponseNotFoundError`.
pub fn previous_response_not_found_error() -> ErrorMessage {
    status_error(
        409,
        r#"{"error":{"message":"Previous response is not available on this websocket; resend the full conversation input without previous_response_id","type":"invalid_request_error","code":"previous_response_not_found","param":"previous_response_id"}}"#,
    )
}

fn set_stream_true(v: &mut Value) {
    cpa_json::set(v, "stream", true);
}

fn parse(raw: &[u8]) -> Value {
    cpa_json::parse(raw)
}

fn fill_from_last_request(normalized: &mut Value, last_request: &Value) {
    if !normalized.g("model").exists() {
        let model = last_request.g("model").str().trim().to_string();
        if !model.is_empty() {
            cpa_json::set(normalized, "model", model);
        }
    }
    if !normalized.g("instructions").exists() {
        let instructions = last_request.g("instructions");
        if instructions.exists() {
            cpa_json::set(normalized, "instructions", instructions.value());
        }
    }
}

/// `normalizeResponsesWebsocketRequestWithIncrementalState`: `(request to execute, new last
/// request)`; the caller keeps its previous last request on error.
pub fn normalize_request(
    raw: &[u8],
    last_request: &[u8],
    last_response_output: &[u8],
    last_response_id: &str,
    pending_tool_call_ids: &[String],
    allow_incremental: bool,
    allow_compaction_bypass: bool,
) -> Result<(Vec<u8>, Vec<u8>), ErrorMessage> {
    let root = parse(raw);
    let request_type = root.g("type").str().trim().to_string();
    match request_type.as_str() {
        WS_REQUEST_TYPE_CREATE => {
            if last_request.is_empty() {
                return normalize_create(raw);
            }
            normalize_subsequent(
                raw,
                last_request,
                last_response_output,
                last_response_id,
                pending_tool_call_ids,
                allow_incremental,
                allow_compaction_bypass,
            )
        }
        WS_REQUEST_TYPE_APPEND => normalize_subsequent(
            raw,
            last_request,
            last_response_output,
            last_response_id,
            pending_tool_call_ids,
            allow_incremental,
            allow_compaction_bypass,
        ),
        other => Err(bad_request(&format!("unsupported websocket request type: {other}"))),
    }
}

/// `normalizeResponseCreateRequest`: first request of a socket.
pub fn normalize_create(raw: &[u8]) -> Result<(Vec<u8>, Vec<u8>), ErrorMessage> {
    let mut v = parse(raw);
    let input = v.g("input");
    if input.exists() && !input.is_array() {
        return Err(bad_request("websocket request requires array field: input"));
    }
    cpa_json::delete(&mut v, "type");
    set_stream_true(&mut v);
    if !v.g("input").exists() {
        cpa_json::set(&mut v, "input", Value::Array(vec![]));
    }
    if v.g("model").str().trim().is_empty() {
        return Err(bad_request("missing model in response.create request"));
    }
    let out = cpa_json::to_vec(&v);
    Ok((out.clone(), out))
}

fn normalize_subsequent(
    raw: &[u8],
    last_request: &[u8],
    last_response_output: &[u8],
    last_response_id: &str,
    pending_tool_call_ids: &[String],
    allow_incremental: bool,
    allow_compaction_bypass: bool,
) -> Result<(Vec<u8>, Vec<u8>), ErrorMessage> {
    if last_request.is_empty() {
        return Err(bad_request("websocket request received before response.create"));
    }
    let root = parse(raw);
    let next_input = root.g("input");
    if !next_input.exists() || !next_input.is_array() {
        return Err(bad_request("websocket request requires array field: input"));
    }
    let next_input_value = next_input.value();

    // Compaction can make clients replace local history with a new transcript; treating that as
    // an append would duplicate stale turn state.
    if should_replace_transcript(&root, &next_input_value) {
        let normalized = transcript_replacement(raw, last_request);
        return Ok((normalized.clone(), normalized));
    }

    let last = parse(last_request);
    if allow_incremental {
        let mut prev = root.g("previous_response_id").str().trim().to_string();
        if prev.is_empty() {
            if !input_satisfies_pending_tool_calls(&next_input_value, pending_tool_call_ids) {
                let normalized = transcript_replacement(raw, last_request);
                return Ok((normalized.clone(), normalized));
            }
            prev = last_response_id.trim().to_string();
        }
        if !prev.is_empty() {
            let mut normalized = root.clone();
            cpa_json::delete(&mut normalized, "type");
            cpa_json::set(&mut normalized, "previous_response_id", prev);
            fill_from_last_request(&mut normalized, &last);
            set_stream_true(&mut normalized);
            let out = cpa_json::to_vec(&normalized);
            return Ok((out.clone(), out));
        }
    }

    // A compact replay for a downstream that can consume it carries the canonical history; do
    // not merge it with stale state.
    let merged_input: Vec<Value> = if allow_compaction_bypass && input_contains_full_transcript(&next_input_value) {
        tracing::info!(
            "responses websocket: full transcript detected, skipping stale merge (input items={})",
            next_input_value.as_array().map(|a| a.len()).unwrap_or(0)
        );
        next_input_value.as_array().cloned().unwrap_or_default()
    } else {
        let append: Vec<Value> = if input_contains_full_transcript(&next_input_value) {
            input_without_compaction_items(&next_input_value)
        } else {
            next_input_value.as_array().cloned().unwrap_or_default()
        };
        merge_input(last_request, last_response_output, append).map_err(|e| bad_request(&e))?
    };

    let mut normalized = root.clone();
    cpa_json::delete(&mut normalized, "type");
    cpa_json::delete(&mut normalized, "previous_response_id");
    fill_from_last_request(&mut normalized, &last);
    set_stream_true(&mut normalized);
    cpa_json::set(&mut normalized, "input", Value::Array(merged_input));
    let out = cpa_json::to_vec(&normalized);
    Ok((out.clone(), out))
}

/// `shouldReplaceWebsocketTranscript`.
fn should_replace_transcript(root: &Value, next_input: &Value) -> bool {
    let request_type = root.g("type").str().trim().to_string();
    if request_type != WS_REQUEST_TYPE_CREATE && request_type != WS_REQUEST_TYPE_APPEND {
        return false;
    }
    let previous = root.g("previous_response_id");
    if !previous.str().trim().is_empty() {
        return false;
    }
    if !next_input.is_array() {
        return false;
    }
    if request_type == WS_REQUEST_TYPE_CREATE && !previous.exists() && input_has_codex_local_compaction_summary(next_input) {
        return true;
    }
    for item in next_input.as_array().into_iter().flatten() {
        match meta_string(item.get("type")).as_str() {
            "function_call" | "custom_tool_call" => return true,
            "message" if meta_string(item.get("role")) == "assistant" => return true,
            _ => {}
        }
    }
    false
}

fn local_compaction_message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| meta_string(p.get("type")) == "input_text")
            .map(|p| p.get("text").and_then(Value::as_str).unwrap_or("").to_string())
            .collect(),
        _ => String::new(),
    }
}

/// `inputHasCodexLocalCompactionSummary`.
fn input_has_codex_local_compaction_summary(input: &Value) -> bool {
    let Some(items) = input.as_array() else {
        return false;
    };
    let mut has_summary = false;
    for (index, item) in items.iter().enumerate() {
        let item_type = meta_string(item.get("type"));
        if item_type == "additional_tools" {
            let tools = item.get("tools");
            if index != 0 || meta_string(item.get("role")) != "developer" || !tools.is_some_and(Value::is_array) {
                return false;
            }
            for tool in tools.and_then(Value::as_array).into_iter().flatten() {
                if !tool.is_object() || meta_string(tool.get("type")).is_empty() {
                    return false;
                }
            }
            continue;
        }
        if !item_type.is_empty() && item_type != "message" {
            return false;
        }
        let role = meta_string(item.get("role"));
        if role != "user" && role != "developer" {
            return false;
        }
        if role == "user" && local_compaction_message_text(item).starts_with(&format!("{CODEX_LOCAL_COMPACTION_SUMMARY_PREFIX}\n")) {
            has_summary = true;
        }
    }
    has_summary
}

/// `inputSatisfiesPendingToolCalls`.
fn input_satisfies_pending_tool_calls(input: &Value, pending: &[String]) -> bool {
    if pending.is_empty() {
        return true;
    }
    let Some(items) = input.as_array() else {
        return false;
    };
    let outputs: std::collections::HashSet<String> = items
        .iter()
        .filter(|item| is_tool_call_output_type(&meta_string(item.get("type"))))
        .map(|item| meta_string(item.get("call_id")))
        .filter(|id| !id.is_empty())
        .collect();
    pending
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .all(|id| outputs.contains(id))
}

/// `normalizeResponseTranscriptReplacement`: self-contained replacement of the transcript.
pub fn transcript_replacement(raw: &[u8], last_request: &[u8]) -> Vec<u8> {
    let mut normalized = parse(raw);
    cpa_json::delete(&mut normalized, "type");
    cpa_json::delete(&mut normalized, "previous_response_id");
    fill_from_last_request(&mut normalized, &parse(last_request));
    set_stream_true(&mut normalized);
    cpa_json::to_vec(&normalized)
}

/// `inputContainsFullTranscript`: compaction markers mean the client sent the whole transcript.
pub fn input_contains_full_transcript(input: &Value) -> bool {
    input.as_array().is_some_and(|items| {
        items.iter().any(|item| {
            matches!(item.get("type").and_then(Value::as_str), Some("compaction" | "compaction_summary"))
        })
    })
}

fn input_without_compaction_items(input: &Value) -> Vec<Value> {
    input
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| !matches!(item.get("type").and_then(Value::as_str), Some("compaction" | "compaction_summary")))
        .cloned()
        .collect()
}

pub fn is_tool_call_type(t: &str) -> bool {
    matches!(t.trim(), "function_call" | "custom_tool_call")
}

pub fn is_tool_call_output_type(t: &str) -> bool {
    matches!(t.trim(), "function_call_output" | "custom_tool_call_output")
}

/// An input item with the metadata the merge needs.
#[derive(Debug, Clone)]
pub struct InputItem {
    pub raw: Value,
    pub item_type: String,
    pub id: String,
    pub call_id: String,
}

impl InputItem {
    pub fn new(raw: Value) -> Self {
        let (mut item_type, mut id, mut call_id) = (String::new(), String::new(), String::new());
        if let Value::Object(map) = &raw {
            for (key, value) in map {
                if key.eq_ignore_ascii_case("type") {
                    item_type = meta_string(Some(value));
                } else if key.eq_ignore_ascii_case("id") {
                    id = meta_string(Some(value));
                } else if key.eq_ignore_ascii_case("call_id") {
                    call_id = meta_string(Some(value));
                }
            }
        }
        InputItem { raw, item_type, id, call_id }
    }
}

/// Previous request `input` (`responsesWebsocketPreviousInputNoCopy`): last matching key wins,
/// must be an array or null.
fn previous_input(last_request: &[u8]) -> Result<Vec<Value>, String> {
    let root: Value = serde_json::from_slice(last_request).map_err(|e| format!("invalid previous request input: {e}"))?;
    match root {
        Value::Null => Ok(vec![]),
        Value::Object(map) => {
            let mut found: Option<&Value> = None;
            let mut invalid = false;
            for (key, value) in &map {
                if key.eq_ignore_ascii_case("input") {
                    found = Some(value);
                    if !matches!(value, Value::Null | Value::Array(_)) {
                        invalid = true;
                    }
                }
            }
            if invalid {
                return Err("invalid previous request input: json: cannot unmarshal into []json.RawMessage".into());
            }
            match found {
                Some(Value::Array(a)) => Ok(a.clone()),
                _ => Ok(vec![]),
            }
        }
        _ => Err("invalid previous request input: json: cannot unmarshal into object".into()),
    }
}

/// `mergeResponsesWebsocketInput`: previous input + previous response output + new input, with
/// duplicate tool calls and duplicate ids removed.
pub fn merge_input(last_request: &[u8], last_response_output: &[u8], append: Vec<Value>) -> Result<Vec<Value>, String> {
    let mut items: Vec<InputItem> = previous_input(last_request)?.into_iter().map(InputItem::new).collect();

    let trimmed = last_response_output.trim_ascii();
    if !trimmed.is_empty() && trimmed[0] == b'[' && cpa_json::valid(trimmed) {
        let response_input = parse(trimmed);
        if input_contains_full_transcript(&response_input) {
            items.retain(|item| item.item_type != "compaction_trigger");
        }
        if let Value::Array(a) = response_input {
            items.extend(a.into_iter().map(InputItem::new));
        }
    }
    items.extend(append.into_iter().map(InputItem::new));

    let items = dedupe_function_calls(items);
    let items = dedupe_input_items(items);
    Ok(items.into_iter().map(|i| i.raw).collect())
}

/// `dedupeResponsesWebsocketMergeFunctionCalls`: first occurrence of a call id wins.
pub fn dedupe_function_calls(items: Vec<InputItem>) -> Vec<InputItem> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|item| {
            if is_tool_call_type(&item.item_type) && !item.call_id.is_empty() {
                return seen.insert(item.call_id.clone());
            }
            true
        })
        .collect()
}

/// `dedupeResponsesWebsocketInputItems`: one item per id; the kept one preserves a call_id that
/// still has a matching output.
pub fn dedupe_input_items(items: Vec<InputItem>) -> Vec<InputItem> {
    use std::collections::{HashMap, HashSet};
    let referenced_call_ids: HashSet<&str> = items
        .iter()
        .filter(|i| is_tool_call_output_type(&i.item_type) && !i.call_id.is_empty())
        .map(|i| i.call_id.as_str())
        .collect();
    let mut keep_index: HashMap<&str, usize> = HashMap::new();
    let mut keep_referenced: HashMap<&str, bool> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if item.id.is_empty() {
            continue;
        }
        let referenced = !item.call_id.is_empty() && referenced_call_ids.contains(item.call_id.as_str());
        if !keep_index.contains_key(item.id.as_str()) {
            keep_index.insert(&item.id, index);
            keep_referenced.insert(&item.id, referenced);
            continue;
        }
        if referenced || !keep_referenced[item.id.as_str()] {
            keep_index.insert(&item.id, index);
            keep_referenced.insert(&item.id, referenced);
        }
    }
    let keep: Vec<bool> = items
        .iter()
        .enumerate()
        .map(|(i, item)| item.id.is_empty() || keep_index.get(item.id.as_str()) == Some(&i))
        .collect();
    items.into_iter().zip(keep).filter(|(_, k)| *k).map(|(i, _)| i).collect()
}

// ------------------------------------------------------------------ prewarm

/// `shouldHandleResponsesWebsocketPrewarmLocally` (HTTP-upstream mode): `generate:false`.
pub fn should_handle_prewarm_locally(raw: &[u8]) -> bool {
    let root = parse(raw);
    if root.g("type").str().trim() != WS_REQUEST_TYPE_CREATE {
        return false;
    }
    let generate = root.g("generate");
    generate.exists() && !generate.bool()
}

/// `normalizeResponsesWebsocketPrewarmFollowup`: materializes the warmup input first so the
/// remaining delta is not mistaken for a full replacement.
pub fn normalize_prewarm_followup(raw: &[u8], warmup_request: &[u8]) -> Result<(Vec<u8>, Vec<u8>), ErrorMessage> {
    let root = parse(raw);
    let request_type = root.g("type").str().trim().to_string();
    if request_type != WS_REQUEST_TYPE_CREATE && request_type != WS_REQUEST_TYPE_APPEND {
        return Err(bad_request(&format!("unsupported websocket request type: {request_type}")));
    }
    let input = root.g("input");
    if !input.is_array() {
        return Err(bad_request("websocket request requires array field: input"));
    }
    let append = input.value().as_array().cloned().unwrap_or_default();
    let merged = merge_input(warmup_request, b"[]", append).map_err(|e| bad_request(&e))?;
    let normalized = transcript_replacement(raw, warmup_request);
    let mut v = parse(&normalized);
    cpa_json::set(&mut v, "input", Value::Array(merged));
    let out = cpa_json::to_vec(&v);
    Ok((out.clone(), out))
}

/// `syntheticResponsesWebsocketPrewarmPayloads`: `response.created` + `response.completed`
/// answered locally for a `generate:false` warmup. Returns the payloads and the response id.
pub fn synthetic_prewarm_payloads(request_json: &[u8]) -> (Vec<Vec<u8>>, String) {
    let response_id = format!("resp_prewarm_{}", uuid::Uuid::new_v4());
    let created_at = chrono::Utc::now().timestamp();
    let model = parse(request_json).g("model").str().trim().to_string();

    let mut created = cpa_json::parse_str(
        r#"{"type":"response.created","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","background":false,"error":null,"output":[]}}"#,
    );
    cpa_json::set(&mut created, "response.id", response_id.clone());
    cpa_json::set(&mut created, "response.created_at", created_at);
    if !model.is_empty() {
        cpa_json::set(&mut created, "response.model", model.clone());
    }
    let mut completed = cpa_json::parse_str(
        r#"{"type":"response.completed","sequence_number":1,"response":{"id":"","object":"response","created_at":0,"status":"completed","background":false,"error":null,"output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0}}}"#,
    );
    cpa_json::set(&mut completed, "response.id", response_id.clone());
    cpa_json::set(&mut completed, "response.created_at", created_at);
    if !model.is_empty() {
        cpa_json::set(&mut completed, "response.model", model);
    }
    (vec![cpa_json::to_vec(&created), cpa_json::to_vec(&completed)], response_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: &[u8]) -> String {
        String::from_utf8(b.to_vec()).unwrap()
    }

    fn norm(raw: &str, last: &str, out: &str) -> Result<(String, String), ErrorMessage> {
        normalize_request(raw.as_bytes(), last.as_bytes(), out.as_bytes(), "", &[], false, false)
            .map(|(a, b)| (s(&a), s(&b)))
    }

    #[test]
    fn first_create_is_normalized() {
        let (n, last) = norm(r#"{"type":"response.create","model":"m","input":[{"type":"message"}]}"#, "", "[]").unwrap();
        assert_eq!(n, r#"{"model":"m","input":[{"type":"message"}],"stream":true}"#);
        assert_eq!(n, last);
        let (n, _) = norm(r#"{"type":"response.create","model":"m"}"#, "", "[]").unwrap();
        assert_eq!(n, r#"{"model":"m","stream":true,"input":[]}"#);
    }

    #[test]
    fn create_validation_errors() {
        let e = norm(r#"{"type":"response.create","model":"m","input":"x"}"#, "", "[]").unwrap_err();
        assert_eq!((e.status, e.text.as_str()), (400, "websocket request requires array field: input"));
        let e = norm(r#"{"type":"response.create","input":[]}"#, "", "[]").unwrap_err();
        assert_eq!(e.text, "missing model in response.create request");
        let e = norm(r#"{"type":"nope"}"#, "", "[]").unwrap_err();
        assert_eq!(e.text, "unsupported websocket request type: nope");
        let e = norm(r#"{"type":"response.append","input":[]}"#, "", "[]").unwrap_err();
        assert_eq!(e.text, "websocket request received before response.create");
    }

    #[test]
    fn append_merges_previous_input_and_response_output() {
        let last = r#"{"model":"m","instructions":"sys","input":[{"type":"message","role":"user","id":"u1"}],"stream":true}"#;
        let out = r#"[{"type":"function_call","call_id":"c1","name":"f","arguments":"{}"}]"#;
        let raw = r#"{"type":"response.append","previous_response_id":"x","input":[{"type":"function_call_output","call_id":"c1","output":"ok"}]}"#;
        let (n, last_new) = norm(raw, last, out).unwrap();
        assert_eq!(
            n,
            r#"{"input":[{"type":"message","role":"user","id":"u1"},{"type":"function_call","call_id":"c1","name":"f","arguments":"{}"},{"type":"function_call_output","call_id":"c1","output":"ok"}],"model":"m","instructions":"sys","stream":true}"#
        );
        assert_eq!(n, last_new);
    }

    #[test]
    fn transcript_with_assistant_message_replaces_history() {
        let last = r#"{"model":"m","input":[{"type":"message","role":"user","id":"old"}]}"#;
        let raw = r#"{"type":"response.create","input":[{"type":"message","role":"assistant","content":"a"}]}"#;
        let (n, _) = norm(raw, last, "[]").unwrap();
        assert_eq!(n, r#"{"input":[{"type":"message","role":"assistant","content":"a"}],"model":"m","stream":true}"#);
    }

    #[test]
    fn duplicate_calls_and_ids_are_deduped() {
        let items = vec![
            InputItem::new(serde_json::json!({"type":"function_call","call_id":"c","id":"a"})),
            InputItem::new(serde_json::json!({"type":"function_call","call_id":"c","id":"b"})),
            InputItem::new(serde_json::json!({"type":"message","id":"m","content":"first"})),
            InputItem::new(serde_json::json!({"type":"message","id":"m","content":"second"})),
        ];
        let out = dedupe_input_items(dedupe_function_calls(items));
        let ids: Vec<_> = out.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "m"]);
        assert_eq!(out[1].raw["content"], "second");
    }

    #[test]
    fn incremental_mode_uses_previous_response_id() {
        let last = r#"{"model":"m","instructions":"i","input":[]}"#;
        let raw = r#"{"type":"response.create","input":[{"type":"message","role":"user"}]}"#;
        let (n, _) = normalize_request(raw.as_bytes(), last.as_bytes(), b"[]", "resp_9", &[], true, false).unwrap();
        assert_eq!(
            s(&n),
            r#"{"input":[{"type":"message","role":"user"}],"previous_response_id":"resp_9","model":"m","instructions":"i","stream":true}"#
        );
    }

    #[test]
    fn prewarm_followup_materializes_warmup_input() {
        let warm = r#"{"model":"m","input":[{"type":"message","role":"user","id":"w"}],"stream":true}"#;
        let raw = r#"{"type":"response.create","previous_response_id":"resp_prewarm_1","input":[{"type":"message","role":"user","id":"n"}]}"#;
        let (n, _) = normalize_prewarm_followup(raw.as_bytes(), warm.as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&n).unwrap();
        assert_eq!(v["input"].as_array().unwrap().len(), 2);
        assert!(v.get("previous_response_id").is_none());
        assert_eq!(v["model"], "m");
    }

    #[test]
    fn prewarm_detection_and_payloads() {
        assert!(should_handle_prewarm_locally(br#"{"type":"response.create","generate":false}"#));
        assert!(!should_handle_prewarm_locally(br#"{"type":"response.create","generate":true}"#));
        assert!(!should_handle_prewarm_locally(br#"{"type":"response.append","generate":false}"#));
        let (payloads, id) = synthetic_prewarm_payloads(br#"{"model":"gpt-x"}"#);
        assert!(id.starts_with("resp_prewarm_"));
        let created: Value = serde_json::from_slice(&payloads[0]).unwrap();
        let completed: Value = serde_json::from_slice(&payloads[1]).unwrap();
        assert_eq!(created["type"], "response.created");
        assert_eq!(created["response"]["model"], "gpt-x");
        assert_eq!(completed["response"]["status"], "completed");
        assert_eq!(completed["response"]["id"], created["response"]["id"]);
    }

    #[test]
    fn local_compaction_summary_is_a_replacement() {
        let prefix = CODEX_LOCAL_COMPACTION_SUMMARY_PREFIX;
        let raw = format!(
            r#"{{"type":"response.create","input":[{{"type":"message","role":"user","content":"{prefix}\nsummary"}}]}}"#
        );
        let root = parse(raw.as_bytes());
        assert!(should_replace_transcript(&root, &root.g("input").value()));
        let normal = parse(br#"{"type":"response.create","input":[{"type":"message","role":"user","content":"hi"}]}"#);
        assert!(!should_replace_transcript(&normal, &normal.g("input").value()));
    }
}

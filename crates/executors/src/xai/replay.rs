//! Reasoning replay for stateless xAI Responses turns (Go: xai_reasoning_replay.go plus the
//! subset of codex_executor_reasoning.go it uses).
//!
//! The terminal `response.completed` output of a turn (encrypted reasoning, assistant message,
//! tool calls) is cached per session; the next request from a client that does not replay those
//! items itself gets them re-injected into `input`. Sessions are isolated by the downstream API
//! key so callers cannot share encrypted state by reusing a prompt cache key.

use std::collections::{HashMap, HashSet};

use cpa_core::cache::{
    XaiReasoningReplayStoreStatus, delete_xai_reasoning_replay_item_required, get_xai_reasoning_replay_items_required,
    store_xai_reasoning_replay_items,
};
use cpa_core::thinking::parse_suffix;
use cpa_json::{J, Kind, Value};
use cpa_runtime::executor::{Metadata, Options, Request, meta};
use cpa_translator::Format;
use http::HeaderMap;
use sha2::{Digest, Sha256};

use super::util::{at, s, ts};
use crate::helps::usage::META_CLIENT_API_KEY;
use crate::helps::session::{claude_code_execution_scope, header_value_case_insensitive, uuid_sha1_oid};

/// Metadata flag the client-facing layer sets when the downstream request arrived over a
/// websocket (Go: `cliproxyexecutor.DownstreamWebsocket(ctx)`).
pub const META_DOWNSTREAM_WEBSOCKET: &str = "downstream_websocket";

/// Model and session a replay entry is stored under (Go: xaiReasoningReplayScope).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayScope {
    pub model_name: String,
    pub session_key: String,
}

impl ReplayScope {
    pub fn valid(&self) -> bool {
        !self.model_name.trim().is_empty() && !self.session_key.trim().is_empty()
    }
}

fn metadata_string(metadata: &Metadata, key: &str) -> String {
    metadata.get(key).and_then(Value::as_str).map(|v| v.trim().to_string()).unwrap_or_default()
}

fn session_key_from_turn_metadata(turn_metadata: &str) -> String {
    let parsed = cpa_json::parse_str(turn_metadata);
    let prompt_cache_key = ts(&parsed, "prompt_cache_key");
    if !prompt_cache_key.is_empty() {
        return format!("prompt-cache:{prompt_cache_key}");
    }
    let window_id = ts(&parsed, "window_id");
    if !window_id.is_empty() {
        return format!("window:{window_id}");
    }
    String::new()
}

fn session_key_from_payload(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    session_key_from_json(&cpa_json::parse(payload))
}

fn session_key_from_json(parsed: &Value) -> String {
    let prompt_cache_key = ts(parsed, "prompt_cache_key");
    if !prompt_cache_key.is_empty() {
        return format!("prompt-cache:{prompt_cache_key}");
    }
    let window_id = ts(parsed, "client_metadata.x-codex-window-id");
    if !window_id.is_empty() {
        return format!("window:{window_id}");
    }
    let turn_metadata = ts(parsed, "client_metadata.x-codex-turn-metadata");
    if !turn_metadata.is_empty() {
        return session_key_from_turn_metadata(&turn_metadata);
    }
    String::new()
}

fn session_key_from_headers(headers: &HeaderMap) -> String {
    let turn_metadata = header_value_case_insensitive(headers, "X-Codex-Turn-Metadata");
    if !turn_metadata.is_empty() {
        let key = session_key_from_turn_metadata(&turn_metadata);
        if !key.is_empty() {
            return key;
        }
    }
    let window_id = header_value_case_insensitive(headers, "X-Codex-Window-Id");
    if !window_id.is_empty() {
        return format!("window:{window_id}");
    }
    for name in ["Session_id", "session_id", "Session-Id"] {
        let value = header_value_case_insensitive(headers, name);
        if !value.is_empty() {
            return format!("session-id:{value}");
        }
    }
    let conversation_id = header_value_case_insensitive(headers, "Conversation_id");
    if !conversation_id.is_empty() {
        return format!("conversation_id:{conversation_id}");
    }
    String::new()
}

/// Go: codexReasoningReplaySessionKey.
pub fn codex_reasoning_replay_session_key(from: Format, req: &Request, opts: &Options, body: &Value) -> String {
    if from == Format::Claude
        && let Some(key) = claude_code_execution_scope(&req.payload, &opts.headers)
    {
        return key;
    }
    for metadata in [&opts.metadata, &req.metadata] {
        let value = metadata_string(metadata, meta::EXECUTION_SESSION_ID);
        if !value.is_empty() {
            return format!("execution:{value}");
        }
    }
    let value = session_key_from_json(body);
    if !value.is_empty() {
        return value;
    }
    let value = session_key_from_payload(&req.payload);
    if !value.is_empty() {
        return value;
    }
    let value = session_key_from_headers(&opts.headers);
    if !value.is_empty() {
        return value;
    }
    if from == Format::OpenAI {
        let api_key = metadata_string(&opts.metadata, META_CLIENT_API_KEY);
        if !api_key.is_empty() {
            let identity = format!("cli-proxy-api:codex:prompt-cache:{api_key}");
            return format!("prompt-cache:{}", uuid_sha1_oid(identity.as_bytes()));
        }
    }
    String::new()
}

fn replay_enabled_for_source(from: Format) -> bool {
    matches!(from, Format::Claude | Format::OpenAIResponse)
}

/// Namespaces client-controlled session keys by the downstream API key (Go:
/// xaiReasoningReplayIsolateSessionKey). Without a caller key they are disabled, not shared.
fn isolate_session_key(opts: &Options, session_key: &str) -> String {
    let session_key = session_key.trim();
    if session_key.is_empty() {
        return String::new();
    }
    if session_key.starts_with("execution:") {
        return session_key.to_string();
    }
    let api_key = metadata_string(&opts.metadata, META_CLIENT_API_KEY);
    if api_key.is_empty() {
        return String::new();
    }
    let sum = Sha256::digest(api_key.as_bytes());
    format!("caller:{}:{session_key}", hex::encode(&sum[..8]))
}

/// Go: xaiReasoningReplayScopeFromRequest.
pub fn scope_from_request(from: Format, req: &Request, opts: &Options, body: &Value) -> ReplayScope {
    if !replay_enabled_for_source(from) {
        return ReplayScope::default();
    }
    // End-to-end websocket requests keep upstream previous_response_id state; replaying the
    // encrypted reasoning as input too would duplicate the turn.
    let downstream_ws = opts.metadata.get(META_DOWNSTREAM_WEBSOCKET).and_then(Value::as_bool).unwrap_or(false);
    if downstream_ws && !ts(&cpa_json::parse(&req.payload), "previous_response_id").is_empty() {
        return ReplayScope::default();
    }
    let session_key = codex_reasoning_replay_session_key(from, req, opts, body);
    ReplayScope {
        model_name: parse_suffix(&req.model).model_name,
        session_key: isolate_session_key(opts, &session_key),
    }
}

/// Go: applyXAIReasoningReplayCacheRequired. Injects cached replay items into `input`.
pub fn apply_reasoning_replay_cache(from: Format, req: &Request, opts: &Options, body: &mut Value) -> ReplayScope {
    let scope = scope_from_request(from, req, opts, body);
    if !scope.valid() {
        return scope;
    }
    let cached = match get_xai_reasoning_replay_items_required(&scope.model_name, &scope.session_key) {
        Ok(Some(cached)) => cached,
        Ok(None) => return scope,
        Err(err) => {
            tracing::warn!("xai reasoning replay cache read failed: {err}");
            return scope;
        }
    };
    let replay: Vec<Value> = filter_replay_items_for_input(body, &cached);
    if replay.is_empty() {
        return scope;
    }
    insert_replay_items(body, replay);
    scope
}

fn input_has_reasoning_encrypted_content(input: &[Value], encrypted: &str) -> bool {
    if encrypted.is_empty() {
        return false;
    }
    input.iter().any(|item| {
        ts(item, "type") == "reasoning"
            && item.g("encrypted_content").kind() == Kind::String
            && item.g("encrypted_content").str() == encrypted
    })
}

/// Go: filterXAIReasoningReplayItemsForInput.
fn filter_replay_items_for_input(body: &Value, cached: &[Vec<u8>]) -> Vec<Value> {
    let Some(input) = at(body, "input").and_then(Value::as_array) else { return Vec::new() };
    let replay: Vec<Value> = cached.iter().map(|raw| cpa_json::parse(raw)).collect();
    let last_assistant = input_last_assistant_message(input);
    let cached_assistant = replay.iter().find(|item| {
        ts(item, "type") == "message" && ts(item, "role").eq_ignore_ascii_case("assistant")
    });
    let assistant_message_matches = match (last_assistant, cached_assistant) {
        (Some(last), Some(cached)) => assistant_message_content_equal(&last.g("content"), &cached.g("content")),
        _ => false,
    };
    if last_assistant.is_some() && cached_assistant.is_some() && !assistant_message_matches {
        return Vec::new();
    }
    let mut existing_calls: HashSet<String> = HashSet::new();
    let mut existing_outputs: HashSet<String> = HashSet::new();
    for item in input {
        let item_type = ts(item, "type");
        if item_type == "function_call_output" || item_type == "custom_tool_call_output" {
            let call_id = ts(item, "call_id");
            if !call_id.is_empty() {
                existing_outputs.extend(replay_comparable_call_ids(&call_id));
            }
        }
        existing_calls.extend(replay_tool_call_keys(item));
    }
    let mut filtered = Vec::with_capacity(replay.len());
    for item in replay {
        match ts(&item, "type").as_str() {
            "reasoning" => {
                if input_has_reasoning_encrypted_content(input, &s(&item, "encrypted_content")) {
                    continue;
                }
            }
            "message" => {
                if assistant_message_matches {
                    continue;
                }
            }
            "function_call" | "custom_tool_call" => {
                let keys = replay_tool_call_keys(&item);
                if keys.is_empty() || keys.iter().any(|k| existing_calls.contains(k)) {
                    continue;
                }
                let call_id = ts(&item, "call_id");
                let has_matching_output = !call_id.is_empty()
                    && replay_comparable_call_ids(&call_id).iter().any(|c| existing_outputs.contains(c));
                if !has_matching_output {
                    continue;
                }
                existing_calls.extend(keys);
            }
            _ => continue,
        }
        filtered.push(item);
    }
    filtered
}

fn input_last_assistant_message(input: &[Value]) -> Option<&Value> {
    input.iter().rev().find(|item| {
        let item_type = ts(item, "type");
        (item_type.is_empty() || item_type == "message") && ts(item, "role").eq_ignore_ascii_case("assistant")
    })
}

#[derive(PartialEq, Eq)]
struct AssistantMessagePart {
    part_type: String,
    value: String,
}

fn assistant_message_content_equal(left: &cpa_json::Res<'_>, right: &cpa_json::Res<'_>) -> bool {
    match (assistant_message_parts(left), assistant_message_parts(right)) {
        (Some(l), Some(r)) => l == r,
        _ => false,
    }
}

fn assistant_message_parts(content: &cpa_json::Res<'_>) -> Option<Vec<AssistantMessagePart>> {
    if content.kind() == Kind::String {
        return Some(vec![AssistantMessagePart { part_type: "output_text".into(), value: content.str() }]);
    }
    if !content.is_array() {
        return None;
    }
    let mut parts = Vec::new();
    for part in content.array() {
        let part_type = part.g("type").str().trim().to_string();
        let field = match part_type.as_str() {
            "output_text" => "text",
            "refusal" => "refusal",
            _ => return None,
        };
        let text = part.g(field);
        if text.kind() != Kind::String {
            return None;
        }
        parts.push(AssistantMessagePart { part_type, value: text.str() });
    }
    (!parts.is_empty()).then_some(parts)
}

/// Go: cacheXAIReasoningReplayFromCompleted. Stores the replayable output items of a completed
/// response; a completed turn without replayable state clears the previous entry.
pub fn cache_reasoning_replay_from_completed(scope: &ReplayScope, completed: &Value) {
    if !scope.valid() {
        return;
    }
    let Some(output) = at(completed, "response.output").and_then(Value::as_array) else { return };
    let replay: Vec<Vec<u8>> = output
        .iter()
        .filter(|item| matches!(ts(item, "type").as_str(), "reasoning" | "message" | "function_call" | "custom_tool_call"))
        .map(cpa_json::to_vec)
        .collect();
    match store_xai_reasoning_replay_items(&scope.model_name, &scope.session_key, &replay) {
        XaiReasoningReplayStoreStatus::Stored => {}
        XaiReasoningReplayStoreStatus::NoReplayableState => {
            // A completed turn without cacheable reasoning must not leave a previous turn's
            // encrypted state to be injected later.
            if let Err(err) = delete_xai_reasoning_replay_item_required(&scope.model_name, &scope.session_key) {
                tracing::warn!("xai reasoning replay cache delete failed after non-replayable completed output: {err}");
            }
        }
        XaiReasoningReplayStoreStatus::BackendError => {
            tracing::debug!("xai reasoning replay cache store backend error; retaining previous entry");
        }
        XaiReasoningReplayStoreStatus::InvalidArgs => {}
    }
}

/// Go: clearXAIReasoningReplayAfterCompaction.
pub fn clear_reasoning_replay_after_compaction(scope: &ReplayScope) {
    if scope.valid()
        && let Err(err) = delete_xai_reasoning_replay_item_required(&scope.model_name, &scope.session_key)
    {
        tracing::warn!("xai reasoning replay cache delete failed after successful compaction: {err}");
    }
}

// ---------------------------------------------------------------- codex replay helpers

/// Go: insertCodexReasoningReplayItems. Places the replay items at the position the replayed
/// turn originally occupied.
fn insert_replay_items(body: &mut Value, replay: Vec<Value>) {
    let Some(input) = at(body, "input").and_then(Value::as_array) else { return };
    if replay.is_empty() {
        return;
    }
    let insert_index = replay_insert_index(input, &replay);
    let replay = align_replay_tool_call_ids(input, replay);
    let mut out: Vec<Value> = Vec::with_capacity(input.len() + replay.len());
    let mut replay = Some(replay);
    for (i, item) in input.iter().enumerate() {
        if i == insert_index
            && let Some(r) = replay.take()
        {
            out.extend(r);
        }
        out.push(item.clone());
    }
    if let Some(r) = replay.take() {
        out.extend(r);
    }
    cpa_json::set(body, "input", Value::Array(out));
}

fn is_tool_output_type(item_type: &str) -> bool {
    item_type == "function_call_output" || item_type == "custom_tool_call_output"
}

fn replay_insert_index(input: &[Value], replay: &[Value]) -> usize {
    let mut replay_call_ids: HashSet<String> = HashSet::new();
    for item in replay {
        let item_type = ts(item, "type");
        if item_type != "function_call" && item_type != "custom_tool_call" {
            continue;
        }
        replay_call_ids.extend(replay_comparable_call_ids(&s(item, "call_id")));
    }
    if !replay_call_ids.is_empty() {
        for (index, item) in input.iter().enumerate() {
            if !is_tool_output_type(&ts(item, "type")) {
                continue;
            }
            let call_id = ts(item, "call_id");
            if call_id.is_empty() || replay_call_ids.contains(&call_id) {
                return index;
            }
        }
    }
    for index in (0..input.len()).rev() {
        if let Some(role) = replay_message_role(&input[index])
            && role == "assistant"
        {
            return index;
        }
    }
    for (index, item) in input.iter().enumerate() {
        if should_insert_replay_before(item) {
            return index;
        }
    }
    input.len()
}

fn should_insert_replay_before(item: &Value) -> bool {
    match replay_message_role(item) {
        None => true,
        Some(role) => !matches!(role.as_str(), "developer" | "system"),
    }
}

fn replay_message_role(item: &Value) -> Option<String> {
    let item_type = ts(item, "type");
    let role = ts(item, "role").to_lowercase();
    if role.is_empty() || (!item_type.is_empty() && item_type != "message") {
        return None;
    }
    Some(role)
}

fn align_replay_tool_call_ids(input: &[Value], replay: Vec<Value>) -> Vec<Value> {
    let mut output_call_ids: HashMap<String, String> = HashMap::new();
    for item in input {
        if !is_tool_output_type(&ts(item, "type")) {
            continue;
        }
        let call_id = ts(item, "call_id");
        if call_id.is_empty() {
            continue;
        }
        for candidate in replay_comparable_call_ids(&call_id) {
            output_call_ids.insert(candidate, call_id.clone());
        }
    }
    if output_call_ids.is_empty() {
        return replay;
    }
    replay
        .into_iter()
        .map(|mut item| {
            let item_type = ts(&item, "type");
            if item_type != "function_call" && item_type != "custom_tool_call" {
                return item;
            }
            let call_id = ts(&item, "call_id");
            let output_call_id = replay_comparable_call_ids(&call_id)
                .iter()
                .find_map(|c| output_call_ids.get(c).filter(|v| !v.is_empty()).cloned())
                .unwrap_or_default();
            if !output_call_id.is_empty() && output_call_id != call_id {
                cpa_json::set(&mut item, "call_id", output_call_id);
            }
            item
        })
        .collect()
}

/// Go: codexReplayToolCallKeys.
fn replay_tool_call_keys(item: &Value) -> Vec<String> {
    let item_type = ts(item, "type");
    if item_type != "function_call" && item_type != "custom_tool_call" {
        return Vec::new();
    }
    replay_comparable_call_ids(&s(item, "call_id")).into_iter().map(|id| format!("{item_type}:{id}")).collect()
}

/// Go: codexReplayComparableCallIDs. The id and its Claude-visible sanitized form.
fn replay_comparable_call_ids(call_id: &str) -> Vec<String> {
    let call_id = call_id.trim();
    if call_id.is_empty() {
        return Vec::new();
    }
    let claude_visible = shorten_replay_call_id(&cpa_core::util::sanitize_claude_tool_id(call_id));
    if claude_visible.is_empty() || claude_visible == call_id {
        return vec![call_id.to_string()];
    }
    vec![call_id.to_string(), claude_visible]
}

fn shorten_replay_call_id(id: &str) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id.to_string();
    }
    let sum = Sha256::digest(id.as_bytes());
    let suffix = format!("_{}", hex::encode(&sum[..8]));
    let prefix_len = LIMIT as isize - suffix.len() as isize;
    if prefix_len <= 0 {
        return suffix[suffix.len() - LIMIT..].to_string();
    }
    // `id` is ASCII after sanitizing, so byte slicing is safe.
    format!("{}{suffix}", &id[..prefix_len as usize])
}

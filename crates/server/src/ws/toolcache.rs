//! Tool call / output caches that let the Responses websocket repair orphaned function calls
//! across turns (Go: openai_responses_websocket_toolcall_repair.go).
//!
//! Caches are keyed by the downstream session (`X-Client-Request-Id`, Codex turn metadata
//! `session_id`, `Session-Id`), bounded to 256 entries per session, and dropped when the last
//! socket of a session closes. (Go builds them with a zero TTL, i.e. entries never expire by
//! age; that is kept.)

use std::collections::{HashMap, VecDeque};

use axum::http::HeaderMap;
use cpa_json::J;
use once_cell::sync::Lazy;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use super::requests::{InputItem, dedupe_input_items, is_tool_call_output_type, is_tool_call_type};

const MAX_PER_SESSION: usize = 256;

#[derive(Default)]
struct SessionEntries {
    values: HashMap<String, Value>,
    order: VecDeque<String>,
}

/// `websocketToolOutputCache`: per-session LRU-by-insertion map of call id -> item.
#[derive(Default)]
pub struct ToolCache {
    sessions: Mutex<HashMap<String, SessionEntries>>,
}

impl ToolCache {
    pub fn record(&self, session_key: &str, call_id: &str, item: &Value) {
        let (session_key, call_id) = (session_key.trim(), call_id.trim());
        if session_key.is_empty() || call_id.is_empty() {
            return;
        }
        let mut sessions = self.sessions.lock();
        let entries = sessions.entry(session_key.to_string()).or_default();
        if !entries.values.contains_key(call_id) {
            entries.order.push_back(call_id.to_string());
        }
        entries.values.insert(call_id.to_string(), item.clone());
        while entries.order.len() > MAX_PER_SESSION {
            if let Some(evict) = entries.order.pop_front() {
                entries.values.remove(&evict);
            }
        }
    }

    pub fn get(&self, session_key: &str, call_id: &str) -> Option<Value> {
        let (session_key, call_id) = (session_key.trim(), call_id.trim());
        if session_key.is_empty() || call_id.is_empty() {
            return None;
        }
        self.sessions.lock().get(session_key)?.values.get(call_id).cloned()
    }

    fn delete_session(&self, session_key: &str) {
        let key = session_key.trim();
        if !key.is_empty() {
            self.sessions.lock().remove(key);
        }
    }
}

static OUTPUT_CACHE: Lazy<ToolCache> = Lazy::new(ToolCache::default);
static CALL_CACHE: Lazy<ToolCache> = Lazy::new(ToolCache::default);
static SESSION_REFS: Lazy<Mutex<HashMap<String, usize>>> = Lazy::new(Default::default);
/// Serializes cache commits against repairs (`defaultWebsocketToolCacheTransactionMu`).
static TRANSACTION: Lazy<RwLock<()>> = Lazy::new(|| RwLock::new(()));

/// `websocketDownstreamSessionKey`.
pub fn downstream_session_key(headers: &HeaderMap) -> String {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    let request_id = header("x-client-request-id");
    if !request_id.is_empty() {
        return request_id;
    }
    let metadata = header("x-codex-turn-metadata");
    if !metadata.is_empty() {
        let session = cpa_json::parse_str(&metadata).g("session_id").str().trim().to_string();
        if !session.is_empty() {
            return session;
        }
    }
    for name in ["session-id", "session_id"] {
        let v = header(name);
        if !v.is_empty() {
            return v;
        }
    }
    String::new()
}

/// `retainResponsesWebsocketToolCaches`.
pub fn retain_session(session_key: &str) {
    let _guard = TRANSACTION.write();
    let key = session_key.trim();
    if key.is_empty() {
        return;
    }
    *SESSION_REFS.lock().entry(key.to_string()).or_insert(0) += 1;
}

/// `releaseResponsesWebsocketToolCaches`: the last release drops the session's caches.
pub fn release_session(session_key: &str) {
    let _guard = TRANSACTION.write();
    let key = session_key.trim();
    if key.is_empty() {
        return;
    }
    let last = {
        let mut refs = SESSION_REFS.lock();
        match refs.get_mut(key) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            _ => {
                refs.remove(key);
                true
            }
        }
    };
    if last {
        OUTPUT_CACHE.delete_session(key);
        CALL_CACHE.delete_session(key);
    }
}

/// `isCompleteResponsesWebsocketToolCall`: named call with a string arguments/input payload.
pub fn is_complete_tool_call(item: &Value) -> bool {
    if !item.is_object() {
        return false;
    }
    let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("");
    if text("call_id").is_empty() || text("name").is_empty() {
        return false;
    }
    match text("type") {
        "function_call" => item.get("arguments").is_some_and(Value::is_string),
        "custom_tool_call" => item.get("input").is_some_and(Value::is_string),
        _ => false,
    }
}

/// Calls and outputs observed during one turn; committed to the caches only when the turn
/// completes (`responsesWebsocketToolCacheTurn`).
pub struct ToolCacheTurn {
    session_key: String,
    outputs: HashMap<String, Value>,
    output_order: Vec<String>,
    calls: HashMap<String, Value>,
    call_order: Vec<String>,
}

impl ToolCacheTurn {
    fn new(session_key: &str) -> Option<Self> {
        let key = session_key.trim();
        if key.is_empty() {
            return None;
        }
        Some(ToolCacheTurn {
            session_key: key.to_string(),
            outputs: HashMap::new(),
            output_order: Vec::new(),
            calls: HashMap::new(),
            call_order: Vec::new(),
        })
    }

    /// `recordResponse`: tool calls announced by upstream events.
    pub fn record_response(&mut self, payload: &Value) {
        match payload.g("type").str().trim() {
            "response.completed" => {
                let output = payload.g("response.output");
                if !output.is_array() {
                    return;
                }
                for item in output.array() {
                    let item = item.value();
                    if is_complete_tool_call(&item) {
                        self.record_item(&item);
                    }
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let item = payload.g("item").value();
                if is_complete_tool_call(&item) {
                    self.record_item(&item);
                }
            }
            _ => {}
        }
    }

    fn record_item(&mut self, item: &Value) {
        let item_type = item.g("type").str();
        let call_id = item.g("call_id").str();
        self.record_raw(&item_type, &call_id, item);
    }

    fn record_input_item(&mut self, item: &InputItem) {
        self.record_raw(&item.item_type, &item.call_id, &item.raw);
    }

    fn record_raw(&mut self, item_type: &str, call_id: &str, raw: &Value) {
        if !is_tool_call_output_type(item_type) && !is_tool_call_type(item_type) {
            return;
        }
        let call_id = call_id.trim().to_string();
        if call_id.is_empty() {
            return;
        }
        if is_tool_call_output_type(item_type) {
            if !self.outputs.contains_key(&call_id) {
                self.output_order.push(call_id.clone());
            }
            self.outputs.insert(call_id, raw.clone());
        } else {
            if !self.calls.contains_key(&call_id) {
                self.call_order.push(call_id.clone());
            }
            self.calls.insert(call_id, raw.clone());
        }
    }

    /// `commit`.
    pub fn commit(&self) {
        let _guard = TRANSACTION.write();
        for call_id in &self.output_order {
            OUTPUT_CACHE.record(&self.session_key, call_id, &self.outputs[call_id]);
        }
        for call_id in &self.call_order {
            CALL_CACHE.record(&self.session_key, call_id, &self.calls[call_id]);
        }
    }
}

/// `prepareResponsesWebsocketFallbackTurn`: repairs orphaned calls/outputs in the outgoing request
/// using the session caches and starts recording the new turn.
pub fn prepare_fallback_turn(session_key: &str, payload: &[u8]) -> (Vec<u8>, Option<ToolCacheTurn>) {
    let mut turn = ToolCacheTurn::new(session_key);
    let _guard = TRANSACTION.read();
    let repaired = repair(&OUTPUT_CACHE, &CALL_CACHE, session_key, payload, false, turn.as_mut());
    (repaired, turn)
}

/// `repairResponsesWebsocketToolCallsWithCachesMode`. Returns the payload unchanged when the
/// input is not repairable or nothing changed.
fn repair(
    output_cache: &ToolCache,
    call_cache: &ToolCache,
    session_key: &str,
    payload: &[u8],
    record: bool,
    turn: Option<&mut ToolCacheTurn>,
) -> Vec<u8> {
    if payload.is_empty() {
        return payload.to_vec();
    }
    let Ok(mut root) = serde_json::from_slice::<Value>(payload) else {
        return payload.to_vec();
    };
    let Value::Object(map) = &root else {
        return payload.to_vec();
    };
    let mut input_key: Option<String> = None;
    let mut previous_response_id = String::new();
    for (key, value) in map {
        if key.eq_ignore_ascii_case("input") {
            if !value.is_array() && !value.is_null() {
                return payload.to_vec();
            }
            input_key = Some(key.clone());
        } else if key.eq_ignore_ascii_case("previous_response_id") {
            previous_response_id = match value {
                Value::Null => String::new(),
                Value::String(s) => s.trim().to_string(),
                other => other.to_string().trim().to_string(),
            };
        }
    }
    let Some(input_key) = input_key else {
        return payload.to_vec();
    };
    let Some(Value::Array(raw_items)) = map.get(&input_key) else {
        return payload.to_vec();
    };
    let raw_items = raw_items.clone();
    let items: Vec<InputItem> = raw_items.iter().cloned().map(InputItem::new).collect();

    let session_key = session_key.trim();
    let repair_enabled = !session_key.is_empty();
    let updated = repair_items(
        output_cache,
        call_cache,
        session_key,
        items,
        repair_enabled && !previous_response_id.is_empty(),
        record && repair_enabled,
        turn,
        repair_enabled,
    );
    let updated_raw: Vec<Value> = updated.into_iter().map(|i| i.raw).collect();
    if updated_raw == raw_items {
        return payload.to_vec();
    }
    if let Value::Object(m) = &mut root {
        m.insert(input_key, Value::Array(updated_raw));
    }
    cpa_json::to_vec(&root)
}

#[allow(clippy::too_many_arguments)]
fn repair_items(
    output_cache: &ToolCache,
    call_cache: &ToolCache,
    session_key: &str,
    items: Vec<InputItem>,
    allow_orphan_outputs: bool,
    record: bool,
    mut turn: Option<&mut ToolCacheTurn>,
    repair_enabled: bool,
) -> Vec<InputItem> {
    if !repair_enabled {
        return dedupe_input_items(items);
    }
    use std::collections::HashSet;
    let mut output_present: HashSet<String> = HashSet::new();
    let mut call_present: HashSet<String> = HashSet::new();
    for item in &items {
        if let Some(t) = turn.as_deref_mut() {
            t.record_input_item(item);
        }
        if is_tool_call_output_type(&item.item_type) {
            if item.call_id.is_empty() {
                continue;
            }
            output_present.insert(item.call_id.clone());
            if record {
                output_cache.record(session_key, &item.call_id, &item.raw);
            }
        } else if is_tool_call_type(&item.item_type) {
            if item.call_id.is_empty() {
                continue;
            }
            call_present.insert(item.call_id.clone());
            if record {
                call_cache.record(session_key, &item.call_id, &item.raw);
            }
        }
    }

    let mut filtered: Vec<InputItem> = Vec::with_capacity(items.len());
    let mut inserted_calls: HashSet<String> = HashSet::new();
    for item in items {
        if is_tool_call_output_type(&item.item_type) {
            if item.call_id.is_empty() {
                // Codex sends standalone named results (heartbeat, delegation) without a call.
                let named = item.raw.get("name").and_then(Value::as_str).is_some_and(|n| !n.trim().is_empty());
                if item.item_type == "function_call_output" && named {
                    filtered.push(item);
                }
                continue;
            }
            if call_present.contains(&item.call_id) || allow_orphan_outputs {
                filtered.push(item);
                continue;
            }
            if let Some(cached) = call_cache.get(session_key, &item.call_id) {
                if !inserted_calls.contains(&item.call_id) {
                    filtered.push(InputItem::new(cached));
                    inserted_calls.insert(item.call_id.clone());
                    call_present.insert(item.call_id.clone());
                }
                filtered.push(item);
                continue;
            }
            // Orphaned output: upstream rejects transcripts with missing calls.
            continue;
        }
        if !is_tool_call_type(&item.item_type) {
            filtered.push(item);
            continue;
        }
        if item.call_id.is_empty() {
            // Upstream rejects tool calls without a call_id.
            continue;
        }
        if output_present.contains(&item.call_id) || allow_orphan_outputs {
            filtered.push(item);
            continue;
        }
        if let Some(cached) = output_cache.get(session_key, &item.call_id) {
            output_present.insert(item.call_id.clone());
            filtered.push(item);
            filtered.push(InputItem::new(cached));
            continue;
        }
        // Orphaned call: upstream rejects transcripts with missing outputs.
    }
    dedupe_input_items(filtered)
}

/// `recordResponsesWebsocketToolCallsFromPayload`: without a turn (no session key) calls from
/// the response are cached right away.
pub fn record_calls_from_payload(session_key: &str, payload: &Value) {
    let key = session_key.trim();
    if key.is_empty() {
        return;
    }
    match payload.g("type").str().trim() {
        "response.completed" => {
            let output = payload.g("response.output");
            if !output.is_array() {
                return;
            }
            for item in output.array() {
                let item = item.value();
                if is_complete_tool_call(&item) {
                    CALL_CACHE.record(key, &item.g("call_id").str(), &item);
                }
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            let item = payload.g("item").value();
            if is_complete_tool_call(&item) {
                CALL_CACHE.record(key, &item.g("call_id").str(), &item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(cache_out: &ToolCache, cache_call: &ToolCache, payload: Value) -> Value {
        let out = repair(cache_out, cache_call, "s", payload.to_string().as_bytes(), true, None);
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn orphan_output_without_cached_call_is_dropped() {
        let (o, c) = (ToolCache::default(), ToolCache::default());
        let out = run(&o, &c, json!({"input":[{"type":"function_call_output","call_id":"x","output":"1"},{"type":"message","role":"user"}]}));
        assert_eq!(out["input"], json!([{"type":"message","role":"user"}]));
    }

    #[test]
    fn orphan_output_gets_its_cached_call_back() {
        let (o, c) = (ToolCache::default(), ToolCache::default());
        c.record("s", "x", &json!({"type":"function_call","call_id":"x","name":"f","arguments":"{}"}));
        let out = run(&o, &c, json!({"input":[{"type":"function_call_output","call_id":"x","output":"1"}]}));
        assert_eq!(
            out["input"],
            json!([
                {"type":"function_call","call_id":"x","name":"f","arguments":"{}"},
                {"type":"function_call_output","call_id":"x","output":"1"}
            ])
        );
    }

    #[test]
    fn orphan_call_gets_cached_output_or_is_dropped() {
        let (o, c) = (ToolCache::default(), ToolCache::default());
        o.record("s", "k", &json!({"type":"function_call_output","call_id":"k","output":"done"}));
        let out = run(
            &o,
            &c,
            json!({"input":[{"type":"function_call","call_id":"k","name":"f","arguments":"{}"},{"type":"function_call","call_id":"z","name":"g","arguments":"{}"}]}),
        );
        let types: Vec<_> = out["input"].as_array().unwrap().iter().map(|i| i["type"].as_str().unwrap().to_string()).collect();
        assert_eq!(types, vec!["function_call", "function_call_output"]);
    }

    #[test]
    fn previous_response_id_keeps_orphan_outputs() {
        let (o, c) = (ToolCache::default(), ToolCache::default());
        let out = run(&o, &c, json!({"previous_response_id":"r","input":[{"type":"function_call_output","call_id":"x","output":"1"}]}));
        assert_eq!(out["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn named_outputs_without_call_id_survive() {
        let (o, c) = (ToolCache::default(), ToolCache::default());
        let out = run(&o, &c, json!({"input":[{"type":"function_call_output","name":"heartbeat","output":"ok"}]}));
        assert_eq!(out["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn cache_is_bounded_per_session() {
        let cache = ToolCache::default();
        for i in 0..300 {
            cache.record("s", &format!("c{i}"), &json!({"i": i}));
        }
        assert!(cache.get("s", "c0").is_none());
        assert!(cache.get("s", "c299").is_some());
        assert!(cache.get("s", "c43").is_none());
        assert!(cache.get("s", "c44").is_some());
    }

    #[test]
    fn session_key_resolution_order() {
        let mut h = HeaderMap::new();
        assert_eq!(downstream_session_key(&h), "");
        h.insert("session-id", "sid".parse().unwrap());
        assert_eq!(downstream_session_key(&h), "sid");
        h.insert("x-codex-turn-metadata", r#"{"session_id":"meta"}"#.parse().unwrap());
        assert_eq!(downstream_session_key(&h), "meta");
        h.insert("x-client-request-id", "req".parse().unwrap());
        assert_eq!(downstream_session_key(&h), "req");
    }

    #[test]
    fn complete_tool_call_requires_named_string_payload() {
        assert!(is_complete_tool_call(&json!({"type":"function_call","call_id":"c","name":"n","arguments":"{}"})));
        assert!(!is_complete_tool_call(&json!({"type":"function_call","call_id":"c","name":"n"})));
        assert!(is_complete_tool_call(&json!({"type":"custom_tool_call","call_id":"c","name":"n","input":""})));
        assert!(!is_complete_tool_call(&json!({"type":"message"})));
    }
}

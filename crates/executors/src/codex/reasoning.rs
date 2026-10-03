//! Reasoning replay for stateless Responses turns (Go: codex_executor_reasoning.go).
//!
//! Claude-style clients resend the transcript without the encrypted reasoning and tool-call items
//! Codex produced. The previous turn's items are cached per `(model, session key)` and re-inserted
//! into `input` at the matching turn positions before the request is sent.

use std::collections::{HashMap, HashSet};

use cpa_core::cache::{
    CODEX_REASONING_REPLAY_TURN_TYPE, append_codex_reasoning_replay_items_best_effort,
    delete_codex_reasoning_replay_item_required, get_codex_reasoning_replay_items_required,
};
use cpa_core::signature::inspect_gpt_reasoning_signature;
use cpa_core::thinking::parse_suffix;
use cpa_core::util::sanitize_claude_tool_id;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, meta};
use cpa_translator::Format;
use http::HeaderMap;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::headers::header_value;
use crate::helps::home_kv::kv_exec_error;
use crate::helps::session::claude_code_execution_scope;
use super::terminal::status_error_classification;

/// Where the cached items of one conversation live.
#[derive(Debug, Clone, Default)]
pub struct ReplayScope {
    pub model_name: String,
    pub session_key: String,
    pub request_fingerprint: String,
}

impl ReplayScope {
    pub fn valid(&self) -> bool {
        !self.model_name.trim().is_empty() && !self.session_key.trim().is_empty()
    }
}

fn is_claude(from: Format) -> bool {
    from == Format::Claude
}

/// Re-inserts cached reasoning and tool-call items into `body.input` for Claude-sourced requests.
/// Returns the (possibly unchanged) body and the scope used to cache this turn's output. A Home
/// KV failure fails the request, as in Go (`applyCodexReasoningReplayCacheRequired`).
pub fn apply_replay_cache(
    from: Format,
    req: &Request,
    opts: &Options,
    body: Vec<u8>,
) -> Result<(Vec<u8>, ReplayScope), ExecError> {
    let scope = scope_from_request(from, req, opts, &body);
    if !scope.valid() {
        return Ok((body, scope));
    }
    let items = get_codex_reasoning_replay_items_required(&scope.model_name, &scope.session_key)
        .map_err(|err| kv_exec_error(&err))?;
    let Some(items) = items else {
        return Ok((body, scope));
    };
    match insert_replay_turns(&body, &items) {
        Some(updated) => Ok((updated, scope)),
        None => Ok((body, scope)),
    }
}

fn scope_from_request(from: Format, req: &Request, opts: &Options, body: &[u8]) -> ReplayScope {
    if !is_claude(from) {
        return ReplayScope::default();
    }
    let parsed = cpa_json::parse(body);
    let mut model_name = parsed.g("model").str().trim().to_string();
    if model_name.is_empty() {
        model_name = parse_suffix(&req.model).model_name;
    }
    let items: Vec<String> = parsed.g("input").array().iter().map(|i| cpa_json::to_string(&i.value())).collect();
    ReplayScope {
        model_name,
        session_key: session_key(from, req, opts, &parsed),
        request_fingerprint: input_prefix_fingerprint(&items, items.len()),
    }
}

/// Session key precedence of the replay cache (Go: codexReasoningReplaySessionKey).
fn session_key(from: Format, req: &Request, opts: &Options, body: &Value) -> String {
    if is_claude(from)
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
    for payload in [body.clone(), cpa_json::parse(&req.payload)] {
        let value = session_key_from_payload(&payload);
        if !value.is_empty() {
            return value;
        }
    }
    let value = session_key_from_headers(&opts.headers);
    if !value.is_empty() {
        return value;
    }
    if from == Format::OpenAI {
        let api_key = metadata_string(&opts.metadata, crate::helps::usage::reporter::META_CLIENT_API_KEY);
        if !api_key.is_empty() {
            return format!("prompt-cache:{}", prompt_cache_uuid_for_api_key(&api_key));
        }
    }
    String::new()
}

/// UUIDv5 (OID namespace) the OpenAI-chat path derives its prompt cache key from.
pub fn prompt_cache_uuid_for_api_key(api_key: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("cli-proxy-api:codex:prompt-cache:{api_key}").as_bytes()).to_string()
}

fn metadata_string(metadata: &Metadata, key: &str) -> String {
    metadata.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn session_key_from_payload(payload: &Value) -> String {
    let key = payload.g("prompt_cache_key").str();
    if !key.trim().is_empty() {
        return format!("prompt-cache:{}", key.trim());
    }
    let window = payload.g("client_metadata.x-codex-window-id").str();
    if !window.trim().is_empty() {
        return format!("window:{}", window.trim());
    }
    let turn = payload.g("client_metadata.x-codex-turn-metadata").str();
    if !turn.trim().is_empty() {
        return session_key_from_turn_metadata(turn.trim());
    }
    String::new()
}

fn session_key_from_headers(headers: &HeaderMap) -> String {
    let turn = header_value(headers, "X-Codex-Turn-Metadata");
    if !turn.is_empty() {
        let key = session_key_from_turn_metadata(&turn);
        if !key.is_empty() {
            return key;
        }
    }
    let window = header_value(headers, "X-Codex-Window-Id");
    if !window.is_empty() {
        return format!("window:{window}");
    }
    for name in ["Session_id", "session_id", "Session-Id"] {
        let value = header_value(headers, name);
        if !value.is_empty() {
            return format!("session-id:{value}");
        }
    }
    let conversation = header_value(headers, "Conversation_id");
    if !conversation.is_empty() {
        return format!("conversation_id:{conversation}");
    }
    String::new()
}

fn session_key_from_turn_metadata(turn_metadata: &str) -> String {
    let parsed = cpa_json::parse(turn_metadata.as_bytes());
    let key = parsed.g("prompt_cache_key").str();
    if !key.trim().is_empty() {
        return format!("prompt-cache:{}", key.trim());
    }
    let window = parsed.g("window_id").str();
    if !window.trim().is_empty() {
        return format!("window:{}", window.trim());
    }
    String::new()
}

fn input_has_valid_reasoning_encrypted_content(body: &Value) -> bool {
    body.g("input").array().iter().any(|item| {
        item.g("type").str().trim() == "reasoning"
            && matches!(item.g("encrypted_content").v(), Some(Value::String(s)) if inspect_gpt_reasoning_signature(s).is_ok())
    })
}

#[derive(Debug, Default)]
struct ReplayTurn {
    marked: bool,
    assistant_fingerprint: String,
    request_fingerprint: String,
    call_ids: Vec<String>,
    items: Vec<Vec<u8>>,
}

/// Item types of tool calls and their outputs.
fn is_call(t: &str) -> bool {
    t == "function_call" || t == "custom_tool_call"
}

fn is_call_output(t: &str) -> bool {
    t == "function_call_output" || t == "custom_tool_call_output"
}

fn type_of(item: &Value) -> String {
    item.g("type").str().trim().to_string()
}

fn insert_replay_turns(body: &[u8], replay_items: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut root = cpa_json::parse(body);
    let input = root.g("input");
    if !input.is_array() || replay_items.is_empty() {
        return None;
    }
    let input_items: Vec<Value> = input.array().iter().map(|r| r.value()).collect();
    let turns = split_replay_turns(replay_items);
    let mut insertions: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
    let mut used_anchors: HashSet<usize> = HashSet::new();
    let mut prefix = PrefixFingerprints::new(&input_items);
    let mut fallback_anchor_end = input_items.len() as isize - 1;
    let mut inserted = false;
    for turn in turns.iter().rev() {
        if turn.items.is_empty() {
            continue;
        }
        if !turn.marked {
            let items = filter_items_for_input(&root, &turn.items);
            if items.is_empty() {
                continue;
            }
            let index = insert_index(&input_items, &items);
            let mut items = align_tool_call_ids(&input_items, items);
            let existing = insertions.remove(&index).unwrap_or_default();
            items.extend(existing);
            insertions.insert(index, items);
            inserted = true;
            continue;
        }
        let Some(anchor) = turn_anchor_index(&input_items, turn, fallback_anchor_end, &used_anchors, &mut prefix) else {
            continue;
        };
        used_anchors.insert(anchor);
        if turn.request_fingerprint.is_empty() {
            fallback_anchor_end = anchor as isize - 1;
        }
        let items = filter_turn_items(&input_items, &turn.items);
        if items.is_empty() {
            continue;
        }
        let mut items = align_tool_call_ids(&input_items, items);
        let existing = insertions.remove(&anchor).unwrap_or_default();
        items.extend(existing);
        insertions.insert(anchor, items);
        inserted = true;
    }
    if !inserted {
        return None;
    }
    let mut out: Vec<Value> = Vec::with_capacity(input_items.len() + replay_items.len());
    for (index, item) in input_items.iter().enumerate() {
        if let Some(items) = insertions.get(&index) {
            out.extend(items.iter().map(|i| cpa_json::parse(i)));
        }
        out.push(item.clone());
    }
    if let Some(items) = insertions.get(&input_items.len()) {
        out.extend(items.iter().map(|i| cpa_json::parse(i)));
    }
    if !cpa_json::set(&mut root, "input", Value::Array(out)) {
        return None;
    }
    Some(cpa_json::to_vec(&root))
}

fn split_replay_turns(items: &[Vec<u8>]) -> Vec<ReplayTurn> {
    let mut turns = Vec::new();
    let mut current = ReplayTurn::default();
    for item in items {
        let parsed = cpa_json::parse(item);
        if type_of(&parsed) == CODEX_REASONING_REPLAY_TURN_TYPE {
            if !current.items.is_empty() {
                turns.push(std::mem::take(&mut current));
            }
            current = ReplayTurn {
                marked: true,
                assistant_fingerprint: parsed.g("assistant_fingerprint").str().trim().to_string(),
                request_fingerprint: parsed.g("request_fingerprint").str().trim().to_string(),
                ..Default::default()
            };
            let call_ids = parsed.g("call_ids");
            if call_ids.is_array() {
                for id in call_ids.array() {
                    let id = id.str().trim().to_string();
                    if !id.is_empty() {
                        current.call_ids.push(id);
                    }
                }
            }
            continue;
        }
        current.items.push(item.clone());
    }
    if !current.items.is_empty() {
        turns.push(current);
    }
    turns
}

fn turn_anchor_index(
    input_items: &[Value],
    turn: &ReplayTurn,
    fallback_end: isize,
    used: &HashSet<usize>,
    prefix: &mut PrefixFingerprints,
) -> Option<usize> {
    let last = input_items.len() as isize - 1;
    let mut search_end = if turn.request_fingerprint.is_empty() { fallback_end } else { last };
    if search_end > last {
        search_end = last;
    }
    let mut matches_prefix = |index: usize| turn.request_fingerprint.is_empty() || prefix.at(index) == turn.request_fingerprint;
    if !turn.call_ids.is_empty() {
        let mut call_ids: HashSet<String> = HashSet::new();
        for id in &turn.call_ids {
            call_ids.extend(comparable_call_ids(id));
        }
        let mut index = search_end;
        while index >= 0 {
            let i = index as usize;
            index -= 1;
            if used.contains(&i) || !matches_prefix(i) {
                continue;
            }
            let t = type_of(&input_items[i]);
            if !is_call(&t) && !is_call_output(&t) {
                continue;
            }
            if comparable_call_ids(&input_items[i].g("call_id").str()).iter().any(|c| call_ids.contains(c)) {
                return Some(i);
            }
        }
    }
    if !turn.assistant_fingerprint.is_empty() {
        let mut index = search_end;
        while index >= 0 {
            let i = index as usize;
            index -= 1;
            if used.contains(&i) || !matches_prefix(i) {
                continue;
            }
            if assistant_message_fingerprint(&input_items[i]) == turn.assistant_fingerprint {
                return Some(i);
            }
        }
    }
    if turn.call_ids.is_empty() && turn.assistant_fingerprint.is_empty() {
        return Some(insert_index(input_items, &turn.items));
    }
    None
}

fn filter_turn_items(input_items: &[Value], items: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut existing_reasoning: HashSet<String> = HashSet::new();
    let mut existing_calls: HashSet<String> = HashSet::new();
    let mut existing_outputs: HashSet<String> = HashSet::new();
    for item in input_items {
        let t = type_of(item);
        if t == "reasoning" {
            let encrypted = item.g("encrypted_content").str().trim().to_string();
            if !encrypted.is_empty() {
                existing_reasoning.insert(encrypted);
            }
        } else if is_call_output(&t) {
            existing_outputs.extend(comparable_call_ids(&item.g("call_id").str()));
        }
        existing_calls.extend(tool_call_keys(item));
    }
    let mut filtered = Vec::with_capacity(items.len());
    for raw in items {
        let item = cpa_json::parse(raw);
        match type_of(&item).as_str() {
            "reasoning" => {
                if existing_reasoning.contains(item.g("encrypted_content").str().trim()) {
                    continue;
                }
            }
            "function_call" | "custom_tool_call" => {
                let keys = tool_call_keys(&item);
                if keys.is_empty() || keys.iter().any(|k| existing_calls.contains(k)) {
                    continue;
                }
                let has_output = comparable_call_ids(&item.g("call_id").str()).iter().any(|c| existing_outputs.contains(c));
                if !has_output {
                    continue;
                }
                existing_calls.extend(keys);
            }
            _ => continue,
        }
        filtered.push(raw.clone());
    }
    filtered
}

fn assistant_message_fingerprint(item: &Value) -> String {
    let t = type_of(item);
    if !t.is_empty() && t != "message" {
        return String::new();
    }
    if !item.g("role").str().trim().eq_ignore_ascii_case("assistant") {
        return String::new();
    }
    let content = item.g("content");
    let mut text = String::new();
    if content.is_string() {
        text.push_str(&content.str());
    } else if content.is_array() {
        for part in content.array() {
            match part.g("type").str().trim() {
                "input_text" | "output_text" => text.push_str(&part.g("text").str()),
                "refusal" => {
                    text.push_str("\0refusal\0");
                    text.push_str(&part.g("refusal").str());
                }
                _ => return String::new(),
            }
        }
    } else {
        return String::new();
    }
    if text.is_empty() {
        return String::new();
    }
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn input_prefix_fingerprint(items: &[String], end: usize) -> String {
    if end > items.len() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    for item in &items[..end] {
        hasher.update(b"\0item\0");
        hasher.update(item.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// Answers prefix fingerprint queries from one incremental hashing pass.
struct PrefixFingerprints {
    items: Vec<String>,
    hasher: Sha256,
    sums: Vec<String>,
}

impl PrefixFingerprints {
    fn new(items: &[Value]) -> Self {
        let hasher = Sha256::new();
        let first = hex::encode(hasher.clone().finalize());
        PrefixFingerprints { items: items.iter().map(cpa_json::to_string).collect(), hasher, sums: vec![first] }
    }

    fn at(&mut self, end: usize) -> String {
        if end > self.items.len() {
            return String::new();
        }
        while self.sums.len() <= end {
            let next = self.sums.len() - 1;
            self.hasher.update(b"\0item\0");
            self.hasher.update(self.items[next].as_bytes());
            self.sums.push(hex::encode(self.hasher.clone().finalize()));
        }
        self.sums[end].clone()
    }
}

fn filter_items_for_input(body: &Value, items: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let input = body.g("input");
    if !input.is_array() {
        return Vec::new();
    }
    let has_input_reasoning = input_has_valid_reasoning_encrypted_content(body);
    let mut existing_calls: HashSet<String> = HashSet::new();
    let mut existing_outputs: HashSet<String> = HashSet::new();
    for item in input.array() {
        let item = item.value();
        if is_call_output(&type_of(&item)) {
            let call_id = item.g("call_id").str().trim().to_string();
            if !call_id.is_empty() {
                existing_outputs.extend(comparable_call_ids(&call_id));
            }
        }
        existing_calls.extend(tool_call_keys(&item));
    }
    let mut filtered = Vec::with_capacity(items.len());
    for raw in items {
        let item = cpa_json::parse(raw);
        match type_of(&item).as_str() {
            "reasoning" => {
                if has_input_reasoning {
                    continue;
                }
            }
            "function_call" | "custom_tool_call" => {
                let keys = tool_call_keys(&item);
                if keys.is_empty() || keys.iter().any(|k| existing_calls.contains(k)) {
                    continue;
                }
                let call_id = item.g("call_id").str().trim().to_string();
                let has_output = !call_id.is_empty() && comparable_call_ids(&call_id).iter().any(|c| existing_outputs.contains(c));
                if !has_output {
                    continue;
                }
                existing_calls.extend(keys);
            }
            _ => continue,
        }
        filtered.push(raw.clone());
    }
    filtered
}

fn insert_index(input_items: &[Value], replay_items: &[Vec<u8>]) -> usize {
    let mut replay_call_ids: HashSet<String> = HashSet::new();
    for raw in replay_items {
        let item = cpa_json::parse(raw);
        if is_call(&type_of(&item)) {
            replay_call_ids.extend(comparable_call_ids(&item.g("call_id").str()));
        }
    }
    if !replay_call_ids.is_empty() {
        for (index, item) in input_items.iter().enumerate() {
            if !is_call_output(&type_of(item)) {
                continue;
            }
            let call_id = item.g("call_id").str().trim().to_string();
            if call_id.is_empty() || replay_call_ids.contains(&call_id) {
                return index;
            }
        }
    }
    for (index, item) in input_items.iter().enumerate().rev() {
        if matches!(message_role(item), Some(role) if role == "assistant") {
            return index;
        }
    }
    for (index, item) in input_items.iter().enumerate() {
        if should_insert_before(item) {
            return index;
        }
    }
    input_items.len()
}

fn align_tool_call_ids(input_items: &[Value], replay_items: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut output_call_ids: HashMap<String, String> = HashMap::new();
    for item in input_items {
        if !is_call_output(&type_of(item)) {
            continue;
        }
        let call_id = item.g("call_id").str().trim().to_string();
        if call_id.is_empty() {
            continue;
        }
        for candidate in comparable_call_ids(&call_id) {
            output_call_ids.insert(candidate, call_id.clone());
        }
    }
    if output_call_ids.is_empty() {
        return replay_items;
    }
    replay_items
        .into_iter()
        .map(|raw| {
            let item = cpa_json::parse(&raw);
            if !is_call(&type_of(&item)) {
                return raw;
            }
            let call_id = item.g("call_id").str().trim().to_string();
            let output_call_id = comparable_call_ids(&call_id)
                .iter()
                .find_map(|c| output_call_ids.get(c).filter(|v| !v.is_empty()).cloned())
                .unwrap_or_default();
            if output_call_id.is_empty() || output_call_id == call_id {
                return raw;
            }
            let mut updated = item;
            if cpa_json::set(&mut updated, "call_id", output_call_id) { cpa_json::to_vec(&updated) } else { raw }
        })
        .collect()
}

fn should_insert_before(item: &Value) -> bool {
    match message_role(item) {
        None => true,
        Some(role) => !matches!(role.as_str(), "developer" | "system"),
    }
}

fn message_role(item: &Value) -> Option<String> {
    let t = type_of(item);
    let role = item.g("role").str().trim().to_lowercase();
    if role.is_empty() || (!t.is_empty() && t != "message") {
        return None;
    }
    Some(role)
}

fn tool_call_keys(item: &Value) -> Vec<String> {
    let t = type_of(item);
    if !is_call(&t) {
        return Vec::new();
    }
    comparable_call_ids(&item.g("call_id").str()).into_iter().map(|id| format!("{t}:{id}")).collect()
}

fn comparable_call_ids(call_id: &str) -> Vec<String> {
    let call_id = call_id.trim();
    if call_id.is_empty() {
        return Vec::new();
    }
    let claude_visible = shorten_call_id_if_needed(&sanitize_claude_tool_id(call_id));
    if claude_visible.is_empty() || claude_visible == call_id {
        return vec![call_id.to_string()];
    }
    vec![call_id.to_string(), claude_visible]
}

fn shorten_call_id_if_needed(id: &str) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id.to_string();
    }
    let sum = Sha256::digest(id.as_bytes());
    let suffix = format!("_{}", hex::encode(&sum[..8]));
    if LIMIT <= suffix.len() {
        return suffix[suffix.len() - LIMIT..].to_string();
    }
    let prefix_len = LIMIT - suffix.len();
    // Ids are ASCII after Claude sanitizing; fall back to a char boundary otherwise.
    let mut end = prefix_len;
    while end > 0 && !id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &id[..end])
}

/// Caches the reasoning and tool-call items of a completed response with a turn marker (Go:
/// cacheCodexReasoningReplayFromCompleted).
pub fn cache_replay_from_completed(scope: &ReplayScope, completed: &Value) {
    if !scope.valid() {
        return;
    }
    let output = completed.g("response.output");
    if !output.is_array() {
        return;
    }
    let mut replay_items: Vec<Vec<u8>> = Vec::new();
    let mut call_ids: Vec<String> = Vec::new();
    let mut assistant_fingerprint = String::new();
    for item in output.array() {
        let item = item.value();
        match type_of(&item).as_str() {
            "reasoning" => replay_items.push(cpa_json::to_vec(&item)),
            "function_call" | "custom_tool_call" => {
                replay_items.push(cpa_json::to_vec(&item));
                let call_id = item.g("call_id").str().trim().to_string();
                if !call_id.is_empty() {
                    call_ids.push(call_id);
                }
            }
            "message" => {
                let fingerprint = assistant_message_fingerprint(&item);
                if !fingerprint.is_empty() {
                    assistant_fingerprint = fingerprint;
                }
            }
            _ => {}
        }
    }
    if replay_items.is_empty() {
        return;
    }
    let mut hasher = Sha256::new();
    hasher.update(scope.request_fingerprint.as_bytes());
    hasher.update(format!("\0assistant\0{assistant_fingerprint}").as_bytes());
    for call_id in &call_ids {
        hasher.update(format!("\0call\0{call_id}").as_bytes());
    }
    for item in &replay_items {
        hasher.update(b"\0item\0");
        hasher.update(item);
    }
    let mut marker = cpa_json::parse_str(&format!(r#"{{"type":"{CODEX_REASONING_REPLAY_TURN_TYPE}"}}"#));
    cpa_json::set(&mut marker, "id", hex::encode(hasher.finalize()));
    if !assistant_fingerprint.is_empty() {
        cpa_json::set(&mut marker, "assistant_fingerprint", assistant_fingerprint);
    }
    if !scope.request_fingerprint.is_empty() {
        cpa_json::set(&mut marker, "request_fingerprint", scope.request_fingerprint.as_str());
    }
    for call_id in call_ids {
        cpa_json::set(&mut marker, "call_ids.-1", call_id);
    }
    let mut items = Vec::with_capacity(replay_items.len() + 1);
    items.push(cpa_json::to_vec(&marker));
    items.extend(replay_items);
    append_codex_reasoning_replay_items_best_effort(&scope.model_name, &scope.session_key, &items);
}

/// Drops the cached state when upstream rejected it as an invalid thinking signature. A Home KV
/// failure is returned and replaces the upstream error at the call sites, as in Go.
pub fn clear_replay_on_invalid_signature(scope: &ReplayScope, status: u16, body: &[u8]) -> Result<(), ExecError> {
    if !scope.valid() {
        return Ok(());
    }
    if matches!(status_error_classification(status, body), Some((code, _)) if code == "thinking_signature_invalid") {
        delete_codex_reasoning_replay_item_required(&scope.model_name, &scope.session_key)
            .map_err(|err| kv_exec_error(&err))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_core::cache::{clear_codex_reasoning_replay_cache, get_codex_reasoning_replay_items};

    fn claude_request(session: &str) -> (Request, Options) {
        let payload = format!(r#"{{"metadata":{{"user_id":"user_x_session_{session}"}}}}"#);
        let req = Request {
            model: "gpt-5".into(),
            payload: payload.clone().into(),
            format: Format::Claude,
            metadata: Metadata::new(),
        };
        let mut opts = Options::new(Format::Claude);
        opts.original_request = payload.into();
        (req, opts)
    }

    #[test]
    fn claude_session_scope_includes_agent() {
        let (req, opts) = claude_request("abc-123");
        assert_eq!(claude_code_execution_scope(&req.payload, &opts.headers).as_deref(), Some("claude:abc-123:agent:main"));
        let mut headers = HeaderMap::new();
        headers.insert("x-claude-code-session-id", "s1".parse().unwrap());
        headers.insert("x-claude-code-agent-id", "sub".parse().unwrap());
        assert_eq!(claude_code_execution_scope(b"{}", &headers).as_deref(), Some("claude:s1:agent:sub"));
    }

    #[test]
    fn completed_turn_is_replayed_into_the_next_request() {
        clear_codex_reasoning_replay_cache();
        let (req, opts) = claude_request("abc-1");
        let body = br#"{"model":"gpt-5","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#.to_vec();
        let (_, scope) = apply_replay_cache(Format::Claude, &req, &opts, body.clone());
        assert!(scope.valid());
        let completed = cpa_json::parse(
            br#"{"type":"response.completed","response":{"output":[
                {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"gAAAAAxxxx"},
                {"type":"function_call","call_id":"call_1","name":"f","arguments":"{}"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]}}"#,
        );
        cache_replay_from_completed(&scope, &completed);
        assert!(get_codex_reasoning_replay_items(&scope.model_name, &scope.session_key).is_some());
        let next = br#"{"model":"gpt-5","input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"function_call_output","call_id":"call_1","output":"ok"}]}"#
            .to_vec();
        let (patched, _) = apply_replay_cache(Format::Claude, &req, &opts, next);
        let parsed = cpa_json::parse(&patched);
        let types: Vec<String> = parsed.g("input").array().iter().map(|i| i.g("type").str()).collect();
        assert_eq!(types, ["message", "function_call", "function_call_output"]);
    }

    #[test]
    fn non_claude_sources_never_replay() {
        let (req, opts) = claude_request("x");
        let (body, scope) = apply_replay_cache(Format::OpenAI, &req, &opts, b"{}".to_vec());
        assert!(!scope.valid());
        assert_eq!(body, b"{}");
    }
}

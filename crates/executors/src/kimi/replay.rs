//! Kimi thinking replay for the delegated Claude path (Go: kimi_thinking_replay.go).
//!
//! Kimi's Anthropic-compatible endpoint signs assistant thinking. Claude clients drop that
//! thinking between turns, so complete signed assistant content arrays from responses are cached
//! per (model family, session) in `cpa_core::cache` and restored into a later request whose
//! assistant turn matches. Failed replays (400/422) clear the cache entry.

use crate::helps::session::{claude_code_execution_scope, header_value_case_insensitive};
use std::collections::BTreeMap;

use bytes::Bytes;
use cpa_core::cache::{
    KIMI_THINKING_REPLAY_CACHE_MAX_BLOCKS_PER_ENTRY, KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY,
    KimiThinkingReplaySnapshot, delete_kimi_thinking_replay_if_unchanged, get_kimi_thinking_replay_with_snapshot_required,
    replace_kimi_thinking_replay_if_unchanged,
};
use cpa_core::thinking::parse_suffix;
use cpa_json::{J, Res};
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, StreamResult, meta};
use http::HeaderMap;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use super::normalize::normalize_kimi_upstream_model;
use crate::helps::usage::META_CLIENT_API_KEY;


/// Cache key, snapshot and state of one replay-eligible request.
#[derive(Clone, Default)]
pub(super) struct ReplayScope {
    model_family: String,
    session_key: String,
    snapshot: KimiThinkingReplaySnapshot,
    cache_ready: bool,
    pub(super) replay_applied: bool,
}

impl ReplayScope {
    fn valid(&self) -> bool {
        !self.model_family.trim().is_empty() && !self.session_key.trim().is_empty()
    }
}

/// K3 variants share one replay family; everything else is its canonical model id.
pub(super) fn model_family(model: &str) -> String {
    let base = parse_suffix(model.trim()).model_name;
    let normalized = normalize_kimi_upstream_model(&base);
    match normalized.as_str() {
        "k3" | "k3-256k" => "k3".to_string(),
        _ => normalized,
    }
}

// ---------------------------------------------------------------- session key

fn replay_key_from_turn_metadata(turn_metadata: &str) -> String {
    let v = cpa_json::parse_str(turn_metadata);
    let key = v.g("prompt_cache_key").str().trim().to_string();
    if !key.is_empty() {
        return format!("prompt-cache:{key}");
    }
    let window = v.g("window_id").str().trim().to_string();
    if !window.is_empty() {
        return format!("window:{window}");
    }
    String::new()
}

fn replay_key_from_payload(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let v = cpa_json::parse(payload);
    let key = v.g("prompt_cache_key").str().trim().to_string();
    if !key.is_empty() {
        return format!("prompt-cache:{key}");
    }
    let window = v.g("client_metadata.x-codex-window-id").str().trim().to_string();
    if !window.is_empty() {
        return format!("window:{window}");
    }
    let turn = v.g("client_metadata.x-codex-turn-metadata").str().trim().to_string();
    if !turn.is_empty() {
        return replay_key_from_turn_metadata(&turn);
    }
    String::new()
}

fn replay_key_from_headers(headers: &HeaderMap) -> String {
    let turn = header_value_case_insensitive(headers, "X-Codex-Turn-Metadata");
    if !turn.is_empty() {
        let key = replay_key_from_turn_metadata(&turn);
        if !key.is_empty() {
            return key;
        }
    }
    let window = header_value_case_insensitive(headers, "X-Codex-Window-Id");
    if !window.is_empty() {
        return format!("window:{window}");
    }
    for name in ["Session_id", "session_id", "Session-Id"] {
        let value = header_value_case_insensitive(headers, name);
        if !value.is_empty() {
            return format!("session-id:{value}");
        }
    }
    let conversation = header_value_case_insensitive(headers, "Conversation_id");
    if !conversation.is_empty() {
        return format!("conversation_id:{conversation}");
    }
    String::new()
}

fn metadata_string(metadata: &Metadata, key: &str) -> String {
    metadata.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// The session key Codex uses for Claude-format requests (Go: codexReasoningReplaySessionKey
/// with a Claude source), then the xAI caller isolation (xaiReasoningReplayIsolateSessionKey).
fn session_key_from_request(req: &Request, opts: &Options) -> String {
    let key = replay_session_key(req, opts);
    isolate_session_key(opts, &key)
}

fn replay_session_key(req: &Request, opts: &Options) -> String {
    let scope = claude_code_execution_scope(&req.payload, &opts.headers).unwrap_or_default();
    if !scope.is_empty() {
        return scope;
    }
    for metadata in [&opts.metadata, &req.metadata] {
        let value = metadata_string(metadata, meta::EXECUTION_SESSION_ID);
        if !value.is_empty() {
            return format!("execution:{value}");
        }
    }
    let value = replay_key_from_payload(&req.payload);
    if !value.is_empty() {
        return value;
    }
    replay_key_from_headers(&opts.headers)
}

/// Non-execution keys are namespaced by the calling client API key so callers never share
/// replay state; without a client key there is no safe scope.
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

fn scope_from_request(req: &Request, opts: &Options) -> ReplayScope {
    ReplayScope {
        model_family: model_family(&req.model),
        session_key: session_key_from_request(req, opts),
        ..Default::default()
    }
}

// ---------------------------------------------------------------- request and response hooks

/// Restores cached thinking into the request's matching assistant turn and returns the scope
/// used to cache the response afterwards (Go: prepareKimiThinkingReplayRequest).
pub(super) fn prepare_request(mut req: Request, opts: &Options) -> (Request, ReplayScope) {
    let mut scope = scope_from_request(&req, opts);
    if !scope.valid() {
        return (req, scope);
    }
    let (content, snapshot) =
        match get_kimi_thinking_replay_with_snapshot_required(&scope.model_family, &scope.session_key) {
            Ok(read) => read,
            Err(err) => {
                tracing::warn!("kimi thinking replay cache read failed: {err}");
                return (req, scope);
            }
        };
    scope.snapshot = snapshot;
    scope.cache_ready = true;
    let Some(content) = content else {
        return (req, scope);
    };
    if let Some(updated) = restore_content(&req.payload, &content) {
        req.payload = Bytes::from(updated);
        scope.replay_applied = true;
    }
    (req, scope)
}

/// Caches the `content` array of a complete non-stream response (Go: cacheKimiThinkingReplayResponse).
pub(super) fn cache_response(scope: &ReplayScope, response: &[u8]) {
    if !scope.valid() || !scope.cache_ready {
        return;
    }
    let root = cpa_json::parse(response);
    let content = root.g("content");
    if !content.is_array() {
        return;
    }
    cache_content(scope, content.raw().as_bytes());
}

fn cache_content(scope: &ReplayScope, content: &[u8]) {
    if !scope.valid() || !scope.cache_ready {
        return;
    }
    if content_is_replayable(content) {
        if let Err(err) =
            replace_kimi_thinking_replay_if_unchanged(&scope.model_family, &scope.session_key, &scope.snapshot, content)
        {
            tracing::warn!("kimi thinking replay cache replace failed: {err}");
        }
        return;
    }
    clear_content(scope);
}

/// Drops the entry this request read, unless another request replaced it since.
pub(super) fn clear_content(scope: &ReplayScope) {
    if !scope.valid() || !scope.cache_ready {
        return;
    }
    if let Err(err) = delete_kimi_thinking_replay_if_unchanged(&scope.model_family, &scope.session_key, &scope.snapshot) {
        tracing::warn!("kimi thinking replay cache delete failed: {err}");
    }
}

/// Only upstream request rejections (400/422) invalidate a replay (Go:
/// shouldClearKimiThinkingReplayAfterError).
pub(super) fn should_clear_after_error(err: &ExecError) -> bool {
    matches!(err.status, 400 | 422)
}

/// Complete means signed thinking plus at least one tool use with an id.
pub(super) fn content_is_replayable(content: &[u8]) -> bool {
    let root = cpa_json::parse(content);
    let Value::Array(parts) = &root else {
        return false;
    };
    let (mut has_signed_thinking, mut has_tool_use) = (false, false);
    for part in parts {
        let part = Res::of(part);
        match part.g("type").str().trim() {
            "thinking" => has_signed_thinking |= !part.g("signature").str().trim().is_empty(),
            "tool_use" => has_tool_use |= !part.g("id").str().trim().is_empty(),
            _ => {}
        }
    }
    has_signed_thinking && has_tool_use
}

// ---------------------------------------------------------------- restore

/// Replaces the newest unthinking assistant message whose non-thinking parts equal the cached
/// ones with the cached complete content. `None` when nothing was restored.
pub(super) fn restore_content(body: &[u8], cached_content: &[u8]) -> Option<Vec<u8>> {
    let cached_value = cpa_json::parse(cached_content);
    let cached_parts = non_thinking_parts(&Res::of(&cached_value))?;
    let mut root = cpa_json::parse(body);
    let Some(Value::Array(messages)) = root.g("messages").v().cloned() else {
        return None;
    };
    for index in (0..messages.len()).rev() {
        let message = Res::of(&messages[index]);
        if !message.g("role").str().trim().eq_ignore_ascii_case("assistant") {
            continue;
        }
        let current = message.g("content");
        if json_equal(current.raw().as_bytes(), cached_content) {
            return None;
        }
        if content_has_thinking(&current) {
            continue;
        }
        let Some(current_parts) = non_thinking_parts(&current) else {
            continue;
        };
        if current_parts != cached_parts {
            continue;
        }
        cpa_json::set_raw(&mut root, &format!("messages.{index}.content"), std::str::from_utf8(cached_content).ok()?).ok()?;
        return Some(cpa_json::to_vec(&root));
    }
    None
}

fn content_has_thinking(content: &Res<'_>) -> bool {
    let Some(Value::Array(parts)) = content.v() else {
        return false;
    };
    parts
        .iter()
        .any(|p| matches!(Res::of(p).g("type").str().trim(), "thinking" | "redacted_thinking"))
}

/// Canonical forms of the non-thinking parts; `None` unless the content is an array whose tool
/// uses all carry ids and at least one tool use exists.
fn non_thinking_parts(content: &Res<'_>) -> Option<Vec<Vec<u8>>> {
    let Some(Value::Array(parts)) = content.v() else {
        return None;
    };
    let mut out = Vec::with_capacity(parts.len());
    let mut has_tool_use = false;
    for part in parts {
        match Res::of(part).g("type").str().trim() {
            "thinking" | "redacted_thinking" => continue,
            "tool_use" => {
                if Res::of(part).g("id").str().trim().is_empty() {
                    return None;
                }
                has_tool_use = true;
            }
            _ => {}
        }
        out.push(canonical_json(part.to_string().as_bytes())?);
    }
    has_tool_use.then_some(out)
}

fn json_equal(left: &[u8], right: &[u8]) -> bool {
    match (canonical_json(left), canonical_json(right)) {
        (Some(l), Some(r)) => l == r,
        _ => false,
    }
}

fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<String, Value> = m.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// Key-sorted compact serialization of the first JSON value in `raw` (Go: kimiCanonicalJSON,
/// which decodes with `UseNumber` so numbers compare by their text).
fn canonical_json(raw: &[u8]) -> Option<Vec<u8>> {
    let first = serde_json::Deserializer::from_slice(raw).into_iter::<Value>().next()?.ok()?;
    serde_json::to_vec(&sort_keys(first)).ok()
}

// ---------------------------------------------------------------- stream accumulator

#[derive(Default)]
struct StreamBlock {
    raw: Value,
    text: String,
    thinking: String,
    signature: String,
    input: String,
    text_initialized: bool,
    thinking_initialized: bool,
    signature_initialized: bool,
    has_input_delta: bool,
    finished: bool,
}

/// Rebuilds the complete assistant `content` array from Anthropic SSE events.
#[derive(Default)]
pub(super) struct StreamAccumulator {
    blocks: BTreeMap<i64, StreamBlock>,
    observed: bool,
    complete: bool,
    pub(super) upstream_error: bool,
    abandoned: bool,
    bytes_used: usize,
}

impl StreamAccumulator {
    /// Feeds one stream chunk (any number of SSE lines).
    pub(super) fn observe(&mut self, chunk: &[u8]) {
        for line in chunk.split(|b| *b == b'\n') {
            let line = crate::helps::text::trim_space(line);
            let Some(rest) = line.strip_prefix(b"data:") else {
                continue;
            };
            let payload = crate::helps::text::trim_space(rest);
            if payload.is_empty() || payload == b"[DONE]" {
                continue;
            }
            if !cpa_json::valid(payload) {
                self.abandon();
                continue;
            }
            let root = cpa_json::parse(payload);
            match root.g("type").str().as_str() {
                "message_start" => self.observed = true,
                "content_block_start" if !self.abandoned => self.observe_block_start(&root),
                "content_block_delta" if !self.abandoned => self.observe_block_delta(&root),
                "content_block_stop" if !self.abandoned => self.finish_block(root.g("index").int()),
                "message_stop" => self.complete = true,
                "error" => {
                    self.upstream_error = true;
                    self.abandon();
                }
                _ => {}
            }
        }
    }

    fn observe_block_start(&mut self, root: &Value) {
        let index = root.g("index").int();
        let block = root.g("content_block").value();
        if !block.is_object() || self.blocks.len() >= KIMI_THINKING_REPLAY_CACHE_MAX_BLOCKS_PER_ENTRY {
            self.abandon();
            return;
        }
        if self.blocks.contains_key(&index) {
            self.abandon();
            return;
        }
        if !self.reserve_bytes(block.to_string().len()) {
            return;
        }
        self.blocks.insert(index, StreamBlock { raw: block, ..Default::default() });
    }

    fn observe_block_delta(&mut self, root: &Value) {
        let index = root.g("index").int();
        if !self.blocks.contains_key(&index) {
            self.abandon();
            return;
        }
        let delta = root.g("delta");
        match delta.g("type").str().as_str() {
            "text_delta" => self.append_text(index, Field::Text, &delta.g("text").str()),
            "thinking_delta" => self.append_text(index, Field::Thinking, &delta.g("thinking").str()),
            "signature_delta" => self.append_text(index, Field::Signature, &delta.g("signature").str()),
            "input_json_delta" => {
                let suffix = delta.g("partial_json").str();
                if self.reserve_bytes(suffix.len())
                    && let Some(block) = self.blocks.get_mut(&index)
                {
                    block.input.push_str(&suffix);
                    block.has_input_delta = true;
                }
            }
            _ => self.abandon(),
        }
    }

    fn append_text(&mut self, index: i64, field: Field, suffix: &str) {
        let Some(block) = self.blocks.get(&index) else { return };
        let initialized = match field {
            Field::Text => block.text_initialized,
            Field::Thinking => block.thinking_initialized,
            Field::Signature => block.signature_initialized,
        };
        if !initialized {
            let initial = block.raw.g(field.path()).str();
            if !self.reserve_bytes(initial.len()) {
                return;
            }
            if let Some(block) = self.blocks.get_mut(&index) {
                let (buf, flag) = field.slots(block);
                buf.push_str(&initial);
                *flag = true;
            }
        }
        if self.reserve_bytes(suffix.len())
            && let Some(block) = self.blocks.get_mut(&index)
        {
            field.slots(block).0.push_str(suffix);
        }
    }

    fn finish_block(&mut self, index: i64) {
        let Some(block) = self.blocks.get_mut(&index) else {
            self.abandon();
            return;
        };
        if block.has_input_delta && !cpa_json::valid(block.input.as_bytes()) {
            self.abandon();
            return;
        }
        block.finished = true;
    }

    fn reserve_bytes(&mut self, count: usize) -> bool {
        if self.bytes_used > KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY.saturating_sub(count) {
            self.abandon();
            return false;
        }
        self.bytes_used += count;
        true
    }

    fn abandon(&mut self) {
        self.abandoned = true;
        self.blocks.clear();
        self.bytes_used = 0;
    }

    /// The assembled content array when the stream completed cleanly.
    pub(super) fn content(&mut self) -> Option<Vec<u8>> {
        if !self.observed || !self.complete || self.upstream_error || self.abandoned {
            return None;
        }
        let mut parts = Vec::with_capacity(self.blocks.len());
        for block in self.blocks.values() {
            if !block.finished {
                self.abandon();
                return None;
            }
            let mut raw = block.raw.clone();
            if block.text_initialized {
                cpa_json::set(&mut raw, "text", block.text.as_str());
            }
            if block.thinking_initialized {
                cpa_json::set(&mut raw, "thinking", block.thinking.as_str());
            }
            if block.signature_initialized {
                cpa_json::set(&mut raw, "signature", block.signature.as_str());
            }
            if block.has_input_delta && cpa_json::set_raw(&mut raw, "input", &block.input).is_err() {
                self.abandon();
                return None;
            }
            parts.push(raw);
        }
        let content = cpa_json::to_vec(&Value::Array(parts));
        if content.len() > KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY {
            self.abandon();
            return None;
        }
        Some(content)
    }
}

#[derive(Clone, Copy)]
enum Field {
    Text,
    Thinking,
    Signature,
}

impl Field {
    fn path(self) -> &'static str {
        match self {
            Field::Text => "text",
            Field::Thinking => "thinking",
            Field::Signature => "signature",
        }
    }

    fn slots(self, block: &mut StreamBlock) -> (&mut String, &mut bool) {
        match self {
            Field::Text => (&mut block.text, &mut block.text_initialized),
            Field::Thinking => (&mut block.thinking, &mut block.thinking_initialized),
            Field::Signature => (&mut block.signature, &mut block.signature_initialized),
        }
    }
}

/// Forwards `result` unchanged while accumulating its SSE events, then caches the complete
/// content (or clears an applied replay after an upstream error event) once the stream ends
/// without a transport error (Go: wrapKimiThinkingReplayStream).
pub(super) fn wrap_stream(mut result: StreamResult, scope: ReplayScope) -> StreamResult {
    if !scope.valid() {
        return result;
    }
    let (tx, rx) = mpsc::channel(8);
    let mut inner = std::mem::replace(&mut result.chunks, rx);
    tokio::spawn(async move {
        let mut accumulator = StreamAccumulator::default();
        let mut has_error = false;
        while let Some(chunk) = inner.recv().await {
            match &chunk {
                Ok(payload) => accumulator.observe(payload),
                Err(_) => has_error = true,
            }
            if tx.send(chunk).await.is_err() {
                return;
            }
        }
        if has_error {
            return;
        }
        if let Some(content) = accumulator.content() {
            cache_content(&scope, &content);
            return;
        }
        if accumulator.upstream_error && scope.replay_applied {
            clear_content(&scope);
        }
    });
    result
}

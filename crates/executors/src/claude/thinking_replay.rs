//! Thinking replay for Claude-compatible API-key providers (Go: executor/claude_thinking_replay.go).
//!
//! Compat providers reject (or drop) assistant turns whose signed `thinking` blocks the client did
//! not echo back. The signed assistant content of each completed turn is cached per (credential,
//! model, session) and re-inserted into the next request's matching assistant message.
//!
//! The Kimi replay helpers this depends on (executor/kimi_thinking_replay.go: restore, replayable
//! check, stream accumulator) live here as private/local items, since the Kimi executor module is
//! ported separately. The Codex/xAI session-key helpers (`codexReasoningReplaySessionKey`,
//! `xaiReasoningReplayIsolateSessionKey`) and `ClaudeCodeExecutionScope` are small local copies
//! below.

use crate::helps::session::{CLAUDE_CODE_AGENT_HEADER, CLAUDE_CODE_SESSION_HEADER, claude_code_execution_scope, header_value_case_insensitive};
use std::collections::BTreeMap;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_auth::types::AUTH_KIND_API_KEY;
use cpa_core::cache::{
    ClaudeThinkingReplaySnapshot, KIMI_THINKING_REPLAY_CACHE_MAX_BLOCKS_PER_ENTRY, KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY,
    delete_claude_thinking_replay_if_unchanged, get_claude_thinking_replay_with_snapshot_required, replace_claude_thinking_replay_if_unchanged,
};
use cpa_core::thinking::parse_suffix;
use cpa_core::util::{GoJsonStyle, go_json_sorted};
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, StreamResult, meta};
use cpa_translator::Format;
use http::HeaderMap;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use super::helps::ClaudeCtx;
use super::signing::{claude_creds, is_claude_oauth_token, json_valid, set_raw_top_level, sjson_string};
use crate::helps::text::trim_space;
use crate::helps::thinking::api_key_model_is_compat;

/// Replay state of one request (Go: `claudeThinkingReplayScope`, an alias of the Kimi scope). A
/// scope without model family or session key is invalid and disables replay.
#[derive(Debug, Clone, Default)]
pub struct ClaudeThinkingReplayScope {
    pub model_family: String,
    pub session_key: String,
    pub snapshot: ClaudeThinkingReplaySnapshot,
    pub cache_ready: bool,
    pub replay_applied: bool,
}

impl ClaudeThinkingReplayScope {
    /// Go: `valid`.
    pub fn valid(&self) -> bool {
        !self.model_family.trim().is_empty() && !self.session_key.trim().is_empty()
    }
}

/// Whether replay applies: a Claude-format request on a `claude` API-key credential (not an OAuth
/// token) whose selected model is marked compat (Go: `claudeThinkingReplayEnabled`).
pub fn claude_thinking_replay_enabled(auth: &Auth, req: &Request, opts: &Options) -> bool {
    if opts.source_format != Format::Claude {
        return false;
    }
    if !auth.provider.trim().eq_ignore_ascii_case("claude") || auth.auth_kind() != AUTH_KIND_API_KEY {
        return false;
    }
    if !api_key_model_is_compat(req) {
        return false;
    }
    let (api_key, _) = claude_creds(auth);
    !api_key.trim().is_empty() && !is_claude_oauth_token(&api_key)
}

/// Replay scope of a request (Go: `claudeThinkingReplayScopeFromRequest`). A missing session
/// identity intentionally yields an invalid scope instead of sharing hidden reasoning across
/// callers. `caller_api_key` is the downstream CPA API key (Go: `APIKeyFromContext`).
pub fn claude_thinking_replay_scope_from_request(
    ctx: &ClaudeCtx,
    auth: &Auth,
    req: &Request,
    opts: &Options,
    caller_api_key: &str,
) -> ClaudeThinkingReplayScope {
    let session_key = replay_session_key(ctx, req, opts);
    let session_key = isolate_session_key(caller_api_key, &session_key);
    ClaudeThinkingReplayScope {
        model_family: claude_thinking_replay_model_family(auth, &req.model),
        session_key,
        ..Default::default()
    }
}

/// `claude:<credential hash>:<base model>` (Go: `claudeThinkingReplayModelFamily`); the hash covers
/// the auth id, else the base URL, else the API key.
pub fn claude_thinking_replay_model_family(auth: &Auth, model: &str) -> String {
    let base_model = parse_suffix(model.trim()).model_name;
    if base_model.is_empty() {
        return String::new();
    }
    let mut identity = auth.id.trim().to_string();
    if identity.is_empty() {
        let (api_key, base_url) = claude_creds(auth);
        identity = base_url.trim().to_string();
        if identity.is_empty() {
            identity = api_key.trim().to_string();
        }
    }
    if identity.is_empty() {
        return format!("claude:{base_model}");
    }
    let sum = Sha256::digest(identity.as_bytes());
    format!("claude:{}:{base_model}", hex::encode(&sum[..8]))
}

/// Restores cached signed assistant turns into the request payload (Go:
/// `prepareClaudeThinkingReplayRequest`). The returned scope carries the cache snapshot for the
/// later conditional update.
pub fn prepare_claude_thinking_replay_request(
    ctx: &ClaudeCtx,
    auth: &Auth,
    mut req: Request,
    opts: &Options,
    caller_api_key: &str,
) -> (Request, ClaudeThinkingReplayScope) {
    let mut scope = claude_thinking_replay_scope_from_request(ctx, auth, &req, opts, caller_api_key);
    if !scope.valid() {
        return (req, scope);
    }
    let (contents, snapshot) = get_claude_thinking_replay_with_snapshot_required(&scope.model_family, &scope.session_key);
    scope.snapshot = snapshot;
    scope.cache_ready = true;
    let Some(contents) = contents else { return (req, scope) };
    let (updated, restored) = restore_claude_thinking_replay_contents(&req.payload, &contents);
    if restored {
        req.payload = Bytes::from(updated);
        scope.replay_applied = true;
    }
    (req, scope)
}

/// Applies each cached turn in order (Go: `restoreClaudeThinkingReplayContents`).
pub fn restore_claude_thinking_replay_contents(body: &[u8], cached_contents: &[Vec<u8>]) -> (Vec<u8>, bool) {
    let mut updated = body.to_vec();
    let mut restored = false;
    for cached in cached_contents {
        let (next, restored_turn) = restore_replay_content(&updated, cached);
        updated = next;
        restored = restored || restored_turn;
    }
    (updated, restored)
}

/// Caches the completed assistant turn of a response (Go: `cacheClaudeThinkingReplayResponse`):
/// the `content` array of a JSON response, or the content rebuilt from an SSE stream.
pub fn cache_claude_thinking_replay_response(scope: &ClaudeThinkingReplayScope, response: &[u8]) {
    if let Some(content) = cpa_json::raw_at(response, "content").filter(|raw| raw.trim_start().starts_with('[')) {
        cache_claude_thinking_replay_content(scope, content.as_bytes());
        return;
    }
    let mut accumulator = ThinkingReplayStreamAccumulator::new();
    accumulator.observe(response);
    if let Some(content) = accumulator.content() {
        cache_claude_thinking_replay_content(scope, &content);
    }
}

/// Stores a completed turn when it is replayable (signed thinking plus a tool use), otherwise
/// clears the session state (Go: `cacheClaudeThinkingReplayContent`).
pub fn cache_claude_thinking_replay_content(scope: &ClaudeThinkingReplayScope, content: &[u8]) {
    if !scope.valid() || !scope.cache_ready {
        return;
    }
    if replay_content_is_replayable(content) {
        replace_claude_thinking_replay_if_unchanged(&scope.model_family, &scope.session_key, &scope.snapshot, content);
        return;
    }
    clear_claude_thinking_replay_content(scope);
}

/// Drops the session state if nobody changed it since the request read it (Go:
/// `clearClaudeThinkingReplayContent`).
pub fn clear_claude_thinking_replay_content(scope: &ClaudeThinkingReplayScope) {
    if !scope.valid() || !scope.cache_ready {
        return;
    }
    delete_claude_thinking_replay_if_unchanged(&scope.model_family, &scope.session_key, &scope.snapshot);
}

/// Whether an upstream error means the replayed content was rejected, so the cache should be
/// cleared (Go: `shouldClearKimiThinkingReplayAfterError`: a `statusErr` with 400 or 422). Any
/// `ExecError` with that status counts here.
pub fn should_clear_kimi_thinking_replay_after_error(err: Option<&ExecError>) -> bool {
    err.is_some_and(|e| e.status == 400 || e.status == 422)
}

/// Observes a stream while forwarding it unchanged and caches the completed turn once the stream
/// ends cleanly (Go: `wrapClaudeThinkingReplayStream`). A stream error, or a client that stops
/// reading, skips caching; an upstream `error` event after a replayed request clears the state.
pub fn wrap_claude_thinking_replay_stream(result: StreamResult, scope: ClaudeThinkingReplayScope) -> StreamResult {
    if !scope.valid() {
        return result;
    }
    let StreamResult { headers, chunks: mut input, usage } = result;
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let mut accumulator = ThinkingReplayStreamAccumulator::new();
        let mut has_error = false;
        while let Some(chunk) = input.recv().await {
            match &chunk {
                Err(_) => has_error = true,
                Ok(payload) => accumulator.observe(payload),
            }
            if tx.send(chunk).await.is_err() {
                return;
            }
        }
        if has_error {
            return;
        }
        if let Some(content) = accumulator.content() {
            cache_claude_thinking_replay_content(&scope, &content);
            return;
        }
        if accumulator.upstream_error && scope.replay_applied {
            clear_claude_thinking_replay_content(&scope);
        }
    });
    StreamResult { headers, chunks: rx, usage }
}

// ---------------------------------------------------------------------------------------------
// Session key (Go: codexReasoningReplaySessionKey for Claude sources + xaiReasoningReplayIsolateSessionKey)

/// Go: `ClaudeCodeExecutionScope` over the options headers, falling back per header to the
/// incoming request headers (Go: `claudeCodeHeader`); "" without a session id.
fn claude_code_scope(ctx: &ClaudeCtx, payload: &[u8], headers: &HeaderMap) -> String {
    let mut merged = headers.clone();
    if let Some(incoming) = ctx.incoming_headers.as_ref() {
        for name in [CLAUDE_CODE_SESSION_HEADER, CLAUDE_CODE_AGENT_HEADER] {
            if header_value_case_insensitive(headers, name).is_empty() {
                for value in incoming.get_all(name) {
                    if let Ok(header) = http::HeaderName::from_bytes(name.as_bytes()) {
                        merged.append(header, value.clone());
                    }
                }
            }
        }
    }
    claude_code_execution_scope(payload, &merged).unwrap_or_default()
}

fn metadata_string(metadata: &Metadata, key: &str) -> String {
    metadata.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Go: `codexReasoningReplaySessionKey(ctx, FormatClaude, req, opts, req.Payload)`.
fn replay_session_key(ctx: &ClaudeCtx, req: &Request, opts: &Options) -> String {
    let scope = claude_code_scope(ctx, &req.payload, &opts.headers);
    if !scope.is_empty() {
        return scope;
    }
    for metadata in [&opts.metadata, &req.metadata] {
        let value = metadata_string(metadata, meta::EXECUTION_SESSION_ID);
        if !value.is_empty() {
            return format!("execution:{value}");
        }
    }
    let value = session_key_from_payload(&req.payload);
    if !value.is_empty() {
        return value;
    }
    let value = session_key_from_headers(&opts.headers);
    if !value.is_empty() {
        return value;
    }
    if let Some(headers) = &ctx.incoming_headers {
        let value = session_key_from_headers(headers);
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

fn session_key_from_payload(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let root = cpa_json::parse(payload);
    let trimmed = |path: &str| root.g(path).str().trim().to_string();
    let prompt_cache_key = trimmed("prompt_cache_key");
    if !prompt_cache_key.is_empty() {
        return format!("prompt-cache:{prompt_cache_key}");
    }
    let window_id = trimmed("client_metadata.x-codex-window-id");
    if !window_id.is_empty() {
        return format!("window:{window_id}");
    }
    let turn_metadata = trimmed("client_metadata.x-codex-turn-metadata");
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
    // Header names are case-insensitive, so the three Go spellings are one lookup.
    for name in ["Session_id", "Session-Id"] {
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

fn session_key_from_turn_metadata(turn_metadata: &str) -> String {
    let root = cpa_json::parse_str(turn_metadata);
    let prompt_cache_key = root.g("prompt_cache_key").str().trim().to_string();
    if !prompt_cache_key.is_empty() {
        return format!("prompt-cache:{prompt_cache_key}");
    }
    let window_id = root.g("window_id").str().trim().to_string();
    if !window_id.is_empty() {
        return format!("window:{window_id}");
    }
    String::new()
}

/// Namespaces client-controlled session keys by the caller's API key so two callers cannot share
/// replay state by reusing a session header (Go: `xaiReasoningReplayIsolateSessionKey`).
/// `execution:` keys are trusted; client keys without a caller API key disable replay.
fn isolate_session_key(caller_api_key: &str, session_key: &str) -> String {
    let session_key = session_key.trim();
    if session_key.is_empty() {
        return String::new();
    }
    if session_key.starts_with("execution:") {
        return session_key.to_string();
    }
    let api_key = caller_api_key.trim();
    if api_key.is_empty() {
        return String::new();
    }
    let sum = Sha256::digest(api_key.as_bytes());
    format!("caller:{}:{session_key}", hex::encode(&sum[..8]))
}

// ---------------------------------------------------------------------------------------------
// Kimi replay helpers (Go: executor/kimi_thinking_replay.go)

/// Canonical form of the first JSON value in `raw`: sorted keys, numbers as written, HTML escaping
/// (Go: `kimiCanonicalJSON`).
fn canonical_json(raw: &[u8]) -> Option<String> {
    let value: Value = serde_json::Deserializer::from_slice(raw).into_iter::<Value>().next()?.ok()?;
    go_json_sorted(&value, GoJsonStyle::MARSHAL_USE_NUMBER)
}

fn json_equal(left: &[u8], right: &[u8]) -> bool {
    match (canonical_json(left), canonical_json(right)) {
        (Some(l), Some(r)) => l == r,
        _ => false,
    }
}

fn is_array_raw(raw: &str) -> bool {
    raw.trim_start().starts_with('[')
}

/// gjson `String()` of the value at `path` inside a raw JSON object ("" when missing).
fn part_field(part: &str, path: &str) -> String {
    cpa_json::raw_at(part.as_bytes(), path).map(|r| cpa_json::Res::owned(cpa_json::parse_str(r)).str()).unwrap_or_default()
}

fn part_type(part: &str) -> String {
    part_field(part, "type").trim().to_string()
}

/// Whether a completed turn can be replayed: a signed `thinking` block and a `tool_use` with an id
/// (Go: `kimiThinkingReplayContentIsReplayable`).
fn replay_content_is_replayable(content: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(content) else { return false };
    if !is_array_raw(text) {
        return false;
    }
    let mut has_signed_thinking = false;
    let mut has_tool_use = false;
    for part in cpa_json::raw_children(content, "") {
        match part_type(part).as_str() {
            "thinking" if !part_field(part, "signature").trim().is_empty() => has_signed_thinking = true,
            "tool_use" if !part_field(part, "id").trim().is_empty() => has_tool_use = true,
            _ => {}
        }
    }
    has_signed_thinking && has_tool_use
}

/// Canonical non-thinking parts of an assistant content array and whether it holds a tool use
/// (Go: `kimiNonThinkingContentParts`); `None` unless the content is an array with a valid tool use.
fn non_thinking_content_parts(content: &str) -> Option<Vec<String>> {
    if !is_array_raw(content) {
        return None;
    }
    let mut parts = Vec::new();
    let mut has_tool_use = false;
    for part in cpa_json::raw_children(content.as_bytes(), "") {
        match part_type(part).as_str() {
            "thinking" | "redacted_thinking" => continue,
            "tool_use" => {
                if part_field(part, "id").trim().is_empty() {
                    return None;
                }
                has_tool_use = true;
            }
            _ => {}
        }
        parts.push(canonical_json(part.as_bytes())?);
    }
    has_tool_use.then_some(parts)
}

fn content_has_thinking(content: &str) -> bool {
    is_array_raw(content)
        && cpa_json::raw_children(content.as_bytes(), "").into_iter().any(|part| matches!(part_type(part).as_str(), "thinking" | "redacted_thinking"))
}

/// Puts a cached signed assistant turn back into the last assistant message whose content equals
/// it minus thinking blocks (Go: `restoreKimiThinkingReplayContent`). The splice is byte-level.
fn restore_replay_content(body: &[u8], cached_content: &[u8]) -> (Vec<u8>, bool) {
    let unchanged = || (body.to_vec(), false);
    let Ok(cached_text) = std::str::from_utf8(cached_content) else { return unchanged() };
    let Some(cached_parts) = non_thinking_content_parts(cached_text) else { return unchanged() };
    let Some(messages) = cpa_json::raw_at(body, "messages").filter(|raw| is_array_raw(raw)) else { return unchanged() };
    let items = cpa_json::raw_children(messages.as_bytes(), "");
    for message in items.into_iter().rev() {
        if !part_field(message, "role").trim().eq_ignore_ascii_case("assistant") {
            continue;
        }
        let current = cpa_json::raw_at(message.as_bytes(), "content").unwrap_or("");
        if json_equal(current.as_bytes(), cached_content) {
            return unchanged();
        }
        if content_has_thinking(current) {
            continue;
        }
        let Some(current_parts) = non_thinking_content_parts(current) else { continue };
        if current_parts != cached_parts {
            continue;
        }
        // `current` borrows from `body`; its offset is the splice position.
        let Some(start) = (current.as_ptr() as usize).checked_sub(body.as_ptr() as usize) else { return unchanged() };
        let mut out = Vec::with_capacity(body.len() + cached_content.len());
        out.extend_from_slice(&body[..start]);
        out.extend_from_slice(cached_content);
        out.extend_from_slice(&body[start + current.len()..]);
        return (out, true);
    }
    unchanged()
}

#[derive(Default)]
struct StreamBlock {
    raw: Vec<u8>,
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

/// Rebuilds the assistant `content` array of a streamed Messages response, bounded in blocks and
/// bytes (Go: `kimiThinkingReplayStreamAccumulator`). Any anomaly abandons the whole turn.
pub struct ThinkingReplayStreamAccumulator {
    blocks: BTreeMap<i64, StreamBlock>,
    observed: bool,
    complete: bool,
    /// An upstream `error` event was seen.
    pub upstream_error: bool,
    abandoned: bool,
    bytes_used: usize,
}

impl Default for ThinkingReplayStreamAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl ThinkingReplayStreamAccumulator {
    pub fn new() -> Self {
        Self { blocks: BTreeMap::new(), observed: false, complete: false, upstream_error: false, abandoned: false, bytes_used: 0 }
    }

    /// Feeds a chunk of SSE lines (Go: `observe`).
    pub fn observe(&mut self, chunk: &[u8]) {
        for line in chunk.split(|b| *b == b'\n') {
            let line = trim_space(line);
            let Some(payload) = line.strip_prefix(b"data:") else { continue };
            let payload = trim_space(payload);
            if payload.is_empty() || payload == b"[DONE]" {
                continue;
            }
            if !json_valid(payload) {
                self.abandon();
                continue;
            }
            let root = cpa_json::parse(payload);
            match root.g("type").str().as_str() {
                "message_start" => self.observed = true,
                "content_block_start" => {
                    if !self.abandoned {
                        self.observe_block_start(&root, payload);
                    }
                }
                "content_block_delta" => {
                    if !self.abandoned {
                        self.observe_block_delta(&root);
                    }
                }
                "content_block_stop" => {
                    if !self.abandoned {
                        self.finish_block(root.g("index").int());
                    }
                }
                "message_stop" => self.complete = true,
                "error" => {
                    self.upstream_error = true;
                    self.abandon();
                }
                _ => {}
            }
        }
    }

    fn observe_block_start(&mut self, root: &Value, payload: &[u8]) {
        let index = root.g("index").int();
        let block_raw = cpa_json::raw_at(payload, "content_block").filter(|raw| raw.starts_with('{'));
        let Some(raw) = block_raw else {
            self.abandon();
            return;
        };
        if self.blocks.len() >= KIMI_THINKING_REPLAY_CACHE_MAX_BLOCKS_PER_ENTRY || self.blocks.contains_key(&index) {
            self.abandon();
            return;
        }
        if !self.reserve_bytes(raw.len()) {
            return;
        }
        self.blocks.insert(index, StreamBlock { raw: raw.as_bytes().to_vec(), ..Default::default() });
    }

    fn observe_block_delta(&mut self, root: &Value) {
        let index = root.g("index").int();
        let Some(mut block) = self.blocks.remove(&index) else {
            self.abandon();
            return;
        };
        let delta = root.g("delta");
        match delta.g("type").str().as_str() {
            "text_delta" => self.append_block_text(&mut block, Field::Text, &delta.g("text").str()),
            "thinking_delta" => self.append_block_text(&mut block, Field::Thinking, &delta.g("thinking").str()),
            "signature_delta" => self.append_block_text(&mut block, Field::Signature, &delta.g("signature").str()),
            "input_json_delta" => {
                let suffix = delta.g("partial_json").str();
                if self.reserve_bytes(suffix.len()) {
                    block.input.push_str(&suffix);
                    block.has_input_delta = true;
                }
            }
            _ => self.abandon(),
        }
        if !self.abandoned {
            self.blocks.insert(index, block);
        }
    }

    /// Go: `appendBlockText`: seeds the builder from the block's initial value once, then appends.
    fn append_block_text(&mut self, block: &mut StreamBlock, field: Field, suffix: &str) {
        let (builder, initialized, path) = match field {
            Field::Text => (&mut block.text, &mut block.text_initialized, "text"),
            Field::Thinking => (&mut block.thinking, &mut block.thinking_initialized, "thinking"),
            Field::Signature => (&mut block.signature, &mut block.signature_initialized, "signature"),
        };
        if !*initialized {
            let initial = cpa_json::parse(&block.raw).g(path).str();
            if !reserve(&mut self.bytes_used, &mut self.abandoned, initial.len()) {
                return;
            }
            builder.push_str(&initial);
            *initialized = true;
        }
        if reserve(&mut self.bytes_used, &mut self.abandoned, suffix.len()) {
            builder.push_str(suffix);
        }
    }

    fn finish_block(&mut self, index: i64) {
        let Some(block) = self.blocks.get_mut(&index) else {
            self.abandon();
            return;
        };
        if block.has_input_delta && !json_valid(block.input.as_bytes()) {
            self.abandon();
            return;
        }
        block.finished = true;
    }

    fn reserve_bytes(&mut self, count: usize) -> bool {
        reserve(&mut self.bytes_used, &mut self.abandoned, count)
    }

    fn abandon(&mut self) {
        self.abandoned = true;
        self.blocks.clear();
        self.bytes_used = 0;
    }

    /// The rebuilt `content` array once the stream completed cleanly (Go: `content`).
    pub fn content(&mut self) -> Option<Vec<u8>> {
        if !self.observed || !self.complete || self.upstream_error || self.abandoned {
            return None;
        }
        let mut parts: Vec<Vec<u8>> = Vec::with_capacity(self.blocks.len());
        for block in std::mem::take(&mut self.blocks).into_values() {
            if !block.finished {
                self.abandon();
                return None;
            }
            match rebuild_block(block) {
                Some(raw) => parts.push(raw),
                None => {
                    self.abandon();
                    return None;
                }
            }
        }
        let mut content = Vec::with_capacity(parts.iter().map(|p| p.len() + 1).sum::<usize>() + 2);
        content.push(b'[');
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                content.push(b',');
            }
            content.extend_from_slice(part);
        }
        content.push(b']');
        if content.len() > KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY {
            self.abandon();
            return None;
        }
        Some(content)
    }
}

enum Field {
    Text,
    Thinking,
    Signature,
}

/// Charges `count` bytes against the per-entry budget; abandons on overflow (Go: `reserveBytes`).
fn reserve(bytes_used: &mut usize, abandoned: &mut bool, count: usize) -> bool {
    if *bytes_used > KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY.saturating_sub(count) || count > KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY {
        *abandoned = true;
        *bytes_used = 0;
        return false;
    }
    *bytes_used += count;
    true
}

/// Block raw JSON with the accumulated text, thinking, signature and input set in place.
fn rebuild_block(block: StreamBlock) -> Option<Vec<u8>> {
    let mut raw = block.raw;
    if block.text_initialized {
        raw = set_raw_top_level(&raw, "text", sjson_string(&block.text).as_bytes()).ok()?;
    }
    if block.thinking_initialized {
        raw = set_raw_top_level(&raw, "thinking", sjson_string(&block.thinking).as_bytes()).ok()?;
    }
    if block.signature_initialized {
        raw = set_raw_top_level(&raw, "signature", sjson_string(&block.signature).as_bytes()).ok()?;
    }
    if block.has_input_delta {
        raw = set_raw_top_level(&raw, "input", block.input.as_bytes()).ok()?;
    }
    Some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_runtime::executor::meta;

    const COMPAT_INFO_KEY: &str = "cliproxy.resolved_api_key_model_info";

    fn test_auth(base_url: &str) -> Auth {
        let mut auth = Auth::default();
        auth.id = "claude-replay-auth".into();
        auth.provider = "claude".into();
        auth.attributes.insert("api_key".into(), "key-claude-replay".into());
        auth.attributes.insert("auth_kind".into(), "apikey".into());
        auth.attributes.insert("base_url".into(), base_url.into());
        auth
    }

    fn test_request(payload: &str, session: &str, is_compat: bool, source: Format) -> (Request, Options) {
        let mut req = Request {
            model: "claude-synthetic-4772".into(),
            payload: Bytes::from(payload.to_string()),
            format: Format::Claude,
            metadata: Metadata::new(),
        };
        req.metadata.insert(COMPAT_INFO_KEY.into(), serde_json::json!({"id": "claude-synthetic-4772", "is_compat": is_compat}));
        let mut opts = Options::new(source);
        opts.metadata.insert(meta::EXECUTION_SESSION_ID.into(), Value::String(session.into()));
        (req, opts)
    }

    fn scope_for(auth: &Auth, req: &Request, opts: &Options) -> ClaudeThinkingReplayScope {
        claude_thinking_replay_scope_from_request(&ClaudeCtx::default(), auth, req, opts, "")
    }

    fn content_of(payload: &[u8], path: &str) -> Vec<Value> {
        cpa_json::parse(payload).g(path).array().iter().map(|v| v.value()).collect()
    }

    #[test]
    fn enabled_requires_compat_claude_api_key() {
        let (request, options) = test_request(r#"{"messages":[]}"#, "scope", true, Format::Claude);
        let base = test_auth("http://127.0.0.1");
        assert!(claude_thinking_replay_enabled(&base, &request, &options), "compat Claude API key");

        let (non_compat, _) = test_request(r#"{"messages":[]}"#, "scope-non-compat", false, Format::Claude);
        assert!(!claude_thinking_replay_enabled(&base, &non_compat, &options), "non compat model");

        let mut oauth = base.clone();
        oauth.attributes.insert("auth_kind".into(), "oauth".into());
        oauth.attributes.insert("api_key".into(), "sk-ant-oat-replay".into());
        assert!(!claude_thinking_replay_enabled(&oauth, &request, &options), "OAuth credential");

        let mut other = base.clone();
        other.provider = "kimi".into();
        assert!(!claude_thinking_replay_enabled(&other, &request, &options), "other provider");

        let mut openai = options.clone();
        openai.source_format = Format::OpenAI;
        assert!(!claude_thinking_replay_enabled(&base, &request, &openai), "OpenAI source format");
    }

    const FIRST_RESPONSE: &str = r#"{"id":"msg-1","type":"message","role":"assistant","model":"claude-synthetic-4772","content":[{"type":"thinking","thinking":"provider reasoning","signature":"EgI="},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}],"stop_reason":"tool_use"}"#;
    const SECOND_PAYLOAD: &str = r#"{"messages":[{"role":"user","content":"inspect"},{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}]}"#;

    /// Runs one request through prepare and, with `response`, caches its turn.
    fn turn(auth: &Auth, payload: &str, session: &str, response: &[u8]) -> Bytes {
        let (req, opts) = test_request(payload, session, true, Format::Claude);
        let (req, scope) = prepare_claude_thinking_replay_request(&ClaudeCtx::default(), auth, req, &opts, "");
        cache_claude_thinking_replay_response(&scope, response);
        req.payload
    }

    #[test]
    fn restores_omitted_block() {
        let auth = test_auth("http://127.0.0.1");
        turn(&auth, r#"{"messages":[{"role":"user","content":"inspect"}]}"#, "nonstream-replay", FIRST_RESPONSE.as_bytes());
        let sent = turn(&auth, SECOND_PAYLOAD, "nonstream-replay", br#"{"id":"msg-2","content":[{"type":"text","text":"done"}]}"#);

        let content = content_of(&sent, "messages.1.content");
        assert_eq!(content.len(), 2, "{}", String::from_utf8_lossy(&sent));
        assert_eq!(content[0].g("type").str(), "thinking");
        assert_eq!(content[0].g("signature").str(), "EgI=");
        // Only the assistant content changed; the restored bytes are the cached response bytes.
        let want = SECOND_PAYLOAD.replacen(
            r#"[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}]"#,
            r#"[{"type":"thinking","thinking":"provider reasoning","signature":"EgI="},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}]"#,
            1,
        );
        assert_eq!(String::from_utf8_lossy(&sent), want);
    }

    const STREAM: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"provider reasoning\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"EgI=\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Read\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    #[test]
    fn restores_omitted_block_from_stream() {
        let auth = test_auth("http://127.0.0.1");
        turn(&auth, r#"{"messages":[{"role":"user","content":"inspect"}]}"#, "stream-replay", STREAM.as_bytes());
        let sent = turn(&auth, SECOND_PAYLOAD, "stream-replay", b"");

        let content = content_of(&sent, "messages.1.content");
        assert!(content.len() == 2 && content[0].g("type").str() == "thinking", "{}", String::from_utf8_lossy(&sent));
        assert_eq!(content[0].g("signature").str(), "EgI=");
        assert_eq!(content[0].g("thinking").str(), "provider reasoning");
        assert_eq!(content[1].g("input.path").str(), "README.md");
    }

    #[test]
    fn accumulator_rebuilds_content_bytes() {
        let mut acc = ThinkingReplayStreamAccumulator::new();
        // Chunks may split anywhere between lines; only whole `data:` lines are read.
        for chunk in STREAM.split_inclusive("\n\n") {
            acc.observe(chunk.as_bytes());
        }
        let content = acc.content().expect("complete stream");
        assert_eq!(
            String::from_utf8_lossy(&content),
            r#"[{"type":"thinking","thinking":"provider reasoning","signature":"EgI="},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}]"#
        );
        assert!(replay_content_is_replayable(&content));
    }

    #[test]
    fn accumulator_abandons_on_anomalies() {
        let start = |index: i64, block: &str| format!("data: {{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{block}}}\n");
        let complete = "data: {\"type\":\"message_start\",\"message\":{}}\ndata: {\"type\":\"message_stop\"}\n";
        // Unfinished block.
        let mut acc = ThinkingReplayStreamAccumulator::new();
        acc.observe(format!("{}{}", start(0, r#"{"type":"text","text":""}"#), complete).as_bytes());
        assert!(acc.content().is_none());
        // Duplicate block index.
        let mut acc = ThinkingReplayStreamAccumulator::new();
        acc.observe(format!("{}{}{}", start(0, r#"{"type":"text"}"#), start(0, r#"{"type":"text"}"#), complete).as_bytes());
        assert!(acc.content().is_none());
        // Invalid JSON in a data line, and an error event.
        let mut acc = ThinkingReplayStreamAccumulator::new();
        acc.observe(format!("data: {{nope\n{complete}").as_bytes());
        assert!(acc.content().is_none());
        let mut acc = ThinkingReplayStreamAccumulator::new();
        acc.observe(format!("data: {{\"type\":\"error\"}}\n{complete}").as_bytes());
        assert!(acc.upstream_error && acc.content().is_none());
        // Truncated tool input JSON.
        let mut acc = ThinkingReplayStreamAccumulator::new();
        acc.observe(
            format!(
                "{}data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"a\"}}}}\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n{complete}",
                start(0, r#"{"type":"tool_use","id":"t","name":"n","input":{}}"#)
            )
            .as_bytes(),
        );
        assert!(acc.content().is_none());
    }

    #[test]
    fn clears_after_upstream_bad_request() {
        let auth = test_auth("http://127.0.0.1");
        let session = "bad-request-replay";
        let first = r#"{"messages":[{"role":"user","content":"inspect"}]}"#;
        turn(&auth, first, session, FIRST_RESPONSE.as_bytes());

        // Second request replays, then the upstream rejects it with 400.
        let (req, opts) = test_request(SECOND_PAYLOAD, session, true, Format::Claude);
        let (_, scope) = prepare_claude_thinking_replay_request(&ClaudeCtx::default(), &auth, req, &opts, "");
        assert!(scope.replay_applied);
        let err = ExecError::new(400, "invalid thinking signature");
        assert!(should_clear_kimi_thinking_replay_after_error(Some(&err)));
        assert!(should_clear_kimi_thinking_replay_after_error(Some(&ExecError::new(422, "x"))));
        assert!(!should_clear_kimi_thinking_replay_after_error(Some(&ExecError::new(500, "x"))));
        assert!(!should_clear_kimi_thinking_replay_after_error(None));
        clear_claude_thinking_replay_content(&scope);

        let (req, opts) = test_request(first, session, true, Format::Claude);
        let scope = scope_for(&auth, &req, &opts);
        assert!(cpa_core::cache::get_claude_thinking_replay_required(&scope.model_family, &scope.session_key).is_none());
    }

    #[test]
    fn restores_multiple_omitted_blocks() {
        let auth = test_auth("http://127.0.0.1");
        let session = "multi-turn-replay";
        let turn1 = r#"{"id":"msg-1","content":[{"type":"thinking","thinking":"first","signature":"EgI="},{"type":"tool_use","id":"toolu-1","name":"Read","input":{"path":"one"}}]}"#;
        let turn2 = r#"{"id":"msg-2","content":[{"type":"thinking","thinking":"second","signature":"EgM="},{"type":"tool_use","id":"toolu-2","name":"Read","input":{"path":"two"}}]}"#;
        let done = br#"{"id":"msg-3","content":[{"type":"text","text":"done"}]}"#;
        turn(&auth, r#"{"messages":[{"role":"user","content":"inspect"}]}"#, session, turn1.as_bytes());
        let second = r#"{"messages":[{"role":"user","content":"inspect"},{"role":"assistant","content":[{"type":"tool_use","id":"toolu-1","name":"Read","input":{"path":"one"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu-1","content":"one result"}]}]}"#;
        turn(&auth, second, session, turn2.as_bytes());
        let third = r#"{"messages":[{"role":"user","content":"inspect"},{"role":"assistant","content":[{"type":"tool_use","id":"toolu-1","name":"Read","input":{"path":"one"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu-1","content":"one result"}]},{"role":"assistant","content":[{"type":"tool_use","id":"toolu-2","name":"Read","input":{"path":"two"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu-2","content":"two result"}]}]}"#;
        let sent = turn(&auth, third, session, done);

        let first_content = content_of(&sent, "messages.1.content");
        let second_content = content_of(&sent, "messages.3.content");
        assert!(first_content.len() == 2 && first_content[0].g("signature").str() == "EgI=", "{}", String::from_utf8_lossy(&sent));
        assert!(second_content.len() == 2 && second_content[0].g("signature").str() == "EgM=", "{}", String::from_utf8_lossy(&sent));
    }

    #[test]
    fn restore_leaves_unrelated_or_already_replayed_messages() {
        let cached = br#"[{"type":"thinking","thinking":"t","signature":"s"},{"type":"tool_use","id":"a","name":"n","input":{}}]"#.to_vec();
        // Already carries the cached content (key order and spacing aside): unchanged.
        let body = br#"{"messages":[{"role":"assistant","content":[{"signature":"s","type":"thinking","thinking":"t"},{"type":"tool_use","id":"a","name":"n","input":{}}]}]}"#;
        assert_eq!(restore_claude_thinking_replay_contents(body, std::slice::from_ref(&cached)), (body.to_vec(), false));
        // A different tool call does not match.
        let body = br#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"b","name":"n","input":{}}]}]}"#;
        assert_eq!(restore_claude_thinking_replay_contents(body, std::slice::from_ref(&cached)), (body.to_vec(), false));
        // Only the last matching assistant message is rewritten.
        let body = br#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"n","input":{}}]},{"role":"assistant","content":[{ "type":"tool_use", "id":"a", "name":"n", "input":{} }]}]}"#;
        let (out, restored) = restore_claude_thinking_replay_contents(body, std::slice::from_ref(&cached));
        assert!(restored);
        let want = format!(
            r#"{{"messages":[{{"role":"assistant","content":[{{"type":"tool_use","id":"a","name":"n","input":{{}}}}]}},{{"role":"assistant","content":{}}}]}}"#,
            String::from_utf8_lossy(&cached)
        );
        assert_eq!(String::from_utf8_lossy(&out), want);
    }

    #[test]
    fn session_keys_and_isolation() {
        let auth = test_auth("http://127.0.0.1");
        // Execution sessions are trusted and need no caller key.
        let (req, opts) = test_request("{}", "abc", true, Format::Claude);
        assert_eq!(scope_for(&auth, &req, &opts).session_key, "execution:abc");

        // Client-controlled keys need a caller key and are namespaced by its hash.
        let mut opts = Options::new(Format::Claude);
        opts.headers.insert("Session_id", "s-1".parse().expect("header value"));
        let (req, _) = test_request("{}", "", true, Format::Claude);
        let ctx = ClaudeCtx::default();
        assert_eq!(claude_thinking_replay_scope_from_request(&ctx, &auth, &req, &opts, "").session_key, "");
        let hashed = claude_thinking_replay_scope_from_request(&ctx, &auth, &req, &opts, "caller-key").session_key;
        let sum = Sha256::digest(b"caller-key");
        assert_eq!(hashed, format!("caller:{}:session-id:s-1", hex::encode(&sum[..8])));

        // Claude Code session and agent identity win over everything else.
        let (req, _) = test_request(r#"{"metadata":{"user_id":"user_x_account__session_0a1b-2c"},"prompt_cache_key":"pck"}"#, "", true, Format::Claude);
        let mut opts = Options::new(Format::Claude);
        opts.headers.insert("X-Claude-Code-Agent-Id", "sub".parse().expect("header value"));
        assert!(claude_thinking_replay_scope_from_request(&ctx, &auth, &req, &opts, "k").session_key.ends_with(":claude:0a1b-2c:agent:sub"));
        let (req, _) = test_request(r#"{"prompt_cache_key":"pck"}"#, "", true, Format::Claude);
        assert!(claude_thinking_replay_scope_from_request(&ctx, &auth, &req, &Options::new(Format::Claude), "k").session_key.ends_with(":prompt-cache:pck"));

        // Model family: credential hash plus base model without thinking suffix.
        let sum = Sha256::digest(b"claude-replay-auth");
        assert_eq!(claude_thinking_replay_model_family(&auth, "m(high)"), format!("claude:{}:m", hex::encode(&sum[..8])));
        assert_eq!(claude_thinking_replay_model_family(&Auth::default(), "m"), "claude:m");
        assert_eq!(claude_thinking_replay_model_family(&auth, ""), "");
    }

    #[tokio::test]
    async fn stream_wrapper_forwards_and_caches() {
        let auth = test_auth("http://127.0.0.1");
        let session = "wrapped-stream-replay";
        let (req, opts) = test_request(r#"{"messages":[{"role":"user","content":"inspect"}]}"#, session, true, Format::Claude);
        let (_, scope) = prepare_claude_thinking_replay_request(&ClaudeCtx::default(), &auth, req, &opts, "");

        let (tx, rx) = mpsc::channel(4);
        let wrapped = wrap_claude_thinking_replay_stream(StreamResult::new(HeaderMap::new(), rx), scope);
        let chunks: Vec<Bytes> = STREAM.split_inclusive("\n\n").map(|c| Bytes::from(c.to_string())).collect();
        let sent = tokio::spawn(async move {
            for chunk in chunks {
                tx.send(Ok(chunk)).await.expect("send");
            }
        });
        let mut got = String::new();
        let mut rx = wrapped.chunks;
        while let Some(chunk) = rx.recv().await {
            got.push_str(&String::from_utf8_lossy(&chunk.expect("chunk")));
        }
        sent.await.expect("sender");
        assert_eq!(got, STREAM);

        // The writer task caches after the stream closes; the next request sees the turn.
        let sent = turn(&auth, SECOND_PAYLOAD, session, b"");
        assert_eq!(content_of(&sent, "messages.1.content").len(), 2, "{}", String::from_utf8_lossy(&sent));
    }
}

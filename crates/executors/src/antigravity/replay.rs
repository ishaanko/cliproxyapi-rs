//! Reasoning replay, apply side (Go: antigravity_reasoning_replay.go).
//!
//! Gemini thought signatures are opaque and must come back on later turns, but translated
//! clients (OpenAI, Claude) drop them. The capture side (see `replay_capture`) records what the
//! upstream emitted per (model, session); this side merges the recorded signatures and native
//! function call identities back into `request.contents` of the next request.
//!
//! Go has a batched splicing path and a sequential path with identical output; this port is the
//! sequential one. Items are applied one at a time against the payload the previous item
//! produced. Locating a target is read-only over an [`Index`]; a located item becomes a [`Plan`]
//! that mutates the payload, after which the index is rebuilt.

use std::cell::RefCell;
use std::collections::HashMap;

use cpa_core::cache::{
    AntigravityReasoningReplaySnapshot, delete_antigravity_reasoning_replay_items_if_unchanged,
    get_antigravity_reasoning_replay_items_with_snapshot_required,
};
use cpa_core::signature::{GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, validate_gemini_function_call_pairing};
use cpa_core::util::{
    GoJsonStyle, gemini_claude_tool_use_id, go_json_sorted, is_gemini_claude_tool_use_id, map_sanitized_function_name,
    sanitized_function_name_map,
};
use cpa_json::J;
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, meta};
use cpa_translator::Format;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::helps::session::{claude_code_execution_scope, header_value_case_insensitive};
use super::request::generate_stable_session_id;
use super::signature::{normalize_function_response_roles, uses_reasoning_replay_cache};
use crate::helps::session::derived_session_id;

const SIGNATURE_FIELDS: [&str; 3] = ["thoughtSignature", "thought_signature", "extra_content.google.thought_signature"];

// ---------------------------------------------------------------- scope

/// Which (model, session) ledger entry a request reads and writes.
#[derive(Clone, Default)]
pub(crate) struct ReplayScope {
    pub model_name: String,
    pub session_key: String,
    pub snapshot: AntigravityReasoningReplaySnapshot,
}

impl ReplayScope {
    pub fn valid(&self) -> bool {
        !self.model_name.trim().is_empty() && !self.session_key.trim().is_empty()
    }
}

/// Non-reversible tag for logging replay identifiers.
pub(crate) fn replay_log_key(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    hex::encode(&Sha256::digest(value.as_bytes())[..8])
}

fn session_id_from_payload(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let v = cpa_json::parse(payload);
    for path in ["sessionId", "session_id", "request.sessionId", "request.session_id"] {
        let id = v.g(path).str().trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    String::new()
}

fn scope_from_payload(model_name: &str, payload: &[u8]) -> ReplayScope {
    let mut session_id = session_id_from_payload(payload);
    if session_id.is_empty() {
        let stable = generate_stable_session_id(&cpa_json::parse(payload));
        let stable = stable.trim();
        if !stable.is_empty() {
            session_id = stable.strip_prefix('-').filter(|s| !s.is_empty()).unwrap_or(stable).to_string();
        }
    }
    if session_id.is_empty() {
        return ReplayScope::default();
    }
    ReplayScope {
        model_name: model_name.trim().to_string(),
        session_key: format!("session:{session_id}"),
        snapshot: Default::default(),
    }
}

fn normalize_system(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(k, _)| !k.trim().eq_ignore_ascii_case("cache_control"))
                .map(|(k, v)| (k, normalize_system(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_system).collect()),
        other => other,
    }
}

/// Hash of the normalized system prompt so Claude Code lanes with different prompts stay apart.
fn claude_replay_system_lane(payload: &[u8]) -> String {
    let v = cpa_json::parse(payload);
    let system = v.g("system");
    if !system.exists() {
        return String::new();
    }
    let normalized = normalize_system(system.value());
    match go_json_sorted(&normalized, GoJsonStyle::MARSHAL_ANY) {
        Some(s) => hex::encode(&Sha256::digest(s.as_bytes())[..16]),
        None => String::new(),
    }
}

fn metadata_string(metadata: &Metadata, key: &str) -> String {
    metadata.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Downstream session identity, in the priority order of Go's
/// `antigravityReasoningReplayClientSessionKey`.
fn client_session_key(req: &Request, opts: &Options) -> String {
    for raw in [&opts.original_request[..], &req.payload[..]] {
        if let Some(scope) = claude_code_execution_scope(raw, &opts.headers) {
            let lane = claude_replay_system_lane(raw);
            return if lane.is_empty() { scope } else { format!("{scope}:context:{lane}") };
        }
    }
    for name in ["Session-Id", "Session_id"] {
        let value = header_value_case_insensitive(&opts.headers, name);
        if !value.is_empty() {
            return format!("responses:{value}");
        }
    }
    for raw in [&opts.original_request[..], &req.payload[..]] {
        if raw.is_empty() {
            continue;
        }
        let v = cpa_json::parse(raw);
        for path in ["session_id", "metadata.session_id"] {
            let value = v.g(path).str().trim().to_string();
            if !value.is_empty() {
                return format!("responses:{value}");
            }
        }
    }
    for metadata in [&opts.metadata, &req.metadata] {
        let value = metadata_string(metadata, meta::EXECUTION_SESSION_ID);
        if !value.is_empty() {
            return format!("execution:{value}");
        }
    }
    for raw in [&opts.original_request[..], &req.payload[..]] {
        let value = cpa_json::parse(raw).g("prompt_cache_key").str().trim().to_string();
        if !value.is_empty() {
            return format!("prompt-cache:{value}");
        }
    }
    let derived = derived_session_id(&[&opts.metadata, &req.metadata]);
    if !derived.is_empty() {
        return format!("derived:{derived}");
    }
    String::new()
}

pub(crate) fn scope_from_request(model_name: &str, req: &Request, opts: &Options, payload: &[u8]) -> ReplayScope {
    // An explicit downstream session beats a sessionId synthesized from request text.
    let key = client_session_key(req, opts);
    if !key.is_empty() {
        return ReplayScope { model_name: model_name.to_string(), session_key: key, snapshot: Default::default() };
    }
    let scope = scope_from_payload(model_name, payload);
    if scope.valid() {
        return scope;
    }
    let scope = scope_from_payload(model_name, &req.payload);
    if scope.valid() { scope } else { ReplayScope::default() }
}

// ---------------------------------------------------------------- small JSON helpers

fn canon_value(v: &Value) -> String {
    go_json_sorted(v, GoJsonStyle::MARSHAL_ANY).unwrap_or_else(|| v.to_string().trim().to_string())
}

fn canon_raw(raw: &str) -> String {
    match cpa_core::util::go_json_canonicalize(raw) {
        Some(s) => s,
        None => raw.trim().to_string(),
    }
}

pub(crate) fn has_native_thought_signature(signature: &str) -> bool {
    let s = signature.trim();
    !s.is_empty() && s != GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
}

pub(crate) fn native_part_thought_signature(part: &Value) -> String {
    for path in SIGNATURE_FIELDS {
        let s = part.g(path).str().trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

fn delete_signature_fields(part: &mut Value) {
    for field in SIGNATURE_FIELDS {
        cpa_json::delete(part, field);
    }
}

fn trimmed(v: &Value, path: &str) -> String {
    v.g(path).str().trim().to_string()
}

fn role_is_model(content: &Value) -> bool {
    content.g("role").str().trim().eq_ignore_ascii_case("model")
}

/// `(kind, fingerprint)` of a semantic text part; empty for function parts and non-text parts.
pub(crate) fn part_fingerprint(part: &Value) -> (String, String) {
    if part.g("functionCall").exists() || part.g("functionResponse").exists() {
        return (String::new(), String::new());
    }
    let text = part.g("text");
    if !text.exists() {
        return (String::new(), String::new());
    }
    let kind = if part.g("thought").bool() { "thought" } else { "text" };
    let sum = Sha256::digest(format!("{kind}\0{}", text.str()).as_bytes());
    (kind.to_string(), hex::encode(sum))
}

/// How many earlier parts share the fingerprint of the part at `target_index`.
pub(crate) fn part_occurrence(parts: &[&Value], target_index: usize, kind: &str, hash: &str) -> usize {
    parts
        .iter()
        .take(target_index)
        .filter(|p| {
            let (k, f) = part_fingerprint(p);
            k == kind && f == hash
        })
        .count()
}

/// Key identifying a call by name, canonical args and id (used for de-duplication).
pub(crate) fn function_call_key(name: &str, args_raw: &str, call_id: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    let args = if args_raw.trim().is_empty() { args_raw.to_string() } else { canon_raw(args_raw) };
    let sum = Sha256::digest([name, &args, call_id].join("\0").as_bytes());
    format!("fc:{}", hex::encode(&sum[..8]))
}

// ---------------------------------------------------------------- request index

#[derive(Clone, Copy)]
struct Located<'a> {
    ci: usize,
    pi: usize,
    part: &'a Value,
    fc: &'a Value,
}

struct IndexedContent<'a> {
    content: &'a Value,
    parts: Vec<&'a Value>,
}

struct Fingerprints {
    hasher: Sha256,
    sums: Vec<String>,
    wrote: bool,
}

impl Fingerprints {
    fn write(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.hasher.update(data);
        self.wrote = true;
    }

    fn sum(&self) -> String {
        if self.wrote { hex::encode(self.hasher.clone().finalize()) } else { String::new() }
    }
}

/// Immutable view over one revision of the request payload. Context fingerprints are memoized
/// as running SHA-256 prefix sums, so it must be rebuilt after every mutation.
pub(crate) struct Index<'a> {
    valid_contents: bool,
    contents: Vec<IndexedContent<'a>>,
    function_calls_by_id: HashMap<String, (usize, usize)>,
    function_response_content_by_id: HashMap<String, usize>,
    fingerprints: RefCell<Fingerprints>,
}

fn contents_of(root: &Value) -> Option<&Vec<Value>> {
    root.get("request")?.get("contents")?.as_array()
}

impl<'a> Index<'a> {
    pub(crate) fn new(root: &'a Value) -> Self {
        let mut index = Index {
            valid_contents: false,
            contents: Vec::new(),
            function_calls_by_id: HashMap::new(),
            function_response_content_by_id: HashMap::new(),
            fingerprints: RefCell::new(Fingerprints { hasher: Sha256::new(), sums: vec![String::new()], wrote: false }),
        };
        let Some(contents) = contents_of(root) else { return index };
        index.valid_contents = true;
        for (ci, content) in contents.iter().enumerate() {
            let parts: Vec<&Value> = match content.get("parts") {
                Some(Value::Array(a)) => a.iter().collect(),
                _ => Vec::new(),
            };
            for (pi, part) in parts.iter().enumerate() {
                if part.g("functionCall").exists() {
                    let id = trimmed(part, "functionCall.id");
                    if !id.is_empty() {
                        index.function_calls_by_id.entry(id).or_insert((ci, pi));
                    }
                }
                if part.g("functionResponse").exists() {
                    let id = trimmed(part, "functionResponse.id");
                    if !id.is_empty() {
                        index.function_response_content_by_id.entry(id).or_insert(ci);
                    }
                }
            }
            index.contents.push(IndexedContent { content, parts });
        }
        let mut fp = Fingerprints { hasher: Sha256::new(), sums: Vec::new(), wrote: false };
        for path in ["request.systemInstruction", "request.tools", "request.toolConfig"] {
            let value = root.g(path);
            if value.exists() {
                fp.write(path.as_bytes());
                fp.write(&[0]);
                fp.write(canon_value(&value.value()).as_bytes());
                fp.write(&[0]);
            }
        }
        let first = fp.sum();
        fp.sums.push(first);
        *index.fingerprints.borrow_mut() = fp;
        index
    }

    fn function_call_location(&self, call_id: &str) -> Option<Located<'a>> {
        let &(ci, pi) = self.function_calls_by_id.get(call_id.trim())?;
        let part = self.contents[ci].parts[pi];
        Some(Located { ci, pi, part, fc: part.get("functionCall")? })
    }

    fn function_response_content_index(&self, call_id: &str) -> Option<usize> {
        self.function_response_content_by_id.get(call_id.trim()).copied()
    }

    /// Fingerprint of everything before `before_content_index` (plus system, tools, tool config).
    pub(crate) fn context_fingerprint(&self, before: usize) -> String {
        if !self.valid_contents || before > self.contents.len() {
            return String::new();
        }
        let mut fp = self.fingerprints.borrow_mut();
        while fp.sums.len() <= before {
            let ci = fp.sums.len() - 1;
            let content = &self.contents[ci];
            let role = content.content.g("role").str().trim().to_lowercase();
            fp.write(role.as_bytes());
            fp.write(&[0]);
            for part in &content.parts {
                let mut normalized = (*part).clone();
                delete_signature_fields(&mut normalized);
                fp.write(canon_value(&normalized).as_bytes());
                fp.write(&[0]);
            }
            let sum = fp.sum();
            fp.sums.push(sum);
        }
        fp.sums[before].clone()
    }

    fn context_matches(&self, item: &Value, content_index: usize) -> bool {
        let expected = trimmed(item, "contextHash");
        expected.is_empty() || expected == self.context_fingerprint(content_index)
    }

    /// The content a streamed response extends: the trailing model turn without responses, or a
    /// new turn after the last one. Second value is the next part index there.
    pub(crate) fn pending_model_content_index(&self) -> (usize, usize) {
        let Some(last) = self.contents.last() else { return (0, 0) };
        let last_index = self.contents.len() - 1;
        if role_is_model(last.content) && !last.parts.iter().any(|p| p.g("functionResponse").exists()) {
            return (last_index, last.parts.len());
        }
        (self.contents.len(), 0)
    }

    fn function_response_content_index_for_replay(&self, item: &Value) -> Option<(usize, String)> {
        let call_id = trimmed(item, "call_id");
        let name = trimmed(item, "name");
        let args_raw = item.g("args").raw();
        let mut candidates = vec![call_id.clone()];
        let stable = gemini_claude_tool_use_id(&call_id, &name, &args_raw);
        if !stable.is_empty() && stable != call_id {
            candidates.push(stable);
        }
        candidates
            .into_iter()
            .find_map(|c| self.function_response_content_index(&c).map(|i| (i, c)))
    }

    fn function_call_part_location_for_replay(
        &self,
        item: &Value,
        schemas: &HashMap<String, Value>,
    ) -> Option<Located<'a>> {
        let name = trimmed(item, "name");
        let args = item.g("args");
        if name.is_empty() || !args.exists() {
            return None;
        }
        let mut call_id = trimmed(item, "call_id");
        if call_id.is_empty() {
            call_id = trimmed(item, "id");
        }
        let stable_id = gemini_claude_tool_use_id(&call_id, &name, &args.raw());
        let mut candidates = vec![call_id.clone()];
        if !stable_id.is_empty() && stable_id != call_id {
            candidates.push(stable_id.clone());
        }
        for candidate in &candidates {
            if candidate.is_empty() {
                continue;
            }
            let Some(location) = self.function_call_location(candidate) else { continue };
            if self.context_matches(item, location.ci) {
                if function_call_matches_replay_item(location.fc, item, schemas) {
                    return Some(location);
                }
                tracing::debug!(
                    "antigravity replay: located call {name:?} at contents[{}].parts[{}] but name/args did not match ledger item (opaque_id={})",
                    location.ci,
                    location.pi,
                    is_gemini_claude_tool_use_id(candidate)
                );
                return None;
            }
            // The id matched exactly, so only the context drifted: the cached signature is
            // invalid but the tool identity is not.
            tracing::debug!(
                "antigravity replay: exact tool ID match for {name:?} at contents[{}].parts[{}] rejected by context hash (opaque_id={})",
                location.ci,
                location.pi,
                is_gemini_claude_tool_use_id(candidate)
            );
            return None;
        }

        let cached_content_index = item.g("contentIndex").int();
        let target_occurrence = item.g("targetOccurrence");
        if target_occurrence.exists() {
            if cached_content_index < 0 || cached_content_index as usize >= self.contents.len() {
                return None;
            }
            let ci = cached_content_index as usize;
            if !self.context_matches(item, ci) {
                return None;
            }
            let wanted = target_occurrence.int();
            let mut occurrence = 0i64;
            for (pi, part) in self.contents[ci].parts.iter().enumerate() {
                let fc = part.get("functionCall");
                let fc_id = part.g("functionCall.id").str();
                let mismatched_opaque = is_gemini_claude_tool_use_id(&fc_id) && fc_id != stable_id;
                let Some(fc) = fc else { continue };
                if mismatched_opaque || !function_call_matches_replay_item(fc, item, schemas) {
                    continue;
                }
                if occurrence == wanted {
                    return Some(Located { ci, pi, part, fc });
                }
                occurrence += 1;
            }
            return None;
        }

        let mut matches: Vec<Located<'a>> = Vec::with_capacity(1);
        for (ci, content) in self.contents.iter().enumerate() {
            if !self.context_matches(item, ci) {
                continue;
            }
            for (pi, part) in content.parts.iter().enumerate() {
                let fc_id = part.g("functionCall.id").str();
                let mismatched_opaque = is_gemini_claude_tool_use_id(&fc_id) && fc_id != stable_id;
                let Some(fc) = part.get("functionCall") else { continue };
                if mismatched_opaque {
                    continue;
                }
                if function_call_matches_replay_item(fc, item, schemas) {
                    matches.push(Located { ci, pi, part, fc });
                }
            }
        }
        if matches.len() == 1 { matches.pop() } else { None }
    }

    /// Exact opaque (Claude-facing) id match: proves identity even when context drifted.
    fn function_call_provenance_location(&self, item: &Value, schemas: &HashMap<String, Value>) -> Option<Located<'a>> {
        let name = trimmed(item, "name");
        let args = item.g("args");
        let call_id = trimmed(item, "call_id");
        if name.is_empty() || !args.exists() || call_id.is_empty() {
            return None;
        }
        let stable_id = gemini_claude_tool_use_id(&call_id, &name, &args.raw());
        if stable_id.is_empty() || stable_id == call_id {
            return None;
        }
        let location = self.function_call_location(&stable_id)?;
        function_call_matches_replay_item(location.fc, item, schemas).then_some(location)
    }

    /// The part a `thought_signature` item belongs to (shared by eligibility and write paths).
    /// A target hash pins the part by its own bytes; the positional fallback stays gated by the
    /// context fingerprint.
    fn thought_signature_part_index(&self, item: &Value) -> Option<(usize, usize)> {
        let ci = item.g("contentIndex").int();
        if ci < 0 || ci as usize >= self.contents.len() {
            return None;
        }
        let ci = ci as usize;
        let content = &self.contents[ci];
        if !role_is_model(content.content) {
            return None;
        }
        let parts = &content.parts;
        let target_kind = trimmed(item, "targetKind");
        let target_hash = trimmed(item, "targetHash");
        let mut part_index: Option<usize> = None;
        if !target_hash.is_empty() {
            let target_occurrence = item.g("targetOccurrence");
            let matches = |part: &Value| {
                let (kind, fp) = part_fingerprint(part);
                fp == target_hash && (target_kind.is_empty() || kind == target_kind)
            };
            if target_occurrence.exists() {
                let wanted = target_occurrence.int();
                let mut occurrence = 0i64;
                for (i, part) in parts.iter().enumerate() {
                    if !matches(part) {
                        continue;
                    }
                    if occurrence == wanted {
                        part_index = Some(i);
                        break;
                    }
                    occurrence += 1;
                }
            } else {
                let candidate = item.g("partIndex").int();
                if candidate >= 0 && (candidate as usize) < parts.len() && matches(parts[candidate as usize]) {
                    part_index = Some(candidate as usize);
                }
                if part_index.is_none() {
                    part_index = parts.iter().position(|p| matches(p));
                }
            }
        } else {
            // Nothing proves which part this signature belongs to, so only a matching context
            // makes the positional guess safe.
            if !self.context_matches(item, ci) {
                return None;
            }
            let candidate = item.g("partIndex").int();
            if candidate >= 0 && (candidate as usize) < parts.len() {
                let part = parts[candidate as usize];
                if !part.is_null() && !part_fingerprint(part).0.is_empty() {
                    part_index = Some(candidate as usize);
                }
            }
            if part_index.is_none() {
                // Legacy entries may point at a streamed signature-only part: attach to the last
                // semantic part of the same model content.
                part_index = parts.iter().rposition(|p| !part_fingerprint(p).0.is_empty());
            }
        }
        part_index.map(|pi| (ci, pi))
    }

    fn has_thought_signature_at(&self, item: &Value) -> bool {
        match self.thought_signature_part_index(item) {
            Some((ci, pi)) => has_native_thought_signature(&self.contents[ci].parts[pi].g("thoughtSignature").str()),
            None => false,
        }
    }

    /// Replay items describing the signatures and calls already present in the request's model
    /// turns (the starting point of the next ledger state).
    pub(crate) fn reasoning_replay_items_from_request(&self) -> Option<Vec<Value>> {
        if !self.valid_contents {
            return None;
        }
        let mut items = Vec::new();
        for (ci, content) in self.contents.iter().enumerate() {
            if !role_is_model(content.content) || content.parts.is_empty() {
                continue;
            }
            let mut fc_occurrences: HashMap<String, usize> = HashMap::new();
            for (pi, part) in content.parts.iter().enumerate() {
                let mut signature = native_part_thought_signature(part);
                if !has_native_thought_signature(&signature) {
                    signature.clear();
                }
                if let Some(fc) = part.get("functionCall") {
                    let key = function_call_key(&fc.g("name").str(), &fc.g("args").raw(), "");
                    let occurrence = fc_occurrences.get(&key).copied().unwrap_or(0);
                    if !key.is_empty() {
                        fc_occurrences.insert(key, occurrence + 1);
                    }
                    let item = build_function_call_part_item(ci, pi, occurrence, fc, &signature);
                    items.push(with_context_hash(item, &self.context_fingerprint(ci)));
                    continue;
                }
                if signature.is_empty() {
                    continue;
                }
                let mut target_index = pi;
                let (mut kind, mut fingerprint) = part_fingerprint(part);
                if fingerprint.is_empty() && pi > 0 {
                    target_index = pi - 1;
                    (kind, fingerprint) = part_fingerprint(content.parts[target_index]);
                }
                if fingerprint.is_empty() {
                    continue;
                }
                let mut item = build_thought_signature_item(ci, target_index, &signature, &kind, &fingerprint);
                cpa_json::set(&mut item, "targetOccurrence", part_occurrence(&content.parts, target_index, &kind, &fingerprint) as i64);
                items.push(with_context_hash(item, &self.context_fingerprint(ci)));
            }
        }
        Some(items)
    }

    /// Part-occurrence statistics of the pending model content, used to seed the accumulator.
    pub(crate) fn pending_content_parts(&self, content_index: usize) -> &[&'a Value] {
        self.contents.get(content_index).map(|c| c.parts.as_slice()).unwrap_or(&[])
    }
}

pub(crate) fn with_context_hash(mut item: Value, context_hash: &str) -> Value {
    if !context_hash.is_empty() {
        cpa_json::set(&mut item, "contextHash", context_hash);
    }
    item
}

/// `{"type":"thought_signature",...}` ledger item.
pub(crate) fn build_thought_signature_item(
    content_index: usize,
    part_index: usize,
    signature: &str,
    target_kind: &str,
    target_hash: &str,
) -> Value {
    let mut item = json!({
        "type": "thought_signature",
        "thoughtSignature": signature,
        "contentIndex": content_index,
        "partIndex": part_index,
    });
    if !target_kind.is_empty() {
        cpa_json::set(&mut item, "targetKind", target_kind);
    }
    if !target_hash.is_empty() {
        cpa_json::set(&mut item, "targetHash", target_hash);
    }
    item
}

/// `{"type":"function_call_part",...}` ledger item (keys sorted like Go's map marshal).
pub(crate) fn build_function_call_part_item(
    content_index: usize,
    part_index: usize,
    target_occurrence: usize,
    fc: &Value,
    signature: &str,
) -> Value {
    let mut item = Map::new();
    let args = fc.g("args");
    if args.exists() {
        item.insert("args".into(), if args.is_string() { Value::String(args.str()) } else { args.value() });
    }
    let id = trimmed(fc, "id");
    if !id.is_empty() {
        item.insert("call_id".into(), Value::String(id));
    }
    item.insert("contentIndex".into(), json!(content_index));
    item.insert("name".into(), Value::String(fc.g("name").str()));
    item.insert("partIndex".into(), json!(part_index));
    item.insert("targetOccurrence".into(), json!(target_occurrence));
    if !signature.is_empty() {
        item.insert("thoughtSignature".into(), Value::String(signature.to_string()));
    }
    item.insert("type".into(), Value::String("function_call_part".into()));
    Value::Object(item)
}

// ---------------------------------------------------------------- tool schemas

/// name -> JSON schema of the client's tools (both declared and sanitized names).
pub(crate) fn replay_tool_schemas_from_requests(raw_requests: &[&[u8]]) -> HashMap<String, Value> {
    let mut schemas = HashMap::new();
    for raw in raw_requests {
        if raw.is_empty() {
            continue;
        }
        let name_map = sanitized_function_name_map(raw);
        let v = cpa_json::parse(raw);
        let tools = v.g("tools");
        if !tools.is_array() {
            continue;
        }
        for tool in tools.array() {
            let mut candidates = vec![tool.clone()];
            let function = tool.g("function");
            if function.exists() {
                candidates.push(function);
            }
            for candidate in candidates {
                let name = candidate.g("name").str().trim().to_string();
                if name.is_empty() {
                    continue;
                }
                let schema = ["input_schema", "parameters", "parametersJsonSchema"]
                    .iter()
                    .map(|p| candidate.g(p))
                    .find(|s| s.exists() && s.is_object());
                let Some(schema) = schema else { continue };
                let value = schema.value();
                for schema_name in [name.clone(), map_sanitized_function_name(&name_map, &name)] {
                    if schema_name.is_empty() {
                        continue;
                    }
                    schemas.entry(schema_name).or_insert_with(|| value.clone());
                }
            }
        }
    }
    schemas
}

/// Go-style numeric normalization so `1` and `1.0` compare equal like decoded float64s.
fn float_normalized(v: &Value) -> Value {
    match v {
        Value::Number(n) => match n.to_string().parse::<f64>() {
            Ok(f) => cpa_json::num_f64(f),
            Err(_) => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(float_normalized).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), float_normalized(x))).collect()),
        other => other.clone(),
    }
}

fn replay_json_value(res: &cpa_json::Res<'_>) -> Option<Value> {
    let raw = if res.is_string() { res.str() } else { res.raw() };
    if raw.trim().is_empty() || !cpa_json::valid(raw.as_bytes()) {
        return None;
    }
    Some(float_normalized(&cpa_json::parse_str(&raw)))
}

/// Drops keys equal to their schema default so a client that stripped defaults still matches.
fn normalize_replay_tool_value(value: &Value, schema: Option<&Value>) -> Value {
    let schema_object = schema.and_then(Value::as_object);
    match value {
        Value::Object(map) => {
            let properties = schema_object.and_then(|s| s.get("properties")).and_then(Value::as_object);
            let mut out = Map::new();
            for (key, child) in map {
                let child_schema = properties.and_then(|p| p.get(key));
                let normalized_child = normalize_replay_tool_value(child, child_schema);
                if let Some(Value::Object(prop)) = child_schema
                    && let Some(default) = prop.get("default")
                    && normalized_child == normalize_replay_tool_value(default, child_schema)
                {
                    continue;
                }
                out.insert(key.clone(), normalized_child);
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let item_schema = schema_object.and_then(|s| s.get("items"));
            Value::Array(items.iter().map(|c| normalize_replay_tool_value(c, item_schema)).collect())
        }
        other => other.clone(),
    }
}

fn function_call_matches_replay_item(fc: &Value, item: &Value, schemas: &HashMap<String, Value>) -> bool {
    let name = trimmed(item, "name");
    if name.is_empty() || trimmed(fc, "name") != name {
        return false;
    }
    let current = fc.g("args");
    let native = item.g("args");
    if !current.exists() || !native.exists() {
        return false;
    }
    if canon_raw(&current.raw()) == canon_raw(&native.raw()) {
        return true;
    }
    let Some(schema) = schemas.get(&name) else { return false };
    let (Some(cv), Some(nv)) = (replay_json_value(&current), replay_json_value(&native)) else {
        return false;
    };
    normalize_replay_tool_value(&cv, Some(schema)) == normalize_replay_tool_value(&nv, Some(schema))
}

// ---------------------------------------------------------------- reserved ids

fn reserved_ids_in(part: &Value) -> usize {
    ["functionCall.id", "functionResponse.id"]
        .iter()
        .filter(|p| is_gemini_claude_tool_use_id(&part.g(p).str()))
        .count()
}

fn each_part(payload: &Value, mut f: impl FnMut(&Value)) {
    let Some(contents) = contents_of(payload) else { return };
    for content in contents {
        match content.get("parts") {
            Some(Value::Array(parts)) => parts.iter().for_each(&mut f),
            Some(Value::Null) | None => {}
            Some(other) => f(other),
        }
    }
}

/// How many reserved Claude-facing provenance ids a Gemini-shaped payload still carries.
pub(crate) fn count_claude_tool_provenance_ids(payload: &[u8]) -> usize {
    let v = cpa_json::parse(payload);
    let mut count = 0;
    each_part(&v, |part| count += reserved_ids_in(part));
    count
}

pub(crate) fn payload_has_claude_tool_provenance_id(v: &Value) -> bool {
    let mut found = false;
    each_part(v, |part| found = found || reserved_ids_in(part) > 0);
    found
}

fn synthetic_tool_call_id(reserved_id: &str) -> String {
    let sum = Sha256::digest(format!("antigravity-degraded-tool-call\0{reserved_id}").as_bytes());
    format!("call_{}", hex::encode(&sum[..6]))
}

/// Rewrites unresolved reserved ids to neutral deterministic ids so the conversation survives a
/// ledger miss. Degraded first calls get the bypass sentinel, later ones lose their signature.
pub(crate) fn degrade_claude_tool_provenance_ids(v: &mut Value) -> usize {
    let Some(contents) = contents_of(v) else { return 0 };
    // (ci, pi, kind) where kind: 0 = call keeping bypass, 1 = call dropping signature, 2 = response
    let mut edits: Vec<(usize, usize, u8, String)> = Vec::new();
    for (ci, content) in contents.iter().enumerate() {
        let Some(Value::Array(parts)) = content.get("parts") else { continue };
        let mut seen_fc = false;
        for (pi, part) in parts.iter().enumerate() {
            if part.g("functionCall").exists() {
                let is_first = !seen_fc;
                seen_fc = true;
                let id = trimmed(part, "functionCall.id");
                if !is_gemini_claude_tool_use_id(&id) {
                    continue;
                }
                let has_sig = part.g("thoughtSignature").exists() && !part.g("thoughtSignature").str().is_empty();
                let kind = if has_sig { if is_first { 0 } else { 1 } } else { 3 };
                edits.push((ci, pi, kind, synthetic_tool_call_id(&id)));
                continue;
            }
            if part.g("functionResponse").exists() {
                let id = trimmed(part, "functionResponse.id");
                if is_gemini_claude_tool_use_id(&id) {
                    edits.push((ci, pi, 2, synthetic_tool_call_id(&id)));
                }
            }
        }
    }
    let count = edits.len();
    for (ci, pi, kind, new_id) in edits {
        let base = format!("request.contents.{ci}.parts.{pi}");
        if kind == 2 {
            cpa_json::set(v, &format!("{base}.functionResponse.id"), new_id);
            continue;
        }
        cpa_json::set(v, &format!("{base}.functionCall.id"), new_id);
        match kind {
            0 => {
                cpa_json::set(v, &format!("{base}.thoughtSignature"), GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR);
            }
            1 => cpa_json::delete(v, &format!("{base}.thoughtSignature")),
            _ => {}
        }
    }
    count
}

/// Restores the bypass sentinel on the first function call of any model turn left unsigned.
pub(crate) fn repair_unsigned_first_function_calls(v: &mut Value) -> bool {
    let Some(contents) = contents_of(v) else { return false };
    let mut fixes = Vec::new();
    for (ci, content) in contents.iter().enumerate() {
        if !role_is_model(content) {
            continue;
        }
        let Some(Value::Array(parts)) = content.get("parts") else { continue };
        if let Some((pi, part)) = parts.iter().enumerate().find(|(_, p)| p.g("functionCall").exists())
            && native_part_thought_signature(part).is_empty()
        {
            fixes.push((ci, pi));
        }
    }
    let changed = !fixes.is_empty();
    for (ci, pi) in fixes {
        cpa_json::set(
            v,
            &format!("request.contents.{ci}.parts.{pi}.thoughtSignature"),
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        );
    }
    changed
}

// ---------------------------------------------------------------- mutation primitives

fn part_mut(doc: &mut Value, ci: usize, pi: usize) -> Option<&mut Value> {
    doc.get_mut("request")?.get_mut("contents")?.get_mut(ci)?.get_mut("parts")?.get_mut(pi)
}

fn remove_thought_signature_from_other_parts(doc: &mut Value, ci: usize, signature: &str, keep_pi: usize) {
    let signature = signature.trim();
    if signature.is_empty() {
        return;
    }
    let Some(Value::Array(parts)) = doc
        .get_mut("request")
        .and_then(|r| r.get_mut("contents"))
        .and_then(|c| c.get_mut(ci))
        .and_then(|c| c.get_mut("parts"))
    else {
        return;
    };
    for (pi, part) in parts.iter_mut().enumerate() {
        if pi == keep_pi || native_part_thought_signature(part) != signature {
            continue;
        }
        delete_signature_fields(part);
    }
}

/// `{"name","id"?,"args"}` built like Go's `antigravityNativeFunctionCallJSON`.
fn native_function_call_json(item: &Value, fallback_id: &str) -> Option<Value> {
    let name = trimmed(item, "name");
    let args = item.g("args");
    if name.is_empty() || !args.exists() {
        return None;
    }
    let mut fc = Map::new();
    fc.insert("name".into(), Value::String(name));
    let mut call_id = trimmed(item, "call_id");
    if call_id.is_empty() {
        call_id = fallback_id.to_string();
    }
    if !call_id.is_empty() {
        fc.insert("id".into(), Value::String(call_id));
    }
    if args.is_string() {
        let text = args.str();
        if cpa_json::valid(text.as_bytes()) {
            fc.insert("args".into(), cpa_json::parse_str(&text));
        } else {
            fc.insert("args".into(), Value::String(text));
        }
    } else {
        fc.insert("args".into(), args.value());
    }
    Some(Value::Object(fc))
}

/// `{"args"?,"id"?,"name"}` with keys sorted (Go builds it from a map).
fn function_call_map(name: &str, call_id: &str, args: &cpa_json::Res<'_>) -> Value {
    let mut fc = Map::new();
    if args.exists() {
        fc.insert("args".into(), args.value());
    }
    if !call_id.is_empty() {
        fc.insert("id".into(), Value::String(call_id.to_string()));
    }
    fc.insert("name".into(), Value::String(name.to_string()));
    Value::Object(fc)
}

fn function_responses_can_restore_id(doc: &Value, current_id: &str, native_name: &str) -> bool {
    if current_id.is_empty() {
        return true;
    }
    if contents_of(doc).is_none() {
        return false;
    }
    let mut valid = true;
    each_part(doc, |part| {
        if !valid {
            return;
        }
        let response = part.g("functionResponse");
        if !response.exists() || response.g("id").str().trim() != current_id {
            return;
        }
        let name = response.g("name").str().trim().to_string();
        valid = name.is_empty() || name == "unknown" || name == native_name;
    });
    valid
}

/// Points every functionResponse of `current_id` at the native id and name; true when any changed.
fn restore_function_response_replay_identity(doc: &mut Value, current_id: &str, native_id: &str, native_name: &str) -> bool {
    let (current_id, native_id, native_name) = (current_id.trim(), native_id.trim(), native_name.trim());
    if current_id.is_empty() || native_id.is_empty() || native_name.is_empty() || current_id == native_id {
        return false;
    }
    let mut targets = Vec::new();
    if let Some(contents) = contents_of(doc) {
        for (ci, content) in contents.iter().enumerate() {
            let Some(Value::Array(parts)) = content.get("parts") else { continue };
            for (pi, part) in parts.iter().enumerate() {
                let response = part.g("functionResponse");
                if response.exists() && response.g("id").str().trim() == current_id {
                    let name_differs = response.g("name").str() != native_name;
                    targets.push((ci, pi, name_differs));
                }
            }
        }
    }
    let changed = !targets.is_empty();
    for (ci, pi, _) in targets {
        let base = format!("request.contents.{ci}.parts.{pi}.functionResponse");
        cpa_json::set(doc, &format!("{base}.id"), native_id);
        cpa_json::set(doc, &format!("{base}.name"), native_name);
    }
    changed
}

fn insert_model_function_call_before_content(
    doc: &mut Value,
    before: usize,
    name: &str,
    call_id: &str,
    sig: &str,
    args: &cpa_json::Res<'_>,
) -> bool {
    let Some(contents) = contents_of(doc) else { return false };
    if before > contents.len() {
        return false;
    }
    let sig = if sig.is_empty() { GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR } else { sig };
    let part = json!({"functionCall": function_call_map(name, call_id, args), "thoughtSignature": sig});
    let new_content = json!({"parts": [part], "role": "model"});
    let mut list = contents.clone();
    list.insert(before, new_content);
    cpa_json::set(doc, "request.contents", Value::Array(list))
}

fn append_function_call_to_model_content(
    doc: &mut Value,
    ci: usize,
    name: &str,
    call_id: &str,
    sig: &str,
    args: &cpa_json::Res<'_>,
) -> bool {
    let path = format!("request.contents.{ci}");
    let content = doc.g(&path);
    if !content.g("role").str().trim().eq_ignore_ascii_case("model") || !content.g("parts").is_array() {
        return false;
    }
    let mut sig = sig.to_string();
    if sig.is_empty() {
        let has_fc = content.g("parts").array().iter().any(|p| p.g("functionCall").exists());
        if !has_fc {
            sig = GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string();
        }
    }
    drop(content);
    let mut part = Map::new();
    part.insert("functionCall".into(), function_call_map(name, call_id, args));
    if !sig.is_empty() {
        part.insert("thoughtSignature".into(), Value::String(sig));
    }
    cpa_json::set(doc, &format!("{path}.parts.-1"), Value::Object(part))
}

/// Rewrites one function call part back to its provider-native identity. `allow_signature`
/// says whether the cached signature may be replayed too (identity-only restores pass false:
/// the context no longer matches the one the signature was issued for).
fn restore_native_function_call_replay(
    doc: &mut Value,
    ci: usize,
    pi: usize,
    item: &Value,
    allow_legacy_id_restore: bool,
    allow_signature: bool,
) -> bool {
    let part_path = format!("request.contents.{ci}.parts.{pi}");
    let current_call = doc.g(&format!("{part_path}.functionCall"));
    if !current_call.exists() {
        return false;
    }
    let current_id = current_call.g("id").str().trim().to_string();
    drop(current_call);
    let native_id = trimmed(item, "call_id");
    let native_name = trimmed(item, "name");
    let restore_identity = current_id == native_id || is_gemini_claude_tool_use_id(&current_id) || allow_legacy_id_restore;
    let signature = trimmed(item, "thoughtSignature");
    if !restore_identity {
        let existing = doc.g(&format!("{part_path}.thoughtSignature")).str();
        if !allow_signature || signature.is_empty() || has_native_thought_signature(&existing) {
            return false;
        }
        remove_thought_signature_from_other_parts(doc, ci, &signature, pi);
        return cpa_json::set(doc, &format!("{part_path}.thoughtSignature"), signature);
    }
    if current_id != native_id && !function_responses_can_restore_id(doc, &current_id, &native_name) {
        return false;
    }
    let Some(native_call) = native_function_call_json(item, &current_id) else { return false };
    let before_part = doc.g(&part_path).value();
    if !cpa_json::set(doc, &format!("{part_path}.functionCall"), native_call) {
        return false;
    }
    if let Some(part) = part_mut(doc, ci, pi) {
        delete_signature_fields(part);
    }
    if allow_signature && !signature.is_empty() {
        remove_thought_signature_from_other_parts(doc, ci, &signature, pi);
        cpa_json::set(doc, &format!("{part_path}.thoughtSignature"), signature);
    }
    let mut changed = doc.g(&part_path).value() != before_part;
    if !current_id.is_empty() && !native_id.is_empty() && current_id != native_id {
        changed |= restore_function_response_replay_identity(doc, &current_id, &native_id, &native_name);
    }
    changed
}

// ---------------------------------------------------------------- planning

type Plan = Box<dyn FnOnce(&mut Value) -> bool>;

fn plan_thought_signature(index: &Index<'_>, item: &Value) -> Option<Plan> {
    if index.has_thought_signature_at(item) {
        return None;
    }
    let sig = trimmed(item, "thoughtSignature");
    if sig.is_empty() {
        return None;
    }
    let (ci, pi) = index.thought_signature_part_index(item)?;
    Some(Box::new(move |doc| {
        remove_thought_signature_from_other_parts(doc, ci, &sig, pi);
        cpa_json::set(doc, &format!("request.contents.{ci}.parts.{pi}.thoughtSignature"), sig)
    }))
}

/// Whether a function_call_part item is worth applying (Go's eligibility filter).
fn function_call_item_eligible(index: &Index<'_>, item: &Value, schemas: &HashMap<String, Value>) -> bool {
    let signature = trimmed(item, "thoughtSignature");
    if let Some(location) = index.function_call_part_location_for_replay(item, schemas) {
        let current_id = trimmed(location.fc, "id");
        let native_id = trimmed(item, "call_id");
        let needs_native_restore =
            current_id != native_id || canon_raw(&location.fc.g("args").raw()) != canon_raw(&item.g("args").raw());
        return needs_native_restore
            || !(signature.is_empty() || has_native_thought_signature(&location.part.g("thoughtSignature").str()));
    }
    if index.function_call_provenance_location(item, schemas).is_some() {
        return true;
    }
    let call_id = trimmed(item, "call_id");
    if call_id.is_empty() {
        return false;
    }
    let Some((response_index, _)) = index.function_response_content_index_for_replay(item) else {
        return false;
    };
    let mut context_matches = index.context_matches(item, response_index);
    if !context_matches && response_index > 0 {
        let previous_is_model = role_is_model(index.contents[response_index - 1].content);
        context_matches = previous_is_model && index.context_matches(item, response_index - 1);
    }
    context_matches
}

fn plan_function_call_part(index: &Index<'_>, item: &Value, schemas: &HashMap<String, Value>) -> Option<Plan> {
    if !function_call_item_eligible(index, item, schemas) {
        return None;
    }
    let name = trimmed(item, "name");
    let args = item.g("args");
    let call_id = trimmed(item, "call_id");
    let sig = trimmed(item, "thoughtSignature");
    if name.is_empty() || !args.exists() {
        return None;
    }
    if let Some(location) = index.function_call_part_location_for_replay(item, schemas) {
        let allow_legacy = schemas.contains_key(&name);
        let (ci, pi) = (location.ci, location.pi);
        let item = item.clone();
        return Some(Box::new(move |doc| restore_native_function_call_replay(doc, ci, pi, &item, allow_legacy, true)));
    }
    if let Some(location) = index.function_call_provenance_location(item, schemas) {
        let (ci, pi) = (location.ci, location.pi);
        let item = item.clone();
        return Some(Box::new(move |doc| restore_native_function_call_replay(doc, ci, pi, &item, false, true)));
    }
    if !call_id.is_empty() {
        let stable_id = gemini_claude_tool_use_id(&call_id, &name, &args.raw());
        let has_native_id = index.function_call_location(&call_id).is_some();
        let has_stable_id = !stable_id.is_empty() && index.function_call_location(&stable_id).is_some();
        if has_native_id || has_stable_id {
            // Already in the history under its native or Claude-facing id and neither lookup
            // accepted it: the client changed it. Never replay onto it or insert a second copy.
            return None;
        }
        if let Some((fr_index, current_response_id)) = index.function_response_content_index_for_replay(item) {
            if fr_index >= 1 {
                let parallel = fr_index - 1;
                let content = &index.contents[parallel];
                if role_is_model(content.content)
                    && index.context_matches(item, parallel)
                    && matches!(content.content.get("parts"), Some(Value::Array(_)))
                {
                    let (name, call_id, sig, item) = (name.clone(), call_id.clone(), sig.clone(), item.clone());
                    return Some(Box::new(move |doc| {
                        let args = item.g("args");
                        if append_function_call_to_model_content(doc, parallel, &name, &call_id, &sig, &args) {
                            restore_function_response_replay_identity(doc, &current_response_id, &call_id, &name);
                            return true;
                        }
                        false
                    }));
                }
            }
            if index.context_matches(item, fr_index) {
                let (name, call_id, sig, item) = (name.clone(), call_id.clone(), sig.clone(), item.clone());
                let fallback = plan_positional_function_call(index, &item, &name, &call_id, &sig);
                return Some(Box::new(move |doc| {
                    let args = item.g("args");
                    if insert_model_function_call_before_content(doc, fr_index, &name, &call_id, &sig, &args) {
                        restore_function_response_replay_identity(doc, &current_response_id, &call_id, &name);
                        return true;
                    }
                    match fallback {
                        Some(plan) => plan(doc),
                        None => false,
                    }
                }));
            }
        }
    } else {
        // Without a native id only an exact semantic match is safe.
        return None;
    }
    plan_positional_function_call(index, item, &name, &call_id, &sig)
}

/// Last resort: write the call at its recorded (content, part) slot when context still matches.
fn plan_positional_function_call(index: &Index<'_>, item: &Value, name: &str, call_id: &str, sig: &str) -> Option<Plan> {
    let cached = item.g("contentIndex").int();
    let ci = if index.valid_contents {
        if cached >= 0 && (cached as usize) < index.contents.len() { cached } else { -1 }
    } else {
        cached
    };
    if ci < 0 || !index.context_matches(item, ci as usize) {
        return None;
    }
    let ci = ci as usize;
    let pi = item.g("partIndex").int();
    let (name, call_id, sig, item) = (name.to_string(), call_id.to_string(), sig.to_string(), item.clone());
    Some(Box::new(move |doc| write_function_call_at_slot(doc, ci, pi, &item, &name, &call_id, &sig)))
}

fn function_call_for_slot(name: &str, call_id: &str, args: &cpa_json::Res<'_>) -> Value {
    let mut fc = Map::new();
    if args.is_string() {
        fc.insert("args".into(), Value::String(args.str()));
    } else if cpa_json::valid(args.raw().as_bytes()) {
        fc.insert("args".into(), cpa_json::parse_str(&args.raw()));
    }
    if !call_id.is_empty() {
        fc.insert("id".into(), Value::String(call_id.to_string()));
    }
    fc.insert("name".into(), Value::String(name.to_string()));
    Value::Object(fc)
}

fn write_function_call_at_slot(doc: &mut Value, ci: usize, pi: i64, item: &Value, name: &str, call_id: &str, sig: &str) -> bool {
    let args = item.g("args");
    let parts_path = format!("request.contents.{ci}.parts");
    let existing_len = match doc.g(&parts_path).v() {
        Some(Value::Array(a)) => Some(a.len()),
        _ => None,
    };
    let existing_part = pi >= 0
        && existing_len.is_some_and(|len| (pi as usize) < len)
        && !doc.g(&format!("{parts_path}.{pi}")).is_null();
    if !existing_part {
        let mut part = Map::new();
        part.insert("functionCall".into(), function_call_for_slot(name, call_id, &args));
        if !sig.is_empty() {
            part.insert("thoughtSignature".into(), Value::String(sig.to_string()));
        }
        let write_path = if existing_len.is_some() { format!("{parts_path}.-1") } else { format!("{parts_path}.0") };
        return cpa_json::set(doc, &write_path, Value::Object(part));
    }
    let part_path = format!("{parts_path}.{pi}");
    let mut changed = false;
    let existing_sig = doc.g(&format!("{part_path}.thoughtSignature")).str();
    if !sig.is_empty() && !has_native_thought_signature(&existing_sig) {
        remove_thought_signature_from_other_parts(doc, ci, sig, pi as usize);
        if cpa_json::set(doc, &format!("{part_path}.thoughtSignature"), sig) {
            changed = true;
        }
    }
    if !doc.g(&format!("{part_path}.functionCall")).exists()
        && cpa_json::set(doc, &format!("{part_path}.functionCall"), function_call_for_slot(name, call_id, &args))
    {
        changed = true;
    }
    changed
}

/// Applies ledger items sequentially, each against the payload the previous one produced.
pub(crate) fn apply_reasoning_replay_items(
    payload: &[u8],
    items: &[Vec<u8>],
    schemas: &HashMap<String, Value>,
) -> (Vec<u8>, bool) {
    let mut doc = cpa_json::parse(payload);
    let parsed: Vec<Value> = items.iter().map(|i| cpa_json::parse(i)).collect();
    let mut changed = false;
    let mut next = 0;
    while next < parsed.len() {
        let mut plan: Option<Plan> = None;
        {
            let index = Index::new(&doc);
            while next < parsed.len() {
                let item = &parsed[next];
                next += 1;
                let candidate = match trimmed(item, "type").as_str() {
                    "thought_signature" => plan_thought_signature(&index, item),
                    "function_call_part" => plan_function_call_part(&index, item, schemas),
                    _ => None,
                };
                if candidate.is_some() {
                    plan = candidate;
                    break;
                }
            }
        }
        if let Some(plan) = plan
            && plan(&mut doc)
        {
            changed = true;
        }
    }
    if changed { (cpa_json::to_vec(&doc), true) } else { (payload.to_vec(), false) }
}

// ---------------------------------------------------------------- request preparation

/// Merges the ledger into the payload (Go: applyAntigravityReasoningReplayCache). Returns the
/// payload, the scope (with the cache snapshot) and whether replay changed anything.
fn apply_reasoning_replay_cache(
    model_name: &str,
    req: &Request,
    opts: &Options,
    payload: &[u8],
) -> (Vec<u8>, ReplayScope, bool) {
    let mut scope = scope_from_request(model_name, req, opts, payload);
    if !scope.valid() {
        return (payload.to_vec(), scope, false);
    }
    let (items, snapshot) =
        get_antigravity_reasoning_replay_items_with_snapshot_required(&scope.model_name, &scope.session_key);
    scope.snapshot = snapshot;
    let reserved_before = count_claude_tool_provenance_ids(payload);
    let items = match items {
        Some(items) if !items.is_empty() => items,
        found => {
            if reserved_before > 0 {
                tracing::debug!(
                    "antigravity replay: ledger miss with {reserved_before} reserved tool provenance ID(s) present (session={} found={})",
                    replay_log_key(&scope.session_key),
                    found.is_some()
                );
            }
            return (payload.to_vec(), scope, false);
        }
    };
    let schemas = if opts.source_format == Format::Claude {
        replay_tool_schemas_from_requests(&[&opts.original_request, &req.payload])
    } else {
        HashMap::new()
    };
    let (updated, changed) = apply_reasoning_replay_items(payload, &items, &schemas);
    if reserved_before > 0 {
        tracing::debug!(
            "antigravity replay: ledger items={} reserved before={reserved_before} after={} applied={changed} (session={})",
            items.len(),
            count_claude_tool_provenance_ids(&updated),
            replay_log_key(&scope.session_key)
        );
    }
    if !changed {
        return (payload.to_vec(), scope, false);
    }
    (updated, scope, true)
}

/// Replay for Gemini-family models: apply the ledger, repair roles and ids, and validate call
/// pairing. Replay problems degrade to the original payload; a history that is invalid even
/// without replay is a 400.
pub(crate) fn prepare_gemini_reasoning_replay_payload(
    model_name: &str,
    req: &Request,
    opts: &Options,
    payload: Vec<u8>,
) -> Result<(Vec<u8>, ReplayScope), ExecError> {
    if !uses_reasoning_replay_cache(model_name) {
        return Ok((payload, ReplayScope::default()));
    }
    let (updated, scope, replay_applied) = apply_reasoning_replay_cache(model_name, req, opts, &payload);
    let mut updated = normalize_function_response_roles(updated);
    let mut doc = cpa_json::parse(&updated);
    if payload_has_claude_tool_provenance_id(&doc) {
        // The ledger could not resolve every id (lane change, expiry, restart, uncommitted
        // turn): degrade those calls instead of killing the conversation.
        let count = degrade_claude_tool_provenance_ids(&mut doc);
        tracing::warn!(
            "antigravity executor: replay state missing for {count} tool ID(s); rewriting them to synthetic IDs and continuing without reasoning replay for those calls"
        );
        updated = normalize_function_response_roles(cpa_json::to_vec(&doc));
        doc = cpa_json::parse(&updated);
    }
    // An identity-only restore drops the cached signature; Gemini rejects an unsigned first call.
    if repair_unsigned_first_function_calls(&mut doc) {
        updated = cpa_json::to_vec(&doc);
    }
    if let Err(err_pairing) = validate_gemini_function_call_pairing(&updated) {
        let original_valid = validate_gemini_function_call_pairing(&payload).is_ok();
        if replay_applied && original_valid && scope.valid() {
            delete_antigravity_reasoning_replay_items_if_unchanged(&scope.model_name, &scope.session_key, &scope.snapshot);
            tracing::warn!(
                "antigravity executor: reasoning replay broke Gemini function call pairing ({err_pairing}); degrading to original payload"
            );
            return Ok((payload, scope));
        }
        return Err(ExecError::new(
            400,
            format!("antigravity executor: invalid Gemini function call history: {err_pairing}"),
        ));
    }
    Ok((updated, scope))
}

/// Drops the ledger entry after the upstream rejected a signature (400 mentioning it).
pub(crate) fn clear_reasoning_replay_on_invalid_signature(scope: &ReplayScope, status: u16, body: &[u8]) {
    if !scope.valid() || status != 400 {
        return;
    }
    if !String::from_utf8_lossy(body).to_lowercase().contains("signature") {
        return;
    }
    delete_antigravity_reasoning_replay_items_if_unchanged(&scope.model_name, &scope.session_key, &scope.snapshot);
}

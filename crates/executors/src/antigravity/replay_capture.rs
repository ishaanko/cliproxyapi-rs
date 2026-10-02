//! Reasoning replay, capture side (Go: antigravityReasoningReplayAccumulator in
//! antigravity_reasoning_replay.go).
//!
//! Observes the upstream response (SSE lines or one non-stream body) and builds the ledger items
//! that the next request merges back: thought signatures bound to text/thought segments, and
//! function call parts with their native ids. The ledger is committed only after a stream ends
//! with a terminal `finishReason`.

use std::collections::{HashMap, HashSet};

use cpa_core::cache::{
    ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY, ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ITEMS_PER_ENTRY,
    delete_antigravity_reasoning_replay_items_if_unchanged, replace_antigravity_reasoning_replay_items_if_unchanged,
};
use cpa_json::J;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::replay::{
    Index, ReplayScope, build_function_call_part_item, build_thought_signature_item, function_call_key,
    has_native_thought_signature, native_part_thought_signature, part_fingerprint, replay_log_key, with_context_hash,
};
use crate::helps::text::json_payload;

const MAX_ITEMS: usize = ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ITEMS_PER_ENTRY;
const MAX_BYTES: usize = ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY;

struct PendingSignature {
    signature: String,
    target_kind: String,
}

pub(crate) struct ReplayAccumulator {
    scope: ReplayScope,
    response_context_hash: String,
    items: Vec<Value>,
    seen_fc: HashSet<String>,
    seen_signatures: HashSet<String>,
    segment_occurrences: HashMap<String, usize>,
    function_call_occurrences: HashMap<String, usize>,
    content_index: usize,
    next_part_index: usize,
    visible_text: String,
    thought_text: String,
    visible_part_index: usize,
    thought_part_index: usize,
    last_response_kind: String,
    pending: Vec<PendingSignature>,
    item_bytes: usize,
    overflow: bool,
    terminal: bool,
}

fn item_len(item: &Value) -> usize {
    cpa_json::to_vec(item).len()
}

/// Key of the call a `functionCall` part represents (id falls back to empty).
fn tool_call_key(fc: &Value) -> Option<String> {
    let mut call_id = fc.g("call_id").str().trim().to_string();
    if call_id.is_empty() {
        call_id = fc.g("id").str().trim().to_string();
    }
    let name = fc.g("name").str().trim().to_string();
    if name.is_empty() {
        return None;
    }
    let key = function_call_key(&name, &fc.g("args").raw(), &call_id);
    (!key.is_empty()).then_some(key)
}

impl ReplayAccumulator {
    /// `None` when the scope is invalid (nothing to record).
    pub(crate) fn new(scope: &ReplayScope, request_payload: &[u8]) -> Option<Self> {
        if !scope.valid() {
            return None;
        }
        let doc = cpa_json::parse(request_payload);
        let index = Index::new(&doc);
        let (content_index, base_part_index) = index.pending_model_content_index();
        let items = index.reasoning_replay_items_from_request().unwrap_or_default();
        let mut seen_signatures = HashSet::new();
        let mut item_bytes = 0;
        for item in &items {
            let sig = item.g("thoughtSignature").str().trim().to_string();
            if !sig.is_empty() {
                seen_signatures.insert(sig);
            }
            item_bytes += item_len(item);
        }
        let mut segment_occurrences = HashMap::new();
        let mut function_call_occurrences = HashMap::new();
        for part in index.pending_content_parts(content_index) {
            if let Some(fc) = part.get("functionCall") {
                let key = function_call_key(&fc.g("name").str(), &fc.g("args").raw(), "");
                if !key.is_empty() {
                    *function_call_occurrences.entry(key).or_insert(0) += 1;
                }
                continue;
            }
            let (kind, fingerprint) = part_fingerprint(part);
            if !fingerprint.is_empty() {
                *segment_occurrences.entry(format!("{kind}\0{fingerprint}")).or_insert(0) += 1;
            }
        }
        let overflow = items.len() > MAX_ITEMS || item_bytes > MAX_BYTES;
        Some(Self {
            scope: scope.clone(),
            response_context_hash: index.context_fingerprint(content_index),
            items,
            seen_fc: HashSet::new(),
            seen_signatures,
            segment_occurrences,
            function_call_occurrences,
            content_index,
            next_part_index: base_part_index,
            visible_text: String::new(),
            thought_text: String::new(),
            visible_part_index: usize::MAX,
            thought_part_index: usize::MAX,
            last_response_kind: String::new(),
            pending: Vec::new(),
            item_bytes,
            overflow,
            terminal: false,
        })
    }

    fn append_item(&mut self, item: Value) {
        if self.overflow {
            return;
        }
        let len = item_len(&item);
        if self.items.len() + 1 > MAX_ITEMS || self.item_bytes + len > MAX_BYTES {
            self.overflow = true;
            return;
        }
        self.item_bytes += len;
        self.items.push(item);
    }

    fn attach_detached_signature_to_last_function_call(&mut self, signature: &str) {
        if signature.is_empty() {
            return;
        }
        for i in (0..self.items.len()).rev() {
            if self.items[i].g("type").str() != "function_call_part" {
                continue;
            }
            if !self.items[i].g("thoughtSignature").str().trim().is_empty() {
                return;
            }
            let before = item_len(&self.items[i]);
            let mut updated = self.items[i].clone();
            cpa_json::set(&mut updated, "thoughtSignature", signature);
            let delta = item_len(&updated).saturating_sub(before);
            if self.item_bytes + delta > MAX_BYTES {
                self.overflow = true;
                return;
            }
            self.items[i] = updated;
            self.item_bytes += delta;
            return;
        }
    }

    /// Feeds one raw SSE line (`data: {...}`).
    pub(crate) fn observe_sse_line(&mut self, line: &[u8]) {
        if let Some(payload) = json_payload(line) {
            self.observe_response_payload(payload);
        }
    }

    /// Feeds one upstream chunk or the full non-stream body.
    pub(crate) fn observe_response_payload(&mut self, payload: &[u8]) {
        let v = cpa_json::parse(payload);
        if !v.g("response.candidates.0.finishReason").str().trim().is_empty() {
            self.terminal = true;
        }
        let parts = v.g("response.candidates.0.content.parts");
        if !parts.is_array() {
            return;
        }
        for part in parts.array() {
            let part = part.value();
            self.observe_part(&part);
        }
    }

    fn observe_part(&mut self, part: &Value) {
        let pi = self.next_part_index;
        self.next_part_index += 1;
        let mut signature = native_part_thought_signature(part);
        if !has_native_thought_signature(&signature) {
            signature.clear();
        }

        if let Some(fc) = part.get("functionCall") {
            if self.last_response_kind == "text" || self.last_response_kind == "thought" {
                let kind = self.last_response_kind.clone();
                self.flush_pending_for_kind(&kind);
            }
            if !signature.is_empty() {
                self.pending.retain(|p| !p.target_kind.is_empty());
            }
            if signature.is_empty()
                && let Some(idx) = self.pending.iter().rposition(|p| p.target_kind.is_empty())
            {
                signature = self.pending.remove(idx).signature;
            }
            if let Some(key) = tool_call_key(fc) {
                let dedupe_key = if signature.is_empty() { format!("{key}\0part:{pi}") } else { format!("{key}\0{signature}") };
                if !self.seen_fc.insert(dedupe_key) {
                    return;
                }
            }
            let occurrence_key = function_call_key(&fc.g("name").str(), &fc.g("args").raw(), "");
            let occurrence = self.function_call_occurrences.get(&occurrence_key).copied().unwrap_or(0);
            if !occurrence_key.is_empty() {
                self.function_call_occurrences.insert(occurrence_key, occurrence + 1);
            }
            let item = build_function_call_part_item(self.content_index, pi, occurrence, fc, &signature);
            self.append_item(with_context_hash(item, &self.response_context_hash.clone()));
            if !signature.is_empty() {
                self.seen_signatures.insert(signature);
            }
            self.last_response_kind = "function_call".into();
            return;
        }

        let mut target_kind = if part.g("thought").bool() { "thought".to_string() } else { String::new() };
        let text = part.g("text");
        let has_semantic_text = text.exists() && !text.str().is_empty();
        let signature_only = !signature.is_empty() && !has_semantic_text;
        if signature_only && self.last_response_kind == "function_call" {
            if !self.seen_signatures.contains(&signature) {
                self.attach_detached_signature_to_last_function_call(&signature);
                self.seen_signatures.insert(signature);
            }
            return;
        }
        if has_semantic_text {
            if target_kind != "thought" {
                target_kind = "text".into();
            }
            if !signature.is_empty() {
                let visible_empty = self.visible_text.is_empty();
                let thought_empty = self.thought_text.is_empty();
                let mut kept = Vec::new();
                for pending in std::mem::take(&mut self.pending) {
                    let mut unbound_prefix = pending.target_kind.is_empty();
                    if pending.target_kind == target_kind {
                        unbound_prefix = (target_kind == "text" && visible_empty) || (target_kind == "thought" && thought_empty);
                    }
                    if unbound_prefix {
                        if pending.signature == signature {
                            self.seen_signatures.remove(&signature);
                        }
                        continue;
                    }
                    kept.push(pending);
                }
                self.pending = kept;
                if self.pending.iter().any(|p| p.target_kind == target_kind && p.signature != signature) {
                    self.flush_pending_for_kind(&target_kind);
                }
            }
            if !self.last_response_kind.is_empty()
                && self.last_response_kind != target_kind
                && (self.last_response_kind == "text" || self.last_response_kind == "thought")
            {
                let kind = self.last_response_kind.clone();
                self.flush_pending_for_kind(&kind);
            }
            if target_kind == "thought" {
                if self.thought_text.is_empty() {
                    self.thought_part_index = pi;
                }
                self.thought_text.push_str(&text.str());
            } else {
                if self.visible_text.is_empty() {
                    self.visible_part_index = pi;
                }
                self.visible_text.push_str(&text.str());
            }
            self.last_response_kind = target_kind.clone();
        }
        let mut accepted = false;
        if !signature.is_empty() && !self.seen_signatures.contains(&signature) {
            if target_kind.is_empty() {
                target_kind = self.last_response_kind.clone();
            }
            let unmatched_detached_carrier = signature_only
                && self.last_response_kind == target_kind
                && ((target_kind == "text" && self.visible_text.is_empty())
                    || (target_kind == "thought" && self.thought_text.is_empty()));
            if unmatched_detached_carrier {
                self.seen_signatures.insert(signature.clone());
            } else if self.pending.len() + self.items.len() + 1 > MAX_ITEMS || self.item_bytes + signature.len() > MAX_BYTES {
                self.overflow = true;
                self.seen_signatures.insert(signature.clone());
            } else {
                self.pending.push(PendingSignature { signature: signature.clone(), target_kind: target_kind.clone() });
                self.seen_signatures.insert(signature.clone());
                accepted = true;
            }
        }
        if accepted && (signature_only || has_semantic_text) {
            match target_kind.as_str() {
                "text" if !self.visible_text.is_empty() => self.flush_pending_for_kind("text"),
                "thought" if !self.thought_text.is_empty() => self.flush_pending_for_kind("thought"),
                _ => {}
            }
        }
    }

    /// Binds the pending signatures of `target_kind` to the segment text collected so far.
    fn flush_pending_for_kind(&mut self, target_kind: &str) {
        if target_kind != "text" && target_kind != "thought" {
            return;
        }
        let (text, part_index) = if target_kind == "thought" {
            (self.thought_text.clone(), self.thought_part_index)
        } else {
            (self.visible_text.clone(), self.visible_part_index)
        };
        let mut target_hash = String::new();
        let mut target_occurrence = 0usize;
        if !text.is_empty() {
            target_hash = hex::encode(Sha256::digest(format!("{target_kind}\0{text}").as_bytes()));
            let occurrence_key = format!("{target_kind}\0{target_hash}");
            target_occurrence = self.segment_occurrences.get(&occurrence_key).copied().unwrap_or(0);
            self.segment_occurrences.insert(occurrence_key, target_occurrence + 1);
        }
        let mut remaining = Vec::new();
        for pending in std::mem::take(&mut self.pending) {
            if pending.target_kind != target_kind || target_hash.is_empty() {
                remaining.push(pending);
                continue;
            }
            let mut item = build_thought_signature_item(self.content_index, part_index, &pending.signature, target_kind, &target_hash);
            cpa_json::set(&mut item, "targetOccurrence", target_occurrence as i64);
            let item = with_context_hash(item, &self.response_context_hash.clone());
            self.append_item(item);
        }
        self.pending = remaining;
        if target_kind == "thought" {
            self.thought_text.clear();
            self.thought_part_index = usize::MAX;
        } else {
            self.visible_text.clear();
            self.visible_part_index = usize::MAX;
        }
    }

    fn append_pending_thought_signatures(&mut self) {
        for i in 0..self.pending.len() {
            if !self.pending[i].target_kind.is_empty() {
                continue;
            }
            let kind = if self.last_response_kind == "text" && !self.visible_text.is_empty() {
                "text"
            } else if self.last_response_kind == "thought" && !self.thought_text.is_empty() {
                "thought"
            } else if !self.visible_text.is_empty() {
                "text"
            } else if !self.thought_text.is_empty() {
                "thought"
            } else {
                continue;
            };
            self.pending[i].target_kind = kind.into();
        }
        self.flush_pending_for_kind("thought");
        self.flush_pending_for_kind("text");
        self.pending.clear();
    }

    /// Publishes the recorded chain to the ledger once the turn completed (a terminal
    /// `finishReason` was seen). Overflow, an empty chain or a failed replace clear the entry.
    pub(crate) fn commit(&mut self) {
        tracing::debug!(
            "antigravity replay: accumulator commit terminal={} overflow={} items={} (session={})",
            self.terminal,
            self.overflow,
            self.items.len(),
            replay_log_key(&self.scope.session_key)
        );
        if !self.terminal {
            // The stream never completed: this turn contributes nothing and its ids stay
            // unresolvable.
            return;
        }
        let clear = |scope: &ReplayScope| {
            delete_antigravity_reasoning_replay_items_if_unchanged(&scope.model_name, &scope.session_key, &scope.snapshot);
        };
        if self.overflow {
            clear(&self.scope);
            return;
        }
        self.append_pending_thought_signatures();
        if self.overflow || self.items.is_empty() {
            clear(&self.scope);
            return;
        }
        let items: Vec<Vec<u8>> = self.items.iter().map(cpa_json::to_vec).collect();
        let replaced = replace_antigravity_reasoning_replay_items_if_unchanged(
            &self.scope.model_name,
            &self.scope.session_key,
            &self.scope.snapshot,
            &items,
        );
        if replaced.is_err() {
            clear(&self.scope);
        }
    }
}

/// Records a non-stream response body in the ledger.
pub(crate) fn cache_reasoning_replay_from_response(scope: &ReplayScope, request_payload: &[u8], body: &[u8]) {
    if !scope.valid() || body.is_empty() {
        return;
    }
    if let Some(mut acc) = ReplayAccumulator::new(scope, request_payload) {
        acc.observe_response_payload(body);
        acc.commit();
    }
}

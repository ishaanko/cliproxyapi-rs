//! Response id remapping and transcript recording for the xAI websocket transport (Go:
//! xaiWebsocketIDState / xaiWebsocketRequestIDMapper in xai_websockets_executor.go).
//!
//! The downstream client sees a monotone `previous_response_id` chain even when the upstream
//! connection (and with it the upstream's response memory) is replaced: ids the new connection
//! does not know are dropped and the recorded transcript is replayed as `input` instead, and a
//! repeated upstream response id is exposed as `<id>-xai-<n>`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use cpa_json::{J, Value};
use parking_lot::Mutex;

use crate::xai::util::items;

static STATES: LazyLock<Mutex<HashMap<String, Arc<IdState>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Per execution session id state; `request_mu` serializes requests that have no websocket
/// session of their own.
#[derive(Default)]
pub struct IdState {
    pub request_mu: Arc<tokio::sync::Mutex<()>>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    downstream_to_upstream: HashMap<String, String>,
    sequence: i64,
    transcript_input: Vec<Value>,
    replay_compacted_transcript_on_reset: bool,
}

/// The state of `session_id`, created on first use (Go: getXAIWebsocketIDState).
pub fn get_state(session_id: &str) -> Option<Arc<IdState>> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    Some(Arc::clone(STATES.lock().entry(session_id.to_string()).or_default()))
}

pub fn delete_state(session_id: &str) {
    let session_id = session_id.trim();
    if !session_id.is_empty() {
        STATES.lock().remove(session_id);
    }
}

/// The array items at `path` when it is an array, else none (Go: xaiJSONRawMessages).
fn array_items(root: &Value, path: &str) -> Vec<Value> {
    items(root, path).to_vec()
}

impl IdState {
    fn upstream_id_for_downstream(&self, downstream_id: &str) -> String {
        let downstream_id = downstream_id.trim();
        if downstream_id.is_empty() {
            return String::new();
        }
        match self.inner.lock().downstream_to_upstream.get(downstream_id) {
            Some(upstream) => upstream.trim().to_string(),
            None => downstream_id.to_string(),
        }
    }

    /// Records that `downstream_id` is served by `upstream_id` ("" for a compaction response
    /// the upstream connection does not know).
    pub fn map_downstream_to_upstream(&self, downstream_id: &str, upstream_id: &str) {
        let downstream_id = downstream_id.trim();
        if downstream_id.is_empty() {
            return;
        }
        self.inner.lock().downstream_to_upstream.insert(downstream_id.to_string(), upstream_id.trim().to_string());
    }

    /// The recorded transcript as a JSON array, `None` when empty (Go: snapshotTranscriptInput).
    pub fn snapshot_transcript_input(&self) -> Option<Vec<Value>> {
        let inner = self.inner.lock();
        (!inner.transcript_input.is_empty()).then(|| inner.transcript_input.clone())
    }

    /// `payload` with the recorded transcript prepended to its `input`.
    fn prepend_items(prefix: Vec<Value>, payload: &[u8]) -> Option<Vec<u8>> {
        let mut root = cpa_json::parse(payload);
        let mut merged = prefix;
        merged.extend(array_items(&root, "input"));
        cpa_json::set(&mut root, "input", Value::Array(merged)).then(|| cpa_json::to_vec(&root))
    }

    /// Go: prependTranscriptInput.
    fn prepend_transcript_input(&self, payload: Vec<u8>) -> Vec<u8> {
        if payload.is_empty() {
            return payload;
        }
        let prefix = self.inner.lock().transcript_input.clone();
        if prefix.is_empty() {
            return payload;
        }
        Self::prepend_items(prefix, &payload).unwrap_or(payload)
    }

    /// Appends one turn (request input plus response output) to the transcript; `reset` starts
    /// a new transcript first (Go: recordTranscriptTurn).
    pub fn record_transcript_turn(&self, request_payload: &[u8], completed_payload: &[u8], reset: bool) {
        if request_payload.is_empty() || completed_payload.is_empty() {
            return;
        }
        let input_items = array_items(&cpa_json::parse(request_payload), "input");
        let output_items = array_items(&cpa_json::parse(completed_payload), "response.output");
        let mut inner = self.inner.lock();
        if reset {
            inner.transcript_input.clear();
            inner.replay_compacted_transcript_on_reset = false;
        }
        if input_items.is_empty() && output_items.is_empty() {
            return;
        }
        inner.transcript_input.extend(input_items);
        inner.transcript_input.extend(output_items);
    }

    /// Replaces the transcript with a compaction result (Go: replaceTranscriptWithItems).
    pub fn replace_transcript_with_items(&self, items: Vec<Value>) {
        let mut inner = self.inner.lock();
        inner.replay_compacted_transcript_on_reset = !items.is_empty();
        inner.transcript_input = items;
    }

    /// Go: prependCompactedTranscriptOnReset. The flag is not cleared here; the next recorded
    /// turn with `reset` clears it.
    fn prepend_compacted_transcript_on_reset(&self, payload: Vec<u8>) -> (Vec<u8>, bool) {
        if payload.is_empty() {
            return (payload, false);
        }
        let prefix = {
            let inner = self.inner.lock();
            if !inner.replay_compacted_transcript_on_reset || inner.transcript_input.is_empty() {
                return (payload, false);
            }
            inner.transcript_input.clone()
        };
        match Self::prepend_items(prefix, &payload) {
            Some(out) => (out, true),
            None => (payload, false),
        }
    }
}

/// Per request view of the id state.
pub struct RequestIdMapper {
    pub state: Arc<IdState>,
    pub downstream_previous_id: String,
    pub upstream_previous_id: String,
    upstream_response_id: String,
    downstream_response_id: String,
    pub replayed_compacted_transcript: bool,
}

impl RequestIdMapper {
    /// Go: newXAIWebsocketRequestIDMapper.
    pub fn new(session_id: &str, downstream_request: &[u8]) -> Option<RequestIdMapper> {
        let state = get_state(session_id)?;
        let downstream_previous_id = cpa_json::parse(downstream_request).g("previous_response_id").str().trim().to_string();
        let mut upstream_previous_id = downstream_previous_id.clone();
        if !downstream_previous_id.is_empty() {
            upstream_previous_id = state.upstream_id_for_downstream(&downstream_previous_id);
        }
        Some(RequestIdMapper {
            state,
            downstream_previous_id,
            upstream_previous_id,
            upstream_response_id: String::new(),
            downstream_response_id: String::new(),
            replayed_compacted_transcript: false,
        })
    }

    /// The request as the upstream connection must see it (Go: upstreamRequestPayload).
    pub fn upstream_request_payload(&mut self, payload: Vec<u8>) -> Vec<u8> {
        if payload.is_empty() {
            return payload;
        }
        if self.downstream_previous_id == self.upstream_previous_id {
            let request_type = cpa_json::parse(&payload).g("type").str().trim().to_string();
            if self.downstream_previous_id.is_empty() && request_type == "response.append" {
                let (out, replayed) = self.state.prepend_compacted_transcript_on_reset(payload);
                self.replayed_compacted_transcript = replayed;
                return out;
            }
            return payload;
        }
        let mut root = cpa_json::parse(&payload);
        if self.upstream_previous_id.is_empty() {
            cpa_json::delete(&mut root, "previous_response_id");
            let mut out = cpa_json::to_vec(&root);
            if !self.downstream_previous_id.is_empty() {
                out = self.state.prepend_transcript_input(out);
                self.replayed_compacted_transcript = true;
            }
            return out;
        }
        if cpa_json::set(&mut root, "previous_response_id", self.upstream_previous_id.as_str()) {
            return cpa_json::to_vec(&root);
        }
        payload
    }

    /// An upstream event with its response ids rewritten for the client (Go:
    /// downstreamResponsePayload).
    pub fn downstream_response_payload(&mut self, payload: Vec<u8>) -> Vec<u8> {
        if payload.is_empty() {
            return payload;
        }
        let upstream_response_id = cpa_json::parse(&payload).g("response.id").str().trim().to_string();
        let downstream_response_id = self.downstream_id_for_upstream_response(&upstream_response_id);
        if downstream_response_id.is_empty() {
            return payload;
        }
        rewrite_downstream_ids(
            payload,
            &self.upstream_response_id,
            &downstream_response_id,
            &self.upstream_previous_id,
            &self.downstream_previous_id,
        )
    }

    fn downstream_id_for_upstream_response(&mut self, upstream_response_id: &str) -> String {
        let upstream_response_id = upstream_response_id.trim();
        if !self.upstream_response_id.is_empty() {
            return self.downstream_response_id.clone();
        }
        if upstream_response_id.is_empty() {
            return String::new();
        }
        let mut inner = self.state.inner.lock();
        self.upstream_response_id = upstream_response_id.to_string();
        self.downstream_response_id = upstream_response_id.to_string();
        let seen = inner.downstream_to_upstream.contains_key(upstream_response_id);
        if (!self.downstream_previous_id.is_empty()
            && !self.upstream_previous_id.is_empty()
            && upstream_response_id == self.upstream_previous_id)
            || seen
        {
            inner.sequence += 1;
            self.downstream_response_id = format!("{upstream_response_id}-xai-{}", inner.sequence);
        }
        inner.downstream_to_upstream.insert(upstream_response_id.to_string(), upstream_response_id.to_string());
        inner.downstream_to_upstream.insert(self.downstream_response_id.clone(), upstream_response_id.to_string());
        self.downstream_response_id.clone()
    }
}

/// Go: rewriteXAIWebsocketDownstreamIDs. The payload is re-encoded the way `json.Marshal` of a
/// decoded `map[string]any` is (sorted keys, numbers kept verbatim) only when an id changed.
fn rewrite_downstream_ids(
    payload: Vec<u8>,
    upstream_response_id: &str,
    downstream_response_id: &str,
    upstream_previous_id: &str,
    downstream_previous_id: &str,
) -> Vec<u8> {
    let (upstream_response_id, downstream_response_id) = (upstream_response_id.trim(), downstream_response_id.trim());
    let (upstream_previous_id, downstream_previous_id) = (upstream_previous_id.trim(), downstream_previous_id.trim());
    if payload.is_empty() || (upstream_response_id == downstream_response_id && upstream_previous_id == downstream_previous_id) {
        return payload;
    }
    if !cpa_json::valid(&payload) {
        return payload;
    }
    let mut value = cpa_json::parse(&payload);
    let ids = Ids { upstream_response_id, downstream_response_id, upstream_previous_id, downstream_previous_id };
    if !rewrite_value(&mut value, &ids) {
        return payload;
    }
    match cpa_core::util::go_json_sorted(&value, cpa_core::util::GoJsonStyle::MARSHAL_USE_NUMBER) {
        Some(out) => out.into_bytes(),
        None => payload,
    }
}

struct Ids<'a> {
    upstream_response_id: &'a str,
    downstream_response_id: &'a str,
    upstream_previous_id: &'a str,
    downstream_previous_id: &'a str,
}

fn rewrite_value(value: &mut Value, ids: &Ids<'_>) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            for (child_key, child) in map.iter_mut() {
                if let Value::String(text) = child {
                    let replaced = rewrite_string(text, child_key, ids);
                    if replaced != *text {
                        *text = replaced;
                        changed = true;
                    }
                    continue;
                }
                if rewrite_value(child, ids) {
                    changed = true;
                }
            }
            changed
        }
        Value::Array(list) => {
            let mut changed = false;
            for child in list.iter_mut() {
                if rewrite_value(child, ids) {
                    changed = true;
                }
            }
            changed
        }
        _ => false,
    }
}

fn rewrite_string(value: &str, key: &str, ids: &Ids<'_>) -> String {
    match key {
        "id" | "item_id" => {
            if !ids.upstream_response_id.is_empty()
                && !ids.downstream_response_id.is_empty()
                && ids.downstream_response_id != ids.upstream_response_id
                && value.contains(ids.upstream_response_id)
            {
                return value.replace(ids.upstream_response_id, ids.downstream_response_id);
            }
        }
        "previous_response_id"
            if !ids.upstream_previous_id.is_empty()
                && !ids.downstream_previous_id.is_empty()
                && value == ids.upstream_previous_id =>
        {
            return ids.downstream_previous_id.to_string();
        }
        _ => {}
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_upstream_ids_get_a_sequence_suffix_and_previous_ids_are_translated() {
        let session = "ids-test-chain";
        delete_state(session);
        let mut first = RequestIdMapper::new(session, br#"{"input":[]}"#).unwrap();
        let out = first.downstream_response_payload(br#"{"type":"response.created","response":{"id":"resp_1"}}"#.to_vec());
        assert_eq!(out, br#"{"type":"response.created","response":{"id":"resp_1"}}"#);

        // The upstream reuses "resp_1" (new connection): the client must see resp_1-xai-1.
        let mut second = RequestIdMapper::new(session, br#"{"previous_response_id":"resp_1"}"#).unwrap();
        assert_eq!(second.upstream_previous_id, "resp_1");
        let out = second.downstream_response_payload(
            br#"{"type":"response.completed","response":{"id":"resp_1","previous_response_id":"resp_1","output":[{"id":"resp_1_msg"}]}}"#.to_vec(),
        );
        let v = cpa_json::parse(&out);
        assert_eq!(v.g("response.id").str(), "resp_1-xai-1");
        assert_eq!(v.g("response.output.0.id").str(), "resp_1-xai-1_msg");

        // A later request chained on the suffixed id resolves back to the upstream id.
        let third = RequestIdMapper::new(session, br#"{"previous_response_id":"resp_1-xai-1"}"#).unwrap();
        assert_eq!(third.upstream_previous_id, "resp_1");
        delete_state(session);
    }

    #[test]
    fn unknown_upstream_previous_id_replays_the_transcript() {
        let session = "ids-test-replay";
        delete_state(session);
        let mapper = RequestIdMapper::new(session, br#"{"previous_response_id":"resp_gone","input":[{"role":"user"}]}"#).unwrap();
        mapper.state.replace_transcript_with_items(vec![serde_json::json!({"type":"compaction"})]);
        mapper.state.map_downstream_to_upstream("resp_gone", "");
        let mut mapper = RequestIdMapper::new(session, br#"{"previous_response_id":"resp_gone","input":[{"role":"user"}]}"#).unwrap();
        let out = mapper.upstream_request_payload(br#"{"previous_response_id":"resp_gone","input":[{"role":"user"}]}"#.to_vec());
        let v = cpa_json::parse(&out);
        assert!(!v.g("previous_response_id").exists());
        assert_eq!(v.g("input.0.type").str(), "compaction");
        assert_eq!(v.g("input.1.role").str(), "user");
        assert!(mapper.replayed_compacted_transcript);
        delete_state(session);
    }
}

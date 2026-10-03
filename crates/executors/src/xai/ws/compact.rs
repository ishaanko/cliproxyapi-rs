//! `compaction_trigger` requests that arrive over the client websocket (Go:
//! executeCompactionTriggerFromWebsocketContext). The upstream websocket cannot compact, so the
//! recorded transcript is sent to `/responses/compact` over HTTP and the result is replayed to
//! the client as a synthetic event stream; the transcript then restarts from the compaction item.

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_json::{J, Kind, Value};
use cpa_runtime::executor::{ExecError, Options, Request, StreamResult};
use tokio::sync::mpsc;

use super::ids::RequestIdMapper;
use crate::helps::status::status_err;
use crate::helps::usage::parse_openai_usage;
use crate::xai::XaiExecutor;
use crate::xai::execute::{
    build_compaction_trigger_stream_chunks, compaction_output_item, compaction_response_id, remove_input_items_by_type,
};
use crate::xai::request::execution_session_id;

/// Go: buildXAIWebsocketCompactionPayload. `payload` with `input` replaced by the transcript
/// and no `previous_response_id`.
fn build_compaction_payload(payload: &[u8], transcript_input: Vec<Value>) -> Vec<u8> {
    let mut out = if payload.is_empty() { cpa_json::parse_str("{}") } else { cpa_json::parse(payload) };
    cpa_json::set(&mut out, "input", Value::Array(transcript_input));
    cpa_json::delete(&mut out, "previous_response_id");
    cpa_json::to_vec(&out)
}

fn missing_state() -> ExecError {
    status_err(502, "xai websocket compaction response is missing compacted state")
}

/// Go: validateXAIWebsocketCompactionResponse. Returns the normalized response id and the
/// compaction item.
fn validate_compaction_response(data: &[u8]) -> Result<(String, Value), ExecError> {
    if data.is_empty() || !cpa_json::valid(data) {
        return Err(status_err(502, "xai websocket compaction returned invalid JSON"));
    }
    let parsed = cpa_json::parse(data);
    let id = parsed.g("id");
    let output = parsed.g("output");
    if id.kind() != Kind::String || id.str().trim().is_empty() || !output.exists() || !output.is_array() {
        return Err(missing_state());
    }
    let items = output.array();
    let Some(item) = items.first() else { return Err(missing_state()) };
    let item_type = item.g("type");
    let encrypted = item.g("encrypted_content");
    if !(item.is_object() || item.is_array())
        || item_type.kind() != Kind::String
        || item_type.str().trim() != "compaction"
        || encrypted.kind() != Kind::String
        || encrypted.str().trim().is_empty()
    {
        return Err(missing_state());
    }
    let response_id = compaction_response_id(&parsed);
    let item = compaction_output_item(&parsed, &response_id);
    Ok((response_id, item))
}

impl XaiExecutor {
    pub(in crate::xai) async fn execute_compaction_trigger_from_websocket(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        mapper: Option<RequestIdMapper>,
    ) -> Result<StreamResult, ExecError> {
        let Some(mapper) = mapper else {
            return Err(status_err(400, "xai websocket compaction context is unavailable"));
        };
        let transcript = mapper.state.snapshot_transcript_input();
        let compact_payload: Vec<u8>;
        let input_items_count: usize;
        let mut keep_previous_response_id = false;
        if let Some(transcript) = transcript {
            input_items_count = transcript.len();
            compact_payload = build_compaction_payload(&req.payload, transcript);
        } else {
            let mut filtered = cpa_json::parse(&req.payload);
            remove_input_items_by_type(&mut filtered, "compaction_trigger");
            let payload_input = filtered.g("input");
            let list = if payload_input.is_array() { payload_input.array() } else { Vec::new() };
            if !list.is_empty() {
                input_items_count = list.len();
                let input: Vec<Value> = list.iter().map(|item| item.value()).collect();
                compact_payload = build_compaction_payload(&cpa_json::to_vec(&filtered), input);
            } else {
                input_items_count = 0;
                let mut prev_id = mapper.upstream_previous_id.clone();
                if prev_id.is_empty() {
                    prev_id = cpa_json::parse(&req.payload).g("previous_response_id").str().trim().to_string();
                }
                if prev_id.is_empty() {
                    return Err(status_err(400, "xai websocket compaction context is empty"));
                }
                keep_previous_response_id = true;
                let mut payload = cpa_json::parse(&req.payload);
                remove_input_items_by_type(&mut payload, "compaction_trigger");
                cpa_json::set(&mut payload, "previous_response_id", prev_id);
                compact_payload = cpa_json::to_vec(&payload);
            }
        }
        tracing::info!(
            "xai websockets: compact fallback session={} auth={} input_items={} keep_previous_response_id={}",
            execution_session_id(req, opts),
            auth.id.trim(),
            input_items_count,
            keep_previous_response_id
        );
        let mut compact_req = req.clone();
        compact_req.payload = Bytes::from(compact_payload);

        let (prepared, data, mut headers, reporter) = self.execute_compact_request(auth, &compact_req, opts).await?;
        let (response_id, compaction_item) = match validate_compaction_response(&data) {
            Ok(ok) => ok,
            Err(err) => {
                reporter.publish_failure(&err);
                return Err(err);
            }
        };
        reporter.publish(parse_openai_usage(&data));
        mapper.state.replace_transcript_with_items(vec![compaction_item]);
        mapper.state.map_downstream_to_upstream(&response_id, "");

        headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("text/event-stream"));
        let chunks = build_compaction_trigger_stream_chunks(&prepared, &data);
        let (tx, rx) = mpsc::channel(chunks.len().max(1));
        for chunk in chunks {
            // The channel holds every chunk, so sending cannot fail or block.
            let _ = tx.try_send(Ok(Bytes::from(chunk)));
        }
        drop(tx);
        Ok(StreamResult::new(headers, rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_response_must_carry_an_encrypted_compaction_item() {
        let ok = br#"{"id":"cmp_1","output":[{"type":"compaction","encrypted_content":"abc"}]}"#;
        let (id, item) = validate_compaction_response(ok).unwrap();
        assert_eq!(id, "resp_1");
        assert_eq!(item.g("type").str(), "compaction");
        for bad in [
            &b"not json"[..],
            br#"{"id":"","output":[{"type":"compaction","encrypted_content":"abc"}]}"#,
            br#"{"id":"x","output":[]}"#,
            br#"{"id":"x","output":[{"type":"compaction","encrypted_content":" "}]}"#,
        ] {
            assert_eq!(validate_compaction_response(bad).unwrap_err().status, 502);
        }
    }
}

//! Behavior the golden corpus cannot reach: it replays with an empty `Param`, so the
//! executor-owned apply_patch bridge path and the created-event model fill are covered here
//! (ported from codex_openai-responses_response_test.go).

use cpa_json::J;

use super::{
    convert_codex_response_to_openai_responses,
    convert_codex_response_to_openai_responses_non_stream,
};
use crate::common::{ApplyPatchResponsesBridge, normalize_apply_patch_responses_request};
use crate::registry::{Ctx, Param};

fn stream(original: &[u8], request: &[u8], raw: &[u8], param: &mut Param) -> Vec<Vec<u8>> {
    convert_codex_response_to_openai_responses(&Ctx::default(), "m", original, request, raw, param)
}

#[test]
fn created_events_take_the_original_request_model() {
    let request = br#"{"model":"original-codex-model"}"#;
    let translated = br#"{"model":"translated-codex-model"}"#;
    for raw in [
        &br#"data: {"type":"response.created","response":{"id":"resp_1"}}"#[..],
        &br#"data: {"type":"response.in_progress","response":{"id":"resp_1"}}"#[..],
    ] {
        let out = stream(request, translated, raw, &mut Param::default());
        assert_eq!(out.len(), 1);
        let payload = cpa_json::parse(&out[0][6..]);
        assert_eq!(payload.g("response.model").str(), "original-codex-model");
    }
}

#[test]
fn non_stream_incomplete_keeps_status_and_reason() {
    let raw = br#"{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#;
    let out = convert_codex_response_to_openai_responses_non_stream(
        &Ctx::default(),
        "gpt-5.5",
        b"",
        b"",
        raw,
        &mut Param::default(),
    );
    let out = cpa_json::parse(&out.expect("body"));
    assert_eq!(out.g("status").str(), "incomplete");
    assert_eq!(
        out.g("incomplete_details.reason").str(),
        "max_output_tokens"
    );
}

/// Only an executor-owned bridge in the param enables apply_patch bridging.
#[test]
fn apply_patch_bridging_requires_an_executor_owned_bridge() {
    let original = br#"{"tools":[{"type":"custom","name":"apply_patch"}]}"#;
    let bridged = normalize_apply_patch_responses_request(original).expect("normalize");
    let raw = br#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":"{\"input\":\"p\"}"}}"#;

    // Native Codex, with or without the normalized request, passes the event through.
    let out = stream(original, original, raw, &mut Param::default());
    assert_eq!(out, vec![raw.to_vec()]);
    let out = stream(original, &bridged, raw, &mut Param::default());
    assert_eq!(out, vec![raw.to_vec()]);

    // An executor-supplied bridge converts the function call into custom tool call events.
    let mut param = Param::default();
    param.state(|| ApplyPatchResponsesBridge::new(original));
    let out = stream(original, &bridged, raw, &mut param);
    assert_eq!(out.len(), 3);
    assert_eq!(cpa_json::parse(&out[2][6..]).g("item.input").str(), "p");

    // A call with invalid arguments fails the stream once and records the tool input error.
    let mut failed = Param::default();
    failed.state(|| ApplyPatchResponsesBridge::new(original));
    let bad = br#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"apply_patch","arguments":"{}"}}"#;
    let out = stream(original, &bridged, bad, &mut failed);
    assert_eq!(out.len(), 1);
    assert_eq!(
        cpa_json::parse(&out[0][6..]).g("type").str(),
        "response.failed"
    );
    assert!(failed.tool_input_error.is_some());
}

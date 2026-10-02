//! Stream behavior that depends on `Ctx.alt` and `[DONE]` synthesis, which the conformance corpus
//! (always `alt: None`) does not reach (ported from antigravity_gemini_response_test.go).

use cpa_json::J;

use super::response::{convert_antigravity_response_to_gemini, convert_antigravity_response_to_gemini_non_stream};
use crate::registry::{Ctx, Param};

fn ctx(alt: &str) -> Ctx {
    Ctx { alt: Some(alt.to_string()) }
}

fn stream(ctx: &Ctx, param: &mut Param, raw: &str) -> Vec<cpa_json::Value> {
    convert_antigravity_response_to_gemini(ctx, "m", b"", b"", raw.as_bytes(), param).iter().map(|c| cpa_json::parse(c)).collect()
}

#[test]
fn no_alt_in_context_emits_nothing() {
    let mut param = Param::default();
    let out = convert_antigravity_response_to_gemini(&Ctx::default(), "m", b"", b"", br#"{"response":{"candidates":[{}]}}"#, &mut param);
    assert!(out.is_empty());
}

#[test]
fn done_synthesizes_terminal_chunk_once_when_finish_reason_was_omitted() {
    let (ctx, mut param) = (ctx(""), Param::default());
    let first = stream(&ctx, &mut param, r#"data: {"response":{"candidates":[{"content":{"parts":[{"thought":true,"text":"Thinking..."}],"role":"model"}}],"usageMetadata":{"promptTokenCount":100,"totalTokenCount":150,"thoughtsTokenCount":50},"modelVersion":"gemini-3.7-flash","responseId":"resp-123"}}"#);
    assert_eq!(first.len(), 1);
    assert!(!first[0].g("candidates.0.finishReason").exists());

    let done = stream(&ctx, &mut param, "[DONE]");
    assert_eq!(done.len(), 1);
    let synthetic = &done[0];
    assert_eq!(synthetic.g("candidates.0.finishReason").str(), "STOP");
    assert_eq!(synthetic.g("candidates.0.content.role").str(), "model");
    assert_eq!(synthetic.g("candidates.0.content.parts").raw(), r#"[{"text":""}]"#);
    assert_eq!(synthetic.g("usageMetadata.totalTokenCount").int(), 150);
    assert_eq!(synthetic.g("modelVersion").str(), "gemini-3.7-flash");
    assert_eq!(synthetic.g("responseId").str(), "resp-123");

    assert!(stream(&ctx, &mut param, "[DONE]").is_empty());
}

#[test]
fn done_after_explicit_finish_reason_emits_nothing() {
    let (ctx, mut param) = (ctx(""), Param::default());
    stream(&ctx, &mut param, r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Hello"}],"role":"model"},"finishReason":"STOP"}]}}"#);
    assert!(stream(&ctx, &mut param, "[DONE]").is_empty());
}

#[test]
fn filtered_usage_is_restored_and_carried_into_the_synthetic_terminal_chunk() {
    let (ctx, mut param) = (ctx(""), Param::default());
    let out = stream(&ctx, &mut param, r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Hello"}],"role":"model"}}],"cpaUsageMetadata":{"promptTokenCount":42,"candidatesTokenCount":7,"totalTokenCount":49}}}"#);
    assert_eq!(out[0].g("usageMetadata.totalTokenCount").int(), 49);
    let done = stream(&ctx, &mut param, "[DONE]");
    assert_eq!(done[0].g("usageMetadata.candidatesTokenCount").int(), 7);
}

#[test]
fn response_free_events_do_not_start_the_stream() {
    let (ctx, mut param) = (ctx(""), Param::default());
    for chunk in ["{}", r#"{"response":{}}"#, r#"{"response":{"candidates":[]}}"#, r#"{"response":{"candidates":[],"usageMetadata":{}}}"#] {
        stream(&ctx, &mut param, chunk);
    }
    assert!(stream(&ctx, &mut param, "[DONE]").is_empty());
}

#[test]
fn non_empty_alt_wraps_chunks_in_arrays() {
    let (ctx, mut param) = (ctx("sse"), Param::default());
    let out = stream(&ctx, &mut param, r#"[{"response":{"candidates":[{"content":{"parts":[{"text":"a"}]}}]}},{"other":1}]"#);
    assert_eq!(out[0].as_array().map(Vec::len), Some(1));
    // Array chunks never register as a started stream (observe() reads object paths), so [DONE]
    // synthesizes nothing; a started stream wraps the synthetic chunk in an array.
    assert!(stream(&ctx, &mut param, "[DONE]").is_empty());
    let mut param = Param::default();
    stream(&ctx, &mut param, r#"{"response":{"candidates":[{"content":{"parts":[{"text":"a"}]}}]}}"#);
    let done = stream(&ctx, &mut param, "[DONE]");
    assert_eq!(done[0].g("0.candidates.0.finishReason").str(), "STOP");
}

#[test]
fn non_stream_defaults_every_candidate_finish_reason() {
    let input = br#"{"response":{"candidates":[{"content":{"parts":[{"text":"a"}]}},{"content":{"parts":[{"text":"b"}]},"finishReason":"MAX_TOKENS"},{"content":{"parts":[{"text":"c"}]}}]}}"#;
    let out = convert_antigravity_response_to_gemini_non_stream(&Ctx::default(), "", b"", b"", input, &mut Param::default()).expect("body");
    let out = cpa_json::parse(&out);
    let reasons: Vec<String> = (0..3).map(|i| out.g(&format!("candidates.{i}.finishReason")).str()).collect();
    assert_eq!(reasons, ["STOP", "MAX_TOKENS", "STOP"]);
}

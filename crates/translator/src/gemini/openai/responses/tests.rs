//! Behavior the golden corpus cannot reach: the executor-driven stream finalizer, malformed
//! upstream payloads, and byte to rune citation offsets.

use cpa_json::{Value, J};

use super::{build_responses_url_citations, convert_gemini_response_to_openai_responses, finalize_tool_input};
use crate::registry::{Ctx, Param};

const APPLY_PATCH_REQUEST: &[u8] = br#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}}]}]}"#;

fn stream(original: &[u8], chunk: &str, param: &mut Param) -> Vec<Vec<u8>> {
    convert_gemini_response_to_openai_responses(&Ctx::default(), "gemini", original, b"", chunk.as_bytes(), param)
}

#[test]
fn finalize_fails_apply_patch_stream_that_ended_without_finish_reason() {
    let mut param = Param::default();
    let events = stream(APPLY_PATCH_REQUEST, r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#, &mut param);
    assert!(!events.is_empty());

    let failure = finalize_tool_input(&mut param);
    assert_eq!(failure.len(), 1);
    assert!(String::from_utf8_lossy(&failure[0]).starts_with("event: response.failed\n"));
    assert!(param.tool_input_error.is_some());
    // Failing twice would emit a second terminal event.
    assert!(finalize_tool_input(&mut param).is_empty());
}

#[test]
fn finalize_ignores_streams_without_apply_patch() {
    let mut param = Param::default();
    stream(b"{}", r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#, &mut param);
    assert!(finalize_tool_input(&mut param).is_empty());
    assert!(param.tool_input_error.is_none());
}

#[test]
fn citation_offsets_are_runes_not_bytes() {
    let gm: Value = cpa_json::parse_str(
        r#"{"groundingChunks":[{"web":{"uri":"https://a.example","title":"A"}}],
            "groundingSupports":[{"segment":{"startIndex":7,"endIndex":12},"groundingChunkIndices":[0]}]}"#,
    );
    // "héllo " is 7 bytes but 6 runes; "world" spans bytes 7..12 and runes 6..11.
    let cites = build_responses_url_citations(&gm, Some("héllo world"));
    assert_eq!(cites.len(), 1);
    assert_eq!(cites[0].g("start_index").int(), 6);
    assert_eq!(cites[0].g("end_index").int(), 11);
}

//! Behavior of the carrier helpers that the conformance corpus does not reach (ported from
//! signature_validation_test.go).

use cpa_json::J;

use super::signature_validation::*;

/// A Gemini "E"-prefixed thought signature (protobuf envelope accepted by the validator).
const GEMINI_SIG: &str = "EggKBgEMOdbHNA==";

fn contents(out: &[u8]) -> Vec<cpa_json::Value> {
    cpa_json::parse(out).g("messages.0.content").array().iter().map(|c| c.value()).collect()
}

fn strip(content: &str) -> Vec<cpa_json::Value> {
    let input = format!(r#"{{"messages":[{{"role":"assistant","content":{content}}}]}}"#);
    contents(&strip_invalid_gemini_signature_thinking_blocks(input.as_bytes()))
}

#[test]
fn carrier_round_trip() {
    for (direction, kind) in [(CARRIER_NEXT, CARRIER_TEXT), (CARRIER_PREVIOUS, CARRIER_FUNCTION), (CARRIER_STANDALONE, CARRIER_ANY)] {
        let encoded = encode_gemini_claude_carrier_signature(GEMINI_SIG, direction, kind);
        let c = decode_gemini_claude_carrier_signature(&encoded);
        assert!(c.marked && c.ok, "{encoded}");
        assert_eq!((c.signature.as_str(), c.direction.as_str(), c.target_kind.as_str()), (GEMINI_SIG, direction, kind));
    }
}

#[test]
fn marked_non_empty_thinking_is_preserved_unless_placed_backward() {
    let standalone = encode_gemini_claude_carrier_signature(GEMINI_SIG, CARRIER_STANDALONE, CARRIER_TEXT);
    let next_function = encode_gemini_claude_carrier_signature(GEMINI_SIG, CARRIER_NEXT, CARRIER_FUNCTION);
    let previous = encode_gemini_claude_carrier_signature(GEMINI_SIG, CARRIER_PREVIOUS, CARRIER_TEXT);
    let content = strip(&format!(
        r#"[{{"type":"thinking","thinking":"signed thought","signature":"{standalone}"}},{{"type":"thinking","thinking":"tool preface","signature":"{next_function}"}},{{"type":"tool_use","id":"tool-1","name":"run","input":{{}}}},{{"type":"thinking","thinking":"invalid backward","signature":"{previous}"}}]"#
    ));
    assert_eq!(content.len(), 3);
    assert_eq!(content[0].g("signature").str(), standalone);
    assert_eq!(content[1].g("signature").str(), next_function);
    assert_eq!(content[2].g("type").str(), "tool_use");
}

#[test]
fn legacy_raw_carrier_survives_only_in_assistant_messages() {
    let input = format!(
        r#"{{"messages":[{{"role":"user","content":[{{"type":"thinking","thinking":"","signature":"{GEMINI_SIG}"}},{{"type":"text","text":"user text"}}]}},{{"role":"assistant","content":[{{"type":"thinking","thinking":"","signature":"{GEMINI_SIG}"}},{{"type":"text","text":"assistant text"}}]}}]}}"#
    );
    let out = cpa_json::parse(&strip_invalid_gemini_signature_thinking_blocks(input.as_bytes()));
    assert_eq!(out.g("messages.0.content").array().len(), 1);
    assert_eq!(out.g("messages.0.content.0.type").str(), "text");
    assert_eq!(out.g("messages.1.content").array().len(), 2);
    assert_eq!(out.g("messages.1.content.0.signature").str(), GEMINI_SIG);
}

#[test]
fn unsigned_thought_is_kept_only_with_a_following_previous_carrier() {
    let carrier = encode_gemini_claude_carrier_signature(GEMINI_SIG, CARRIER_PREVIOUS, CARRIER_TEXT);
    let content = strip(&format!(
        r#"[{{"type":"thinking","thinking":"let me think","signature":""}},{{"type":"text","text":"answer text"}},{{"type":"thinking","thinking":"","signature":"{carrier}"}}]"#
    ));
    assert_eq!(content.len(), 3);
    assert_eq!(content[0].g("thinking").str(), "let me think");

    let dropped = strip(r#"[{"type":"thinking","thinking":"a","signature":""},{"type":"thinking","thinking":"b","signature":""},{"type":"text","text":"visible answer"}]"#);
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].g("text").str(), "visible answer");
}

#[test]
fn invalid_thinking_between_does_not_propagate_a_trailing_carrier() {
    let carrier = encode_gemini_claude_carrier_signature(GEMINI_SIG, CARRIER_PREVIOUS, CARRIER_TEXT);
    let content = strip(&format!(
        r#"[{{"type":"thinking","thinking":"thought A","signature":""}},{{"type":"thinking","thinking":"invalid boundary","signature":"invalid"}},{{"type":"thinking","thinking":"","signature":"{carrier}"}}]"#
    ));
    assert!(content.is_empty());
}

#[test]
fn malformed_carrier_is_rejected() {
    let c = decode_gemini_claude_carrier_signature("cpa-gemini-carrier-v1:previous:text:invalid-base64");
    assert!(c.marked && !c.ok);
}

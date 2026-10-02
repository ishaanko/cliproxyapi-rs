use serde_json::{Value, json};

use super::*;
use crate::devin::test_support::golden;

fn prompt_json(p: &Prompt) -> Value {
    json!({
        "message_id": "",
        "source": p.source,
        "content": p.content,
        "images": p.images.iter().map(|i| json!({"base64_data": i.base64_data, "mime_type": i.mime_type})).collect::<Vec<_>>(),
        "tool_calls": p.tool_calls.iter().map(|t| json!({"id": t.id, "name": t.name, "arguments": t.arguments})).collect::<Vec<_>>(),
        "tool_call_id": p.tool_call_id,
        "original_tool_call_id": p.original_tool_call_id,
        "is_orphaned_tool": p.is_orphaned_tool,
        "thinking": p.thinking,
        "signature_b64": if p.signature.is_empty() { String::new() } else { base64::engine::general_purpose::STANDARD.encode(&p.signature) },
        "signature_type": p.signature_type,
    })
}

fn parsed_json(p: &ParsedPayload) -> Value {
    json!({
        "system_prompt": p.system_prompt,
        "prompts": p.prompts.iter().map(prompt_json).collect::<Vec<_>>(),
        "tools": p.tools.iter().map(|t| json!({
            "name": t.name, "description": t.description, "parameters": String::from_utf8_lossy(&t.parameters)
        })).collect::<Vec<_>>(),
        "max_tokens": p.max_tokens,
        "session_id": p.session_id,
        "cascade_id": p.cascade_id,
        "thinking_level": p.thinking_level,
        "budget_tokens": p.budget_tokens,
    })
}

#[test]
fn interactions_payload_parsing_matches_go() {
    for case in golden()["parse"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let parsed =
            parse_interactions_payload(case["payload"].as_str().unwrap().as_bytes(), case["original"].as_str().unwrap().as_bytes());
        // Message ids are random in both implementations: the golden blanks them, so must we, but
        // each prompt must have received one.
        assert!(parsed.prompts.iter().all(|p| !p.message_id.is_empty()), "case {name}");
        // Temperatures compare numerically (Go prints 0, serde keeps 0.0 for a float zero).
        assert_eq!(parsed.temperature, case["out"]["temperature"].as_f64(), "case {name}");
        let mut want = case["out"].clone();
        want.as_object_mut().unwrap().remove("temperature");
        assert_eq!(parsed_json(&parsed), want, "case {name}");
    }
}

#[test]
fn signature_classification_matches_go() {
    for case in golden()["sigs"].as_array().unwrap() {
        let input = case["in"].as_str().unwrap();
        let (bytes, ty) = parse_signature_bytes(input);
        let want_bytes = base64::engine::general_purpose::STANDARD.decode(case["bytes_b64"].as_str().unwrap()).unwrap();
        assert_eq!((bytes, ty.as_str()), (want_bytes, case["type"].as_str().unwrap()), "signature {input:?}");
        assert_eq!(detect_signature_type(input), case["detect"].as_str().unwrap(), "signature {input:?}");
    }
}

#[test]
fn session_ids_normalize_like_go() {
    for case in golden()["uuids"].as_array().unwrap() {
        let input = case["in"].as_str().unwrap();
        assert_eq!(normalize_uuid(input), case["out"].as_str().unwrap(), "input {input:?}");
    }
    // Blank input gets a fresh random UUID.
    assert!(Uuid::parse_str(&normalize_uuid("  ")).is_ok());
}

#[test]
fn session_resolution_prefers_payload_then_context_then_canonical() {
    let canonical = || "canonical-session".to_string();
    let (s, c) = resolve_session_and_cascade_ids("payload-session", "payload-session", "ctx", canonical);
    assert_eq!((s.clone(), c), (normalize_uuid("payload-session"), s));
    let (s, _) = resolve_session_and_cascade_ids("", "", " ctx-session ", canonical);
    assert_eq!(s, normalize_uuid("ctx-session"));
    let (s, c) = resolve_session_and_cascade_ids("", "", "", canonical);
    assert_eq!((s.clone(), c), (normalize_uuid("canonical-session"), s));
    // An explicit cascade id is normalized separately from the session.
    let (s, c) = resolve_session_and_cascade_ids("s1", "casc", "", canonical);
    assert_eq!((s, c), (normalize_uuid("s1"), normalize_uuid("casc")));
}

//! The fast path must emit exactly the bytes of the general `Value` path, including for the
//! shapes it declines.

use super::{convert_claude_response_to_openai, convert_general, StreamState};
use crate::registry::{Ctx, Param};

/// Masks the `created` clock read, which differs between the two runs.
fn mask(frames: Vec<Vec<u8>>) -> Vec<String> {
    frames
        .into_iter()
        .map(|f| {
            let s = String::from_utf8(f).unwrap_or_default();
            let mut out = String::new();
            let mut rest = s.as_str();
            while let Some(i) = rest.find("\"created\":") {
                let (head, tail) = rest.split_at(i + 10);
                out.push_str(head);
                out.push('N');
                rest = &tail[tail.find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len())..];
            }
            out.push_str(rest);
            out
        })
        .collect()
}

fn assert_same(events: &[&str]) {
    let mut param = Param::default();
    let mut general = StreamState::default();
    for ev in events {
        let line = format!("data: {ev}");
        let got = mask(convert_claude_response_to_openai(&Ctx::default(), "claude-x", b"", b"", line.as_bytes(), &mut param));
        let want = mask(convert_general(&mut general, "claude-x", ev.as_bytes()));
        assert_eq!(got, want, "event {ev}");
    }
}

#[test]
fn canonical_stream() {
    assert_same(&[
        r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"usage":{"input_tokens":11,"cache_creation_input_tokens":2,"cache_read_input_tokens":3,"output_tokens":1,"cache_creation":{"a":1}}}}"#,
        r#"{"type":"ping"}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi \"there\"\n\t\u0001 é é 😀 </script> \/"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_2","name":"noargs"}}"#,
        r#"{"type":"content_block_stop","index":2}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":7}}"#,
        r#"{"type":"message_stop"}"#,
        r#"{"type":"message_stop"}"#,
    ]);
}

/// Derived serde structs would fill fields by position from an array; gjson sees no fields.
#[test]
fn array_for_object_shapes() {
    assert_same(&[
        r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":11,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":["tool_use","t1","f"]}"#,
        r#"{"type":"content_block_delta","index":0,"delta":["text_delta","hi"]}"#,
        r#"{"type":"message_delta","delta":["end_turn"],"usage":[1,2,3,4]}"#,
        r#"{"type":"message_delta","usage":[1,2,3,4]}"#,
        r#"["message_stop"]"#,
        r#"["message_start",{"id":"x"}]"#,
        r#"{"type":"message_start","message":["m1",{"input_tokens":3}]}"#,
        r#"{"type":"message_stop"}"#,
    ]);
}

#[test]
fn declined_shapes() {
    assert_same(&[
        r#"{"type":"message_start"}"#,
        r#"{"type":"message_start","message":{"id":null}}"#,
        r#"{"type":"message_start","message":{"id":7,"usage":null}}"#,
        r#"{"type":"content_block_delta","index":1.0,"delta":{"type":"text_delta","text":"a"}}"#,
        r#"{"type":"content_block_delta","index":"0","delta":{"type":"text_delta","text":"a"}}"#,
        r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":null}}"#,
        r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":5}}"#,
        r#"{"type":"content_block_delta","delta":null}"#,
        r#"{"type":"content_block_delta","delta":{"type":"text_delta"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":null},"usage":null}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":1.5}}"#,
        r#"{"type":"message_delta","usage":{"input_tokens":"4"}}"#,
        r#"{"type":"message_delta"}"#,
        r#"{"type":"message_stop"}"#,
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":1,"name":"x"}}"#,
        r#"{"index":0}"#,
        r#"{"type":5}"#,
        r#"[1,2]"#,
        r#"{"type":"content_block_delta","type":"message_stop"}"#,
        r#"{"type":"message_start" "#,
        r#"not json"#,
        r#"{"type":"message_start","message":{"id":"dup","id":"dup2"}}"#,
    ]);
}

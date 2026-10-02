use futures_util::stream;

use super::*;
use crate::devin::pb;
use crate::devin::test_support::{golden, prompt_from_json, tool_from_json};

fn frame_stream(chunks: Vec<Vec<u8>>) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Unpin {
    stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from(c))).collect::<Vec<_>>())
}

fn str_field(num: u32, v: &str) -> Vec<u8> {
    let mut b = Vec::new();
    pb::put_str(&mut b, num, v);
    b
}

fn varint_field(num: u32, v: u64) -> Vec<u8> {
    let mut b = Vec::new();
    pb::put_varint_field(&mut b, num, v);
    b
}

#[test]
fn chat_request_matches_go_encoding() {
    for case in golden()["wire"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let session = case["session_id"].as_str().unwrap();
        reset_session_turn_index(session);
        let prompts: Vec<Prompt> = case["prompts"].as_array().unwrap().iter().map(prompt_from_json).collect();
        let tools: Vec<Tool> = case["tools"].as_array().unwrap().iter().map(tool_from_json).collect();
        let words: Vec<String> =
            case["sensitive_words"].as_array().unwrap().iter().map(|w| w.as_str().unwrap().to_string()).collect();
        let matcher = SensitiveWordMatcher::new(&words);
        for (i, want) in case["hex"].as_array().unwrap().iter().enumerate() {
            let got = build_get_chat_message_request(&ChatRequest {
                session_token: case["session_token"].as_str().unwrap(),
                device_seed: case["device_seed"].as_str().unwrap(),
                chat_model_uid: case["chat_model_uid"].as_str().unwrap(),
                system_prompt: case["system_prompt"].as_str().unwrap(),
                prompts: &prompts,
                tools: &tools,
                temperature: case["temperature"].as_f64(),
                max_tokens: case["max_tokens"].as_i64().unwrap(),
                session_id: session,
                cascade_id: case["cascade_id"].as_str().unwrap(),
                matcher: matcher.as_ref(),
            });
            assert_eq!(hex::encode(got), want.as_str().unwrap(), "case {name} request {i}");
        }
        reset_session_turn_index(session);
    }
}

#[test]
fn system_prompt_sanitizing_matches_go() {
    for case in golden()["sanitize"].as_array().unwrap() {
        let words: Vec<String> = case["words"].as_array().unwrap().iter().map(|w| w.as_str().unwrap().into()).collect();
        let matcher = SensitiveWordMatcher::new(&words);
        let got = sanitize_system_prompt(case["prompt"].as_str().unwrap(), matcher.as_ref());
        assert_eq!(got, case["out"].as_str().unwrap(), "prompt {:?}", case["prompt"]);
    }
}

#[test]
fn trailer_errors_match_go() {
    for case in golden()["trailers"].as_array().unwrap() {
        let payload = case["payload"].as_str().unwrap();
        let got = parse_trailer_error(payload.as_bytes());
        let want_status = case["status"].as_u64().unwrap() as u16;
        match got {
            None => assert_eq!(want_status, 0, "payload {payload:?}"),
            Some(e) => {
                assert_eq!(e.status, want_status, "payload {payload:?}");
                assert_eq!(e.message, case["err"].as_str().unwrap(), "payload {payload:?}");
            }
        }
    }
}

#[test]
fn field15_turn_counter_is_per_session() {
    // Fresh sessions start at 0 (omitted), then count up; blank ids never count.
    let id = format!("turn-test-{}", uuid::Uuid::new_v4());
    assert_eq!([0, 1, 2], [next_session_turn_index(&id), next_session_turn_index(&id), next_session_turn_index(&id)]);
    reset_session_turn_index(&id);
    assert_eq!(next_session_turn_index(&id), 0);
    assert_eq!([next_session_turn_index("  "), next_session_turn_index("")], [0, 0]);
}

#[tokio::test]
async fn frame_reader_handles_split_chunks_gzip_and_eof() {
    let mut gz = Vec::new();
    {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
        enc.write_all(b"compressed payload").unwrap();
        enc.finish().unwrap();
    }
    let mut wire_bytes = wrap_connect_envelope(b"hello devin connect-rpc");
    wire_bytes.extend(wrap_connect_envelope_with_flag(CONNECT_FLAG_COMPRESSED, &gz));
    wire_bytes.extend(wrap_connect_envelope_with_flag(CONNECT_FLAG_END_STREAM, b"{}"));
    // One byte per chunk exercises every header/payload boundary.
    let chunks = wire_bytes.iter().map(|b| vec![*b]).collect();
    let mut reader = ConnectFrameReader::new(frame_stream(chunks));
    let f = reader.read_frame().await.unwrap();
    assert_eq!((f.flag, f.payload.as_slice()), (CONNECT_FLAG_DATA, &b"hello devin connect-rpc"[..]));
    let f = reader.read_frame().await.unwrap();
    assert_eq!((f.flag, f.payload.as_slice()), (CONNECT_FLAG_COMPRESSED, &b"compressed payload"[..]));
    let f = reader.read_frame().await.unwrap();
    assert_eq!((f.flag, f.payload.as_slice()), (CONNECT_FLAG_END_STREAM, &b"{}"[..]));
    assert_eq!(reader.read_frame().await, Err(FrameError::Eof));
}

#[tokio::test]
async fn frame_reader_error_kinds() {
    let read = |bytes: Vec<u8>| async move { ConnectFrameReader::new(frame_stream(vec![bytes])).read_frame().await };
    assert_eq!(read(vec![0, 0, 0]).await, Err(FrameError::UnexpectedEof));
    // Header complete but no payload byte at all reads as a clean EOF, like io.ReadFull.
    assert_eq!(read(vec![0, 0, 0, 0, 5]).await, Err(FrameError::Eof));
    assert_eq!(read(vec![0, 0, 0, 0, 5, b'a']).await, Err(FrameError::UnexpectedEof));
    assert!(matches!(read(vec![4, 0, 0, 0, 0]).await, Err(FrameError::Invalid(m)) if m.contains("0x04")));
    let too_big = {
        let mut v = vec![0];
        v.extend((16u32 * 1024 * 1024 + 1).to_be_bytes());
        v
    };
    assert!(matches!(read(too_big).await, Err(FrameError::Invalid(m)) if m.contains("exceeds maximum")));
    let bad_gzip = wrap_connect_envelope_with_flag(CONNECT_FLAG_COMPRESSED, b"nope");
    assert!(matches!(read(bad_gzip).await, Err(FrameError::Invalid(m)) if m.starts_with("decompress gzip")));
}

#[test]
fn utf8_split_buffer_reassembles_characters() {
    let mut buf = Utf8SplitBuffer::default();
    assert_eq!(buf.feed(&[0xe4, 0xbd]), "");
    assert_eq!(buf.feed(&[0xa0, 0xe5, 0xa5]), "\u{4f60}");
    assert_eq!(buf.feed(&[0xbd]), "\u{597d}");
    // Invalid bytes are replaced one by one; a trailing partial sequence waits for more input.
    assert_eq!(buf.feed(b"a\xffb\xe4"), "a\u{FFFD}b");
    assert_eq!(buf.feed(&[0xbd, 0xa0]), "\u{4f60}");
}

#[test]
fn parse_frame_decodes_text_thinking_and_signatures() {
    let mut payload = Vec::new();
    for (n, v) in [(1, "bot-uuid-123"), (3, "Hello "), (3, "world"), (9, "Let me think..."), (10, "CAQS-sig"), (21, "anthropic")] {
        payload.extend(str_field(n, v));
    }
    payload.extend(varint_field(5, 10));
    let res = parse_frame(&payload).unwrap();
    assert_eq!(res.output_id, "bot-uuid-123");
    assert_eq!(res.content_text, b"Hello world");
    assert_eq!(res.thinking_text, b"Let me think...");
    assert_eq!(res.delta_signature, b"CAQS-sig");
    assert_eq!(res.delta_signature_type, "anthropic");
    assert_eq!(res.stop_reason, 10);
    assert!(parse_frame(&[0x0a, 0xff]).is_err());
    // Wire type 3 (group) is rejected at the frame level.
    assert!(parse_frame(&[0x0b]).is_err());
}

#[test]
fn usage_field_collects_counters_headers_and_request_id() {
    let mut u = Vec::new();
    for (n, v) in [(2, 3), (4, 58), (3, 39), (5, 19179), (6, 66)] {
        u.extend(varint_field(n, v));
    }
    for (k, v) in [("openai-version", "2020-10-01"), ("Request-Id", "req_011C"), ("openai-processing-ms", "419")] {
        let mut h = str_field(1, k);
        h.extend(str_field(2, v));
        pb::put_bytes(&mut u, 8, &h);
    }
    u.extend(str_field(9, "gpt-5-6-luna-low"));
    let usage = parse_usage_field(&u);
    assert_eq!(
        (usage.prompt_tokens, usage.cache_write_tokens, usage.completion_tokens, usage.cached_tokens, usage.status_code),
        (3, 58, 39, 19179, 66)
    );
    assert_eq!(usage.request_id, "req_011C");
    assert_eq!(usage.model_name, "gpt-5-6-luna-low");
    assert_eq!(usage.headers["openai-processing-ms"], "419");
    // Prompt tokens accumulate across repeated field 2; an unnamed printable field 8 is a request id.
    let mut u2 = varint_field(2, 1);
    u2.extend(varint_field(2, 4));
    pb::put_bytes(&mut u2, 8, b"plain-id");
    let usage = parse_usage_field(&u2);
    assert_eq!((usage.prompt_tokens, usage.request_id.as_str()), (5, "plain-id"));
}

#[test]
fn dimension_groups_read_token_usage_group() {
    let metric = |key: &str, val: f32| {
        let mut dim = Vec::new();
        pb::put_tag(&mut dim, 2, pb::FIXED32);
        dim.extend(val.to_le_bytes());
        let mut m = Vec::new();
        pb::put_bytes(&mut m, 4, &dim);
        pb::put_str(&mut m, 5, key);
        m
    };
    let mut group = str_field(1, "Token Usage");
    for (k, v) in [("input_tokens", 575.0), ("output_tokens", 5.0), ("cached_input_tokens", 128.0)] {
        pb::put_bytes(&mut group, 2, &metric(k, v));
    }
    let mut envelope = Vec::new();
    pb::put_bytes(&mut envelope, 28, &group);
    assert_eq!(parse_response_dimension_groups(&[envelope]), (575, 5, 128, true));
    let unrelated = str_field(1, "Latency Metrics");
    assert_eq!(parse_response_dimension_groups(&[unrelated.clone(), group]), (575, 5, 128, true));
    assert!(!parse_response_dimension_groups(&[unrelated]).3);
}

#[test]
fn tool_call_delta_fields_decode() {
    let mut tc = Vec::new();
    for (n, v) in [(1, "call_999"), (2, "custom_bash"), (3, r#"{"cmd":"pwd"}"#), (4, "pwd && ls"), (5, "syntax error")] {
        tc.extend(str_field(n, v));
    }
    tc.extend(varint_field(6, 1));
    let mut frame = Vec::new();
    pb::put_bytes(&mut frame, 6, &tc);
    let res = parse_frame(&frame).unwrap();
    let d = &res.tool_call_deltas[0];
    assert_eq!((d.id.as_str(), d.name.as_str(), d.arguments.as_str()), ("call_999", "custom_bash", r#"{"cmd":"pwd"}"#));
    assert_eq!((d.invalid_json_str.as_str(), d.invalid_json_err.as_str()), ("pwd && ls", "syntax error"));
    assert!(d.is_custom_tool_call);
}

#[test]
fn identity_material_has_expected_shape() {
    let trace = generate_sentry_trace();
    let parts: Vec<&str> = trace.split('-').collect();
    assert_eq!((parts.len(), parts[0].len(), parts[1].len(), parts[2]), (3, 32, 16, "1"));
    assert_ne!(trace, generate_sentry_trace());
    assert_eq!(generate_device_fingerprint("seed-1"), generate_device_fingerprint("seed-1"));
    assert_eq!(generate_device_fingerprint("seed-1").len(), FINGERPRINT_HEX_LEN);
    assert_ne!(generate_device_fingerprint(""), generate_device_fingerprint(""));
    // Client metadata never carries field 28 and starts with the client name.
    let meta = build_client_metadata_bytes("tok", "seed", "linux");
    let (num, typ, n) = pb::get_tag(&meta).unwrap();
    let (first, _) = pb::get_bytes(&meta[n..]).unwrap();
    assert_eq!((num, typ, first), (1, pb::BYTES, DEFAULT_CLIENT_NAME.as_bytes()));
}

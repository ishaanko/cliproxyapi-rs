use std::sync::Arc;

use cpa_config::Config;
use cpa_runtime::executor::Options;
use serde_json::Value;
use tokio::sync::mpsc;

use super::*;
use crate::devin::test_support::{golden, normalize};
use crate::devin::wire::wrap_connect_envelope_with_flag;

/// The golden frames of a scenario as one Connect byte stream.
fn scenario_bytes(frames: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    for f in frames.as_array().unwrap() {
        if let Some(tail) = f.get("raw_tail").and_then(Value::as_str) {
            out.extend(hex::decode(tail).unwrap());
            continue;
        }
        let payload = hex::decode(f["hex"].as_str().unwrap()).unwrap();
        out.extend(wrap_connect_envelope_with_flag(
            f["flag"].as_u64().unwrap() as u8,
            &payload,
        ));
    }
    out
}

fn reader(
    bytes: Vec<u8>,
) -> ConnectFrameReader<impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Unpin> {
    ConnectFrameReader::new(futures_util::stream::iter(vec![Ok(Bytes::from(bytes))]))
}

/// Client requests of a scenario: its own override or the shared ones.
fn scenario_requests(case: &Value) -> &Value {
    case.get("requests").unwrap_or(&golden()["requests"])
}

fn render_err(err: &ExecError) -> String {
    if err.status == 0 {
        format!("ERR: {}", err.message)
    } else {
        format!("ERR: {} code={}", err.message, err.status)
    }
}

async fn run_stream(bytes: Vec<u8>, format: Format, request: &str) -> Vec<String> {
    let (tx, mut rx) = mpsc::channel(16384);
    let (usage_tx, _usage_rx) = oneshot::channel();
    let request = Bytes::from(request.to_string());
    let params = StreamParams {
        model: "devin/swe-2".into(),
        request: request.clone(),
        original: request.clone(),
        client_original: request,
        source_format: format,
        response_format: format,
        chat_model_uid: "swe-2-high".into(),
        reporter: UsageReporter::new("devin", "DevinExecutor", "swe-2", None, None),
        log: crate::helps::gemini_log::UpstreamLog::new(&Options::new(Format::Interactions), &Arc::new(Config::default())),
    };
    stream_frames(reader(bytes), params, tx, usage_tx).await;
    let mut chunks = Vec::new();
    while let Ok(item) = rx.try_recv() {
        chunks.push(match item {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => render_err(&e),
        });
    }
    chunks
}

#[tokio::test]
async fn streams_match_go_for_every_client_format() {
    for case in golden()["streams"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let requests = scenario_requests(case);
        let bytes = scenario_bytes(&case["frames"]);
        for fmt in case["formats"].as_array().unwrap() {
            let format_name = fmt["format"].as_str().unwrap();
            let format = Format::parse(format_name).unwrap();
            let got = run_stream(
                bytes.clone(),
                format,
                requests[format_name].as_str().unwrap(),
            )
            .await;
            let got: Vec<String> = got.iter().map(|c| normalize(c)).collect();
            let want: Vec<String> = fmt["chunks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| normalize(c.as_str().unwrap()))
                .collect();
            assert_eq!(got, want, "stream {name} as {format_name}");
        }
    }
}

#[tokio::test]
async fn consumed_responses_match_go() {
    for case in golden()["streams"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let requests = scenario_requests(case);
        let original = requests["interactions"].as_str().unwrap().as_bytes();
        let (result, _log) = consume_frames_to_interactions(
            reader(scenario_bytes(&case["frames"])),
            "devin/swe-2",
            original,
        )
        .await;
        let want_err = case["consume_err"].as_str().unwrap();
        match &result {
            Err(e) => {
                assert_eq!(normalize(&e.message), normalize(want_err), "stream {name}");
                assert_eq!(
                    i64::from(e.status),
                    case["consume_code"].as_i64().unwrap(),
                    "stream {name}"
                );
                continue;
            }
            Ok(c) => {
                assert_eq!(want_err, "", "stream {name}");
                let got = normalize(&cpa_json::to_string(&c.interactions));
                assert_eq!(
                    got,
                    normalize(case["interactions"].as_str().unwrap()),
                    "stream {name}"
                );
            }
        }
        // Non-stream responses translated to every client format.
        let interactions = cpa_json::to_vec(&result.unwrap().interactions);
        for fmt in case["formats"].as_array().unwrap() {
            let format_name = fmt["format"].as_str().unwrap();
            let want = fmt["non_stream"].as_str().unwrap();
            if want.is_empty() {
                continue;
            }
            let req = requests[format_name].as_str().unwrap().as_bytes();
            let mut param = cpa_translator::Param::default();
            let got = cpa_translator::translate_non_stream(
                &cpa_translator::Ctx::default(),
                Format::Interactions,
                Format::parse(format_name).unwrap(),
                "devin/swe-2",
                req,
                req,
                &interactions,
                &mut param,
            )
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
            assert_eq!(
                normalize(&got),
                normalize(want),
                "non-stream {name} as {format_name}"
            );
        }
    }
}

#[tokio::test]
async fn chunk_boundaries_do_not_change_stream_output() {
    // Byte-at-a-time delivery must produce the same events as one big chunk.
    let requests = &golden()["requests"];
    let case = golden()["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "pre_text_tool_post_text")
        .unwrap();
    let bytes = scenario_bytes(&case["frames"]);
    let split = ConnectFrameReader::new(futures_util::stream::iter(
        bytes
            .iter()
            .map(|b| Ok::<_, std::convert::Infallible>(Bytes::from(vec![*b])))
            .collect::<Vec<_>>(),
    ));
    let (tx, mut rx) = mpsc::channel(1024);
    let (usage_tx, _usage_rx) = oneshot::channel();
    let request = Bytes::from(requests["interactions"].as_str().unwrap().to_string());
    let params = StreamParams {
        model: "devin/swe-2".into(),
        request: request.clone(),
        original: request.clone(),
        client_original: request,
        source_format: Format::Interactions,
        response_format: Format::Interactions,
        chat_model_uid: "swe-2-high".into(),
        reporter: UsageReporter::new("devin", "DevinExecutor", "swe-2", None, None),
        log: crate::helps::gemini_log::UpstreamLog::new(&Options::new(Format::Interactions), &Arc::new(Config::default())),
    };
    stream_frames(split, params, tx, usage_tx).await;
    let mut got = Vec::new();
    while let Ok(Ok(b)) = rx.try_recv() {
        got.push(normalize(&String::from_utf8_lossy(&b)));
    }
    let want: Vec<String> = case["formats"][0]["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| normalize(c.as_str().unwrap()))
        .collect();
    assert_eq!(got, want);
}

#[tokio::test]
async fn completed_stream_reports_usage_before_closing() {
    let case = golden()["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "tool_call_single")
        .unwrap();
    let (tx, mut rx) = mpsc::channel(1024);
    let (usage_tx, usage_rx) = oneshot::channel();
    let request = Bytes::from_static(b"{}");
    let params = StreamParams {
        model: "devin/swe-2".into(),
        request: request.clone(),
        original: request.clone(),
        client_original: request,
        source_format: Format::Interactions,
        response_format: Format::Interactions,
        chat_model_uid: "swe-2-high".into(),
        reporter: UsageReporter::new("devin", "DevinExecutor", "swe-2", None, None),
        log: crate::helps::gemini_log::UpstreamLog::new(&Options::new(Format::Interactions), &Arc::new(Config::default())),
    };
    stream_frames(
        reader(scenario_bytes(&case["frames"])),
        params,
        tx,
        usage_tx,
    )
    .await;
    while rx.try_recv().is_ok() {}
    let usage = usage_rx.await.unwrap();
    assert_eq!(usage["input_tokens"], 120);
    assert_eq!(usage["output_tokens"], 50);
    assert_eq!(usage["cached_tokens"], 20);
    assert_eq!(usage["total_tokens"], 170);
}

#[tokio::test]
async fn dropping_the_receiver_stops_the_stream_task() {
    let case = golden()["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "many_tools_130")
        .unwrap();
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let (usage_tx, usage_rx) = oneshot::channel();
    let request = Bytes::from_static(b"{}");
    let params = StreamParams {
        model: "devin/swe-2".into(),
        request: request.clone(),
        original: request.clone(),
        client_original: request,
        source_format: Format::Interactions,
        response_format: Format::Interactions,
        chat_model_uid: "swe-2-high".into(),
        reporter: UsageReporter::new("devin", "DevinExecutor", "swe-2", None, None),
        log: crate::helps::gemini_log::UpstreamLog::new(&Options::new(Format::Interactions), &Arc::new(Config::default())),
    };
    // Returns promptly instead of blocking on a full channel, and sends no usage.
    stream_frames(
        reader(scenario_bytes(&case["frames"])),
        params,
        tx,
        usage_tx,
    )
    .await;
    assert!(usage_rx.await.is_err());
}

#[tokio::test]
async fn request_log_gets_the_event_stream_and_the_summary() {
    let case = golden()["streams"].as_array().unwrap().iter().find(|c| c["name"] == "tool_call_single").unwrap();
    let handle = cpa_runtime::apilog::ApiLogHandle::new(Arc::new(cpa_runtime::apilog::ApiLog::new()));
    let mut opts = Options::new(Format::Interactions);
    opts.api_log = handle.clone();
    let cfg = Arc::new(Config { request_log: true, ..Config::default() });
    let (tx, mut rx) = mpsc::channel(1024);
    let (usage_tx, _usage_rx) = oneshot::channel();
    let request = Bytes::from_static(b"{}");
    let params = StreamParams {
        model: "devin/swe-2".into(),
        request: request.clone(),
        original: request.clone(),
        client_original: request,
        source_format: Format::Interactions,
        response_format: Format::Interactions,
        chat_model_uid: "swe-2-high".into(),
        reporter: UsageReporter::new("devin", "DevinExecutor", "swe-2", None, None),
        log: crate::helps::gemini_log::UpstreamLog::new(&opts, &cfg),
    };
    stream_frames(reader(scenario_bytes(&case["frames"])), params, tx, usage_tx).await;
    while rx.try_recv().is_ok() {}
    let log = String::from_utf8(handle.get().expect("log").api_response()).unwrap();
    assert!(log.contains("=== INTERMEDIATE INTERACTIONS STREAM ===\n\n{\"event_type\":\"interaction.created\""), "{log}");
    assert_eq!(log.matches("=== INTERMEDIATE INTERACTIONS STREAM ===").count(), 1);
    assert!(log.contains("=== DEVIN UPSTREAM RESPONSE SUMMARY ===\n{\n  \"status\": \"completed\",\n  \"frames_count\": "), "{log}");
    assert!(log.contains("\"usage\": {\n    \"prompt_tokens\": 100,"), "{log}");
}

// ---------------------------------------------------------------- content before thinking closes
// Ported from devin_stream_content_test.go. A checkpoint runs when the executor asks for the next
// frame, i.e. after all output of the previous frame has been enqueued.

type Received = Vec<Result<String, String>>;

enum Step {
    Data(Vec<u8>),
    /// Runs with everything received so far, before the next frame is delivered.
    Check(Box<dyn FnMut(&Received)>),
}

fn drain(rx: &std::sync::Mutex<mpsc::Receiver<Result<Bytes, ExecError>>>, sink: &std::sync::Mutex<Received>) {
    let (Ok(mut rx), Ok(mut sink)) = (rx.lock(), sink.lock()) else { return };
    while let Ok(item) = rx.try_recv() {
        sink.push(match item {
            Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
            Err(e) => Err(render_err(&e)),
        });
    }
}

/// Streams `steps` through the Devin stream translator as `format`; returns everything sent to
/// the client.
async fn run_steps(steps: Vec<Step>, format: Format, request: &str) -> Received {
    use std::sync::{Arc, Mutex};
    let (tx, rx) = mpsc::channel(1024);
    let rx = Arc::new(Mutex::new(rx));
    let sink: Arc<Mutex<Received>> = Arc::default();
    let (usage_tx, _usage_rx) = oneshot::channel();
    let request = Bytes::from(request.to_string());
    let params = StreamParams {
        model: "devin/swe-2".into(),
        request: request.clone(),
        original: request.clone(),
        client_original: request,
        source_format: format,
        response_format: format,
        chat_model_uid: "swe-2-high".into(),
        reporter: UsageReporter::new("devin", "DevinExecutor", "swe-2", None, None),
        log: crate::helps::gemini_log::UpstreamLog::new(&Options::new(Format::Interactions), &Arc::new(Config::default())),
    };
    let source = futures_util::stream::unfold((steps.into_iter(), rx.clone(), sink.clone()), |(mut it, rx, sink)| async move {
        loop {
            match it.next()? {
                Step::Data(bytes) => return Some((Ok::<_, std::convert::Infallible>(Bytes::from(bytes)), (it, rx, sink))),
                Step::Check(mut check) => {
                    drain(&rx, &sink);
                    if let Ok(received) = sink.lock() {
                        check(&received);
                    }
                }
            }
        }
    });
    stream_frames(ConnectFrameReader::new(Box::pin(source)), params, tx, usage_tx).await;
    drain(&rx, &sink);
    sink.lock().map(|r| r.clone()).unwrap_or_default()
}

fn varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// One length-delimited protobuf field.
fn pb_field(number: u64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 4);
    varint((number << 3) | 2, &mut out);
    varint(value.len() as u64, &mut out);
    out.extend_from_slice(value);
    out
}

fn content_frame(field: u64, value: &[u8]) -> Vec<u8> {
    wrap_connect_envelope_with_flag(0, &pb_field(field, value))
}

fn end_stream(payload: &str) -> Vec<u8> {
    wrap_connect_envelope_with_flag(2, payload.as_bytes())
}

/// Parsed JSON events of the received `data:` lines.
fn parsed_events(received: &Received) -> Vec<Value> {
    let mut events = Vec::new();
    for chunk in received.iter().flatten() {
        for line in chunk.split('\n') {
            let data = line.trim().trim_start_matches("data: ").trim();
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                events.push(v);
            }
        }
    }
    events
}

fn str_at<'a>(v: &'a Value, path: &str) -> &'a str {
    path.split('.').try_fold(v, |cur, key| match cur {
        Value::Array(a) => key.parse::<usize>().ok().and_then(|i| a.get(i)),
        other => other.get(key),
    })
    .and_then(Value::as_str)
    .unwrap_or("")
}

fn client_request(format: Format) -> &'static str {
    if format == Format::OpenAIResponse {
        r#"{"model":"devin/swe-2","stream":true,"input":"hi"}"#
    } else {
        r#"{"model":"devin/swe-2","stream":true,"messages":[{"role":"user","content":"hi"}]}"#
    }
}

#[tokio::test]
async fn content_streams_before_thinking_closes() {
    use std::sync::{Arc, Mutex};
    for format in [Format::OpenAI, Format::OpenAIResponse] {
        let content_of = move |received: &Received| -> (String, Vec<String>) {
            let (mut thinking, mut content) = (String::new(), Vec::new());
            for event in parsed_events(received) {
                let text = if format == Format::OpenAI {
                    thinking.push_str(str_at(&event, "choices.0.delta.reasoning_content"));
                    str_at(&event, "choices.0.delta.content").to_string()
                } else {
                    match str_at(&event, "type") {
                        "response.reasoning_summary_text.delta" => {
                            thinking.push_str(str_at(&event, "delta"));
                            String::new()
                        }
                        "response.output_text.delta" => str_at(&event, "delta").to_string(),
                        _ => String::new(),
                    }
                };
                if !text.is_empty() {
                    content.push(text);
                }
            }
            (thinking, content)
        };
        let checkpoints = Arc::new(Mutex::new(0usize));
        let stages: [(&str, u64, &[u8], &[&str]); 4] = [
            ("thinking", 9, b"planning", &[]),
            ("incomplete UTF-8", 3, &"\u{7532}".as_bytes()[..1], &[]),
            ("first complete UTF-8 chunk", 3, &"\u{7532}".as_bytes()[1..], &["\u{7532}"]),
            ("second complete UTF-8 chunk", 3, "\u{4e59}".as_bytes(), &["\u{7532}", "\u{4e59}"]),
        ];
        let mut steps = Vec::new();
        for (name, field, text, want) in stages {
            steps.push(Step::Data(content_frame(field, text)));
            let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
            let counter = checkpoints.clone();
            steps.push(Step::Check(Box::new(move |received| {
                *counter.lock().unwrap() += 1;
                let (thinking, content) = content_of(received);
                assert_eq!(thinking, "planning", "{name}: thinking before EOS");
                assert_eq!(content, want, "{name}: content deltas before upstream EOS");
            })));
        }
        steps.push(Step::Data(end_stream("{}")));
        let received = run_steps(steps, format, client_request(format)).await;
        assert_eq!(*checkpoints.lock().unwrap(), 4, "{format:?}");
        let (_, content) = content_of(&received);
        assert_eq!(content, ["\u{7532}", "\u{4e59}"], "{format:?}: content without loss or duplication");
    }
}

#[tokio::test]
async fn content_late_signatures() {
    use std::sync::{Arc, Mutex};
    let payload: Vec<u8> = (0..(1 + 8 + 16 + 16 + 32) as u8).map(|i| match i { 0 => 0x80, 1..=8 => 0, _ => i }).collect();
    let signature = {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE.encode(&payload)
    };
    for format in [Format::OpenAI, Format::OpenAIResponse] {
        for split in [false, true] {
            for ending in ["eos", "trailer_error", "truncated", "read_error"] {
                let label = format!("{format:?} split={split} {ending}");
                let mut steps = vec![Step::Data(content_frame(9, b"planning"))];
                let tail_signature = if split {
                    steps.push(Step::Data(content_frame(10, &signature.as_bytes()[..20])));
                    &signature[20..]
                } else {
                    &signature[..]
                };
                steps.push(Step::Data(content_frame(3, "\u{7532}".as_bytes())));
                let reached = Arc::new(Mutex::new(false));
                let flag = reached.clone();
                let label_check = label.clone();
                steps.push(Step::Check(Box::new(move |received| {
                    *flag.lock().unwrap() = true;
                    assert!(received.iter().all(Result::is_ok), "{label_check}: unexpected pre-signature errors");
                    let mut text = String::new();
                    for event in parsed_events(received) {
                        if format == Format::OpenAI {
                            text.push_str(str_at(&event, "choices.0.delta.content"));
                        } else {
                            if str_at(&event, "type") == "response.output_text.delta" {
                                text.push_str(str_at(&event, "delta"));
                            }
                            assert!(
                                !(str_at(&event, "type") == "response.output_item.done" && str_at(&event, "item.type") == "reasoning"),
                                "{label_check}: reasoning item finalized before its late signature"
                            );
                        }
                    }
                    assert_eq!(text, "\u{7532}", "{label_check}: content before late signature and termination");
                })));
                steps.push(Step::Data(content_frame(10, tail_signature.as_bytes())));
                match ending {
                    "eos" => steps.push(Step::Data(end_stream("{}"))),
                    "trailer_error" => {
                        steps.push(Step::Data(end_stream(r#"{"error":{"code":"internal","message":"upstream failed"}}"#)))
                    }
                    // An incomplete frame header is an unexpected EOF.
                    "read_error" => steps.push(Step::Data(vec![0, 0])),
                    _ => {}
                }
                let received = run_steps(steps, format, "").await;
                assert!(*reached.lock().unwrap(), "{label}: pre-signature checkpoint not reached");
                let errs = received.iter().filter(|r| r.is_err()).count();
                let want_error = ending != "eos";
                assert_eq!(errs, usize::from(want_error), "{label}: stream errors {received:?}");

                let events = parsed_events(&received);
                let (mut text, mut failed, mut completed, mut reasoning_done, mut message_done) =
                    (String::new(), 0, 0, 0, 0);
                let (mut done_item, mut completed_reasoning) = (Value::Null, Value::Null);
                for event in &events {
                    if format == Format::OpenAI {
                        text.push_str(str_at(event, "choices.0.delta.content"));
                        if event.get("error").is_some() {
                            failed += 1;
                        }
                        continue;
                    }
                    match str_at(event, "type") {
                        "response.output_text.delta" => text.push_str(str_at(event, "delta")),
                        "response.output_item.done" => {
                            match str_at(event, "item.type") {
                                "message" => message_done += 1,
                                "reasoning" => {
                                    done_item = event["item"].clone();
                                    reasoning_done += 1;
                                }
                                _ => {}
                            }
                        }
                        "response.completed" => {
                            completed += 1;
                            completed_reasoning = event["response"]["output"][0].clone();
                        }
                        "response.failed" => failed += 1,
                        _ => {}
                    }
                }
                assert_eq!(text, "\u{7532}", "{label}: final content without duplication");
                assert_eq!(failed, usize::from(want_error), "{label}: failure event count");
                if format == Format::OpenAIResponse {
                    assert_eq!(reasoning_done, 1, "{label}: reasoning done count");
                    assert_eq!(str_at(&done_item, "encrypted_content"), signature, "{label}: full signature");
                    assert_eq!(message_done, 1, "{label}: message done count");
                    if want_error {
                        assert_eq!(str_at(events.last().unwrap(), "type"), "response.failed", "{label}: failure terminates the stream");
                        assert_eq!(completed, 0, "{label}: response.completed after upstream failure");
                    } else {
                        assert_eq!(completed, 1, "{label}");
                        assert_eq!(completed_reasoning, done_item, "{label}: completed reasoning matches done item");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn content_does_not_overtake_queued_tool() {
    for format in [Format::OpenAI, Format::OpenAIResponse] {
        let mut tool = Vec::new();
        for (i, value) in ["call_1", "bash", r#"{"cmd":"ls"}"#].iter().enumerate() {
            tool.extend(pb_field(i as u64 + 1, value.as_bytes()));
        }
        let steps = vec![
            Step::Data(content_frame(9, b"planning")),
            Step::Data(content_frame(6, &tool)),
            Step::Data(content_frame(3, "\u{7532}".as_bytes())),
            Step::Check(Box::new(move |received| {
                for event in parsed_events(received) {
                    let leaked = !str_at(&event, "choices.0.delta.content").is_empty()
                        || event.pointer("/choices/0/delta/tool_calls").is_some()
                        || str_at(&event, "type") == "response.output_text.delta"
                        || str_at(&event, "item.type") == "function_call";
                    assert!(!leaked, "{format:?}: queued tool or following text emitted before thinking signature: {event}");
                }
            })),
            Step::Data(content_frame(10, b"CAQS-late-tool-signature")),
            Step::Data(end_stream("{}")),
        ];
        let received = run_steps(steps, format, "").await;
        assert!(received.iter().all(Result::is_ok), "{format:?}: {received:?}");
        let (mut tool_seen, mut text_seen) = (false, false);
        for event in parsed_events(&received) {
            if str_at(&event, "choices.0.delta.tool_calls.0.id") == "call_1"
                || (str_at(&event, "type") == "response.output_item.done" && str_at(&event, "item.call_id") == "call_1")
            {
                tool_seen = true;
            }
            if str_at(&event, "choices.0.delta.content") == "\u{7532}"
                || (str_at(&event, "type") == "response.output_text.delta" && str_at(&event, "delta") == "\u{7532}")
            {
                text_seen = true;
                assert!(tool_seen, "{format:?}: content overtook queued tool");
            }
        }
        assert!(tool_seen && text_seen, "{format:?}: tool seen = {tool_seen}, text seen = {text_seen}");
    }
}

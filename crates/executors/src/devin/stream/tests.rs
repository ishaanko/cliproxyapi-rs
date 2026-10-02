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
    let requests = &golden()["requests"];
    for case in golden()["streams"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
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
    let requests = &golden()["requests"];
    for case in golden()["streams"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let result = consume_frames_to_interactions(
            reader(scenario_bytes(&case["frames"])),
            "devin/swe-2",
            b"",
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

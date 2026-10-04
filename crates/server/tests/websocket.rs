//! Client-facing Responses websocket against a real `Manager` with a scripted fake executor.

mod common;

use std::time::Duration;

use common::{Harness, Script, harness};
use cpa_runtime::executor::ExecError;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn serve_ws(h: &Harness) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = h.router.clone().into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("ws://{addr}/v1/responses")
}

async fn connect(url: &str) -> Socket {
    connect_with(url, &[]).await
}

async fn connect_with(url: &str, headers: &[(&'static str, &'static str)]) -> Socket {
    let mut req = url.into_client_request().expect("request");
    req.headers_mut().insert("x-api-key", "k1".parse().expect("header"));
    for (name, value) in headers {
        req.headers_mut().insert(*name, value.parse().expect("header"));
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await.expect("connect");
    ws
}

/// Reads text frames until `n` arrived or the socket ended.
async fn read_frames(ws: &mut Socket, n: usize) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    while out.len() < n {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(t)))) => out.push(serde_json::from_str(t.as_str()).expect("json frame")),
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    out
}

fn turn() -> Vec<common::Chunk> {
    [
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"m1\"}}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
    ]
    .into_iter()
    .map(Ok)
    .collect()
}

#[tokio::test]
async fn turns_stream_json_frames_and_append_merges_history() {
    let h = harness("ws", |_| {}).await;
    h.script("first", Script::Stream(turn()));
    h.script("second", Script::Stream(turn()));
    let mut ws = connect(&serve_ws(&h).await).await;

    let create = format!(
        r#"{{"type":"response.create","model":"{}","input":[{{"type":"message","role":"user","id":"u1","content":"first"}}]}}"#,
        h.model
    );
    ws.send(WsMessage::Text(create.into())).await.expect("send");
    let frames = read_frames(&mut ws, 3).await;
    let types: Vec<_> = frames.iter().map(|f| f["type"].as_str().unwrap_or("").to_string()).collect();
    assert_eq!(types, ["response.created", "response.output_item.done", "response.completed"]);
    // the empty completion output is rebuilt from the collected items
    assert_eq!(frames[2]["response"]["output"][0]["id"], "m1");

    let append = r#"{"type":"response.append","input":[{"type":"message","role":"user","id":"u2","content":"second"}]}"#;
    ws.send(WsMessage::Text(append.into())).await.expect("send");
    assert_eq!(read_frames(&mut ws, 3).await.len(), 3);
    // the second upstream request carried the full history: first input, first output, second input
    let seen = h.exec.seen.lock();
    let second: serde_json::Value = serde_json::from_str(&seen[1].1).expect("json");
    let ids: Vec<_> = second["input"]
        .as_array()
        .expect("input array")
        .iter()
        .map(|i| i["id"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(ids, ["u1", "m1", "u2"]);
    assert_eq!(second["model"], h.model.as_str());
    assert_eq!(second["stream"], true);
    assert!(second.get("type").is_none());
}

#[tokio::test]
async fn prewarm_is_answered_locally() {
    let h = harness("ws-prewarm", |_| {}).await;
    let mut ws = connect(&serve_ws(&h).await).await;
    let warm = format!(r#"{{"type":"response.create","model":"{}","generate":false,"input":[]}}"#, h.model);
    ws.send(WsMessage::Text(warm.into())).await.expect("send");
    let frames = read_frames(&mut ws, 2).await;
    assert_eq!(frames[0]["type"], "response.created");
    assert_eq!(frames[1]["type"], "response.completed");
    assert!(frames[0]["response"]["id"].as_str().unwrap_or("").starts_with("resp_prewarm_"));
    assert!(h.exec.seen.lock().is_empty(), "prewarm must not reach the upstream");
    // a follow-up naming another previous response is rejected and the socket stays open
    let bad = r#"{"type":"response.create","previous_response_id":"resp_other","input":[]}"#;
    ws.send(WsMessage::Text(bad.into())).await.expect("send");
    let frames = read_frames(&mut ws, 1).await;
    assert_eq!(frames[0]["type"], "error");
    assert_eq!(frames[0]["status"], 409);
    assert_eq!(frames[0]["error"]["code"], "previous_response_not_found");
}

#[tokio::test]
async fn validation_errors_keep_the_socket_open_and_upstream_faults_close_it() {
    let h = harness("ws-err", |_| {}).await;
    h.script(
        "rejected",
        Script::Fail(ExecError::new(400, r#"{"error":{"message":"bad input","type":"invalid_request_error"}}"#)),
    );
    let mut ws = connect(&serve_ws(&h).await).await;

    ws.send(WsMessage::Text(r#"{"type":"nope"}"#.into())).await.expect("send");
    let frames = read_frames(&mut ws, 1).await;
    assert_eq!(frames[0]["status"], 400);
    assert_eq!(frames[0]["error"]["message"], "unsupported websocket request type: nope");

    // a request-shape failure upstream is written as one error frame, then the socket closes
    let create = format!(r#"{{"type":"response.create","model":"{}","input":[{{"type":"message","content":"rejected"}}]}}"#, h.model);
    ws.send(WsMessage::Text(create.into())).await.expect("send");
    let frames = read_frames(&mut ws, 2).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0],
        serde_json::json!({"type":"error","status":400,"error":{"message":"bad input","type":"invalid_request_error"}})
    );
    let end = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
    assert!(matches!(end, Ok(None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))))));

    // a server-side failure closes the socket without any frame
    let h = harness("ws-err2", |_| {}).await;
    h.script("broken", Script::Fail(ExecError::new(500, "upstream down")));
    let mut ws = connect(&serve_ws(&h).await).await;
    let create = format!(r#"{{"type":"response.create","model":"{}","input":[{{"type":"message","content":"broken"}}]}}"#, h.model);
    ws.send(WsMessage::Text(create.into())).await.expect("send");
    assert!(read_frames(&mut ws, 1).await.is_empty());
}

/// Go `TestResponsesWebsocketPreparesCodexMultiAgentV2Tools` plus the orphan-delegation step: both
/// run on the normalized request before the tool-call cache, so an orphan `create_thread` output
/// that carries a `call_id` becomes a user message instead of being dropped, and the stored
/// transcript holds the rewritten input.
#[tokio::test]
async fn codex_tool_preparation_and_orphan_delegation_run_before_the_tool_cache() {
    let h = harness("ws-prep", |c| {
        c.client.codex.optimize_multi_agent_v2 = true;
        c.codex.orphan_delegation_compatibility = true;
    })
    .await;
    h.script("collaboration", Script::Stream(turn()));
    let headers = [("user-agent", "codex_cli_rs/0.144.1"), ("x-openai-subagent", "collab_spawn")];
    let mut ws = connect_with(&serve_ws(&h).await, &headers).await;

    let create = format!(
        r#"{{"type":"response.create","model":"{}","input":[{{"type":"function_call_output","call_id":"call_orphan","name":"create_thread","namespace":"codex_app","output":"collaboration task"}}],"tools":[{{"type":"namespace","name":"collaboration","tools":[{{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{{"properties":{{"message":{{"encrypted":true}}}}}}}}]}}]}}"#,
        h.model
    );
    ws.send(WsMessage::Text(create.into())).await.expect("send");
    assert_eq!(read_frames(&mut ws, 3).await.len(), 3);

    let sent: serde_json::Value = serde_json::from_str(&h.exec.seen.lock()[0].1).expect("json");
    assert_eq!(sent["input"][0]["type"], "message");
    assert_eq!(sent["input"][0]["content"][0]["text"], "Tool output from codex_app__create_thread:\ncollaboration task");
    assert_eq!(sent["tools"][0]["name"], "collaboration");
    assert!(sent["tools"][0]["tools"][0]["parameters"]["properties"]["message"].get("encrypted").is_none(), "{sent}");
    assert_eq!(*h.exec.prepared.lock(), [true]);
}

//! HTTP-level behavior of the proxy handlers against a real `Manager` with a scripted fake
//! executor. Expected bytes come from the Go server (observed with a mock upstream).
//!
//! These need the conductor port: on bases where `Manager` is still a stub they are ignored.
//! Run them with `cargo test -p cpa-server --test pipeline -- --include-ignored`.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use common::{Script, harness};
use cpa_runtime::executor::ExecError;
use tower::ServiceExt;

const DONE: &str = "data: [DONE]\n\n";

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn chat_non_stream_is_passed_through_with_json_headers() {
    let h = harness("chat-ns", |_| {}).await;
    let (status, headers, body) = h.call("POST", "/v1/chat/completions", &[], &h.chat("hi")).await;
    assert_eq!(status, 200);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert!(body.contains(r#""content":"Hello""#));
    // the executor saw the resolved model and the untouched request body
    let seen = h.exec.seen.lock();
    assert_eq!(seen[0].0, h.model);
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn chat_stream_frames_chunks_and_ends_with_done() {
    let h = harness("chat-s", |_| {}).await;
    h.script("hello", Script::Stream(vec![Ok(r#"{"id":"1","n":1}"#), Ok(r#"{"id":"1","n":2}"#)]));
    let (status, headers, body) = h.call("POST", "/v1/chat/completions", &[], &h.chat_stream("hello")).await;
    assert_eq!(status, 200);
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    assert_eq!(headers[header::CONNECTION], "keep-alive");
    assert_eq!(body, format!("data: {{\"id\":\"1\",\"n\":1}}\n\ndata: {{\"id\":\"1\",\"n\":2}}\n\n{DONE}"));
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn chat_stream_error_after_first_chunk_is_a_data_frame_without_done() {
    let h = harness("chat-se", |_| {}).await;
    h.script("boom", Script::Stream(vec![Ok(r#"{"n":1}"#), Err(ExecError::new(502, "upstream broke"))]));
    let (status, _, body) = h.call("POST", "/v1/chat/completions", &[], &h.chat_stream("boom")).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        "data: {\"n\":1}\n\ndata: {\"error\":{\"message\":\"upstream broke\",\"type\":\"server_error\",\"code\":\"internal_server_error\"}}\n\n"
    );
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn upstream_errors_keep_their_status_and_shape() {
    let h = harness("chat-err", |_| {}).await;
    h.script("err400", Script::Fail(ExecError::new(400, r#"{"error": {"message": "bad thing", "type": "invalid_request_error", "code": "bad"}}"#)));
    h.script("errtext", Script::Fail(ExecError::new(500, "plain text failure")));
    let (status, headers, body) = h.call("POST", "/v1/chat/completions", &[], &h.chat("err400")).await;
    assert_eq!(status, 400);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(body, r#"{"error": {"message": "bad thing", "type": "invalid_request_error", "code": "bad"}}"#);
    // a stream that fails before any chunk answers with a normal JSON error status
    let (status, headers, body) = h.call("POST", "/v1/chat/completions", &[], &h.chat_stream("err400")).await;
    assert_eq!(status, 400);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert!(body.contains("bad thing"));
    // a 5xx cools the credential down (like Go), so it goes last
    let (status, _, body) = h.call("POST", "/v1/chat/completions", &[], &h.chat("errtext")).await;
    assert_eq!(status, 500);
    assert_eq!(body, r#"{"error":{"message":"plain text failure","type":"server_error","code":"internal_server_error"}}"#);
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn unknown_model_and_api_key_errors() {
    let h = harness("access", |_| {}).await;
    let (status, _, body) = h
        .call("POST", "/v1/chat/completions", &[], r#"{"model":"no-such-model","messages":[]}"#)
        .await;
    assert_eq!(status, 400);
    assert_eq!(
        body,
        r#"{"error":{"message":"unknown provider for model no-such-model","type":"invalid_request_error","code":"model_not_found","param":"model"}}"#
    );
    // Claude dialect: Anthropic error shape
    let (status, _, body) = h.call("POST", "/v1/messages", &[], r#"{"model":"no-such-model","messages":[]}"#).await;
    assert_eq!(status, 400);
    assert_eq!(body, r#"{"type":"error","error":{"type":"invalid_request_error","message":"unknown provider for model no-such-model"}}"#);

    // credentials: missing / invalid / open
    let resp = h.router.clone().oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), 401);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], br#"{"error":"Missing API key"}"#);
    let resp = h
        .router
        .clone()
        .oneshot(Request::builder().uri("/v1/models").header("authorization", "Bearer nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let open = harness("access-open", |c| c.api_keys.clear()).await;
    let resp = open.router.clone().oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn legacy_completions_convert_request_and_response() {
    let h = harness("compl", |_| {}).await;
    h.script("Complete this:", Script::Body(r#"{"id":"c1","created":5,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}]}"#));
    let (status, _, body) = h
        .call("POST", "/v1/completions", &[], &format!(r#"{{"model":"{}"}}"#, h.model))
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        r#"{"id":"c1","object":"text_completion","created":5,"model":"m","choices":[{"finish_reason":"stop","index":0,"text":"Hello"}]}"#
    );
    assert!(h.exec.seen.lock()[0].1.contains(r#""content":"Complete this:""#));
    // stream: empty deltas are dropped, then DONE
    h.script("streamme", Script::Stream(vec![
        Ok(r#"{"id":"c","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#),
        Ok(r#"{"id":"c","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"x"},"finish_reason":null}]}"#),
    ]));
    let (_, _, body) = h
        .call("POST", "/v1/completions", &[], &format!(r#"{{"model":"{}","prompt":"streamme","stream":true}}"#, h.model))
        .await;
    assert_eq!(
        body,
        format!("data: {{\"id\":\"c\",\"object\":\"text_completion\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"finish_reason\":\"\",\"index\":0,\"text\":\"x\"}}]}}\n\n{DONE}")
    );
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn claude_messages_stream_is_raw_and_errors_use_event_error() {
    let h = harness("claude", |_| {}).await;
    h.script("M-OK", Script::Stream(vec![Ok("event: message_start\ndata: {\"type\":\"message_start\"}\n\n"), Ok("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")]));
    h.script("M-BAD", Script::Stream(vec![Ok("event: message_start\ndata: {}\n\n"), Err(ExecError::new(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#))]));
    let body = |m: &str| format!(r#"{{"model":"{}","stream":true,"max_tokens":1,"messages":[{{"role":"user","content":"{m}"}}]}}"#, h.model);
    let (status, headers, out) = h.call("POST", "/v1/messages", &[], &body("M-OK")).await;
    assert_eq!(status, 200);
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(out, "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    let (_, _, out) = h.call("POST", "/v1/messages", &[], &body("M-BAD")).await;
    assert_eq!(
        out,
        "event: message_start\ndata: {}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n"
    );
    // non-stream error keeps the Anthropic shape and status (fresh credential: the 529 above
    // cooled the first one down)
    let h = harness("claude-ns", |_| {}).await;
    h.script("limited", Script::Fail(ExecError::new(429, "slow down")));
    let (status, _, out) = h
        .call("POST", "/v1/messages", &[], &format!(r#"{{"model":"{}","messages":[{{"role":"user","content":"limited"}}]}}"#, h.model))
        .await;
    assert_eq!(status, 429);
    assert_eq!(out, r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#);
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn claude_cloaked_model_ids_are_decoded_before_routing() {
    let h = harness("claude-dd", |_| {}).await;
    let cloaked = format!("claude-fable-5-dd-{}", h.model.chars().rev().collect::<String>());
    let (status, _, _) = h
        .call("POST", "/v1/messages", &[], &format!(r#"{{"model":"{cloaked}","messages":[{{"role":"user","content":"x"}}]}}"#))
        .await;
    assert_eq!(status, 200);
    assert_eq!(h.exec.seen.lock()[0].0, h.model);
    // count_tokens uses the same decoding
    let (status, _, _) = h
        .call("POST", "/v1/messages/count_tokens", &[], &format!(r#"{{"model":"{cloaked}","messages":[]}}"#))
        .await;
    assert_eq!(status, 200);
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn gemini_actions_and_alt_framing() {
    let h = harness("gemini", |_| {}).await;
    h.script("stream", Script::Stream(vec![Ok(r#"{"candidates":[1]}"#), Ok(r#"{"candidates":[2]}"#)]));
    let path = |action: &str| format!("/v1beta/models/{}:{action}", h.model);
    let body = r#"{"contents":[{"parts":[{"text":"stream"}]}]}"#;
    let (status, headers, out) = h.call("POST", &path("streamGenerateContent"), &[], body).await;
    assert_eq!(status, 200);
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(out, "data: {\"candidates\":[1]}\n\ndata: {\"candidates\":[2]}\n\n");
    // alt=json: raw chunks, no SSE headers
    let (_, headers, out) = h.call("POST", &format!("{}?alt=json", path("streamGenerateContent")), &[], body).await;
    assert!(headers.get(header::CONTENT_TYPE).is_none());
    assert_eq!(out, "{\"candidates\":[1]}{\"candidates\":[2]}");
    // alt=sse behaves like no alt
    let (_, _, out) = h.call("POST", &format!("{}?alt=sse", path("streamGenerateContent")), &[], body).await;
    assert!(out.starts_with("data: "));
    // non-stream + count + unknown method + malformed action
    let (status, headers, _) = h.call("POST", &path("generateContent"), &[], r#"{"contents":[]}"#).await;
    assert_eq!((status.as_u16(), headers[header::CONTENT_TYPE].to_str().unwrap()), (200, "application/json"));
    let (status, _, _) = h.call("POST", &path("countTokens"), &[], r#"{"contents":[]}"#).await;
    assert_eq!(status, 200);
    let (status, _, out) = h.call("POST", &path("other"), &[], "{}").await;
    assert_eq!((status.as_u16(), out.as_str()), (200, ""));
    let (status, _, out) = h.call("POST", "/v1beta/models/justamodel", &[], "{}").await;
    assert_eq!(status, 404);
    assert_eq!(out, r#"{"error":{"message":"/v1beta/models/justamodel not found.","type":"invalid_request_error"}}"#);
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn responses_stream_is_reframed_and_unterminated_streams_get_an_error_event() {
    let h = harness("resp", |_| {}).await;
    h.script("complete", Script::Stream(vec![
        Ok("data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n"),
        Ok("event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"m\"}}\n\n"),
        Ok("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"),
    ]));
    h.script("cutoff", Script::Stream(vec![Ok("data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n")]));
    let body = |m: &str| format!(r#"{{"model":"{}","stream":true,"input":"{m}"}}"#, h.model);
    let (status, headers, out) = h.call("POST", "/v1/responses", &[], &body("complete")).await;
    assert_eq!(status, 200);
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
    assert!(out.contains("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"id\":\"m\"}]}}\n\n"), "{out}");
    // clean close after a terminal event ends with a lone newline, no [DONE]
    assert!(out.ends_with("}\n\n\n"), "{out}");
    assert!(!out.contains("[DONE]"));

    let (_, _, out) = h.call("POST", "/v1/responses", &[], &body("cutoff")).await;
    assert!(out.starts_with("data: {\"type\":\"response.created\""));
    assert!(out.contains("\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"internal_server_error\",\"message\":\"upstream stream closed before a terminal event (last event: response.created)\""), "{out}");

    // Codex clients get response.failed
    let (_, _, out) = h.call("POST", "/v1/responses", &[("user-agent", "codex_cli_rs/0.1")], &body("cutoff")).await;
    assert!(out.contains("\nevent: response.failed\ndata: {\"type\":\"response.failed\""), "{out}");
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn responses_compact_and_validation() {
    let h = harness("compact", |_| {}).await;
    let (status, _, out) = h.call("POST", "/v1/responses/compact", &[], &format!(r#"{{"model":"{}","stream":true}}"#, h.model)).await;
    assert_eq!(status, 400);
    assert_eq!(out, r#"{"error":{"message":"Streaming not supported for compact responses","type":"invalid_request_error"}}"#);
    let (status, _, _) = h.call("POST", "/v1/responses/compact", &[], &format!(r#"{{"model":"{}","stream":false,"input":"x"}}"#, h.model)).await;
    assert_eq!(status, 200);
    // `stream:false` is removed before execution
    assert!(!h.exec.seen.lock()[0].1.contains("stream"));
}

#[tokio::test(start_paused = true)]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn slow_non_stream_requests_get_leading_newlines() {
    let h = harness("keepalive", |c| c.nonstream_keepalive_interval = 1).await;
    h.script("slow", Script::Slow(Duration::from_millis(2500), r#"{"ok":true}"#));
    let (status, headers, out) = h.call("POST", "/v1/chat/completions", &[], &h.chat("slow")).await;
    assert_eq!(status, 200);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(out, "\n\n{\"ok\":true}");
    // a failure after the first newline can no longer change the status line
    h.script("slowfail", Script::Fail(ExecError::new(500, "late failure")));
    let _ = &h.cfg_tx;
}

#[tokio::test]
#[ignore = "needs the conductor port (Manager is a stub on this base)"]
async fn zstd_request_bodies_are_decoded_for_openai_endpoints() {
    let h = harness("zstd", |_| {}).await;
    let payload = h.chat("zstd-me");
    let compressed = zstd::stream::encode_all(payload.as_bytes(), 1).unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("x-api-key", "k1")
        .header("content-encoding", "zstd")
        .body(Body::from(compressed))
        .unwrap();
    let resp = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(h.exec.seen.lock()[0].1.contains("zstd-me"));
}

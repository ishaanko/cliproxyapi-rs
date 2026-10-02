//! Executor behavior against a mock upstream: request building (URLs, headers, bodies), response
//! translation, usage hand-off and error classification. Mirrors the pinned behavior of the Go
//! executor tests.

use bytes::Bytes;
use cpa_json::J;
use cpa_runtime::executor::{Executor, Options, Request};
use cpa_translator::Format;

use super::test_support::{Reply, config_rx, json_reply, key_auth, mock_upstream, sse_reply};
use super::{AiStudioExecutor, GeminiExecutor, GeminiVertexExecutor};

const OK_BODY: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#;

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from(payload.to_string()),
        format: Format::Gemini,
        metadata: Default::default(),
    }
}

fn gemini_opts() -> Options {
    Options::new(Format::Gemini)
}

#[tokio::test]
async fn gemini_generate_request_shape_and_usage() {
    let (base, mut seen) = mock_upstream(vec![json_reply(OK_BODY)]).await;
    let exec = GeminiExecutor::new(config_rx());
    let payload = r#"{"contents":[{"role":"model","parts":[{"text":"prior"}]},{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"text":"a"}]}],"generationConfig":{"maxOutputTokens":500000,"temperature":0.2},"session_id":"s"}"#;
    let resp = exec
        .execute(&key_auth("gemini", &format!("{base}/")), request("gemini-3.1-pro-preview", payload), gemini_opts())
        .await
        .expect("execute");

    let upstream = seen.recv().await.unwrap();
    assert_eq!(upstream.path(), "/v1beta/models/gemini-3.1-pro-preview:generateContent");
    assert_eq!(upstream.query(), "");
    assert_eq!(upstream.headers["x-goog-api-key"], "test-key");
    assert_eq!(upstream.headers["content-type"], "application/json");
    assert!(upstream.headers["user-agent"].starts_with("Go-http-client"));
    assert!(!upstream.headers.contains_key("authorization"));
    let body = upstream.json();
    assert_eq!(body.g("model").str(), "gemini-3.1-pro-preview");
    assert_eq!(body.g("generationConfig.maxOutputTokens").int(), 65536);
    assert_eq!(body.g("generationConfig.temperature").float(), 0.2);
    assert!(!body.g("session_id").exists());
    let roles: Vec<String> = body.g("contents").array().iter().map(|c| c.g("role").str()).collect();
    assert_eq!(roles, ["user", "model", "user", "model", "user"]);
    assert_eq!(body.g("contents.0.parts.0.text").str(), "");
    assert_eq!(body.g("contents.4.parts.0.text").str(), "");

    assert_eq!(cpa_json::parse(&resp.payload).g("candidates.0.content.parts.0.text").str(), "ok");
    let usage = &resp.metadata["usage"];
    assert_eq!(
        (usage["input_tokens"].as_i64(), usage["output_tokens"].as_i64(), usage["total_tokens"].as_i64()),
        (Some(1), Some(1), Some(2))
    );
}

#[tokio::test]
async fn gemini_count_tokens_strips_generation_fields_and_keeps_trailing_model_turn() {
    let (base, mut seen) = mock_upstream(vec![json_reply(r#"{"totalTokens":7}"#)]).await;
    let exec = GeminiExecutor::new(config_rx());
    let payload = r#"{"contents":[{"role":"model","parts":[{"text":"prior output"}]}],"tools":[{"functionDeclarations":[]}],"generationConfig":{"temperature":1}}"#;
    let resp = exec
        .count_tokens(&key_auth("gemini", &base), request("gemini-3.7-flash", payload), gemini_opts())
        .await
        .expect("count");
    let upstream = seen.recv().await.unwrap();
    assert_eq!(upstream.path(), "/v1beta/models/gemini-3.7-flash:countTokens");
    let body = upstream.json();
    assert!(!body.g("tools").exists() && !body.g("generationConfig").exists() && !body.g("safetySettings").exists());
    let roles: Vec<String> = body.g("contents").array().iter().map(|c| c.g("role").str()).collect();
    assert_eq!(roles, ["user", "model"]);
    assert_eq!(cpa_json::parse(&resp.payload).g("totalTokens").int(), 7);

    // The countTokens action through Execute keeps the trailing model turn as well.
    let mut req = request("gemini-3.7-flash", payload);
    req.metadata.insert("action".into(), "countTokens".into());
    let (base2, mut seen2) = mock_upstream(vec![json_reply(r#"{"totalTokens":7}"#)]).await;
    exec.execute(&key_auth("gemini", &base2), req, gemini_opts()).await.expect("execute count");
    let upstream = seen2.recv().await.unwrap();
    assert_eq!(upstream.path(), "/v1beta/models/gemini-3.7-flash:countTokens");
    assert_eq!(upstream.json().g("contents.#").int(), 2);
}

#[tokio::test]
async fn gemini_alt_hint_and_custom_headers() {
    let (base, mut seen) = mock_upstream(vec![json_reply(OK_BODY)]).await;
    let exec = GeminiExecutor::new(config_rx());
    let mut auth = key_auth("gemini", &base);
    auth.attributes.insert("header:X-Team".into(), "blue".into());
    auth.attributes.insert("header:X-Echo".into(), "$X-Client".into());
    let mut opts = gemini_opts();
    opts.alt = "json".into();
    opts.headers.insert("x-client", "hello".parse().unwrap());
    exec.execute(&auth, request("gemini-2.5-flash", r#"{"contents":[{"role":"user","parts":[{"text":"q"}]}]}"#), opts)
        .await
        .expect("execute");
    let upstream = seen.recv().await.unwrap();
    assert_eq!(upstream.query(), "$alt=json");
    assert_eq!(upstream.headers["x-team"], "blue");
    assert_eq!(upstream.headers["x-echo"], "hello");
}

#[tokio::test]
async fn gemini_stream_uses_alt_sse_and_reports_usage() {
    let sse = format!(
        "data: {}\n\ndata: {}\n\n",
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hel"}]}}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":1,"totalTokenCount":5}}"#,
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"lo"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":3,"totalTokenCount":7}}"#,
    );
    let (base, mut seen) = mock_upstream(vec![sse_reply(&sse)]).await;
    let exec = GeminiExecutor::new(config_rx());
    let mut stream = exec
        .execute_stream(
            &key_auth("gemini", &base),
            request("gemini-2.5-flash", r#"{"contents":[{"role":"user","parts":[{"text":"q"}]}]}"#),
            Options { stream: true, ..gemini_opts() },
        )
        .await
        .expect("stream");
    let upstream = seen.recv().await.unwrap();
    assert_eq!(upstream.path(), "/v1beta/models/gemini-2.5-flash:streamGenerateContent");
    assert_eq!(upstream.query(), "alt=sse");

    let mut text = String::new();
    while let Some(chunk) = stream.chunks.recv().await {
        text.push_str(&String::from_utf8_lossy(&chunk.expect("chunk")));
    }
    assert!(text.contains("\"Hel\"") && text.contains("\"lo\""), "{text}");
    let usage = stream.usage.take().expect("usage rx").await.expect("usage");
    assert_eq!((usage["input_tokens"].as_i64(), usage["total_tokens"].as_i64()), (Some(4), Some(7)));
}

#[tokio::test]
async fn upstream_errors_are_plain_status_errors_and_compact_is_rejected() {
    let reply = Reply { status: 429, content_type: "application/json", body: r#"{"error":{"message":"slow"}}"#.into() };
    let (base, _seen) = mock_upstream(vec![reply]).await;
    let exec = GeminiExecutor::new(config_rx());
    let err = exec
        .execute(
            &key_auth("gemini", &base),
            request("gemini-2.5-flash", r#"{"contents":[{"role":"user","parts":[{"text":"q"}]}]}"#),
            gemini_opts(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.status, 429);
    assert_eq!(err.message, r#"{"error":{"message":"slow"}}"#);
    assert!(err.retry_after.is_none() && err.upstream_attempted);

    let mut opts = gemini_opts();
    opts.alt = "responses/compact".into();
    let err = exec.execute(&key_auth("gemini", &base), request("gemini-2.5-flash", "{}"), opts).await.unwrap_err();
    assert_eq!(
        (err.status, err.message.as_str(), err.upstream_attempted),
        (501, "/responses/compact not supported", false)
    );
}

#[tokio::test]
async fn interactions_through_a_gemini_key_translate_to_generate_content() {
    let (base, mut seen) = mock_upstream(vec![json_reply(OK_BODY)]).await;
    let exec = GeminiExecutor::new(config_rx());
    let mut opts = Options::new(Format::Interactions);
    opts.response_format = Some(Format::Interactions);
    let req = Request {
        model: "gemini-3.5-flash".into(),
        payload: Bytes::from_static(br#"{"model":"gemini-3.5-flash","input":"hi"}"#),
        format: Format::Interactions,
        metadata: Default::default(),
    };
    exec.execute(&key_auth("gemini", &base), req, opts).await.expect("execute");
    let upstream = seen.recv().await.unwrap();
    assert_eq!(upstream.path(), "/v1beta/models/gemini-3.5-flash:generateContent");
    assert!(!upstream.headers.contains_key("api-revision"));
    assert!(upstream.json().g("contents.0.parts.0.text").exists());
    assert!(!upstream.json().g("input").exists());
}

#[tokio::test]
async fn native_interactions_endpoint_revision_and_input_ids() {
    let reply = r#"{"id":"interaction_1","object":"interaction","status":"completed","steps":[],"usage":{"total_input_tokens":1,"total_output_tokens":1,"total_tokens":2}}"#;
    let (base, mut seen) = mock_upstream(vec![json_reply(reply)]).await;
    let exec = GeminiExecutor::new_interactions(config_rx());
    let auth = key_auth("gemini-interactions", &base);
    let mut opts = Options::new(Format::Interactions);
    opts.response_format = Some(Format::Interactions);
    let payload = r#"{"agent":"agents/test-agent","input":[{"type":"function_call","call_id":"c1","name":"f"},{"type":"user_input","id":"u","content":[{"type":"text","id":"p","text":"hi"}]}]}"#;
    let req = Request {
        model: "agents/test-agent".into(),
        payload: Bytes::from(payload.to_string()),
        format: Format::Interactions,
        metadata: Default::default(),
    };
    let resp = exec.execute(&auth, req.clone(), opts.clone()).await.expect("execute");
    let upstream = seen.recv().await.unwrap();
    assert_eq!(upstream.path(), "/v1beta/interactions");
    assert_eq!(upstream.headers["api-revision"], "2026-05-20");
    assert_eq!(upstream.headers["x-goog-api-key"], "test-key");
    let body = upstream.json();
    assert!(!body.g("model").exists());
    assert_eq!(body.g("input.0.id").str(), "c1");
    assert!(!body.g("input.0.call_id").exists());
    assert!(!body.g("input.1.id").exists() && !body.g("input.1.content.0.id").exists());
    assert_eq!(cpa_json::parse(&resp.payload).g("id").str(), "interaction_1");
    assert_eq!(resp.metadata["usage"]["total_tokens"].as_i64(), Some(2));

    // The client's Api-Revision is used unless the credential pins its own header.
    let (base, mut seen) = mock_upstream(vec![json_reply(reply)]).await;
    let mut with_client = opts.clone();
    with_client.headers.insert("api-revision", "2026-06-01".parse().unwrap());
    exec.execute(&key_auth("gemini-interactions", &base), req.clone(), with_client.clone()).await.expect("execute");
    assert_eq!(seen.recv().await.unwrap().headers["api-revision"], "2026-06-01");
    let (base, mut seen) = mock_upstream(vec![json_reply(reply)]).await;
    let mut pinned = key_auth("gemini-interactions", &base);
    pinned.attributes.insert("header:Api-Revision".into(), "2026-07-01".into());
    exec.execute(&pinned, req, with_client).await.expect("execute");
    assert_eq!(seen.recv().await.unwrap().headers["api-revision"], "2026-07-01");
}

#[tokio::test]
async fn native_interactions_stream_forwards_frames_and_parses_usage() {
    let sse = "event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"i1\"}}\n\nevent: interaction.completed\ndata: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"status\":\"completed\",\"usage\":{\"total_input_tokens\":2,\"total_output_tokens\":3,\"total_tokens\":5}}}\n\n";
    let (base, mut seen) = mock_upstream(vec![sse_reply(sse)]).await;
    let exec = GeminiExecutor::new_interactions(config_rx());
    let mut opts = Options::new(Format::Interactions);
    opts.stream = true;
    opts.response_format = Some(Format::Interactions);
    let req = Request {
        model: "agents/a".into(),
        payload: Bytes::from_static(br#"{"agent":"agents/a","input":"hi"}"#),
        format: Format::Interactions,
        metadata: Default::default(),
    };
    let mut stream = exec.execute_stream(&key_auth("gemini-interactions", &base), req, opts).await.expect("stream");
    assert_eq!(seen.recv().await.unwrap().json().g("stream").v(), Some(&cpa_json::Value::Bool(true)));
    let mut frames = Vec::new();
    while let Some(chunk) = stream.chunks.recv().await {
        frames.push(String::from_utf8(chunk.expect("chunk").to_vec()).unwrap());
    }
    assert_eq!(frames.len(), 2);
    assert!(frames[0].starts_with("event: interaction.created\ndata: ") && frames[0].ends_with("\n\n"));
    let usage = stream.usage.take().unwrap().await.expect("usage");
    assert_eq!(
        (usage["input_tokens"].as_i64(), usage["output_tokens"].as_i64(), usage["total_tokens"].as_i64()),
        (Some(2), Some(3), Some(5))
    );
}

#[tokio::test]
async fn vertex_api_key_requests_use_project_less_paths() {
    let (base, mut seen) = mock_upstream(vec![json_reply(OK_BODY)]).await;
    let exec = GeminiVertexExecutor::new(config_rx());
    let payload =
        r#"{"contents":[{"role":"user","parts":[{"text":"q"}]}],"generationConfig":{"maxOutputTokens":500000}}"#;
    exec.execute(&key_auth("vertex", &base), request("gemini-3.1-pro-preview", payload), gemini_opts())
        .await
        .expect("execute");
    let upstream = seen.recv().await.unwrap();
    assert_eq!(upstream.path(), "/v1/publishers/google/models/gemini-3.1-pro-preview:generateContent");
    assert_eq!(upstream.headers["x-goog-api-key"], "test-key");
    assert!(!upstream.headers.contains_key("authorization"));
    // Unlike the Gemini API executor, Vertex does not clamp maxOutputTokens.
    assert_eq!(upstream.json().g("generationConfig.maxOutputTokens").int(), 500000);

    let (base, mut seen) = mock_upstream(vec![json_reply(r#"{"totalTokens":3}"#)]).await;
    let resp = exec
        .count_tokens(&key_auth("vertex", &base), request("gemini-2.5-flash", payload), gemini_opts())
        .await
        .expect("count");
    let upstream = seen.recv().await.unwrap();
    assert_eq!(upstream.path(), "/v1/publishers/google/models/gemini-2.5-flash:countTokens");
    assert_eq!(cpa_json::parse(&resp.payload).g("totalTokens").int(), 3);

    // Stream: `?alt=sse`, raw lines go to the translator.
    let (base, mut seen) = mock_upstream(vec![sse_reply(&format!("data: {OK_BODY}\n\n"))]).await;
    let mut stream = exec
        .execute_stream(
            &key_auth("vertex", &base),
            request("gemini-2.5-flash", payload),
            Options { stream: true, ..gemini_opts() },
        )
        .await
        .expect("stream");
    let upstream = seen.recv().await.unwrap();
    assert_eq!(
        (upstream.path(), upstream.query()),
        ("/v1/publishers/google/models/gemini-2.5-flash:streamGenerateContent", "alt=sse")
    );
    let mut seen_text = false;
    while let Some(chunk) = stream.chunks.recv().await {
        seen_text |= String::from_utf8_lossy(&chunk.expect("chunk")).contains("\"ok\"");
    }
    assert!(seen_text);
}

#[tokio::test]
async fn vertex_strips_openai_responses_call_ids() {
    let (base, mut seen) = mock_upstream(vec![json_reply(OK_BODY)]).await;
    let exec = GeminiVertexExecutor::new(config_rx());
    let payload = r#"{"model":"gemini-2.5-flash","input":[{"type":"function_call","call_id":"call_1","name":"f","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}]}"#;
    let mut opts = Options::new(Format::OpenAIResponse);
    opts.response_format = Some(Format::OpenAIResponse);
    let req = Request {
        model: "gemini-2.5-flash".into(),
        payload: Bytes::from(payload.to_string()),
        format: Format::OpenAIResponse,
        metadata: Default::default(),
    };
    exec.execute(&key_auth("vertex", &base), req, opts).await.expect("execute");
    let body = seen.recv().await.unwrap().json();
    let mut calls = 0;
    for content in body.g("contents").array() {
        for part in content.g("parts").array() {
            calls += usize::from(part.g("functionCall").exists() || part.g("functionResponse").exists());
            assert!(!part.g("functionCall.id").exists() && !part.g("functionResponse.id").exists());
        }
    }
    assert_eq!(calls, 2);
}

#[tokio::test]
async fn vertex_without_credentials_fails_before_any_request() {
    let exec = GeminiVertexExecutor::new(config_rx());
    let auth = cpa_auth::Auth::new("v", "vertex");
    let err = exec.execute(&auth, request("gemini-2.5-flash", "{}"), gemini_opts()).await.unwrap_err();
    assert_eq!(err.message, "vertex executor: missing auth metadata");
    assert!(!err.upstream_attempted);
}

#[tokio::test]
async fn aistudio_relays_requests_through_the_page() {
    use super::wsrelay::{Inbound, Manager, Message, Outbound};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct Rx(mpsc::Receiver<Result<Inbound, String>>);
    impl futures_util::Stream for Rx {
        type Item = Result<Inbound, String>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }

    let relay = Arc::new(Manager::new(""));
    let (out_tx, mut out_rx) = mpsc::channel(16);
    let (in_tx, in_rx) = mpsc::channel(16);
    let channel = relay.attach(out_tx, Rx(in_rx));
    let exec = AiStudioExecutor::new(config_rx(), relay.clone());
    let auth = cpa_auth::Auth::new(channel.clone(), "aistudio");

    let page = tokio::spawn(async move {
        let mut requests = Vec::new();
        while requests.len() < 3 {
            let Some(Outbound::Text(text)) = out_rx.recv().await else { continue };
            let msg: Message = serde_json::from_str(&text).unwrap();
            let payload = msg.payload.clone().unwrap();
            let url = payload["url"].as_str().unwrap().to_string();
            let reply = if url.ends_with(":countTokens") {
                serde_json::json!({"id": msg.id, "type": "http_response", "payload": {"status": 200, "body": "{\"totalTokens\":9}"}})
            } else if url.contains(":streamGenerateContent") {
                let chunk = format!("data: {OK_BODY}\n\n");
                for (kind, p) in [
                    ("stream_start", serde_json::json!({"status": 200})),
                    ("stream_chunk", serde_json::json!({"data": chunk})),
                    ("stream_end", serde_json::json!({})),
                ] {
                    let frame = serde_json::json!({"id": msg.id, "type": kind, "payload": p}).to_string();
                    in_tx.send(Ok(Inbound::Text(frame))).await.unwrap();
                }
                requests.push(payload);
                continue;
            } else {
                serde_json::json!({"id": msg.id, "type": "http_response", "payload": {"status": 200, "headers": {"Content-Type": ["application/json"]}, "body": OK_BODY}})
            };
            in_tx.send(Ok(Inbound::Text(reply.to_string()))).await.unwrap();
            requests.push(payload);
        }
        requests
    });

    let payload = r#"{"contents":[{"role":"model","parts":[{"text":"prior"}]}],"generationConfig":{"maxOutputTokens":10,"responseMimeType":"application/json"},"tools":[{"x":1}]}"#;
    let resp = exec.execute(&auth, request("gemini-2.5-flash", payload), gemini_opts()).await.expect("execute");
    // AI Studio replies are re-encoded with sorted keys and `": "` separators.
    let text = String::from_utf8(resp.payload.to_vec()).unwrap();
    assert!(text.contains("\"finishReason\": \"STOP\""), "{text}");

    let mut stream = exec
        .execute_stream(&auth, request("gemini-2.5-flash", payload), Options { stream: true, ..gemini_opts() })
        .await
        .expect("stream");
    let mut streamed = String::new();
    while let Some(chunk) = stream.chunks.recv().await {
        streamed.push_str(&String::from_utf8_lossy(&chunk.expect("chunk")));
    }
    assert!(streamed.contains("\"text\": \"ok\""), "{streamed}");

    let counted = exec.count_tokens(&auth, request("gemini-2.5-flash", payload), gemini_opts()).await.expect("count");
    assert_eq!(cpa_json::parse(&counted.payload).g("totalTokens").int(), 9);

    let requests = page.await.unwrap();
    assert_eq!(
        requests[0]["url"],
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
    );
    assert_eq!(requests[0]["headers"]["Content-Type"][0], "application/json");
    let body = cpa_json::parse(requests[0]["body"].as_str().unwrap().as_bytes());
    assert!(
        !body.g("generationConfig.maxOutputTokens").exists() && !body.g("generationConfig.responseMimeType").exists()
    );
    assert_eq!(body.g("contents.0.role").str(), "user");
    assert_eq!(body.g("contents.2.role").str(), "user");
    assert_eq!(
        requests[1]["url"],
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
    let count_body = cpa_json::parse(requests[2]["body"].as_str().unwrap().as_bytes());
    assert!(!count_body.g("tools").exists() && !count_body.g("generationConfig").exists());
    assert_eq!(count_body.g("contents.#").int(), 2);
}

#[tokio::test]
async fn aistudio_disconnected_channel_fails_before_sending() {
    let exec = AiStudioExecutor::new(config_rx(), std::sync::Arc::new(super::wsrelay::Manager::new("")));
    let auth = cpa_auth::Auth::new("aistudio-gone", "aistudio");
    let err = exec
        .execute(
            &auth,
            request("gemini-2.5-flash", r#"{"contents":[{"role":"user","parts":[{"text":"q"}]}]}"#),
            gemini_opts(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.message, "wsrelay: provider aistudio-gone not connected");
    assert!(!err.upstream_attempted);
}

//! AI Studio relay with the real service: a connecting page becomes a runtime-only `aistudio`
//! credential that serves chat requests through its socket, and leaves the pool when it
//! disconnects.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use cpa_auth::OAuthSessions;
use cpa_executors::gemini::wsrelay;
use cpa_runtime::service::ServiceBuilder;
use cpa_runtime::usage::UsageTracker;
use cpa_server::{AppState, aistudio, build_router};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

const GEMINI_REPLY: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hello from the page"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":7}}"#;

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn connected_page_is_a_runtime_credential_that_serves_requests() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.yaml");
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir).expect("auth dir");
    std::fs::write(&config, format!("host: 127.0.0.1\nport: 0\nauth-dir: {}\napi-keys:\n  - k1\n", auth_dir.display())).expect("config");

    let service = Arc::new(ServiceBuilder::new(&config).watch(false).dotenv_dir(None).build().expect("service"));
    service.start().await.expect("start");
    for executor in cpa_executors::all_executors(service.subscribe_config()) {
        service.register_executor(executor);
    }
    aistudio::install_relay_hooks(&service);

    let state = AppState::new(
        service.subscribe_config(),
        service.manager(),
        service.store(),
        Arc::new(OAuthSessions::default()),
        Arc::new(UsageTracker::default()),
    );
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = router.clone().into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // The page connects (ws-auth is on by default): a runtime-only aistudio credential appears.
    let mut upgrade = format!("ws://{addr}/v1/ws").into_client_request().expect("upgrade request");
    upgrade.headers_mut().insert("x-api-key", "k1".parse().expect("header"));
    let (mut page, _) = tokio_tungstenite::connect_async(upgrade).await.expect("connect");
    let manager = service.manager();
    eventually("the runtime credential", || manager.list().iter().any(|a| a.provider == "aistudio")).await;
    let auth = manager.list().into_iter().find(|a| a.provider == "aistudio").expect("aistudio auth");
    assert!(auth.id.starts_with("aistudio-"), "{}", auth.id);
    assert_eq!(auth.attributes.get("runtime_only").map(String::as_str), Some("true"));

    // A chat request is relayed to the page and its answer translated back.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer k1")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}"#))
        .expect("request");
    let call = tokio::spawn(router.clone().oneshot(request));
    let envelope = loop {
        match tokio::time::timeout(Duration::from_secs(10), page.next()).await.expect("frame in time") {
            Some(Ok(WsMessage::Text(t))) => break serde_json::from_str::<serde_json::Value>(t.as_str()).expect("json"),
            Some(Ok(_)) => continue,
            other => panic!("socket ended: {other:?}"),
        }
    };
    assert_eq!(envelope["type"], "http_request");
    assert!(envelope["payload"]["url"].as_str().is_some_and(|u| u.contains("gemini-2.5-flash") && u.contains("generateContent")), "{envelope}");
    let reply = serde_json::json!({
        "id": envelope["id"], "type": "http_response",
        "payload": {"status": 200, "headers": {"Content-Type": ["application/json"]}, "body": GEMINI_REPLY}
    });
    page.send(WsMessage::Text(reply.to_string().into())).await.expect("reply");
    let response = call.await.expect("join").expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.expect("body");
    let chat: serde_json::Value = serde_json::from_slice(&body).expect("chat json");
    assert_eq!(chat["choices"][0]["message"]["content"], "hello from the page", "{chat}");

    // Disconnecting removes the credential.
    drop(page);
    eventually("the credential removal", || !manager.list().iter().any(|a| a.provider == "aistudio") && wsrelay::global().connected().is_empty()).await;
}

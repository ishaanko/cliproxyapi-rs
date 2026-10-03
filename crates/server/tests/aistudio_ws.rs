//! AI Studio relay socket: a page connecting to `/v1/ws` becomes a relay session that serves
//! the executor's proxied HTTP requests, and `ws-auth` gates the upgrade.

mod common;

use std::time::Duration;

use common::harness;
use cpa_executors::gemini::wsrelay::{self, HttpRequest};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

async fn serve(h: &common::Harness) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = h.router.clone().into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

#[tokio::test]
async fn socket_serves_relayed_requests_and_ws_auth_gates_the_upgrade() {
    let h = harness("aistudio-ws", |_| {}).await;
    let addr = serve(&h).await;
    let url = format!("ws://{addr}/v1/ws");

    let (mut page, _) = tokio_tungstenite::connect_async(url.as_str()).await.expect("connect without ws-auth");
    let provider = loop {
        if let Some(p) = wsrelay::global().connected().into_iter().next() {
            break p;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(provider.starts_with("aistudio-"), "{provider}");

    let relay = tokio::spawn({
        let provider = provider.clone();
        async move {
            let req = HttpRequest { method: "POST".into(), url: "https://example.test/x".into(), ..Default::default() };
            wsrelay::global().non_stream(&provider, &req).await
        }
    });
    let frame = loop {
        match tokio::time::timeout(Duration::from_secs(5), page.next()).await.expect("frame") {
            Some(Ok(WsMessage::Text(t))) => break serde_json::from_str::<serde_json::Value>(t.as_str()).expect("json"),
            Some(Ok(_)) => continue,
            other => panic!("socket ended: {other:?}"),
        }
    };
    assert_eq!(frame["type"], "http_request");
    assert_eq!(frame["payload"]["url"], "https://example.test/x");
    let reply = serde_json::json!({
        "id": frame["id"], "type": "http_response",
        "payload": {"status": 200, "headers": {"Content-Type": ["application/json"]}, "body": "{\"ok\":true}"}
    });
    page.send(WsMessage::Text(reply.to_string().into())).await.expect("reply");
    let resp = relay.await.expect("join").expect("relayed response");
    assert_eq!((resp.status, resp.body.as_slice()), (200, br#"{"ok":true}"#.as_slice()));

    // Turning ws-auth on rejects keyless upgrades and accepts a valid key.
    let mut cfg = (*h.cfg_tx.borrow().clone()).clone();
    cfg.websocket_auth = true;
    h.cfg_tx.send(std::sync::Arc::new(cfg)).expect("config update");
    let denied = tokio_tungstenite::connect_async(url.as_str()).await;
    assert!(matches!(denied, Err(tokio_tungstenite::tungstenite::Error::Http(r)) if r.status() == 401));
    let mut req = url.as_str().into_client_request().expect("request");
    req.headers_mut().insert("x-api-key", "k1".parse().expect("header"));
    assert!(tokio_tungstenite::connect_async(req).await.is_ok());
}

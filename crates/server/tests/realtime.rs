//! Realtime route wiring: which middleware guards each route and how an issued client secret
//! authenticates the `realtimeAuth` routes but not the `standardAuth` ones.

mod common;

use axum::body::Body;
use axum::http::Request;
use common::{Harness, harness};
use serde_json::Value;
use tower::ServiceExt;

/// Sends a request with only the given `Authorization` header (the harness adds an API key).
async fn with_auth(h: &Harness, method: &str, path: &str, auth: Option<&str>) -> (u16, String) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(auth) = auth {
        req = req.header("authorization", auth);
    }
    let resp = h.router.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn issued_client_secret_authenticates_only_realtime_auth_routes() {
    let h = harness("rt-secret", |_| {}).await;
    let (status, _, body) = h.call("POST", "/v1/realtime/client_secrets", &[], r#"{"session":{"model":"gpt-realtime"}}"#).await;
    assert_eq!(status, 200, "{body}");
    let secret = serde_json::from_str::<Value>(&body).unwrap()["value"].as_str().unwrap().to_string();
    let bearer = format!("Bearer {secret}");
    // realtimeAuth route: the local secret is accepted (translations are a 501 stub).
    let (status, body) = with_auth(&h, "POST", "/v1/realtime/translations", Some(&bearer)).await;
    assert_eq!(status, 501, "{body}");
    // standardAuth route: the secret is not an API key.
    let (status, body) = with_auth(&h, "POST", "/v1/realtime/translations/client_secrets", Some(&bearer)).await;
    assert_eq!(status, 401, "{body}");
    assert!(body.contains(r#""code":"invalid_api_key""#));
    // An unknown ek_ token is reported as an invalid client secret, not as an API key failure.
    let (status, body) = with_auth(&h, "POST", "/v1/realtime/translations", Some("Bearer ek_unknown")).await;
    assert_eq!(status, 401);
    assert!(body.contains(r#""code":"invalid_realtime_client_secret""#), "{body}");
}

#[tokio::test]
async fn live_routes_use_plain_key_errors_and_realtime_routes_use_openai_errors() {
    let h = harness("rt-shape", |_| {}).await;
    let (status, body) = with_auth(&h, "POST", "/v1/live", Some("Bearer wrong")).await;
    assert_eq!(status, 401);
    assert_eq!(body, r#"{"error":"Invalid API key"}"#);
    let (status, body) = with_auth(&h, "POST", "/v1/realtime/calls", Some("Bearer wrong")).await;
    assert_eq!(status, 401);
    assert_eq!(body, r#"{"error":{"code":"invalid_api_key","message":"Invalid API key","param":null,"type":"authentication_error"}}"#);
    // Valid key but no Codex OAuth credential configured: selection fails before the upstream.
    let (status, _, body) = h.call("POST", "/v1/live", &[], "{}").await;
    assert_eq!(status, 503, "{body}");
}

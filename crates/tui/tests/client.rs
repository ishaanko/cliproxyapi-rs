//! Client behaviour against the mock management API (ports of internal/tui/client_test.go).

mod common;

use common::Mock;
use cpa_tui::Client;
use cpa_tui::client::query_escape;

#[test]
fn base_url_normalisation_matches_go() {
    let cases = [
        ("http://192.168.1.100:8317", "http://192.168.1.100:8317"),
        ("https://proxy.example.com/", "https://proxy.example.com"),
        ("HTTPS://proxy.example.com/", "HTTPS://proxy.example.com"),
        ("proxy.example.com:9000", "http://proxy.example.com:9000"),
        ("https://proxy.example.com/prefix/", "https://proxy.example.com/prefix"),
        ("", "http://127.0.0.1:8317"),
    ];
    for (input, expected) in cases {
        assert_eq!(Client::with_base_url(input, "secret").base_url(), expected, "input {input:?}");
    }
    assert_eq!(Client::new(8317, "test-secret").base_url(), "http://127.0.0.1:8317");
}

#[test]
fn query_escape_matches_go() {
    assert_eq!(query_escape("a b&c=d/é"), "a+b%26c%3Dd%2F%C3%A9");
    assert_eq!(query_escape("claude-test@example.com.json"), "claude-test%40example.com.json");
}

#[tokio::test]
async fn sends_bearer_secret_and_hits_management_path() {
    let mock = Mock::start().await;
    let client = Client::with_base_url(&mock.url(), "mgmt-secret");
    let cfg = client.get_config().await.expect("config");
    assert_eq!(cfg["port"], 8317);
    let calls = mock.calls_matching("GET", "/v0/management/config");
    assert_eq!(calls[0].auth, "Bearer mgmt-secret");
}

#[tokio::test]
async fn http_errors_carry_status_and_body() {
    let mock = Mock::start().await;
    let client = Client::with_base_url(&mock.url(), "nope");
    let err = client.get_config().await.expect_err("unauthorized");
    assert_eq!(err, r#"HTTP 401: {"error":"invalid management key"}"#);
    // The secret can be replaced after construction (password gate).
    client.set_secret_key(" mgmt-secret ");
    assert!(client.get_config().await.is_ok());
}

#[tokio::test]
async fn log_polling_query_and_latest_timestamp() {
    let mock = Mock::start().await;
    let client = Client::with_base_url(&mock.url(), "mgmt-secret");
    let (lines, latest) = client.get_logs(0, 200).await.expect("logs");
    assert_eq!((lines.len(), latest), (4, 100));
    let (_, latest) = client.get_logs(500, 200).await.expect("logs");
    assert_eq!(latest, 500, "latest never goes below `after`");
    let calls = mock.calls_matching("GET", "/v0/management/logs");
    assert_eq!(calls[0].path, "/v0/management/logs?limit=200");
    assert_eq!(calls[1].path, "/v0/management/logs?after=500&limit=200");
}

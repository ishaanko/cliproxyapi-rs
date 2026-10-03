//! Request logs pushed to Home instead of files (Go: internal/logging/request_logger_home.go).
//!
//! In Home mode with `request-log: true` each finished exchange is rendered like a log file and
//! sent as `RPUSH request-log <json>`; nothing is written locally. Forced error logs (request-log
//! off) still go to files, exactly as in Go. Without a healthy Home client the log is dropped.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::HeaderMap;
use cpa_home::{Client, HomeError};
use serde::Serialize;

use crate::logging::go_json_html_escape;
use crate::reqlog::canonical_header_name;

/// JSON pushed to Home's `request-log` list (Go: `homeRequestLogPayload`). Headers are the raw
/// request headers (unmasked), keyed by canonical name.
#[derive(Serialize)]
struct HomeRequestLogPayload<'a> {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "str::is_empty")]
    request_id: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    request_log: &'a str,
}

/// `cloneHeaders`: canonical keys, values in order. `Host` is not part of Go's `Request.Header`.
fn payload_headers(headers: &HeaderMap) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        if name.as_str() == "host" {
            continue;
        }
        out.entry(canonical_header_name(name.as_str()))
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    out
}

/// Serializes the payload like Go's `json.Marshal` (field order, HTML escaping).
pub fn build_payload(headers: &HeaderMap, request_id: &str, request_log: &str) -> Result<String, serde_json::Error> {
    let payload = HomeRequestLogPayload {
        headers: payload_headers(headers),
        request_id: request_id.trim(),
        request_log,
    };
    serde_json::to_string(&payload).map(go_json_html_escape)
}

/// The process-wide Home client when it can take request logs (`currentHomeRequestLogClient`
/// plus the `HeartbeatOK` gate).
pub fn ready_client() -> Option<Arc<Client>> {
    cpa_home::kv::current().filter(|client| client.heartbeat_ok())
}

/// `forwardRequestLogToHome`: no-op without a healthy client.
pub async fn forward_request_log(headers: &HeaderMap, request_id: &str, request_log: &str) -> Result<(), HomeError> {
    let Some(client) = ready_client() else { return Ok(()) };
    let payload = build_payload(headers, request_id, request_log).map_err(HomeError::other)?;
    client.rpush_request_log(payload.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_carries_raw_headers_and_trimmed_request_id() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().expect("header"));
        headers.insert("authorization", "Bearer secret".parse().expect("header"));
        headers.insert("host", "example.test".parse().expect("header"));
        headers.append("x-multi", "a".parse().expect("header"));
        headers.append("x-multi", "b".parse().expect("header"));
        let raw = build_payload(&headers, " req-1 ", "=== REQUEST INFO ===\n<x>").expect("payload");
        let got: serde_json::Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(got["headers"]["Authorization"][0], "Bearer secret");
        assert_eq!(got["headers"]["Content-Type"][0], "application/json");
        assert_eq!(got["headers"]["X-Multi"], serde_json::json!(["a", "b"]));
        assert!(got["headers"].get("Host").is_none());
        assert_eq!(got["request_id"], "req-1");
        assert!(raw.contains("\\u003cx\\u003e"), "{raw}");
        assert!(raw.starts_with(r#"{"headers":"#), "{raw}");
    }

    #[test]
    fn empty_fields_are_omitted() {
        let raw = build_payload(&HeaderMap::new(), "  ", "log").expect("payload");
        assert_eq!(raw, r#"{"request_log":"log"}"#);
    }
}

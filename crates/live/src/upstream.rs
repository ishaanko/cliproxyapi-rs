//! Credential injection and the buffered HTTP call to the Codex realtime endpoints (Go:
//! `NewHttpRequest`/`HttpRequest` of the codex executor plus `readLimitedBody`).

use std::collections::HashMap;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::util::apply_custom_headers_from_attrs;
use cpa_executors::helps::proxy::new_proxy_aware_http_client;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use serde_json::Value;

/// Maximum request and response body size (16 MiB).
pub const MAX_BODY_SIZE: usize = 16 << 20;

/// Cap for one websocket message or frame on either leg of a relay (Go has no limit; unbounded
/// buffering of client-controlled frames is not acceptable here).
pub const MAX_WS_MESSAGE_SIZE: usize = MAX_BODY_SIZE;

/// `liveProtocolHeaders`.
pub const LIVE_PROTOCOL_HEADERS: [&str; 9] = [
    "OpenAI-Alpha",
    "X-Session-Id",
    "Session-Id",
    "Thread-Id",
    "Originator",
    "OpenAI-Safety-Identifier",
    "OpenAI-Organization",
    "OpenAI-Project",
    "X-Oai-Attestation",
];

/// `protocolHeaders`: only the forwarded protocol headers of the client request.
pub fn protocol_headers(source: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in LIVE_PROTOCOL_HEADERS {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else { continue };
        for value in source.get_all(&name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// `codexCreds`: the `api_key` attribute, else the OAuth access token.
fn codex_token(auth: &Auth) -> String {
    let api_key = auth.attributes.get("api_key").cloned().unwrap_or_default();
    if api_key.is_empty()
        && let Some(Value::String(token)) = auth.metadata.get("access_token")
    {
        return token.clone();
    }
    api_key
}

/// `CodexExecutor.PrepareRequest`: bearer token plus the credential's custom headers.
pub fn prepare_request_headers(auth: &Auth, headers: &mut HeaderMap) {
    let token = codex_token(auth);
    if token.trim().is_empty() {
        headers.remove(http::header::AUTHORIZATION);
    } else if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(http::header::AUTHORIZATION, v);
    }
    let attrs: HashMap<String, String> = auth.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    apply_custom_headers_from_attrs(headers, &attrs, None, None);
}

/// `setAccountHeader`: `Chatgpt-Account-Id` from the credential metadata.
pub fn set_account_header(headers: &mut HeaderMap, auth: &Auth) {
    if let Some(Value::String(account)) = auth.metadata.get("account_id")
        && !account.trim().is_empty()
        && let Ok(v) = HeaderValue::from_str(account)
    {
        headers.insert("chatgpt-account-id", v);
    }
}

/// `headersForLogging`: the attestation header is redacted.
pub fn headers_for_logging(source: &HeaderMap) -> HeaderMap {
    let mut headers = source.clone();
    if headers.get("x-oai-attestation").is_some_and(|v| !v.is_empty()) {
        headers.insert("x-oai-attestation", HeaderValue::from_static("[REDACTED]"));
    }
    headers
}

/// `callResponseHeaders`: the upstream headers relayed to the client.
pub fn call_response_headers(source: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in ["Content-Type", "Location", "Retry-After", "X-Request-Id", "OpenAI-Request-Id"] {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else { continue };
        for value in source.get_all(&name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// `copyRealtimeHandshakeHeaders`.
pub fn copy_handshake_headers(destination: &mut HeaderMap, source: &HeaderMap) {
    for name in ["Retry-After", "X-Request-Id", "OpenAI-Request-Id"] {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else { continue };
        for value in source.get_all(&name) {
            destination.append(name.clone(), value.clone());
        }
    }
}

/// Outcome of reading an upstream body with the size cap.
pub enum BodyRead {
    Ok(Bytes),
    /// More than [`MAX_BODY_SIZE`] bytes arrived; carries the first `MAX_BODY_SIZE`.
    TooLarge(Bytes),
    /// Transport failure mid-body; carries the partial payload.
    Failed(Bytes, String),
}

/// `readLimitedBody` over a reqwest response.
pub async fn read_limited(mut response: reqwest::Response) -> BodyRead {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                buf.extend_from_slice(&chunk);
                if buf.len() > MAX_BODY_SIZE {
                    buf.truncate(MAX_BODY_SIZE);
                    return BodyRead::TooLarge(Bytes::from(buf));
                }
            }
            Ok(None) => return BodyRead::Ok(Bytes::from(buf)),
            Err(e) => return BodyRead::Failed(Bytes::from(buf), error_chain(&e)),
        }
    }
}

/// Error text including its sources, closest to Go's wrapped `err.Error()`.
pub fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(s) = source {
        let next = s.to_string();
        if !text.contains(&next) {
            text.push_str(": ");
            text.push_str(&next);
        }
        source = s.source();
    }
    text
}

/// The upstream response before its body is read.
pub struct UpstreamResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub response: reqwest::Response,
}

/// Transport failure of [`send`]; `status` is the HTTP status the error maps to (Go:
/// `HTTPStatusFromError`), 0 when it carries none.
#[derive(Debug, Clone)]
pub struct SendError {
    pub status: u16,
    pub message: String,
}

impl SendError {
    /// `HTTPStatusFromErrorOr`.
    pub fn status_or(&self, fallback: u16) -> u16 {
        if self.status > 0 { self.status } else { fallback }
    }
}

/// `HttpRequest` with the credential's proxy settings: sends one prepared request.
pub async fn send(
    cfg: &Config,
    auth: &Auth,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Result<UpstreamResponse, SendError> {
    let client = new_proxy_aware_http_client("", Some(cfg), Some(auth), None);
    let response = client
        .request(method, url)
        .headers(headers)
        .body(body)
        .send()
        .await
        // A client timeout is Go's context deadline exceeded (504).
        .map_err(|e| SendError { status: if e.is_timeout() { 504 } else { 0 }, message: error_chain(&e) })?;
    Ok(UpstreamResponse { status: response.status().as_u16(), headers: response.headers().clone(), response })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logging_headers_redact_attestation() {
        let mut source = HeaderMap::new();
        source.insert("authorization", HeaderValue::from_static("Bearer oauth-token"));
        source.insert("x-oai-attestation", HeaderValue::from_static("attestation-token"));
        let logged = headers_for_logging(&source);
        assert_eq!(logged.get("x-oai-attestation").unwrap(), "[REDACTED]");
        assert_eq!(source.get("x-oai-attestation").unwrap(), "attestation-token");
    }
}

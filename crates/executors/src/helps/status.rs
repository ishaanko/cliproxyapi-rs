//! Upstream failures as [`ExecError`] (Go: the executors' `statusErr` and its constructors).
//!
//! Go executors turn a non-2xx upstream response into `statusErr{code, msg: string(body)}`; some
//! add `retryAfter` and response headers. Every upstream HTTP failure must carry its status so the
//! conductor can classify it (cooldown, retry, failover).

use std::time::{Duration, SystemTime};

use cpa_json::J;
use cpa_runtime::executor::ExecError;
use http::HeaderMap;

use super::sse::ScanError;

/// Go `statusErr{code, msg}`: the message is `msg`, or `status N` when empty.
pub fn status_err(status: u16, msg: impl Into<String>) -> ExecError {
    let msg = msg.into();
    let msg = if msg.is_empty() { format!("status {status}") } else { msg };
    ExecError::new(status, msg)
}

/// A non-2xx upstream response: `statusErr{code, msg: string(body)}` plus the upstream body and
/// headers (for passthrough) and a `Retry-After` hint on 429 (see [`parse_retry_after_header`]).
pub fn upstream_status_error(status: u16, headers: &HeaderMap, body: &[u8]) -> ExecError {
    let mut err = status_err(status, String::from_utf8_lossy(body).into_owned());
    err.body = Some(bytes::Bytes::copy_from_slice(body));
    err.headers = headers.clone();
    if status == 429
        && let Some(delay) = parse_retry_after_header(headers, SystemTime::now())
    {
        err.retry_after = Some(delay);
    }
    err
}

/// `Retry-After` as a delay: delta-seconds, or an HTTP date relative to `now` (past dates give
/// zero). `None` when absent or unparseable.
pub fn parse_retry_after_header(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let raw = headers.get(http::header::RETRY_AFTER)?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(seconds) = raw.parse::<i64>()
        && seconds >= 0
    {
        return Some(Duration::from_secs(seconds as u64));
    }
    let deadline = httpdate::parse_http_date(raw).ok()?;
    Some(deadline.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Fallback delay for per-minute token limits reported without `Retry-After`.
pub const OPENAI_COMPAT_TPM_FALLBACK_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Go `openAICompatRetryAfter`: only 429 carries a delay; the `Retry-After` header wins, else a
/// TPM-limit error code/message gets a one-minute fallback so the same large request is not
/// replayed immediately.
pub fn openai_compat_retry_after(
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
    now: SystemTime,
) -> Option<Duration> {
    if status != 429 {
        return None;
    }
    if let Some(delay) = parse_retry_after_header(headers, now) {
        return Some(delay);
    }
    let v = cpa_json::parse(body);
    let code = v.g("error.code").str().trim().to_lowercase();
    let message = v.g("error.message").str().trim().to_lowercase();
    let tpm = code.contains("tpmratelimitexceeded")
        || (message.contains("tokens per minute") && message.contains("limit") && message.contains("exceeded"));
    tpm.then_some(OPENAI_COMPAT_TPM_FALLBACK_RETRY_AFTER)
}

/// [`status_err`] for an OpenAI-compatible upstream response (Go: newOpenAICompatStatusError):
/// message is the body, retry hint per [`openai_compat_retry_after`]; no headers or body kept.
pub fn openai_compat_status_error(status: u16, headers: &HeaderMap, body: &[u8]) -> ExecError {
    let mut err = status_err(status, String::from_utf8_lossy(body).into_owned());
    err.retry_after = openai_compat_retry_after(status, headers, body, SystemTime::now());
    err
}

/// A transport error and its sources joined with `: `, the way Go renders `net/http` and `io`
/// errors (`dial tcp ...: connection refused`). The conductor classifies status-less failures
/// by message text (Go: `isTransientTransportMessage`), so the cause must be present. The request
/// URL reqwest appends to its top-level text is dropped to keep it out of client-visible errors.
pub fn error_chain_text(err: &(dyn std::error::Error + 'static)) -> String {
    let top = err.to_string();
    let mut text = top.split(" for url").next().unwrap_or(&top).to_string();
    let mut source = err.source();
    while let Some(s) = source {
        let part = s.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = s.source();
    }
    text
}

/// Go-style message of a failed request or body read: [`error_chain_text`], except that a body
/// cut short reads `unexpected EOF` (what Go surfaces and the conductor retries on).
pub fn transport_message(err: &reqwest::Error) -> String {
    go_transport_text(error_chain_text(err))
}

/// [`transport_message`] for an already formatted cause chain (also used for the fingerprinted
/// transports, whose errors carry no `reqwest::Error`).
pub fn go_transport_text(text: String) -> String {
    const INCOMPLETE: [&str; 5] = [
        "unexpected eof",
        "unexpected end of file",
        "connection closed before message completed",
        "end of file before message length reached",
        "incomplete message",
    ];
    let lower = text.to_lowercase();
    if INCOMPLETE.iter().any(|needle| lower.contains(needle)) {
        return "unexpected EOF".to_string();
    }
    text
}

/// A transport failure before or while reading a response (Go returns the raw `error`): no status.
pub fn transport_error(err: &reqwest::Error) -> ExecError {
    ExecError::new(0, transport_message(err))
}

impl From<ScanError> for ExecError {
    /// A stream scan failure (Go: `scanner.Err()` forwarded as `StreamChunk{Err}`): no status.
    fn from(err: ScanError) -> Self {
        ExecError::new(0, err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        h
    }

    #[test]
    fn status_error_carries_body_headers_and_retry_after() {
        let h = headers(&[("retry-after", "7"), ("x-request-id", "r1")]);
        let err = upstream_status_error(429, &h, br#"{"error":"slow down"}"#);
        assert_eq!(err.status, 429);
        assert_eq!(err.message, r#"{"error":"slow down"}"#);
        assert_eq!(err.body.as_deref(), Some(&br#"{"error":"slow down"}"#[..]));
        assert_eq!(err.retry_after, Some(Duration::from_secs(7)));
        assert_eq!(err.headers.get("x-request-id").unwrap(), "r1");
        // Retry-After is only a hint on 429, and an empty body renders as `status N`.
        assert_eq!(upstream_status_error(503, &h, b"").retry_after, None);
        assert_eq!(upstream_status_error(503, &h, b"").message, "status 503");
    }

    #[test]
    fn retry_after_http_date_is_relative_to_now() {
        let now = httpdate::parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        let future = headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:30 GMT")]);
        assert_eq!(parse_retry_after_header(&future, now), Some(Duration::from_secs(30)));
        let past = headers(&[("retry-after", "Wed, 21 Oct 2015 07:00:00 GMT")]);
        assert_eq!(parse_retry_after_header(&past, now), Some(Duration::ZERO));
        assert_eq!(parse_retry_after_header(&headers(&[("retry-after", "-5")]), now), None);
        assert_eq!(parse_retry_after_header(&HeaderMap::new(), now), None);
    }

    #[test]
    fn openai_compat_tpm_fallback() {
        let now = SystemTime::now();
        let none = HeaderMap::new();
        let tpm = br#"{"error":{"code":"TPMRateLimitExceeded","message":"x"}}"#;
        assert_eq!(openai_compat_retry_after(429, &none, tpm, now), Some(Duration::from_secs(60)));
        let msg = br#"{"error":{"message":"Tokens per minute limit exceeded"}}"#;
        assert_eq!(openai_compat_retry_after(429, &none, msg, now), Some(Duration::from_secs(60)));
        assert_eq!(openai_compat_retry_after(500, &none, tpm, now), None);
        assert_eq!(openai_compat_retry_after(429, &none, b"{}", now), None);
        let hdr = headers(&[("retry-after", "3")]);
        assert_eq!(openai_compat_retry_after(429, &hdr, tpm, now), Some(Duration::from_secs(3)));
    }
}

//! Retry-delay extraction from Google API errors (Go: helps/json_retry_helpers.go).
//!
//! `DeleteJSONField` lives in [`super::payload::delete_json_field`]; the delay parser is shared
//! with the auth layer, which already ports it.

use std::time::Duration;

/// Retry delay of a Google API 429 body: `RetryInfo.retryDelay`, else `ErrorInfo`
/// `metadata.quotaResetDelay`, else "after Ns" / "after 1h2m3s" in `error.message`.
pub fn parse_retry_delay(error_body: &[u8]) -> Option<Duration> {
    cpa_auth::retry::parse_retry_delay(error_body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_retry_delay_shapes() {
        let info = br#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"3.5s"}]}}"#;
        assert_eq!(parse_retry_delay(info), Some(Duration::from_millis(3500)));
        let msg = br#"{"error":{"message":"Quota exceeded. Please retry after 42s."}}"#;
        assert_eq!(parse_retry_delay(msg), Some(Duration::from_secs(42)));
        assert_eq!(parse_retry_delay(b"{}"), None);
    }
}

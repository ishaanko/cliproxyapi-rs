//! Retry-delay extraction from Google API 429 bodies (`helps.ParseRetryDelay`) and Go-style
//! duration strings (`time.ParseDuration`).

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

/// `time.ParseDuration` for non-negative values: `[+]?(<number><unit>)+` with units
/// `ns us µs μs ms s m h`; a bare `0` is allowed. `None` on anything malformed or out of range.
pub fn parse_go_duration(input: &str) -> Option<Duration> {
    let mut s = input;
    if let Some(rest) = s.strip_prefix('+') {
        s = rest;
    } else if s.starts_with('-') {
        return None;
    }
    if s == "0" {
        return Some(Duration::ZERO);
    }
    if s.is_empty() {
        return None;
    }
    let mut total_ns = 0f64;
    while !s.is_empty() {
        let num_end = s
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(s.len());
        let (num, rest) = s.split_at(num_end);
        if num.is_empty() || num == "." {
            return None;
        }
        let value: f64 = num.parse().ok()?;
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let (unit, rest) = rest.split_at(unit_end);
        let ns_per_unit = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => return None,
        };
        total_ns += value * ns_per_unit;
        s = rest;
    }
    // Go's Duration is an i64 of nanoseconds.
    if !total_ns.is_finite() || total_ns > i64::MAX as f64 {
        return None;
    }
    Some(Duration::from_nanos(total_ns as u64))
}

static AFTER_SECONDS: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"after\s+(\d+)s\.?").ok());
static AFTER_HUMAN: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"after\s+((?:\d+h)?(?:\d+m)?(?:\d+s)?)\.?").ok());

/// Retry delay of a Google API error body: `RetryInfo.retryDelay`, else `ErrorInfo`
/// `metadata.quotaResetDelay`, else "after Ns" / "after 1h2m3s" in the message.
pub fn parse_retry_delay(error_body: &[u8]) -> Option<Duration> {
    let v: Value = serde_json::from_slice(error_body).ok()?;
    if let Some(Value::Array(details)) = v.pointer("/error/details") {
        for d in details {
            if d.get("@type").and_then(Value::as_str)
                != Some("type.googleapis.com/google.rpc.RetryInfo")
            {
                continue;
            }
            let delay = d.get("retryDelay").and_then(Value::as_str).unwrap_or("");
            if delay.is_empty() {
                continue;
            }
            // Go returns an error (no delay) when the first non-empty RetryInfo delay is unparseable.
            return parse_go_duration(delay);
        }
        for d in details {
            if d.get("@type").and_then(Value::as_str)
                != Some("type.googleapis.com/google.rpc.ErrorInfo")
            {
                continue;
            }
            let delay = d
                .pointer("/metadata/quotaResetDelay")
                .and_then(Value::as_str)
                .unwrap_or("");
            if delay.is_empty() {
                continue;
            }
            if let Some(dur) = parse_go_duration(delay) {
                return Some(dur);
            }
        }
    }
    let message = v
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    if message.is_empty() {
        return None;
    }
    if let Some(re) = AFTER_SECONDS.as_ref()
        && let Some(secs) = re
            .captures(message)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u64>().ok())
    {
        return Some(Duration::from_secs(secs));
    }
    if let Some(re) = AFTER_HUMAN.as_ref() {
        let lower = message.to_lowercase();
        if let Some(d) = re
            .captures(&lower)
            .and_then(|c| c.get(1))
            .and_then(|m| parse_go_duration(m.as_str()))
            && !d.is_zero()
        {
            return Some(d);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_durations() {
        assert_eq!(parse_go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_go_duration("1h2m3s"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_go_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_go_duration("0"), Some(Duration::ZERO));
        for bad in [
            "",
            "5",
            "s",
            "1x",
            "-3s",
            "1e3s",
            "1..2s",
            "99999999999999999999h",
        ] {
            assert_eq!(parse_go_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn retry_delay_sources_in_go_order() {
        let retry_info = br#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"30s"}]}}"#;
        assert_eq!(parse_retry_delay(retry_info), Some(Duration::from_secs(30)));

        let quota = br#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","metadata":{"quotaResetDelay":"1.5s"}}]}}"#;
        assert_eq!(parse_retry_delay(quota), Some(Duration::from_millis(1500)));

        let msg = br#"{"error":{"message":"Resource exhausted. Please retry after 42s."}}"#;
        assert_eq!(parse_retry_delay(msg), Some(Duration::from_secs(42)));
        let human = br#"{"error":{"message":"Quota reset. Try again after 1h30m."}}"#;
        assert_eq!(parse_retry_delay(human), Some(Duration::from_secs(5400)));

        // An unparseable RetryInfo delay yields no delay (Go returns an error there).
        let bad = br#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"soon"}]}}"#;
        assert_eq!(parse_retry_delay(bad), None);
        assert_eq!(parse_retry_delay(b"not json"), None);
        assert_eq!(
            parse_retry_delay(br#"{"error":{"message":"nothing here"}}"#),
            None
        );
    }
}

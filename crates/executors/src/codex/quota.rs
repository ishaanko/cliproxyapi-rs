//! Codex websocket quota events as pseudo response headers (Go: helps/codex_quota.go).
//!
//! A `codex.rate_limits` event (or an `error` frame carrying `headers`) is converted into the
//! same bounded `x-codex-*` header set the HTTP path observes from real response headers.
//! `http::HeaderMap` lowercases names, so Go's canonical `X-Codex-Primary-Used-Percent` is
//! stored as `x-codex-primary-used-percent`; lookups through `HeaderMap::get` are
//! case-insensitive.

use cpa_json::{Res, Value};
use http::{HeaderMap, HeaderName, HeaderValue};

const CODEX_QUOTA_ADDITIONAL_HEADER_KEY: &str = "x-codex-additional-";
const MAX_CODEX_ADDITIONAL_RATE_LIMITS: usize = 8;

#[derive(PartialEq, Eq)]
enum EventKind {
    Other,
    Error,
    RateLimits,
}

/// Converts one Codex websocket quota event into bounded `x-codex-*` headers. The additional
/// rate limits arrive as an object on the websocket path and as an array from the
/// `/wham/usage` probe; both become `x-codex-additional-{identifier}-*` headers. Returns an
/// empty map (Go: nil) when the payload is not a quota event or carries no usable data.
pub fn parse_codex_quota_event_headers(payload: &[u8]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if payload.is_empty() {
        return headers;
    }
    let kind = codex_quota_event_kind(payload);
    if kind == EventKind::Other {
        return headers;
    }
    let doc = cpa_json::parse(payload);
    let root = Res::of(&doc);
    if kind == EventKind::Error {
        parse_headers_object(&mut headers, &root.get("headers"));
        return headers;
    }

    let mut has_quota_data = false;

    // The main rate-limit object has no user-facing name in the websocket payload, so its
    // fields keep the HTTP response header names.
    let base = first_result(&root, &["rate_limits", "rateLimit"]);
    if add_rate_limit_headers(&mut headers, "x-codex-", &base) {
        has_quota_data = true;
    }

    let additional = first_result(&root, &["additional_rate_limits", "additionalRateLimits"]);
    let mut additional_count = 0usize;
    if additional.is_object() || additional.is_array() {
        let is_array = additional.is_array();
        additional.for_each(|key, value| {
            if additional_count >= MAX_CODEX_ADDITIONAL_RATE_LIMITS {
                return false;
            }
            let limit_name = if is_array {
                first_result_string(&value, &["limit_name", "limitName", "name"])
            } else {
                key.str().trim().to_string()
            };
            if limit_name.is_empty() {
                return true;
            }
            let identifier = normalize_header_identifier(&limit_name);
            if identifier.is_empty() {
                return true;
            }
            let mut rate_info = first_result(&value, &["rate_limit", "rateLimit"]);
            if !rate_info.exists() {
                rate_info = value.clone();
            }
            let prefix = format!("{CODEX_QUOTA_ADDITIONAL_HEADER_KEY}{identifier}-");
            if add_rate_limit_headers(&mut headers, &prefix, &rate_info) {
                // The limit name is upstream-controlled and lands in the plain-text request
                // log, so control characters are rejected like the plan type below.
                if valid_event_text(&limit_name) {
                    set_header(&mut headers, &format!("{prefix}limit-name"), &limit_name);
                }
                has_quota_data = true;
                additional_count += 1;
            }
            true
        });
    }

    let code_review = first_result(&root, &["code_review_rate_limits", "codeReviewRateLimits"]);
    if add_rate_limit_headers(&mut headers, "x-codex-code-review-", &code_review) {
        has_quota_data = true;
    }

    let credits = first_result(&root, &["credits"]);
    if credits.is_object() {
        has_quota_data =
            set_scalar_header(&mut headers, "x-codex-credits-has-credits", &credits, &["has_credits", "hasCredits"])
                || has_quota_data;
        has_quota_data =
            set_scalar_header(&mut headers, "x-codex-credits-unlimited", &credits, &["unlimited"]) || has_quota_data;
        has_quota_data =
            set_scalar_header(&mut headers, "x-codex-credits-balance", &credits, &["balance"]) || has_quota_data;
    }

    if !has_quota_data {
        return HeaderMap::new();
    }
    // A malformed active-limit name only invalidates that one header; the window percentages
    // collected above stay valid.
    let active_limit = first_result_string(
        &root,
        &["metered_limit_name", "meteredLimitName", "limit_name", "limitName"],
    );
    if valid_event_identifier(&active_limit) {
        set_header(&mut headers, "x-codex-active-limit", &active_limit);
    }
    let plan_type = first_result_string(&root, &["plan_type", "planType"]);
    if valid_event_text(&plan_type) {
        set_header(&mut headers, "x-codex-plan-type", &plan_type);
    }
    headers
}

/// Cheap discriminator for the common non-quota websocket frame: only the first 256 bytes are
/// scanned for the top-level `"type"` member, so full JSON parsing is reserved for quota and
/// error events.
fn codex_quota_event_kind(payload: &[u8]) -> EventKind {
    const SCAN_LIMIT: usize = 256;
    const RATE_LIMITS: &[u8] = b"\"codex.rate_limits\"";
    let end = payload.len().min(SCAN_LIMIT);
    let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n');
    let mut i = 0;
    while i + 6 < end {
        if &payload[i..i + 6] != b"\"type\"" {
            i += 1;
            continue;
        }
        let mut j = i + 6;
        while j < end && is_ws(payload[j]) {
            j += 1;
        }
        if j >= end || payload[j] != b':' {
            i += 1;
            continue;
        }
        j += 1;
        while j < end && is_ws(payload[j]) {
            j += 1;
        }
        if j + 7 <= end && &payload[j..j + 7] == b"\"error\"" {
            return EventKind::Error;
        }
        if j + RATE_LIMITS.len() <= end && &payload[j..j + RATE_LIMITS.len()] == RATE_LIMITS {
            return EventKind::RateLimits;
        }
        i += 1;
    }
    EventKind::Other
}

/// Writes the allowed/limit-reached flags and each complete primary/secondary window of
/// `rate_info` under `prefix`; true when anything was recorded.
fn add_rate_limit_headers(headers: &mut HeaderMap, prefix: &str, rate_info: &Res<'_>) -> bool {
    if !rate_info.is_object() {
        return false;
    }
    let mut changed = false;
    if set_scalar_header(headers, &format!("{prefix}allowed"), rate_info, &["allowed"]) {
        changed = true;
    }
    if set_scalar_header(headers, &format!("{prefix}limit-reached"), rate_info, &["limit_reached", "limitReached"]) {
        changed = true;
    }
    for window_name in ["primary", "secondary"] {
        let window = first_result(rate_info, &[window_name]);
        if !window.is_object() {
            continue;
        }
        let used = first_result(&window, &["used_percent", "usedPercent"]);
        let minutes = first_result(&window, &["window_minutes", "windowMinutes"]);
        let reset_after = first_result(&window, &["reset_after_seconds", "resetAfterSeconds"]);
        let reset_at = first_result(&window, &["reset_at", "resetAt"]);
        let has_reset_after = reset_after.exists() && reset_after.int() >= 0;
        let has_reset_at = reset_at.exists() && reset_at.int() > 0;
        if !used.exists()
            || !minutes.exists()
            || used.float() < 0.0
            || used.float() > 100.0
            || minutes.int() <= 0
            || (!has_reset_after && !has_reset_at)
        {
            continue;
        }
        let window_prefix = format!("{prefix}{window_name}-");
        set_scalar_header(headers, &format!("{window_prefix}used-percent"), &window, &["used_percent", "usedPercent"]);
        set_scalar_header(
            headers,
            &format!("{window_prefix}window-minutes"),
            &window,
            &["window_minutes", "windowMinutes"],
        );
        if has_reset_after {
            set_scalar_header(
                headers,
                &format!("{window_prefix}reset-after-seconds"),
                &window,
                &["reset_after_seconds", "resetAfterSeconds"],
            );
        }
        if has_reset_at {
            set_scalar_header(headers, &format!("{window_prefix}reset-at"), &window, &["reset_at", "resetAt"]);
        }
        changed = true;
    }
    changed
}

/// Copies the quota-related members of an error frame's `headers` object.
fn parse_headers_object(headers: &mut HeaderMap, node: &Res<'_>) {
    if !node.is_object() {
        return;
    }
    node.for_each(|key, value| {
        let name = key.str().trim().to_string();
        if !is_quota_header_name(&name) {
            return true;
        }
        let raw = scalar_value(&value);
        if !raw.is_empty() {
            set_header(headers, &name, &raw);
        }
        true
    });
}

/// Allowlist of error-frame header names that carry rate-limit information.
fn is_quota_header_name(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    if lower == "retry-after" || lower.starts_with("x-ratelimit-") {
        return true;
    }
    if lower == "x-codex-active-limit" || lower == "x-codex-plan-type" || lower.starts_with("x-codex-credits-") {
        return true;
    }
    if !lower.starts_with("x-codex-") {
        return false;
    }
    [
        "-allowed",
        "-limit-reached",
        "-limit-name",
        "-used-percent",
        "-window-minutes",
        "-reset-after-seconds",
        "-reset-at",
        "-over-secondary-limit-percent",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// Sets `name` from the first present scalar in `paths`; true when a header was written.
fn set_scalar_header(headers: &mut HeaderMap, name: &str, object: &Res<'_>, paths: &[&str]) -> bool {
    let value = first_result(object, paths);
    if !value.exists() {
        return false;
    }
    let raw = scalar_value(&value);
    if raw.is_empty() {
        return false;
    }
    set_header(headers, name, &raw);
    true
}

/// Trimmed text of a string, number or bool; empty for other kinds.
fn scalar_value(value: &Res<'_>) -> String {
    match value.v() {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(_) | Value::Bool(_)) => value.raw().trim().to_string(),
        _ => String::new(),
    }
}

/// First of `paths` that exists and is not JSON null; missing otherwise.
fn first_result<'a>(object: &'a Res<'_>, paths: &[&str]) -> Res<'a> {
    for path in paths {
        let value = object.get(path);
        if value.exists() && !value.is_null() {
            return value;
        }
    }
    Res::NONE
}

/// Scalar text of the first of `paths` that yields a non-empty scalar.
fn first_result_string(object: &Res<'_>, paths: &[&str]) -> String {
    for path in paths {
        let raw = scalar_value(&first_result(object, &[path]));
        if !raw.is_empty() {
            return raw;
        }
    }
    String::new()
}

/// Turns an upstream limit name into a header-safe identifier (`GPT-5.3-Codex-Spark` stays,
/// runs of other characters collapse to one `-`); empty when nothing valid remains.
fn normalize_header_identifier(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        return String::new();
    }
    let mut out = String::with_capacity(value.len());
    let mut last_dash = false;
    for c in value.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => {
                out.push(c);
                last_dash = false;
            }
            _ => {
                if !last_dash {
                    out.push('-');
                    last_dash = true;
                }
            }
        }
    }
    let identifier = out.trim_matches(['-', '_', '.']);
    if identifier.is_empty() || !valid_event_identifier(identifier) {
        return String::new();
    }
    identifier.to_string()
}

fn valid_event_identifier(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 {
        return false;
    }
    value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn valid_event_text(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.len() <= 256 && !value.contains(['\r', '\n'])
}

/// Go `http.Header.Set`: replaces any previous value. Names or values that `http` rejects
/// (Go would keep them verbatim) are dropped.
fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    let (Ok(name), Ok(value)) =
        (HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()), HeaderValue::from_bytes(value.as_bytes()))
    else {
        return;
    };
    headers.insert(name, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(h: &'a HeaderMap, k: &str) -> &'a str {
        h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    #[test]
    fn preserves_active_limit() {
        let h = parse_codex_quota_event_headers(
            br#"{
		"type":"codex.rate_limits",
		"plan_type":"pro",
		"metered_limit_name":"codex_bengalfox",
		"rate_limits":{
			"primary":{"used_percent":2,"window_minutes":10080,"reset_at":1782951970},
			"secondary":null
		}
	}"#,
        );
        assert_eq!(get(&h, "X-Codex-Active-Limit"), "codex_bengalfox");
        assert_eq!(get(&h, "X-Codex-Primary-Used-Percent"), "2");
        assert_eq!(get(&h, "X-Codex-Primary-Window-Minutes"), "10080");
        assert_eq!(get(&h, "X-Codex-Primary-Reset-At"), "1782951970");
        assert_eq!(get(&h, "X-Codex-Plan-Type"), "pro");
    }

    #[test]
    fn preserves_additional_limits_and_credits() {
        let h = parse_codex_quota_event_headers(
            br#"{
		"type":"codex.rate_limits",
		"plan_type":"pro",
		"rate_limits":{
			"allowed":true,
			"limit_reached":false,
			"primary":{"used_percent":48,"window_minutes":10080,"reset_after_seconds":523210,"reset_at":1786677299},
			"secondary":null
		},
		"additional_rate_limits":{
			"GPT-5.3-Codex-Spark":{
				"allowed":true,
				"limit_reached":false,
				"primary":{"used_percent":3,"window_minutes":300,"reset_after_seconds":10148,"reset_at":1787231961},
				"secondary":{"used_percent":63,"window_minutes":10080,"reset_after_seconds":75420,"reset_at":1787290791}
			}
		},
		"credits":{"has_credits":false,"unlimited":false,"balance":"0"}
	}"#,
        );
        for (key, want) in [
            ("X-Codex-Primary-Used-Percent", "48"),
            ("X-Codex-Primary-Window-Minutes", "10080"),
            ("X-Codex-Primary-Reset-After-Seconds", "523210"),
            ("X-Codex-Additional-GPT-5.3-Codex-Spark-Limit-Name", "GPT-5.3-Codex-Spark"),
            ("X-Codex-Additional-GPT-5.3-Codex-Spark-Primary-Used-Percent", "3"),
            ("X-Codex-Additional-GPT-5.3-Codex-Spark-Primary-Window-Minutes", "300"),
            ("X-Codex-Additional-GPT-5.3-Codex-Spark-Secondary-Used-Percent", "63"),
            ("X-Codex-Credits-Has-Credits", "false"),
            ("X-Codex-Credits-Unlimited", "false"),
            ("X-Codex-Credits-Balance", "0"),
        ] {
            assert_eq!(get(&h, key), want, "header {key}; all headers = {h:?}");
        }
    }

    #[test]
    fn reads_quota_headers_from_error_frames() {
        let h = parse_codex_quota_event_headers(
            br#"{
		"type":"error",
		"status_code":429,
		"headers":{
			"X-Codex-Primary-Used-Percent":"100",
			"X-Codex-Primary-Window-Minutes":"10080",
			"X-Codex-Primary-Reset-After-Seconds":"437380",
			"X-Codex-Credits-Balance":"0",
			"X-Codex-Turn-State":"must-not-be-observed",
			"Set-Cookie":"must-not-be-observed"
		}
	}"#,
        );
        assert_eq!(get(&h, "X-Codex-Primary-Used-Percent"), "100");
        assert_eq!(get(&h, "X-Codex-Primary-Reset-After-Seconds"), "437380");
        assert_eq!(get(&h, "X-Codex-Credits-Balance"), "0");
        assert_eq!(get(&h, "Set-Cookie"), "");
        assert_eq!(get(&h, "X-Codex-Turn-State"), "");
    }

    #[test]
    fn does_not_invent_active_limit_and_rejects_incomplete_windows() {
        let h = parse_codex_quota_event_headers(
            br#"{
		"type":"codex.rate_limits",
		"rate_limits":{"primary":{"used_percent":42,"window_minutes":300,"reset_after_seconds":60}}
	}"#,
        );
        assert_eq!(get(&h, "X-Codex-Active-Limit"), "");
        assert_eq!(get(&h, "X-Codex-Primary-Reset-After-Seconds"), "60");
        let incomplete = parse_codex_quota_event_headers(
            br#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":42}}}"#,
        );
        assert!(incomplete.is_empty(), "incomplete quota event produced headers: {incomplete:?}");

        let h = parse_codex_quota_event_headers(
            br#"{
		"type":"codex.rate_limits",
		"rate_limits":{
			"primary":{"used_percent":42,"window_minutes":300},
			"secondary":{"used_percent":17,"window_minutes":10080,"reset_after_seconds":60}
		}
	}"#,
        );
        assert_eq!(get(&h, "X-Codex-Primary-Used-Percent"), "");
        assert_eq!(get(&h, "X-Codex-Secondary-Used-Percent"), "17");
    }

    #[test]
    fn supports_additional_array_and_camel_case() {
        let h = parse_codex_quota_event_headers(
            br#"{
		"type":"codex.rate_limits",
		"planType":"pro",
		"meteredLimitName":"premium",
		"rateLimit":{
			"allowed":true,
			"limitReached":false,
			"primary":{"usedPercent":71,"windowMinutes":10080,"resetAfterSeconds":60}
		},
		"additionalRateLimits":[{
			"limitName":"GPT-5.3-Codex-Spark",
			"rateLimit":{
				"primary":{"usedPercent":13,"windowMinutes":300,"resetAt":1787399817},
				"secondary":{"usedPercent":51,"windowMinutes":10080,"resetAfterSeconds":120}
			}
		}]
	}"#,
        );
        for (key, want) in [
            ("X-Codex-Active-Limit", "premium"),
            ("X-Codex-Plan-Type", "pro"),
            ("X-Codex-Primary-Used-Percent", "71"),
            ("X-Codex-Additional-GPT-5.3-Codex-Spark-Limit-Name", "GPT-5.3-Codex-Spark"),
            ("X-Codex-Additional-GPT-5.3-Codex-Spark-Primary-Used-Percent", "13"),
            ("X-Codex-Additional-GPT-5.3-Codex-Spark-Secondary-Reset-After-Seconds", "120"),
        ] {
            assert_eq!(get(&h, key), want, "header {key}; all headers = {h:?}");
        }
    }

    #[test]
    fn keeps_only_safe_error_headers() {
        let h = parse_codex_quota_event_headers(
            br#"{
		"type":"error",
		"headers":{
			"X-Ratelimit-Remaining-Requests":"7",
			"Retry-After":"60",
			"X-Codex-Primary-Over-Secondary-Limit-Percent":"0",
			"X-Codex-Turn-State":"secret",
			"Authorization":"Bearer secret"
		}
	}"#,
        );
        for (key, want) in [
            ("X-Ratelimit-Remaining-Requests", "7"),
            ("Retry-After", "60"),
            ("X-Codex-Primary-Over-Secondary-Limit-Percent", "0"),
        ] {
            assert_eq!(get(&h, key), want, "header {key}");
        }
        assert_eq!(get(&h, "X-Codex-Turn-State"), "");
        assert_eq!(get(&h, "Authorization"), "");
    }

    #[test]
    fn non_quota_events_yield_nothing() {
        let payload = br#"{"type":"response.output_text.delta","delta":"hello"}"#;
        assert!(parse_codex_quota_event_headers(payload).is_empty());
        let big = "x".repeat(4096);
        let payload = format!(r#"{{"sequence_number":7,"type":"response.output_text.delta","delta":"{big}"}}"#);
        assert!(parse_codex_quota_event_headers(payload.as_bytes()).is_empty());
    }

    // Quota markers live in the frame prefix, so large quota events must still be parsed.
    #[test]
    fn large_rate_limits_frame_is_parsed() {
        let padding = "p".repeat(4096);
        let payload = format!(
            r#"{{"type":"codex.rate_limits","rate_limits":{{"primary":{{"used_percent":42,"limit_percent":100,"window_minutes":10080,"reset_after_seconds":3600,"used_minutes":120}},"secondary":{{"used_percent":1,"limit_percent":100,"window_minutes":300,"reset_after_seconds":60,"used_minutes":2}},"active_limit":"primary","credits":{{"total_credits":"10","available_credits":"5","pct_remaining":"50"}}}},"padding":"{padding}"}}"#
        );
        let h = parse_codex_quota_event_headers(payload.as_bytes());
        assert!(!h.is_empty(), "large rate_limits frame produced no headers");
        assert_eq!(get(&h, "X-Codex-Primary-Used-Percent"), "42");
    }

    #[test]
    fn rejects_invalid_and_empty_events() {
        for payload in [
            "",
            r#"{"type":"response.completed"}"#,
            r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":101,"window_minutes":300,"reset_after_seconds":60}}}"#,
        ] {
            let h = parse_codex_quota_event_headers(payload.as_bytes());
            assert!(h.is_empty(), "payload {payload:?} produced unexpected headers: {h:?}");
        }
    }

    // A malformed active-limit name must not discard the window watermarks parsed from the event.
    #[test]
    fn keeps_windows_when_active_limit_is_invalid() {
        let h = parse_codex_quota_event_headers(
            br#"{
		"type":"codex.rate_limits",
		"metered_limit_name":"bad limit",
		"rate_limits":{"primary":{"used_percent":1,"window_minutes":300,"reset_after_seconds":60}}
	}"#,
        );
        assert!(!h.is_empty(), "valid window watermarks were discarded");
        assert_eq!(get(&h, "X-Codex-Primary-Used-Percent"), "1");
        assert_eq!(get(&h, "X-Codex-Active-Limit"), "");
    }

    // Real websocket events namespace additional limits by limit name and carry a
    // code_review_rate_limits sibling; both must survive parsing.
    #[test]
    fn observed_production_event() {
        let h = parse_codex_quota_event_headers(
            br#"{
		"type":"codex.rate_limits",
		"plan_type":"pro",
		"rate_limits":{"allowed":true,"limit_reached":false,"primary":{"used_percent":81,"window_minutes":10080,"reset_after_seconds":137160,"reset_at":1787588999},"secondary":null},
		"code_review_rate_limits":{"primary":{"used_percent":4,"window_minutes":300,"reset_after_seconds":900}},
		"additional_rate_limits":{"GPT-5.3-Codex-Spark":{"allowed":true,"limit_reached":false,"primary":{"used_percent":0,"window_minutes":300,"reset_after_seconds":18000}}},
		"credits":{"has_credits":false,"unlimited":false,"balance":"0"},
		"promo":null
	}"#,
        );
        assert!(!h.is_empty(), "observed production event produced no headers");
        for (key, want) in [
            ("X-Codex-Plan-Type", "pro"),
            ("X-Codex-Primary-Used-Percent", "81"),
            ("X-Codex-Primary-Reset-At", "1787588999"),
            ("X-Codex-Code-Review-Primary-Used-Percent", "4"),
            ("X-Codex-Additional-Gpt-5.3-Codex-Spark-Limit-Name", "GPT-5.3-Codex-Spark"),
            ("X-Codex-Additional-Gpt-5.3-Codex-Spark-Primary-Used-Percent", "0"),
            ("X-Codex-Credits-Balance", "0"),
            ("X-Codex-Credits-Has-Credits", "false"),
        ] {
            assert_eq!(get(&h, key), want, "header {key}");
        }
        // secondary is null upstream and must not be invented.
        assert_eq!(get(&h, "X-Codex-Secondary-Used-Percent"), "");
    }

    // An upstream-controlled limit name with control characters must not reach the request log.
    #[test]
    fn rejects_control_characters_in_limit_name() {
        let h = parse_codex_quota_event_headers(
            br#"{
		"type":"codex.rate_limits",
		"additional_rate_limits":[{"limit_name":"evil\r\nX-Injected: 1","primary":{"used_percent":3,"window_minutes":300,"reset_after_seconds":60}}]
	}"#,
        );
        assert!(!h.is_empty(), "expected window watermarks to survive");
        for (key, value) in &h {
            let v = value.as_bytes();
            assert!(!v.contains(&b'\r') && !v.contains(&b'\n'), "header {key} carried control characters");
        }
    }
}

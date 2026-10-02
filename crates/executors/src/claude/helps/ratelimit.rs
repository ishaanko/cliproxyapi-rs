//! Anthropic unified rate-limit header handling (Go: helps/claude_ratelimit.go).

use std::time::Duration;

use chrono::{DateTime, Utc};
use http::HeaderMap;
use rand::Rng;

const DEFAULT_FUZZ_MIN_SECONDS: i64 = 1;
const DEFAULT_FUZZ_MAX_SECONDS: i64 = 30;

/// First value of header `name` ("" when absent), like Go's `getHeaderCaseInsensitive`.
fn header(headers: &HeaderMap, name: &str) -> String {
    headers.get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).unwrap_or_default()
}

/// Lower-cased, trimmed header value (the status headers are compared this way).
fn header_norm(headers: &HeaderMap, name: &str) -> String {
    header(headers, name).trim().to_lowercase()
}

/// Whether response headers explicitly declare an Anthropic shared 5h or 7d rate-limit rejection
/// (Go: `ClaudeHeadersIndicateUnifiedRateLimitRejection`). An overage-only or Fable-only rejection
/// stays model-scoped while the shared windows are allowed (or omitted with healthy utilization).
pub fn claude_headers_indicate_unified_rate_limit_rejection(headers: &HeaderMap) -> bool {
    let unified_status = header_norm(headers, "Anthropic-Ratelimit-Unified-Status");
    let status_5h = header_norm(headers, "Anthropic-Ratelimit-Unified-5h-Status");
    if status_5h == "rejected" {
        return true;
    }
    let status_7d = header_norm(headers, "Anthropic-Ratelimit-Unified-7d-Status");
    if status_7d == "rejected" {
        return true;
    }
    if unified_status != "rejected" {
        return false;
    }
    let status_7d_oi = header_norm(headers, "Anthropic-Ratelimit-Unified-7d_oi-Status");
    !is_overage_or_fable_only_rejection(headers, &status_5h, &status_7d, &status_7d_oi)
}

fn is_window_allowed(status: &str) -> bool {
    status == "allowed" || status == "allowed_warning"
}

fn is_overage_or_fable_only_rejection(headers: &HeaderMap, status_5h: &str, status_7d: &str, status_7d_oi: &str) -> bool {
    if status_5h == "rejected" || status_7d == "rejected" {
        return false;
    }

    let overage_status = header_norm(headers, "Anthropic-Ratelimit-Unified-Overage-Status");
    let overage_disabled_reason = header(headers, "Anthropic-Ratelimit-Unified-Overage-Disabled-Reason");
    let representative_claim = header_norm(headers, "Anthropic-Ratelimit-Unified-Representative-Claim");

    let is_overage_rejected = status_7d_oi == "rejected"
        || overage_status == "rejected"
        || !overage_disabled_reason.trim().is_empty()
        || representative_claim.contains("overage");
    if !is_overage_rejected {
        return false;
    }

    let shared_5h_allowed = is_window_allowed(status_5h);
    let shared_7d_allowed = is_window_allowed(status_7d);
    if shared_5h_allowed && shared_7d_allowed {
        return true;
    }

    // Anthropic often omits one shared window's status when its utilization is 0.00; require an
    // explicit utilization in [0, 1) before treating the unmentioned window as healthy.
    if shared_7d_allowed && status_5h.is_empty() && is_utilization_healthy(&header(headers, "Anthropic-Ratelimit-Unified-5h-Utilization")) {
        return true;
    }
    if shared_5h_allowed && status_7d.is_empty() && is_utilization_healthy(&header(headers, "Anthropic-Ratelimit-Unified-7d-Utilization")) {
        return true;
    }
    false
}

fn is_utilization_healthy(raw: &str) -> bool {
    let raw = raw.trim();
    if raw.is_empty() {
        return false;
    }
    match raw.parse::<f64>() {
        Ok(u) => u.is_finite() && (0.0..1.0).contains(&u),
        Err(_) => false,
    }
}

/// Conservative cooldown from Anthropic's unified rate-limit and `Retry-After` headers, plus a
/// random 1-30 second grace (Go: `ParseClaudeRateLimitReset`). `None` when no valid future reset
/// is present, so callers fall back to generic exponential backoff.
pub fn parse_claude_rate_limit_reset(headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    parse_claude_rate_limit_reset_with_fuzz(headers, now, DEFAULT_FUZZ_MIN_SECONDS, DEFAULT_FUZZ_MAX_SECONDS)
}

/// [`parse_claude_rate_limit_reset`] with explicit fuzz bounds in whole seconds (Go:
/// `parseClaudeRateLimitResetWithFuzz`); `(0, 0)` makes the result deterministic.
pub fn parse_claude_rate_limit_reset_with_fuzz(headers: &HeaderMap, now: DateTime<Utc>, min_fuzz_sec: i64, max_fuzz_sec: i64) -> Option<Duration> {
    parse_claude_rate_limit_reset_with(headers, now, || random_fuzz(min_fuzz_sec, max_fuzz_sec))
}

/// Core of the reset parser with an injectable fuzz source, called at most once and only when a
/// deadline was found.
pub fn parse_claude_rate_limit_reset_with(headers: &HeaderMap, now: DateTime<Utc>, fuzz: impl FnOnce() -> Duration) -> Option<Duration> {
    let unified_status = header_norm(headers, "Anthropic-Ratelimit-Unified-Status");
    let status_5h = header_norm(headers, "Anthropic-Ratelimit-Unified-5h-Status");
    let status_7d = header_norm(headers, "Anthropic-Ratelimit-Unified-7d-Status");
    let status_7d_oi = header_norm(headers, "Anthropic-Ratelimit-Unified-7d_oi-Status");
    let overage_only_rejection = is_overage_or_fable_only_rejection(headers, &status_5h, &status_7d, &status_7d_oi);

    let mut candidates: Vec<DateTime<Utc>> = Vec::new();
    let mut rejected_windows: Vec<&str> = Vec::new();

    if unified_status == "rejected" {
        rejected_windows.push("unified");
    }
    if status_5h == "rejected" {
        rejected_windows.push("5h");
    }
    if status_7d == "rejected" {
        rejected_windows.push("7d");
    }
    if status_7d_oi == "rejected" {
        rejected_windows.push("7d_oi");
    }

    // 1. Retry-After (skipped for an overage/Fable-only rejection, which does not describe the credential).
    if !overage_only_rejection {
        let raw = header(headers, "Retry-After");
        if !raw.is_empty() {
            if !rejected_windows.contains(&"retry-after") {
                rejected_windows.push("retry-after");
            }
            if let Some(t) = parse_retry_after_header(&raw, now).filter(|t| *t > now) {
                candidates.push(t);
            }
        }
    }

    // 2-4. Window resets, only for rejected windows.
    let mut push_reset = |name: &str| {
        let raw = header(headers, name);
        if !raw.is_empty() {
            if let Some(t) = parse_unix_or_timestamp(&raw).filter(|t| *t > now) {
                candidates.push(t);
            }
        }
    };
    if status_5h == "rejected" {
        push_reset("Anthropic-Ratelimit-Unified-5h-Reset");
    }
    if status_7d == "rejected" {
        push_reset("Anthropic-Ratelimit-Unified-7d-Reset");
    }
    if status_7d_oi == "rejected" && !overage_only_rejection {
        push_reset("Anthropic-Ratelimit-Unified-7d_oi-Reset");
    }

    // 5. Unified reset header.
    let unified_rejected = !overage_only_rejection
        && (unified_status == "rejected"
            || status_5h == "rejected"
            || status_7d == "rejected"
            || status_7d_oi == "rejected"
            || (unified_status.is_empty() && !is_window_allowed(&status_5h) && !is_window_allowed(&status_7d)));
    if unified_rejected {
        let raw = header(headers, "Anthropic-Ratelimit-Unified-Reset");
        if !raw.is_empty() {
            if !rejected_windows.contains(&"unified") {
                rejected_windows.push("unified");
            }
            if let Some(t) = parse_unix_or_timestamp(&raw).filter(|t| *t > now) {
                candidates.push(t);
            }
        }
    }

    // Latest applicable deadline across rejected windows.
    let Some(latest) = candidates.into_iter().max() else {
        log_fallback(&rejected_windows);
        return None;
    };

    let base = latest.signed_duration_since(now).to_std().ok()?;
    let fuzz = fuzz();
    let effective = base + fuzz;
    tracing::info!(
        rejected_windows = %rejected_windows.join(","),
        effective_cooldown = ?effective,
        base_cooldown = ?base,
        fuzz = ?fuzz,
        deadline = %latest.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "parsed Anthropic rate limit reset headers"
    );
    Some(effective)
}

fn log_fallback(rejected_windows: &[&str]) {
    if !rejected_windows.is_empty() {
        tracing::info!(
            rejected_windows = %rejected_windows.join(","),
            status = "fallback_exponential_backoff",
            "Anthropic rate limit window rejected; falling back to generic exponential backoff"
        );
    }
}

/// Positive finite float seconds (Go: `ParseFloat` with `sec > 0`).
fn parse_positive_seconds(raw: &str) -> Option<f64> {
    raw.parse::<f64>().ok().filter(|s| s.is_finite() && *s > 0.0)
}

fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw).ok().map(|t| t.with_timezone(&Utc))
}

fn parse_http_date(raw: &str) -> Option<DateTime<Utc>> {
    httpdate::parse_http_date(raw).ok().map(DateTime::<Utc>::from)
}

/// Unix seconds (float), RFC3339 or HTTP date (Go: `parseUnixOrTimestamp`).
fn parse_unix_or_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(sec) = parse_positive_seconds(raw) {
        let sec_int = sec as i64;
        let nsec = ((sec - sec_int as f64) * 1e9) as i64;
        return DateTime::from_timestamp(sec_int, nsec.clamp(0, 999_999_999) as u32);
    }
    parse_rfc3339(raw).or_else(|| parse_http_date(raw))
}

/// Seconds (float), HTTP date or RFC3339 relative to `now` (Go: `parseRetryAfterHeader`).
fn parse_retry_after_header(raw: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(sec) = parse_positive_seconds(raw) {
        return now.checked_add_signed(chrono::Duration::nanoseconds((sec * 1e9) as i64));
    }
    parse_http_date(raw).or_else(|| parse_rfc3339(raw))
}

/// Uniform whole seconds in `[min, max]` (Go: `randomClaudeFuzzDuration`).
fn random_fuzz(min_sec: i64, max_sec: i64) -> Duration {
    if max_sec <= min_sec {
        return Duration::from_secs(min_sec.max(0) as u64);
    }
    Duration::from_secs(rand::rng().random_range(min_sec..=max_sec).max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(http::HeaderName::from_bytes(k.as_bytes()).expect("header name"), HeaderValue::from_str(v).expect("header value"));
        }
        h
    }

    fn unix_after(now: DateTime<Utc>, secs: i64) -> String {
        (now + chrono::Duration::seconds(secs)).timestamp().to_string()
    }

    fn reset(h: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
        parse_claude_rate_limit_reset_with_fuzz(h, now, 0, 0)
    }

    fn near(got: Option<Duration>, want: Duration) {
        let got = got.expect("expected a cooldown");
        let delta = if got > want { got - want } else { want - got };
        assert!(delta <= Duration::from_secs(5), "got {got:?}, want ~{want:?}");
    }

    #[test]
    fn reset_cases() {
        let now = Utc::now();
        let h5 = 5 * 3600;
        let d7 = 7 * 24 * 3600;
        let in5 = unix_after(now, h5);
        let in7d = unix_after(now, d7);

        assert_eq!(parse_claude_rate_limit_reset(&HeaderMap::new(), now), None);
        assert_eq!(reset(&headers(&[("Retry-After", "60")]), now), Some(Duration::from_secs(60)));

        let date = (now + chrono::Duration::seconds(90)).format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        near(reset(&headers(&[("Retry-After", &date)]), now), Duration::from_secs(90));

        // 5h rejected, 7d allowed, unified reset present.
        let h = headers(&[
            ("Anthropic-Ratelimit-Unified-5h-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-5h-Reset", &in5),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-7d-Reset", &in7d),
            ("Anthropic-Ratelimit-Unified-Reset", &in5),
        ]);
        near(reset(&h, now), Duration::from_secs(h5 as u64));

        // 7d rejected, 5h allowed.
        let h = headers(&[
            ("Anthropic-Ratelimit-Unified-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-5h-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-5h-Reset", &in5),
            ("Anthropic-Ratelimit-Unified-7d-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-7d-Reset", &in7d),
        ]);
        near(reset(&h, now), Duration::from_secs(d7 as u64));

        // Both rejected: the longest wins.
        let h = headers(&[
            ("Anthropic-Ratelimit-Unified-5h-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-5h-Reset", &in5),
            ("Anthropic-Ratelimit-Unified-7d-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-7d-Reset", &in7d),
        ]);
        near(reset(&h, now), Duration::from_secs(d7 as u64));

        // All allowed.
        let h = headers(&[
            ("Anthropic-Ratelimit-Unified-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-5h-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-5h-Reset", &in5),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-7d-Reset", &in7d),
        ]);
        assert_eq!(parse_claude_rate_limit_reset(&h, now), None);

        // Fable-only rejection ignores 7d_oi reset, unified reset and Retry-After.
        let fable = |extra: &[(&str, &str)], shared_7d: &'static str| {
            let mut pairs = vec![
                ("Anthropic-Ratelimit-Unified-Status", "rejected"),
                ("Anthropic-Ratelimit-Unified-5h-Status", "allowed"),
                ("Anthropic-Ratelimit-Unified-7d-Status", shared_7d),
                ("Anthropic-Ratelimit-Unified-7d_oi-Status", "rejected"),
            ];
            pairs.extend_from_slice(extra);
            headers(&pairs)
        };
        let extra = [("Anthropic-Ratelimit-Unified-7d_oi-Reset", in7d.as_str()), ("Anthropic-Ratelimit-Unified-Reset", in7d.as_str()), ("Retry-After", "60")];
        assert_eq!(reset(&fable(&extra, "allowed"), now), None);
        assert_eq!(reset(&fable(&extra[..2], "allowed"), now), None);
        assert_eq!(reset(&fable(&extra, "allowed_warning"), now), None);

        // Missing unified status with allowed_warning windows: unified reset ignored, Retry-After used.
        let h = headers(&[
            ("Anthropic-Ratelimit-Unified-5h-Status", "allowed_warning"),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed_warning"),
            ("Anthropic-Ratelimit-Unified-Reset", &in7d),
            ("Retry-After", "60"),
        ]);
        assert_eq!(reset(&h, now), Some(Duration::from_secs(60)));

        // Non-fable combined rejection keeps the 7d_oi reset.
        let h = headers(&[
            ("Anthropic-Ratelimit-Unified-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-5h-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-5h-Reset", &in5),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-7d_oi-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-7d_oi-Reset", &in7d),
        ]);
        near(reset(&h, now), Duration::from_secs(d7 as u64));

        // Past reset.
        let past = unix_after(now, -h5);
        let h = headers(&[("Anthropic-Ratelimit-Unified-5h-Status", "rejected"), ("Anthropic-Ratelimit-Unified-5h-Reset", &past)]);
        assert_eq!(parse_claude_rate_limit_reset(&h, now), None);
    }

    #[test]
    fn fuzz_is_bounded() {
        let now = Utc::now();
        let h = headers(&[("Retry-After", "100")]);
        for _ in 0..50 {
            let got = parse_claude_rate_limit_reset(&h, now).expect("cooldown");
            let diff = got - Duration::from_secs(100);
            assert!((Duration::from_secs(1)..=Duration::from_secs(30)).contains(&diff), "fuzz {diff:?}");
        }
        // The injected source is what gets added.
        let got = parse_claude_rate_limit_reset_with(&h, now, || Duration::from_secs(7));
        assert_eq!(got, Some(Duration::from_secs(107)));
    }

    #[test]
    fn unified_rejection_allowed_warning() {
        let cases: &[(&[(&str, &str)], bool)] = &[
            (&[("Status", "rejected"), ("5h-Status", "allowed"), ("7d-Status", "allowed"), ("7d_oi-Status", "rejected")], false),
            (&[("Status", "rejected"), ("5h-Status", "allowed"), ("7d-Status", "allowed_warning"), ("7d_oi-Status", "rejected")], false),
            (&[("Status", "rejected"), ("5h-Status", "allowed_warning"), ("7d-Status", "allowed"), ("7d_oi-Status", "rejected")], false),
            (&[("Status", "rejected"), ("5h-Status", "allowed_warning"), ("7d-Status", "allowed_warning"), ("7d_oi-Status", "rejected")], false),
            (&[("Status", "rejected"), ("5h-Status", "rejected"), ("7d-Status", "allowed_warning"), ("7d_oi-Status", "rejected")], true),
            (&[("Status", "rejected"), ("5h-Status", "allowed_warning"), ("7d-Status", "rejected"), ("7d_oi-Status", "rejected")], true),
        ];
        for (pairs, want) in cases {
            let named: Vec<(String, &str)> = pairs.iter().map(|(k, v)| (format!("Anthropic-Ratelimit-Unified-{k}"), *v)).collect();
            let refs: Vec<(&str, &str)> = named.iter().map(|(k, v)| (k.as_str(), *v)).collect();
            assert_eq!(claude_headers_indicate_unified_rate_limit_rejection(&headers(&refs)), *want, "{pairs:?}");
        }
    }

    #[test]
    fn overage_rejection_with_healthy_shared_window() {
        // Issue #5915 headers: 5h status omitted (utilization 0.00), 7d allowed, overage rejected.
        let base = [
            ("Anthropic-Ratelimit-Unified-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-Representative-Claim", "seven_day_overage_included"),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.69"),
            ("Anthropic-Ratelimit-Unified-7d_oi-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-Overage-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-Overage-Disabled-Reason", "org_spend_cap_reached"),
        ];
        let cases = [
            ("0.00", false),
            ("0.50", false),
            ("", true),
            ("invalid", true),
            ("NaN", true),
            ("+Inf", true),
            ("-Inf", true),
            ("-0.1", true),
            ("1.0", true),
            ("1.05", true),
        ];
        for (utilization, want) in cases {
            let mut pairs = base.to_vec();
            if !utilization.is_empty() {
                pairs.push(("Anthropic-Ratelimit-Unified-5h-Utilization", utilization));
            }
            assert_eq!(claude_headers_indicate_unified_rate_limit_rejection(&headers(&pairs)), want, "5h-utilization {utilization:?}");
        }
    }
}

//! GitHub API cooldown tracking (Go `github_rate_limit.go`).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, LazyLock};

use chrono::{DateTime, Duration, TimeZone, Utc};
use parking_lot::Mutex;

use crate::goturl::GoUrl;
use crate::http::Headers;
use crate::request_identity::request_identity;

/// Reports a GitHub API cooldown without exposing response bodies or credentials.
/// `retry_at` is also populated for requests blocked locally.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitError {
    pub status_code: u16,
    pub retry_at: DateTime<Utc>,
}

impl fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GitHub API rate limited; retry after {}",
            self.retry_at.format("%Y-%m-%dT%H:%M:%SZ")
        )
    }
}

impl std::error::Error for RateLimitError {}

impl RateLimitError {
    /// Seconds until `retry_at`, rounded up so clients never retry early.
    pub fn retry_after_seconds(&self, now: DateTime<Utc>) -> i64 {
        let delay = self.retry_at.signed_duration_since(now);
        if delay <= Duration::zero() {
            return 0;
        }
        let nanos = delay.num_nanoseconds().unwrap_or(i64::MAX);
        let seconds = nanos / 1_000_000_000;
        if nanos % 1_000_000_000 != 0 { seconds + 1 } else { seconds }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Cooldown {
    retry_at: Option<DateTime<Utc>>,
    status: u16,
    failures: u8,
}

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

#[derive(Default)]
struct State {
    entries: HashMap<String, Cooldown>,
    next_prune_at: Option<DateTime<Utc>>,
}

/// Shares cooldowns across repositories, release metadata and API asset downloads.
/// Clients without an explicit limiter share the process-wide default.
#[derive(Default)]
pub struct GitHubRateLimiter {
    state: Mutex<State>,
    now_func: Option<Clock>,
}

impl fmt::Debug for GitHubRateLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubRateLimiter").finish_non_exhaustive()
    }
}

pub(crate) static DEFAULT_GITHUB_RATE_LIMITER: LazyLock<Arc<GitHubRateLimiter>> =
    LazyLock::new(|| Arc::new(GitHubRateLimiter::default()));

fn before(a: DateTime<Utc>, b: Option<DateTime<Utc>>) -> bool {
    b.is_some_and(|b| a < b)
}

impl GitHubRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Limiter with an injected clock (tests).
    pub fn with_clock(now_func: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        Self { state: Mutex::default(), now_func: Some(Arc::new(now_func)) }
    }

    fn now(&self) -> DateTime<Utc> {
        match &self.now_func {
            Some(now) => now(),
            None => Utc::now(),
        }
    }

    /// Fails with the active cooldown for `key`, if any. Empty keys are never limited.
    pub fn check(&self, key: &str) -> Result<(), RateLimitError> {
        if key.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock();
        let now = self.now();
        prune_locked(&mut state, now);
        let entry = state.entries.get(key).copied().unwrap_or_default();
        if before(now, entry.retry_at) {
            return Err(RateLimitError {
                status_code: entry.status,
                retry_at: entry.retry_at.unwrap_or(now),
            });
        }
        Ok(())
    }

    /// Records a response. Even successful responses that consume the final request are
    /// recorded; a late success never clears a cooldown established by another request.
    pub fn observe(&self, key: &str, status: u16, headers: &Headers, body: &[u8]) -> Result<(), RateLimitError> {
        if key.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock();
        let now = self.now();
        prune_locked(&mut state, now);
        let mut entry = state.entries.get(key).copied().unwrap_or_default();
        let remaining_zero = headers.get("X-RateLimit-Remaining").trim() == "0";
        let mut retry_at = github_retry_after(headers.get("Retry-After"), now);
        let rate_limited = status == 429
            || (status == 403 && (remaining_zero || retry_at.is_some() || github_rate_limit_message(body)));
        if !rate_limited && !remaining_zero {
            if (200..300).contains(&status) && !before(now, entry.retry_at) {
                state.entries.remove(key);
            }
            return Ok(());
        }
        if remaining_zero {
            if let Ok(reset) = headers.get("X-RateLimit-Reset").trim().parse::<i64>() {
                if reset > 0 {
                    if let Some(reset_at) = Utc.timestamp_opt(reset, 0).single() {
                        if retry_at.is_none_or(|current| reset_at > current) {
                            retry_at = Some(reset_at);
                        }
                    }
                }
            }
        }
        let mut effective = retry_at.filter(|at| *at > now);
        if effective.is_none() {
            if !rate_limited {
                return Ok(());
            }
            // Headerless secondary limits start at one minute and back off up to an
            // hour. Only an actual upstream rejection increments this counter.
            let delay = Duration::minutes(1_i64 << entry.failures).min(Duration::hours(1));
            effective = Some(now + delay);
            if entry.failures < 6 {
                entry.failures += 1;
            }
        }
        if let Some(at) = effective {
            if entry.retry_at.is_none_or(|current| at > current) {
                entry.retry_at = Some(at);
            }
        }
        entry.status = if rate_limited { status } else { 429 };
        state.entries.insert(key.to_string(), entry);
        if rate_limited {
            return Err(RateLimitError {
                status_code: status,
                retry_at: entry.retry_at.unwrap_or(now),
            });
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn has_entry(&self, key: &str) -> bool {
        self.state.lock().entries.contains_key(key)
    }

    #[cfg(test)]
    pub(crate) fn remove_entry(&self, key: &str) {
        self.state.lock().entries.remove(key);
    }
}

/// Drops inactive identities after a grace period so credential and proxy rotation
/// cannot leave cooldown state behind. Recently expired entries are retained to
/// preserve secondary-limit backoff across retries.
fn prune_locked(state: &mut State, now: DateTime<Utc>) {
    if before(now, state.next_prune_at) {
        return;
    }
    state.entries.retain(|_, entry| match entry.retry_at {
        Some(at) => now < at + Duration::hours(1),
        None => false,
    });
    state.next_prune_at = Some(now + Duration::hours(1));
}

/// Key of the `api.github.com` quota bucket for a request, or empty for other hosts.
pub(crate) fn github_rate_limit_key(
    request_url: &str,
    network_scope: &str,
    headers: &Headers,
    authenticated: bool,
) -> String {
    let Ok(parsed) = GoUrl::parse(request_url) else {
        return String::new();
    };
    let port = parsed.port();
    if !parsed.scheme.eq_ignore_ascii_case("https")
        || !parsed.hostname().eq_ignore_ascii_case("api.github.com")
        || (!port.is_empty() && port != "443")
    {
        return String::new();
    }
    format!("api.github.com/{}", request_identity(network_scope, headers, authenticated))
}

/// `Retry-After` as delta seconds or an HTTP date; `None` when absent or invalid.
fn github_retry_after(value: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<i64>() {
        if seconds >= 0 && seconds <= i64::MAX / 1_000_000_000 {
            return Duration::try_seconds(seconds).and_then(|d| now.checked_add_signed(d));
        }
    }
    httpdate::parse_http_date(value).ok().map(DateTime::<Utc>::from)
}

/// Whether a 403 body carries one of GitHub's rate-limit messages.
fn github_rate_limit_message(body: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Message {
        #[serde(default, deserialize_with = "crate::registry::null_default")]
        message: String,
    }
    let Ok(parsed) = serde_json::from_slice::<Message>(body) else {
        return false;
    };
    let text = parsed.message.to_lowercase();
    text.contains("secondary rate limit")
        || text.contains("api rate limit exceeded")
        || text.contains("abuse detection")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[(&str, &str)]) -> Headers {
        let mut headers = Headers::new();
        for (name, value) in values {
            headers.set(name, *value);
        }
        headers
    }

    fn base_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().expect("valid time")
    }

    fn shared_clock() -> (Arc<Mutex<DateTime<Utc>>>, GitHubRateLimiter) {
        let now = Arc::new(Mutex::new(base_time()));
        let handle = now.clone();
        (now, GitHubRateLimiter::with_clock(move || *handle.lock()))
    }

    #[test]
    fn rate_limit_headers() {
        let now = base_time();
        let reset = (now + Duration::hours(1)).timestamp().to_string();
        let http_date = |d: Duration| httpdate::fmt_http_date((now + d).into());
        struct Case {
            name: &'static str,
            status: u16,
            headers: Vec<(&'static str, String)>,
            body: &'static str,
            wait: Option<Duration>,
        }
        let case = |name, status, headers: Vec<(&'static str, String)>, body, wait| Case { name, status, headers, body, wait };
        let cases = vec![
            case("primary forbidden", 403, vec![("X-RateLimit-Remaining", "0".into()), ("X-RateLimit-Reset", reset.clone())], "", Some(Duration::hours(1))),
            case("retry seconds", 429, vec![("Retry-After", "120".into())], "", Some(Duration::minutes(2))),
            case("retry date", 429, vec![("Retry-After", http_date(Duration::minutes(3)))], "", Some(Duration::minutes(3))),
            case("reset wins", 403, vec![("Retry-After", "120".into()), ("X-RateLimit-Remaining", "0".into()), ("X-RateLimit-Reset", reset.clone())], "", Some(Duration::hours(1))),
            case("retry wins", 429, vec![("Retry-After", "7200".into()), ("X-RateLimit-Remaining", "0".into()), ("X-RateLimit-Reset", reset.clone())], "", Some(Duration::hours(2))),
            case("secondary message", 403, vec![], r#"{"message":"You have exceeded a secondary rate limit."}"#, Some(Duration::minutes(1))),
            case("abuse message", 403, vec![], r#"{"message":"You have triggered an abuse detection mechanism."}"#, Some(Duration::minutes(1))),
            case("retry forbidden", 403, vec![("Retry-After", "60".into())], "", Some(Duration::minutes(1))),
            case("headerless too many requests", 429, vec![], "", Some(Duration::minutes(1))),
            case("invalid retry", 429, vec![("Retry-After", "invalid".into())], "", Some(Duration::minutes(1))),
            case("overflow retry", 429, vec![("Retry-After", "9223372036854775807".into())], "", Some(Duration::minutes(1))),
            case("negative retry", 429, vec![("Retry-After", "-10".into())], "", Some(Duration::minutes(1))),
            case("past reset", 403, vec![("X-RateLimit-Remaining", "0".into()), ("X-RateLimit-Reset", "1".into())], "", Some(Duration::minutes(1))),
            case("invalid reset", 403, vec![("X-RateLimit-Remaining", "0".into()), ("X-RateLimit-Reset", "invalid".into())], "", Some(Duration::minutes(1))),
            case("permission denied", 403, vec![], r#"{"message":"Resource not accessible by integration"}"#, None),
            case("invalid body", 403, vec![], "forbidden", None),
            case("not found", 404, vec![], "", None),
        ];
        for tt in cases {
            let limiter = GitHubRateLimiter::with_clock(move || now);
            let pairs: Vec<(&str, &str)> = tt.headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let result = limiter.observe("quota", tt.status, &headers(&pairs), tt.body.as_bytes());
            match tt.wait {
                None => {
                    assert!(result.is_ok() && limiter.check("quota").is_ok(), "{}: started cooldown", tt.name);
                }
                Some(wait) => {
                    let err = result.expect_err(tt.name);
                    assert_eq!(err.status_code, tt.status, "{}", tt.name);
                    assert_eq!(err.retry_at, now + wait, "{}", tt.name);
                    assert!(limiter.check("quota").is_err(), "{}: check should see cooldown", tt.name);
                }
            }
        }
    }

    #[test]
    fn key_canonical_host() {
        let none = Headers::new();
        let want = github_rate_limit_key("https://api.github.com/repos/owner/repo", "", &none, false);
        assert!(!want.is_empty());
        for url in ["https://api.github.com:443/repos/other/repo", "https://API.GITHUB.COM/releases"] {
            assert_eq!(github_rate_limit_key(url, "", &none, false), want, "{url}");
        }
        for url in [
            "https://api.github.com:444/repos/owner/repo",
            "http://api.github.com/",
            "https://api.github.com.example/",
            "https://raw.githubusercontent.com/",
        ] {
            assert_eq!(github_rate_limit_key(url, "", &none, false), "", "{url}");
        }
    }

    #[test]
    fn prunes_inactive_identities() {
        let (now, limiter) = shared_clock();
        let none = Headers::new();
        let _ = limiter.observe("old-token", 429, &none, b"");
        let _ = limiter.observe("active-token", 429, &headers(&[("Retry-After", "7200")]), b"");
        *now.lock() += Duration::hours(1) + Duration::minutes(1);
        let _ = limiter.check("new-token");
        assert!(!limiter.has_entry("old-token"), "inactive credential cooldown was not pruned");
        assert!(limiter.check("active-token").is_err(), "active cooldown was pruned");
    }

    #[test]
    fn backoff_and_recovery() {
        let (now, limiter) = shared_clock();
        let none = Headers::new();
        let delays = [1, 2, 4, 8, 16, 32, 60, 60].map(Duration::minutes);
        for delay in delays {
            let err = limiter.observe("quota", 429, &none, b"").expect_err("backoff");
            assert_eq!(err.retry_at, *now.lock() + delay);
            for _ in 0..5 {
                assert!(limiter.check("quota").is_err(), "local check lost cooldown");
            }
            *now.lock() += delay;
            assert!(limiter.check("quota").is_ok(), "expired cooldown");
        }
        let _ = limiter.observe("quota", 200, &none, b"");
        let err = limiter.observe("quota", 429, &none, b"").expect_err("reset backoff");
        assert_eq!(err.retry_at, *now.lock() + Duration::minutes(1), "success did not reset backoff");
    }

    #[test]
    fn successful_exhaustion_and_late_responses() {
        let (now, limiter) = shared_clock();
        let none = Headers::new();
        let reset = (*now.lock() + Duration::hours(1)).timestamp().to_string();
        let exhausted = headers(&[("X-RateLimit-Remaining", "0"), ("X-RateLimit-Reset", reset.as_str())]);
        assert!(limiter.observe("quota", 200, &exhausted, b"").is_ok(), "final successful request failed");
        let _ = limiter.observe("quota", 200, &none, b"");
        let _ = limiter.observe("quota", 429, &headers(&[("Retry-After", "60")]), b"");
        let err = limiter.check("quota").expect_err("cooldown");
        assert_eq!(err.retry_at, *now.lock() + Duration::hours(1), "late response shortened cooldown");
    }

    #[test]
    fn retry_after_rounds_up() {
        let now = Utc.timestamp_opt(100, 0).single().expect("time");
        let cases = [
            (Duration::seconds(-1), 0),
            (Duration::zero(), 0),
            (Duration::nanoseconds(1), 1),
            (Duration::seconds(1), 1),
            (Duration::seconds(1) + Duration::nanoseconds(1), 2),
        ];
        for (delay, want) in cases {
            let err = RateLimitError { status_code: 0, retry_at: now + delay };
            assert_eq!(err.retry_after_seconds(now), want, "{delay:?}");
        }
    }
}

//! Management access control (Go: `Handler.Middleware`, `AuthenticateManagementKey`,
//! `managementAvailabilityMiddleware`) and the global CORS middleware.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::http::{empty, json_response};
use crate::state::{AttemptInfo, ManagementState};

const MAX_FAILURES: u32 = 5;
const BAN_DURATION: Duration = Duration::from_secs(30 * 60);
/// Idle entries older than this are purged (Go: `attemptMaxIdleTime`).
const ATTEMPT_MAX_IDLE: Duration = Duration::from_secs(2 * 3600);
const ATTEMPT_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

const CORS_EXPOSED_HEADERS: &str = "X-CPA-TRACE-ID, X-CPA-VERSION, X-CPA-COMMIT, X-CPA-BUILD-DATE, X-CPA-SUPPORT-PLUGIN, \
X-CPA-HOME-VERSION, X-CPA-HOME-BUILD-DATE, X-SERVER-VERSION, X-SERVER-BUILD-DATE, Location, Retry-After, X-Request-Id, OpenAI-Request-Id";

/// Global CORS: wildcard origin, `OPTIONS` answered with 204 (Go: `corsMiddleware`).
pub(crate) async fn cors(req: Request, next: Next) -> Response {
    let preflight = req.method() == Method::OPTIONS;
    let mut resp = if preflight { empty(204) } else { next.run(req).await };
    let h = resp.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static(CORS_EXPOSED_HEADERS));
    resp
}

/// Bare 404 when management is disabled: Home mode, or no secret configured anywhere.
pub(crate) async fn availability(State(st): State<ManagementState>, req: Request, next: Next) -> Response {
    if !management_available(&st) {
        return empty(404);
    }
    next.run(req).await
}

fn management_available(st: &ManagementState) -> bool {
    let cfg = st.cfg();
    if cfg.home.enabled {
        return false;
    }
    !cfg.remote_management.secret_key.is_empty() || st.env_secret.is_some() || st.local_password.is_some()
}

/// Management key check. Sets the `X-CPA-*` headers on every response, authenticated or not.
pub(crate) async fn authenticate(State(st): State<ManagementState>, req: Request, next: Next) -> Response {
    let ip = client_ip(&st, &req);
    let local = ip == "127.0.0.1" || ip == "::1";
    let provided = provided_key(req.headers());
    let denied = authenticate_key(&st, &ip, local, &provided).await;
    let mut resp = match denied {
        Some((status, msg)) => json_response(status, &json!({"error": msg})),
        None => next.run(req).await,
    };
    set_cpa_headers(&st, resp.headers_mut());
    resp
}

fn set_cpa_headers(st: &ManagementState, h: &mut HeaderMap) {
    let pairs = [
        ("x-cpa-version", st.build.version.as_str()),
        ("x-cpa-commit", st.build.commit.as_str()),
        ("x-cpa-build-date", st.build.build_date.as_str()),
        // No plugin host in this build.
        ("x-cpa-support-plugin", "0"),
    ];
    for (name, value) in pairs {
        if let Ok(v) = HeaderValue::from_str(value) {
            h.insert(HeaderName::from_static(name), v);
        }
    }
}

/// `Authorization: Bearer <key>` (any other `Authorization` value is taken whole), else
/// `X-Management-Key`.
fn provided_key(headers: &HeaderMap) -> String {
    let mut provided = String::new();
    if let Some(ah) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()) {
        provided = match ah.split_once(' ') {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.to_string(),
            _ => ah.to_string(),
        };
    }
    if provided.is_empty() {
        provided = headers.get("x-management-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    }
    provided
}

/// Returns `Some((status, message))` when the request is refused.
async fn authenticate_key(st: &ManagementState, ip: &str, local: bool, provided: &str) -> Option<(u16, String)> {
    let cfg = st.cfg();
    let allow_remote = cfg.remote_management.allow_remote || st.env_secret.is_some();
    let secret_hash = cfg.remote_management.secret_key.clone();
    let now = Instant::now();

    {
        let mut attempts = st.shared.attempts.lock();
        if let Some(ai) = attempts.get_mut(ip)
            && let Some(until) = ai.blocked_until
        {
            if now < until {
                let remaining = until - now;
                return Some((403, format!("IP banned due to too many failed attempts. Try again in {}", go_duration(remaining))));
            }
            ai.blocked_until = None;
            ai.count = 0;
        }
    }

    if !local && !allow_remote {
        return Some((403, "remote management disabled".into()));
    }
    if secret_hash.is_empty() && st.env_secret.is_none() {
        return Some((403, "remote management key not set".into()));
    }
    if provided.is_empty() {
        record_failure(st, ip);
        return Some((401, "missing management key".into()));
    }

    let ct_eq = |a: &str, b: &str| a.as_bytes().ct_eq(b.as_bytes()).unwrap_u8() == 1;
    let accepted = (local && st.local_password.as_deref().is_some_and(|lp| ct_eq(provided, lp)))
        || st.env_secret.as_deref().is_some_and(|es| ct_eq(provided, es))
        || (!secret_hash.is_empty() && bcrypt_matches(provided, &secret_hash).await);
    if accepted {
        reset_failures(st, ip);
        return None;
    }
    record_failure(st, ip);
    Some((401, "invalid management key".into()))
}

/// bcrypt at cost 10 takes tens of milliseconds; keep it off the async workers.
async fn bcrypt_matches(provided: &str, hash: &str) -> bool {
    let (provided, hash) = (provided.to_string(), hash.to_string());
    tokio::task::spawn_blocking(move || bcrypt::non_truncating_verify(provided, &hash).unwrap_or(false))
        .await
        .unwrap_or(false)
}

fn record_failure(st: &ManagementState, ip: &str) {
    let now = Instant::now();
    let mut attempts = st.shared.attempts.lock();
    purge_stale(st, &mut attempts, now);
    let ai = attempts.entry(ip.to_string()).or_default();
    ai.count += 1;
    ai.last_activity = Some(now);
    if ai.count >= MAX_FAILURES {
        ai.blocked_until = Some(now + BAN_DURATION);
        ai.count = 0;
    }
}

fn reset_failures(st: &ManagementState, ip: &str) {
    if let Some(ai) = st.shared.attempts.lock().get_mut(ip) {
        ai.count = 0;
        ai.blocked_until = None;
    }
}

/// Drops entries that are not banned and idle for 2h, at most hourly (Go: a cleanup goroutine).
fn purge_stale(st: &ManagementState, attempts: &mut std::collections::HashMap<String, AttemptInfo>, now: Instant) {
    let mut last = st.shared.last_purge.lock();
    if last.is_some_and(|t| now.duration_since(t) < ATTEMPT_CLEANUP_INTERVAL) {
        return;
    }
    *last = Some(now);
    attempts.retain(|_, ai| {
        ai.blocked_until.is_some_and(|until| now < until)
            || ai.last_activity.is_some_and(|t| now.duration_since(t) <= ATTEMPT_MAX_IDLE)
    });
}

/// `time.Duration.String()` of a duration rounded to whole seconds (`29m59s`, `1h0m0s`, `45s`).
fn go_duration(d: Duration) -> String {
    let secs = d.as_secs_f64().round() as u64;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// Client IP like gin's `ClientIP()` with `trusted-proxies`: the peer address, replaced by the
/// forwarded address (`X-Forwarded-For`, then `X-Real-IP`) only when the peer is a trusted proxy.
/// Requests without connection info (no `ConnectInfo<SocketAddr>` layer) have an unknown IP.
fn client_ip(st: &ManagementState, req: &Request) -> String {
    let Some(peer) = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_canonical()) else {
        return "unknown".into();
    };
    let cfg = st.cfg();
    let proxies = parse_trusted(&cfg.trusted_proxies);
    if is_trusted(peer, &proxies) {
        for name in ["x-forwarded-for", "x-real-ip"] {
            if let Some(ip) = forwarded_client(req.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or(""), &proxies) {
                return ip;
            }
        }
    }
    peer.to_string()
}

/// gin `validateHeader`: walks the list from the right until the first untrusted address.
fn forwarded_client(header: &str, proxies: &[Cidr]) -> Option<String> {
    if header.is_empty() {
        return None;
    }
    let items: Vec<&str> = header.split(',').collect();
    for (i, item) in items.iter().enumerate().rev() {
        let item = item.trim();
        let ip: IpAddr = item.parse().ok()?;
        if i == 0 || !is_trusted(ip.to_canonical(), proxies) {
            return Some(ip.to_canonical().to_string());
        }
    }
    None
}

struct Cidr {
    net: IpAddr,
    prefix: u32,
}

fn parse_trusted(entries: &[String]) -> Vec<Cidr> {
    entries
        .iter()
        .filter_map(|e| {
            let e = e.trim();
            match e.split_once('/') {
                Some((ip, prefix)) => Some(Cidr { net: ip.parse().ok()?, prefix: prefix.parse().ok()? }),
                None => {
                    let ip: IpAddr = e.parse().ok()?;
                    Some(Cidr { net: ip, prefix: if ip.is_ipv4() { 32 } else { 128 } })
                }
            }
        })
        .collect()
}

fn is_trusted(ip: IpAddr, proxies: &[Cidr]) -> bool {
    proxies.iter().any(|c| match (ip, c.net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => prefix_eq(&a.octets(), &b.octets(), c.prefix),
        (IpAddr::V6(a), IpAddr::V6(b)) => prefix_eq(&a.octets(), &b.octets(), c.prefix),
        _ => false,
    })
}

fn prefix_eq(a: &[u8], b: &[u8], prefix: u32) -> bool {
    let full = (prefix / 8) as usize;
    let rest = prefix % 8;
    if full > a.len() || a[..full] != b[..full] {
        return false;
    }
    rest == 0 || full >= a.len() || (a[full] ^ b[full]) >> (8 - rest) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_duration_formats_like_go() {
        assert_eq!(go_duration(Duration::from_secs(1799)), "29m59s");
        assert_eq!(go_duration(Duration::from_secs(1800)), "30m0s");
        assert_eq!(go_duration(Duration::from_secs(45)), "45s");
        assert_eq!(go_duration(Duration::from_secs(3600)), "1h0m0s");
    }

    #[test]
    fn forwarded_for_skips_trusted_proxies_from_the_right() {
        let proxies = parse_trusted(&["10.0.0.0/8".into()]);
        assert_eq!(forwarded_client("203.0.113.9, 10.1.1.1", &proxies).as_deref(), Some("203.0.113.9"));
        assert_eq!(forwarded_client("198.51.100.1, 203.0.113.9, 10.1.1.1", &proxies).as_deref(), Some("203.0.113.9"));
        assert_eq!(forwarded_client("garbage", &proxies), None);
        assert!(is_trusted("10.200.0.1".parse().unwrap(), &proxies));
        assert!(!is_trusted("11.0.0.1".parse().unwrap(), &proxies));
    }
}

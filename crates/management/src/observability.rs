//! Usage endpoints: per-API-key request buckets (Go: `api_key_usage.go`), the read-only usage
//! summary and request feed from ui/API_EXTENSIONS.md, and the (unsupported) destructive usage
//! queue.

use std::collections::BTreeMap;

use axum::extract::{Request, State};
use cpa_auth::types::RecentRequestBucket;
use serde::Serialize;

use crate::http::{ApiError, ApiResult, no_store, ok_json, ok_struct, query_get};
use crate::state::ManagementState;

#[derive(Serialize)]
struct ApiKeyUsageEntry {
    success: i64,
    failed: i64,
    recent_requests: Vec<RecentRequestBucket>,
}

fn merge_buckets(dst: &mut Vec<RecentRequestBucket>, src: &[RecentRequestBucket]) {
    if dst.is_empty() {
        dst.extend_from_slice(src);
        return;
    }
    for (d, s) in dst.iter_mut().zip(src) {
        d.success += s.success;
        d.failed += s.failed;
    }
}

/// `GET /observability/usage/api-keys`: recent request buckets of every API-key credential,
/// `provider -> "<base_url>|<api_key>" -> usage`.
pub(crate) async fn api_key_usage(State(st): State<ManagementState>) -> ApiResult {
    let now = chrono::Utc::now();
    let mut out: BTreeMap<String, BTreeMap<String, ApiKeyUsageEntry>> = BTreeMap::new();
    for auth in st.registry.list() {
        let (kind, api_key) = auth.account_info();
        let api_key = api_key.trim().to_string();
        if !kind.eq_ignore_ascii_case("api_key") || api_key.is_empty() {
            continue;
        }
        let base_url = [auth.attr("base_url"), auth.attr("base-url")]
            .into_iter()
            .find(|b| !b.is_empty())
            .unwrap_or_default();
        let composite = format!("{base_url}|{api_key}");
        let compat = auth.attr("compat_name");
        let provider = if compat.is_empty() {
            auth.provider.trim().to_lowercase()
        } else {
            compat.to_lowercase()
        };
        let provider = if provider.is_empty() {
            "unknown".to_string()
        } else {
            provider
        };
        let recent = auth.recent_requests_snapshot(now);
        match out.entry(provider).or_default().entry(composite) {
            std::collections::btree_map::Entry::Occupied(mut e) => {
                let e = e.get_mut();
                e.success += auth.success;
                e.failed += auth.failed;
                merge_buckets(&mut e.recent_requests, &recent);
            }
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(ApiKeyUsageEntry {
                    success: auth.success,
                    failed: auth.failed,
                    recent_requests: recent,
                });
            }
        }
    }
    Ok(ok_json(&out))
}

fn require_usage_statistics(st: &ManagementState) -> ApiResult<()> {
    if st.cfg().usage_statistics_enabled {
        Ok(())
    } else {
        Err(ApiError::new(404, "usage_statistics_disabled"))
    }
}

/// `GET /observability/usage/summary`.
pub(crate) async fn usage_summary(State(st): State<ManagementState>) -> ApiResult {
    require_usage_statistics(&st)?;
    Ok(no_store(ok_struct(&st.usage.summary())))
}

/// `limit`: default 100, non-integer values use the default; integers are clamped to 1..=1000.
fn parse_limit(raw: Option<String>) -> usize {
    const DEFAULT: usize = 100;
    let Some(raw) = raw else { return DEFAULT };
    let raw = raw.trim();
    raw.parse::<i64>()
        .map_or(DEFAULT, |n| n.clamp(1, 1000) as usize)
}

/// `GET /observability/requests?limit=&after=`.
pub(crate) async fn usage_requests(State(st): State<ManagementState>, req: Request) -> ApiResult {
    require_usage_statistics(&st)?;
    let after = match query_get(req.uri(), "after") {
        None => None,
        Some(raw) => Some(
            raw.parse::<u64>()
                .map_err(|_| ApiError::bad_request("invalid_after"))?,
        ),
    };
    let limit = parse_limit(query_get(req.uri(), "limit"));
    Ok(no_store(ok_struct(&st.usage.requests(limit, after))))
}

/// Go: `parseUsageQueueCount`: empty is 1, anything else must be a positive integer.
fn parse_queue_count(raw: &str) -> Result<usize, ApiError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(1);
    }
    match raw.parse::<i64>() {
        Ok(n) if n > 0 => Ok(usize::try_from(n).unwrap_or(usize::MAX)),
        _ => Err(ApiError::bad_request("count must be a positive integer")),
    }
}

/// `GET /usage-queue` and `GET /observability/usage/queue`: pops the oldest queued usage records
/// (`?count=`, default 1) from the process-wide usage queue (Go: `GetUsageQueue`). Records that
/// are not valid JSON are returned as strings.
pub(crate) async fn usage_queue(req: Request) -> ApiResult {
    let count = parse_queue_count(&query_get(req.uri(), "count").unwrap_or_default())?;
    let records: Vec<serde_json::Value> = cpa_home::queue::pop_oldest(count)
        .into_iter()
        .map(|item| {
            serde_json::from_slice(&item)
                .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&item).into_owned()))
        })
        .collect();
    Ok(ok_json(&records))
}

//! Usage endpoints: per-API-key request buckets (Go: `api_key_usage.go`), the read-only usage
//! summary and request feed from ui/API_EXTENSIONS.md, and the (unsupported) destructive usage
//! queue.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Request, State};
use cpa_auth::types::RecentRequestBucket;
use cpa_runtime::usage::UsageEvent;
use serde::Serialize;
use serde_json::json;

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

/// Go executor type names (`reflect.Type.Name`) by provider id, as the usage queue reports them.
fn go_executor_type(provider: &str) -> &'static str {
    match provider {
        "claude" => "ClaudeExecutor",
        "codex" => "CodexAutoExecutor",
        "xai" => "XAIAutoExecutor",
        "gemini" | "gemini-interactions" => "GeminiExecutor",
        "vertex" => "GeminiVertexExecutor",
        "aistudio" => "AIStudioExecutor",
        "antigravity" => "AntigravityExecutor",
        "kimi" | "kimi-ai" => "KimiExecutor",
        "devin" => "DevinExecutor",
        "meta" => "MetaExecutor",
        _ => "OpenAICompatExecutor",
    }
}

/// One usage event in the shape of Go's `redisqueue` record (`queuedUsageDetail`). Fields the
/// usage tracker does not collect (client address, response headers, session ids, token
/// breakdown quality) are filled with their zero values.
fn queue_record(e: &UsageEvent, sources: &HashMap<String, String>) -> serde_json::Value {
    let r = &e.record;
    let t = &r.tokens;
    let cache_read = t.cached_tokens;
    let uncached = (t.input_tokens - cache_read).max(0);
    let non_reasoning = (t.output_tokens - t.reasoning_tokens).max(0);
    let non_empty = |s: &str, default: &str| {
        if s.trim().is_empty() {
            default.to_string()
        } else {
            s.trim().to_string()
        }
    };
    let model = non_empty(&r.model, "unknown");
    let alias = non_empty(&r.alias, &model);
    let fail = if r.failed {
        let status = if r.fail.status_code == 0 {
            500
        } else {
            r.fail.status_code
        };
        json!({"status_code": status, "body": r.fail.body.trim()})
    } else {
        json!({"status_code": 200, "body": ""})
    };
    let mut m = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| {
        m.insert(k.to_string(), v);
    };
    put("timestamp", crate::http::rfc3339(r.timestamp).into());
    put("latency_ms", r.latency_ms.into());
    put("ttft_ms", r.ttft_ms.into());
    // Go reports the credential's account (API key or e-mail) as the source.
    let source = sources
        .get(&r.auth_index)
        .filter(|s| !s.is_empty())
        .unwrap_or(&r.source);
    put("source", source.clone().into());
    put("auth_index", r.auth_index.clone().into());
    put("client_ip", "".into());
    put("resolved_client_ip", "".into());
    put("x_forwarded_for", "".into());
    put("user_agent", "".into());
    put(
        "tokens",
        json!({
            "input_tokens": t.input_tokens,
            "output_tokens": t.output_tokens,
            "reasoning_tokens": t.reasoning_tokens,
            "cached_tokens": t.cached_tokens,
            "cache_read_tokens": cache_read,
            "cache_read_tokens_present": true,
            "cache_creation_tokens": 0,
            "total_tokens": t.total_tokens,
        }),
    );
    put("failed", r.failed.into());
    put("generate", true.into());
    put("stream", r.stream.into());
    put("fail", fail);
    put("accounting_version", 2.into());
    put(
        "token_breakdown",
        json!({
            "schema_version": 2,
            "quality": "complete",
            "total_tokens": t.total_tokens,
            "input": {
                "total_tokens": t.input_tokens,
                "uncached_tokens": uncached,
                "cache_read_tokens": cache_read,
                "cache_write_tokens": 0,
            },
            "output": {
                "total_tokens": t.output_tokens,
                "non_reasoning_tokens": non_reasoning,
                "reasoning_tokens": t.reasoning_tokens,
            },
            "unclassified_tokens": 0,
        }),
    );
    put("provider", non_empty(&r.provider, "unknown").into());
    put("executor_type", go_executor_type(r.provider.trim()).into());
    put("model", model.into());
    put("alias", alias.into());
    put("endpoint", r.endpoint.clone().into());
    put("auth_type", non_empty(&r.auth_type, "unknown").into());
    put("api_key", r.api_key.trim().into());
    put("request_id", r.request_id.clone().into());
    put("execution_id", uuid::Uuid::new_v4().to_string().into());
    if !r.request_id.is_empty() {
        put("trace_id", r.request_id.clone().into());
    }
    put("reasoning_effort", "".into());
    put("service_tier", "auto".into());
    if !r.failed {
        put("response_model", r.model.trim().into());
    }
    serde_json::Value::Object(m)
}

/// `GET /usage-queue` and `GET /observability/usage/queue`: pops the oldest unread usage events
/// (`?count=`, default 1). Go keeps these in an in-memory queue that expires entries after
/// `redis-usage-queue-retention-seconds`; here the tracker's ring buffer is the source and a
/// shared cursor marks what was already popped. Nothing is queued while usage statistics are off.
pub(crate) async fn usage_queue(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let count = parse_queue_count(&query_get(req.uri(), "count").unwrap_or_default())?;
    let cfg = st.cfg();
    if !cfg.usage_statistics_enabled {
        return Ok(ok_json(&json!([])));
    }
    let retention = match cfg.redis_usage_queue_retention_seconds {
        n if n <= 0 => 60,
        n => n.min(3600),
    };
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(retention);
    let sources: HashMap<String, String> = st
        .registry
        .list()
        .into_iter()
        .map(|mut a| (a.ensure_index(), a.account_info().1))
        .collect();
    let mut cursor = st.shared.usage_queue_cursor.lock();
    let page = st.usage.requests(count, Some(*cursor));
    if let Some(last) = page.events.last() {
        *cursor = last.seq;
    }
    let records: Vec<serde_json::Value> = page
        .events
        .iter()
        .filter(|e| {
            e.record.timestamp + chrono::Duration::milliseconds(e.record.latency_ms) >= cutoff
        })
        .map(|e| queue_record(e, &sources))
        .collect();
    Ok(ok_json(&records))
}

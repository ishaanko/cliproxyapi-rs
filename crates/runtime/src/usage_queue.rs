//! Usage queue record (Go: `redisqueue/plugin.go` `usageQueuePlugin.HandleUsage`): turns a
//! finished-attempt [`UsageRecord`] into the JSON payload the Redis-protocol usage output and
//! `GET /usage-queue` deliver, and feeds it to the process-wide queue in `cpa_home::queue`.

use std::sync::Arc;

use chrono::{Local, SecondsFormat};
use serde_json::{Map, Value, json};

use crate::usage::{UsageRecord, UsageTracker};

/// `http.Header` as JSON: canonical header names mapped to value lists, keys sorted.
fn response_headers_json(headers: &http::HeaderMap) -> Value {
    let mut map: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for (name, value) in headers {
        let canonical = name
            .as_str()
            .split('-')
            .map(|part| {
                let mut chars = part.chars();
                chars
                    .next()
                    .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("-");
        map.entry(canonical)
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    json!(map)
}

/// The queue payload of one record, with Go's field order. Fields the tracker does not collect
/// (cache-creation tokens, response service tier) carry their defaults.
pub fn queue_payload(r: &UsageRecord) -> Vec<u8> {
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
        let status = if r.fail.status_code == 0 { 500 } else { r.fail.status_code };
        json!({"status_code": status, "body": r.fail.body.trim()})
    } else {
        json!({"status_code": 200, "body": ""})
    };
    let mut m = Map::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_string(), v);
    };
    put(
        "timestamp",
        r.timestamp
            .with_timezone(&Local)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true)
            .into(),
    );
    put("latency_ms", r.latency_ms.into());
    put("ttft_ms", r.ttft_ms.into());
    let x = &r.extra;
    // Go reports the credential's account (API key or e-mail) as the source.
    let source = if x.queue_source.is_empty() { &r.source } else { &x.queue_source };
    put("source", source.clone().into());
    put("auth_index", r.auth_index.clone().into());
    if !x.access_token_sha256.is_empty() {
        put("access_token_sha256", x.access_token_sha256.clone().into());
    }
    put("client_ip", x.client_ip.clone().into());
    put("resolved_client_ip", x.resolved_client_ip.clone().into());
    put("x_forwarded_for", x.x_forwarded_for.clone().into());
    put("user_agent", x.user_agent.clone().into());
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
    put("generate", x.generate.unwrap_or(true).into());
    put("stream", r.stream.into());
    put("fail", fail);
    if !x.response_headers.is_empty() {
        put("response_headers", response_headers_json(&x.response_headers));
    }
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
    put("executor_type", non_empty(&r.executor_type, "unknown").into());
    put("model", model.into());
    put("alias", alias.into());
    put("endpoint", r.endpoint.clone().into());
    put("auth_type", non_empty(&r.auth_type, "unknown").into());
    put("api_key", r.api_key.trim().into());
    put("request_id", r.request_id.clone().into());
    put("execution_id", uuid::Uuid::new_v4().to_string().into());
    let trace_id = if x.trace_id.is_empty() { r.request_id.as_str() } else { x.trace_id.as_str() };
    if !trace_id.is_empty() {
        put("trace_id", trace_id.into());
    }
    if !x.session_id.is_empty() {
        put("session_id", x.session_id.clone().into());
    }
    if !x.parent_session_id.is_empty() {
        put("parent_session_id", x.parent_session_id.clone().into());
    }
    if !x.node_kind.is_empty() {
        put("node_kind", x.node_kind.clone().into());
    }
    if x.is_fork {
        put("is_fork", true.into());
    }
    if x.is_compaction {
        put("is_compaction", true.into());
    }
    put("reasoning_effort", x.reasoning_effort.clone().unwrap_or_default().into());
    put("service_tier", x.service_tier.clone().unwrap_or_else(|| "auto".into()).into());
    if !r.failed {
        put("response_model", r.model.trim().into());
    }
    serde_json::to_vec(&Value::Object(m)).unwrap_or_default()
}

/// Installs the queue as a sink of `tracker`: every recorded usage event is also published to the
/// Redis-protocol usage queue (a no-op while the queue is disabled).
pub fn install(tracker: &UsageTracker) {
    tracker.set_sink(Some(Arc::new(|record: &UsageRecord| {
        if !cpa_home::queue::enabled() || !cpa_home::queue::usage_statistics_enabled() {
            return;
        }
        cpa_home::queue::enqueue(&queue_payload(record));
    })));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(r: &UsageRecord) -> Value {
        serde_json::from_slice(&queue_payload(r)).expect("payload is JSON")
    }

    // Go: TestUsageQueuePluginPayloadIncludesGenerateFalse / DefaultsGenerateTrue / ServiceTier.
    #[test]
    fn request_facts_flow_into_the_payload() {
        let mut r = UsageRecord::default();
        let p = payload(&r);
        assert_eq!((p["generate"].clone(), p["service_tier"].clone(), p["reasoning_effort"].clone()), (true.into(), "auto".into(), "".into()));
        assert!(p.get("access_token_sha256").is_none() && p.get("node_kind").is_none() && p.get("is_fork").is_none());

        r.extra.generate = Some(false);
        r.extra.service_tier = Some("priority".into());
        r.extra.reasoning_effort = Some("high".into());
        r.extra.access_token_sha256 = "abc".into();
        r.extra.session_id = "6ae58c5c-b8ab-81a6-ab97-26da30358da1".into();
        r.extra.node_kind = "fork".into();
        r.extra.is_fork = true;
        r.extra.is_compaction = true;
        let p = payload(&r);
        assert_eq!((p["generate"].clone(), p["service_tier"].clone(), p["reasoning_effort"].clone()), (false.into(), "priority".into(), "high".into()));
        assert_eq!(p["access_token_sha256"], "abc");
        assert_eq!((p["node_kind"].clone(), p["is_fork"].clone(), p["is_compaction"].clone()), ("fork".into(), true.into(), true.into()));
        // The hash sits right after auth_index and the session hierarchy before reasoning_effort.
        let keys: Vec<&str> = p.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
        let pos = |k: &str| keys.iter().position(|x| *x == k);
        assert_eq!(pos("access_token_sha256"), pos("auth_index").map(|i| i + 1));
        assert!(pos("is_compaction") < pos("reasoning_effort") && pos("session_id") < pos("node_kind"));
    }
}

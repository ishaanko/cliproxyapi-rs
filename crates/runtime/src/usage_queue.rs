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

/// Go's `time.Time` JSON (RFC 3339 nano): the fraction keeps only significant digits.
fn rfc3339_nano(t: chrono::DateTime<chrono::Utc>) -> String {
    let text = t.with_timezone(&Local).to_rfc3339_opts(SecondsFormat::Nanos, true);
    let Some(dot) = text.find('.') else { return text };
    let frac_end = text[dot + 1..].find(|c: char| !c.is_ascii_digit()).map_or(text.len(), |i| dot + 1 + i);
    let digits = text[dot + 1..frac_end].trim_end_matches('0');
    let head = &text[..dot];
    let tail = &text[frac_end..];
    if digits.is_empty() { format!("{head}{tail}") } else { format!("{head}.{digits}{tail}") }
}

/// The queue payload of one record, with Go's field order. Tokens, the canonical breakdown,
/// the served model and tier and the reasoning effort come from the executor's report carried by
/// the record (see `usage_report`); records built without one are rebuilt from their counters.
pub fn queue_payload(r: &UsageRecord) -> Vec<u8> {
    let d = r.detail();
    let non_empty = |s: &str, default: &str| {
        if s.trim().is_empty() {
            default.to_string()
        } else {
            s.trim().to_string()
        }
    };
    let model = non_empty(&r.model, "unknown");
    let alias = non_empty(&r.alias, &model);
    // Go: `failed = record.Failed || !resolveSuccess(ctx)`. The final client status is only known
    // once the handler finished, so a record dispatched earlier (status 0) counts as a success.
    let response_status = r.extra.response_status.as_ref().map_or(0, crate::apilog::ResponseStatus::get);
    let failed = r.failed || response_status >= 400;
    let fail = if failed {
        // `resolveFail`: the record's own status, else the response status, else 500.
        let status = match (r.fail.status_code, response_status) {
            (0, 0) => 500,
            (0, status) => status,
            (status, _) => status,
        };
        json!({"status_code": status, "body": r.fail.body.trim()})
    } else {
        json!({"status_code": 200, "body": ""})
    };
    let mut m = Map::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_string(), v);
    };
    put("timestamp", rfc3339_nano(r.timestamp).into());
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
            "input_tokens": d.input_tokens,
            "output_tokens": d.output_tokens,
            "reasoning_tokens": d.reasoning_tokens,
            "cached_tokens": d.cached_tokens,
            "cache_read_tokens": d.cache_read_tokens,
            "cache_read_tokens_present": true,
            "cache_creation_tokens": d.cache_creation_tokens,
            "total_tokens": d.total_tokens,
        }),
    );
    put("failed", failed.into());
    put("generate", x.generate.unwrap_or(true).into());
    put("stream", r.stream.into());
    put("fail", fail);
    if !x.response_headers.is_empty() {
        put("response_headers", response_headers_json(&x.response_headers));
    }
    put("accounting_version", 2.into());
    put("token_breakdown", serde_json::to_value(d.token_breakdown).unwrap_or_default());
    put("provider", non_empty(&r.provider, "unknown").into());
    put("executor_type", non_empty(&r.executor_type, "unknown").into());
    put("model", model.into());
    put("alias", alias.into());
    put("endpoint", r.endpoint.clone().into());
    put("auth_type", non_empty(&r.auth_type, "unknown").into());
    put("api_key", r.api_key.trim().into());
    put("request_id", r.request_id.clone().into());
    let execution_id = if x.execution_id.trim().is_empty() { uuid::Uuid::new_v4().to_string() } else { x.execution_id.trim().to_string() };
    put("execution_id", execution_id.into());
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
    // Go: the record's tier, else the context's, which defaults to "default".
    put("service_tier", x.service_tier.clone().unwrap_or_else(|| "default".into()).into());
    if !x.response_service_tier.is_empty() {
        put("response_service_tier", x.response_service_tier.clone().into());
    }
    if !x.response_model.is_empty() {
        put("response_model", x.response_model.clone().into());
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
    use crate::usage_accounting::{Detail, ensure_token_breakdown_for_provider};
    use crate::usage_report::Record;

    fn payload(r: &UsageRecord) -> Value {
        serde_json::from_slice(&queue_payload(r)).expect("payload is JSON")
    }

    // Go: TestUsageQueuePluginPayloadIncludesGenerateFalse / DefaultsGenerateTrue / ServiceTier.
    #[test]
    fn request_facts_flow_into_the_payload() {
        let mut r = UsageRecord::default();
        let p = payload(&r);
        assert_eq!((p["generate"].clone(), p["service_tier"].clone(), p["reasoning_effort"].clone()), (true.into(), "default".into(), "".into()));
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

    // Go: `failed = record.Failed || !resolveSuccess(ctx)` where the context carries the final
    // client status; a record dispatched before the handler finished sees no status.
    #[test]
    fn failed_folds_in_the_final_response_status() {
        let status = crate::apilog::ResponseStatus::default();
        let mut r = UsageRecord::default();
        r.extra.response_status = Some(status.clone());
        let fail_of = |r: &UsageRecord| {
            let p = payload(r);
            (p["failed"].clone(), p["fail"]["status_code"].clone())
        };
        assert_eq!(fail_of(&r), (false.into(), 200.into()), "status unknown yet");
        status.set(200);
        assert_eq!(fail_of(&r), (false.into(), 200.into()));
        status.set(502);
        assert_eq!(fail_of(&r), (true.into(), 502.into()), "late record sees the client's 502");
        r.fail.status_code = 429;
        assert_eq!(fail_of(&r), (true.into(), 429.into()), "the record's own status wins");
        let failed_unknown = UsageRecord { failed: true, ..Default::default() };
        assert_eq!(fail_of(&failed_unknown), (true.into(), 500.into()));
    }

    // Go's RFC3339Nano drops trailing zeros of the fraction (and the dot when it is zero).
    #[test]
    fn timestamp_fraction_keeps_significant_digits_only() {
        use chrono::TimeZone;
        let at = |nanos: u32| rfc3339_nano(chrono::Utc.timestamp_opt(1_700_000_000, nanos).single().expect("valid time"));
        let trimmed = at(120_000_000);
        assert!(trimmed.contains(".12") && !trimmed.contains(".120"), "{trimmed}");
        assert!(at(123_456_789).contains(".123456789"));
        assert!(!at(0).contains('.'));
    }

    fn report(provider: &str, executor_type: &str, detail: Detail) -> UsageRecord {
        let detail = ensure_token_breakdown_for_provider(detail, provider, executor_type);
        Record {
            request_id: "exec-1".into(),
            trace_id: String::new(),
            provider: provider.into(),
            base_url: String::new(),
            executor_type: executor_type.into(),
            model: "m".into(),
            alias: String::new(),
            api_key: String::new(),
            session_id: String::new(),
            parent_session_id: String::new(),
            auth_id: String::new(),
            auth_index: "0".into(),
            access_token_sha256: String::new(),
            auth_type: "apikey".into(),
            source: "user@example.com".into(),
            reasoning_effort: "medium".into(),
            service_tier: "auto".into(),
            response_service_tier: "default".into(),
            response_model: "served-m".into(),
            generate: true,
            stream: false,
            requested_at: chrono::Utc::now(),
            latency: std::time::Duration::from_millis(5),
            ttft: std::time::Duration::ZERO,
            failed: false,
            fail: Default::default(),
            detail,
        }
        .to_usage_record()
    }

    // Go: TestUsageQueuePluginPayloadIncludesStableFieldsAndSuccess (report-sourced fields).
    #[test]
    fn report_fields_reach_the_payload() {
        let r = report("openai", "KimiExecutor", Detail { input_tokens: 10, output_tokens: 20, total_tokens: 30, ..Default::default() });
        let p = payload(&r);
        assert_eq!(p["response_model"], "served-m");
        assert_eq!(p["response_service_tier"], "default");
        assert_eq!((p["service_tier"].clone(), p["reasoning_effort"].clone()), ("auto".into(), "medium".into()));
        assert_eq!((p["execution_id"].clone(), p["source"].clone()), ("exec-1".into(), "user@example.com".into()));
        assert_eq!((p["token_breakdown"]["quality"].clone(), p["token_breakdown"]["total_tokens"].clone()), ("complete".into(), 30.into()));
        assert_eq!(p["tokens"]["cache_read_tokens_present"], true);
    }

    // Response model and tier are omitted when the upstream reported none.
    #[test]
    fn unreported_response_model_and_tier_are_omitted() {
        let mut r = UsageRecord::default();
        r.model = "m".into();
        let p = payload(&r);
        assert!(p.get("response_model").is_none() && p.get("response_service_tier").is_none());
    }

    // Go: TestUsageQueuePluginNormalizesDirectSDKUsageByProvider, plus Claude's independent buckets.
    #[test]
    fn breakdown_semantics_follow_the_provider() {
        let detail = Detail { input_tokens: 100, output_tokens: 30, reasoning_tokens: 12, ..Default::default() };
        for (provider, total) in [("openai", 130), ("gemini", 142)] {
            let mut r = UsageRecord { provider: provider.into(), model: "direct-sdk-model".into(), ..Default::default() };
            r.extra.detail = detail.clone();
            r.tokens.total_tokens = 0;
            let p = payload(&r);
            assert_eq!(p["tokens"]["total_tokens"], total, "{provider}");
            assert_eq!((p["token_breakdown"]["quality"].clone(), p["token_breakdown"]["total_tokens"].clone()), ("complete".into(), total.into()));
        }
        // Claude: cache reads and writes sit next to (not inside) the input tokens.
        let claude = Detail { input_tokens: 30, output_tokens: 5, cache_read_tokens: 7, cache_creation_tokens: 13, cached_tokens: 7, ..Default::default() };
        let p = payload(&report("claude", "ClaudeExecutor", claude));
        let (t, b) = (&p["tokens"], &p["token_breakdown"]);
        assert_eq!((t["cache_read_tokens"].clone(), t["cache_creation_tokens"].clone(), t["total_tokens"].clone()), (7.into(), 13.into(), 55.into()));
        assert_eq!((b["input"]["total_tokens"].clone(), b["input"]["uncached_tokens"].clone(), b["input"]["cache_write_tokens"].clone()), (50.into(), 30.into(), 13.into()));
    }

    // Go: TestUsageQueuePluginPreservesLegacyCachedOnlyUsage.
    #[test]
    fn legacy_cached_only_usage_is_unclassified_cache_read() {
        let mut r = UsageRecord { provider: "openai".into(), model: "gpt-5.4".into(), ..Default::default() };
        r.extra.detail = Detail { cached_tokens: 13, ..Default::default() };
        let p = payload(&r);
        assert_eq!((p["tokens"]["cache_read_tokens"].clone(), p["tokens"]["total_tokens"].clone()), (13.into(), 13.into()));
        assert_eq!((p["token_breakdown"]["quality"].clone(), p["token_breakdown"]["unclassified_tokens"].clone()), ("unclassified".into(), 13.into()));
    }
}

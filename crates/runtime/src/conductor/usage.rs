//! Usage record construction for completed upstream attempts.
//!
//! The Go executors publish usage themselves; here each attempt's options carry a
//! [`UsageCollector`](crate::usage_report::UsageCollector) the executors' reporters publish into,
//! and the conductor turns those reports into [`UsageRecord`](crate::usage::UsageRecord)s:
//! tokens, response model and tier, reasoning effort, latency and failure come from the report,
//! the endpoint, client metadata and response headers from the execution facts. Only when an
//! executor published no report (an executor without a reporter) does the conductor fall back to
//! the token counts it can read from the (already translated) response: either a `usage` object
//! an executor placed in `Response.metadata["usage"]`, or the usage block of the client-format
//! payload. Streams merge usage across chunks (latest value per field wins).

use std::time::Duration;

use chrono::{DateTime, Utc};
use cpa_auth::types::{Auth, AuthError};
use cpa_json::J;
use cpa_translator::Format;
use serde_json::Value;

use super::Manager;
use super::cooldown::ExecResult;
use crate::executor::{Metadata, meta};
use super::session::{bound_session_identity, normalize_to_canonical_uuid};
use crate::usage::{TokenUsage, UsageExtra, UsageFailure, UsageRecord};
use crate::usage_report::Record;

/// Metadata key a client-facing layer may set with the downstream API key (sha-masked by the
/// usage tracker, not here).
pub const META_CLIENT_API_KEY: &str = "client_api_key";
/// Metadata key carrying the inbound request id for correlation.
pub const META_REQUEST_ID: &str = "request_id";

/// Go executor type name (`reflect.Type.Name`) of the executor serving `provider`, as the usage
/// queue reports it.
pub fn go_executor_type(provider: &str) -> &'static str {
    match provider.trim() {
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

/// `syncMetadataSessionToContext`: the session (canonical, LCP, execution or derived id) and
/// parent session the conductor resolved for the request.
fn session_from_metadata(md: &Metadata) -> (String, String) {
    let trimmed = |key: &str| meta_str(md, key).trim().to_string();
    let mut id = trimmed(meta::CANONICAL_SESSION_ID);
    if id.is_empty() {
        id = trimmed("lcp_affinity_session_id");
    }
    if id.is_empty() {
        let exec = trimmed(meta::EXECUTION_SESSION_ID);
        if !exec.is_empty() {
            id = if exec.starts_with("execution:") { exec } else { format!("execution:{exec}") };
        }
    }
    if id.is_empty() {
        let derived = trimmed(meta::DERIVED_SESSION_ID);
        if !derived.is_empty() {
            id = if derived.starts_with("derived:") { derived } else { format!("derived:{derived}") };
        }
    }
    if id.is_empty() {
        return (String::new(), String::new());
    }
    (bound_session_identity(&id), trimmed(meta::PARENT_SESSION_ID))
}

/// SHA-256 hex of the auth's access token (`access_token` / `accessToken`, or the same inside a
/// `token` / `Token` object); empty when there is none (Go: `helps.AccessTokenSHA256`).
fn access_token_sha256(auth: &Auth) -> String {
    use sha2::{Digest, Sha256};
    let nonblank = |v: Option<&Value>| v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let token = ["access_token", "accessToken"]
        .iter()
        .find_map(|k| nonblank(auth.metadata.get(*k)))
        .or_else(|| {
            ["token", "Token"].iter().find_map(|k| match auth.metadata.get(*k) {
                Some(Value::Object(m)) => ["access_token", "accessToken"].iter().find_map(|tk| nonblank(m.get(*tk))),
                _ => None,
            })
        });
    token.map(|t| hex::encode(Sha256::digest(t.as_bytes()))).unwrap_or_default()
}

/// Session and request context of a record (Go: `ClientRequestMetadata` plus the reporter's
/// trace id). The session is the request's canonical session projected to a UUID.
fn usage_extra(result: &ExecResult, auth: Option<&Auth>) -> UsageExtra {
    let md = &result.options.metadata;
    let request_id = meta_str(md, META_REQUEST_ID);
    let trace_id = {
        let t = meta_str(md, meta::TRACE_ID);
        if t.trim().is_empty() { request_id.trim().to_string() } else { t.trim().to_string() }
    };
    let (session, parent) = session_from_metadata(md);
    let session = normalize_to_canonical_uuid(&session);
    let mut parent = normalize_to_canonical_uuid(&parent);
    if session.is_empty() || session == parent {
        parent.clear();
    }
    // Go: syncMetadataSessionToContext only carries these with a canonical session.
    let has_session = !session_from_metadata(md).0.is_empty();
    let flag = |key: &str| has_session && md.get(key).and_then(Value::as_bool).unwrap_or(false);
    let nonblank = |key: &str| Some(meta_str(md, key).trim().to_string()).filter(|s| !s.is_empty());
    UsageExtra {
        reasoning_effort: nonblank(meta::REASONING_EFFORT),
        service_tier: nonblank(meta::SERVICE_TIER),
        generate: md.get(meta::GENERATE).and_then(Value::as_bool),
        node_kind: if has_session { meta_str(md, "node_kind").trim().to_string() } else { String::new() },
        is_fork: flag(meta::IS_FORK),
        is_compaction: flag(meta::IS_COMPACTION),
        client_ip: meta_str(md, meta::CLIENT_IP).trim().to_string(),
        resolved_client_ip: meta_str(md, meta::RESOLVED_CLIENT_IP).trim().to_string(),
        x_forwarded_for: meta_str(md, meta::X_FORWARDED_FOR).trim().to_string(),
        user_agent: meta_str(md, meta::USER_AGENT).trim().to_string(),
        session_id: session,
        parent_session_id: parent,
        trace_id,
        // Go's http.Header holds no Transfer-Encoding (the transport consumes it).
        response_headers: {
            let mut h = result.response_headers.clone();
            h.remove(http::header::TRANSFER_ENCODING);
            h
        },
        base_url: auth.map(|a| a.attr("base_url").trim().to_string()).unwrap_or_default(),
        auth_id: auth.map(|a| a.id.clone()).unwrap_or_default(),
        queue_source: String::new(),
        access_token_sha256: String::new(),
        response_status: result.options.api_log.response_status(),
        ..Default::default()
    }
}
/// `Response.metadata` key under which an executor may report exact token counts.
pub const META_USAGE: &str = "usage";

/// Per-attempt facts the record needs beyond the `ExecResult`.
#[derive(Debug, Clone, Default)]
pub struct UsageFacts {
    pub latency: Duration,
    /// Time to first payload chunk (streams only).
    pub ttft: Option<Duration>,
    pub stream: bool,
    pub tokens: TokenUsage,
    /// Upstream model used for the attempt.
    pub upstream_model: String,
    /// Client-requested model.
    pub requested_model: String,
    /// Records the executor's reporters published during the attempt; when present they replace
    /// the response-derived `tokens`.
    pub reports: Vec<Record>,
}

const MAX_FAILURE_BODY: usize = 2048;

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn meta_str(m: &Metadata, key: &str) -> String {
    m.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn failure_of(err: &AuthError) -> UsageFailure {
    UsageFailure {
        status_code: err.http_status.clamp(0, u16::MAX as i32) as u16,
        body: truncate(&err.message, MAX_FAILURE_BODY),
    }
}

/// Builds the usage record of one finished attempt.
pub fn build_usage_record(
    result: &ExecResult,
    auth: Option<&Auth>,
    facts: &UsageFacts,
    now: DateTime<Utc>,
) -> UsageRecord {
    let latency_ms = i64::try_from(facts.latency.as_millis()).unwrap_or(i64::MAX);
    let requested = facts.requested_model.trim();
    let upstream = if facts.upstream_model.trim().is_empty() {
        result.model.as_str()
    } else {
        facts.upstream_model.trim()
    };
    let alias = if !requested.is_empty() && requested != upstream {
        requested.to_string()
    } else {
        String::new()
    };
    let (source, auth_index, auth_type) = match auth {
        Some(a) => {
            let mut source = a.label.trim().to_string();
            if source.is_empty() {
                source = a.meta_str("email");
            }
            if source.is_empty() {
                source = a.id.clone();
            }
            (source, a.index.clone(), a.auth_kind().to_string())
        }
        None => (String::new(), String::new(), String::new()),
    };
    let queue_source = auth.map(|a| a.account_info().1).unwrap_or_default();
    let path = meta_str(&result.options.metadata, meta::REQUEST_PATH);
    UsageRecord {
        timestamp: now - chrono::Duration::milliseconds(latency_ms),
        latency_ms,
        ttft_ms: facts
            .ttft
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
        source,
        auth_index,
        auth_type,
        provider: result.provider.clone(),
        executor_type: go_executor_type(&result.provider).to_string(),
        model: upstream.to_string(),
        alias,
        endpoint: if path.is_empty() {
            String::new()
        } else {
            format!("POST {path}")
        },
        api_key: meta_str(&result.options.metadata, META_CLIENT_API_KEY),
        request_id: meta_str(&result.options.metadata, META_REQUEST_ID),
        failed: !result.success,
        stream: facts.stream,
        fail: result.error.as_ref().map(failure_of).unwrap_or_default(),
        tokens: facts.tokens.clone(),
        extra: UsageExtra {
            queue_source,
            access_token_sha256: auth.map(access_token_sha256).unwrap_or_default(),
            ..usage_extra(result, auth)
        },
    }
}

impl Manager {
    /// Records the usage events of `facts.reports` and nothing else: no auth state, hook or
    /// affinity update. Used where the attempt's result is deliberately not marked (Claude OAuth
    /// cancellations, a client that hung up) or was marked earlier (per-response records of a
    /// long stream). Nothing is recorded without reports, so no response-derived fallback.
    pub(crate) fn record_usage_only(&self, result: &ExecResult, auth: Option<&Auth>, facts: UsageFacts) {
        if !facts.reports.is_empty() {
            self.record_usage(result, auth, Some(facts), self.now());
        }
    }
}

/// Builds the usage records of one finished attempt: one per executor report, or a single one
/// from the response-derived `facts.tokens` when the executor reported none. Consumes the
/// reports; `base` is reused for the last one.
pub fn build_usage_records(
    result: &ExecResult,
    auth: Option<&Auth>,
    mut facts: UsageFacts,
    now: DateTime<Utc>,
) -> Vec<UsageRecord> {
    let base = build_usage_record(result, auth, &facts, now);
    let mut reports = std::mem::take(&mut facts.reports);
    let Some(last) = reports.pop() else {
        return vec![base];
    };
    let mut out = Vec::with_capacity(reports.len() + 1);
    for r in reports {
        out.push(overlay_report(base.clone(), r, &facts));
    }
    out.push(overlay_report(base, last, &facts));
    out
}

/// `base` (conductor facts) with everything the executor's report knows better: timing, model,
/// requested-model alias, outcome, tokens, served model and tier, reasoning effort, credential
/// fingerprint and source.
fn overlay_report(mut rec: UsageRecord, r: Record, facts: &UsageFacts) -> UsageRecord {
    let rep = r.into_usage_record();
    rec.timestamp = rep.timestamp;
    rec.latency_ms = rep.latency_ms;
    rec.ttft_ms = rep.ttft_ms;
    // The report's model and alias are a pair: the alias is the requested model (the suffix
    // carrying `gpt-5(high)`), empty when it equals the model. The base alias alone would be
    // empty whenever the conductor's requested and upstream models agree.
    if !rep.model.is_empty() {
        rec.model = rep.model;
        rec.alias = rep.alias;
    }
    if !rep.provider.is_empty() {
        rec.provider = rep.provider;
    }
    if !rep.executor_type.is_empty() {
        rec.executor_type = rep.executor_type;
    }
    // Go: failed = record.Failed || !resolveSuccess(ctx). A stream's attempt result can fail
    // after earlier reports already succeeded (per-response records), so only unary attempts
    // keep the attempt-level failure.
    let rep_has_fail = rep.fail.status_code != 0 || !rep.fail.body.is_empty();
    if facts.stream {
        rec.failed = rep.failed;
        rec.fail = rep.fail;
    } else {
        rec.failed |= rep.failed;
        if rep_has_fail {
            rec.fail = rep.fail;
        }
    }
    rec.stream = rep.stream || facts.stream;
    rec.tokens = rep.tokens;
    let (x, rx) = (&mut rec.extra, rep.extra);
    x.execution_id = rx.execution_id;
    x.response_service_tier = rx.response_service_tier;
    x.response_model = rx.response_model;
    x.detail = rx.detail;
    if !rx.queue_source.is_empty() {
        x.queue_source = rx.queue_source;
    }
    if !rx.access_token_sha256.is_empty() {
        x.access_token_sha256 = rx.access_token_sha256;
    }
    if rx.reasoning_effort.is_some() {
        x.reasoning_effort = rx.reasoning_effort;
    }
    if rx.service_tier.is_some() {
        x.service_tier = rx.service_tier;
    }
    if !rx.base_url.is_empty() {
        x.base_url = rx.base_url;
    }
    if !rx.auth_id.is_empty() {
        x.auth_id = rx.auth_id;
    }
    rec
}

fn int(v: &Value, path: &str) -> i64 {
    v.g(path).int()
}

fn first_nonzero(v: &Value, paths: &[&str]) -> i64 {
    paths
        .iter()
        .map(|p| int(v, p))
        .find(|n| *n != 0)
        .unwrap_or(0)
}

/// Token counts from one usage-bearing JSON object (`usage` / `usageMetadata`), by schema.
fn tokens_from_usage_object(format: Format, u: &Value) -> TokenUsage {
    // Gemini family: usageMetadata.
    if u.g("promptTokenCount").exists()
        || u.g("candidatesTokenCount").exists()
        || u.g("totalTokenCount").exists()
    {
        let input = int(u, "promptTokenCount").saturating_add(int(u, "toolUsePromptTokenCount"));
        let output = int(u, "candidatesTokenCount");
        let reasoning = int(u, "thoughtsTokenCount");
        let total = match int(u, "totalTokenCount") {
            0 => input.saturating_add(output).saturating_add(reasoning),
            t => t,
        };
        return TokenUsage {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: reasoning,
            cached_tokens: int(u, "cachedContentTokenCount"),
            total_tokens: total,
        };
    }
    let input = first_nonzero(u, &["prompt_tokens", "input_tokens"]);
    let output = first_nonzero(u, &["completion_tokens", "output_tokens"]);
    let cache_read = int(u, "cache_read_input_tokens");
    let cache_create = int(u, "cache_creation_input_tokens");
    let claude_style = format == Format::Claude || cache_read != 0 || cache_create != 0;
    if claude_style {
        let reasoning = first_nonzero(
            u,
            &[
                "output_tokens_details.thinking_tokens",
                "output_tokens_details.reasoning_tokens",
                "thinking_tokens",
            ],
        );
        let cached = if cache_read != 0 {
            cache_read
        } else {
            cache_create
        };
        return TokenUsage {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: reasoning,
            cached_tokens: cached,
            total_tokens: input
                .saturating_add(output)
                .saturating_add(cache_read)
                .saturating_add(cache_create),
        };
    }
    let cached = first_nonzero(
        u,
        &[
            "prompt_tokens_details.cached_tokens",
            "input_tokens_details.cached_tokens",
        ],
    );
    let reasoning = first_nonzero(
        u,
        &[
            "completion_tokens_details.reasoning_tokens",
            "output_tokens_details.reasoning_tokens",
        ],
    );
    let total = match int(u, "total_tokens") {
        0 => input.saturating_add(output),
        t => t,
    };
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: reasoning,
        cached_tokens: cached,
        total_tokens: total,
    }
}

/// Locates the usage object in a payload of the client format and extracts its counts.
pub fn tokens_from_value(format: Format, v: &Value) -> Option<TokenUsage> {
    for path in [
        "usage",
        "response.usage",
        "message.usage",
        "usageMetadata",
        "response.usageMetadata",
    ] {
        let r = v.g(path);
        if r.is_object() {
            let t = tokens_from_usage_object(format, &r.value());
            if t != TokenUsage::default() {
                return Some(t);
            }
        }
    }
    None
}

/// Token counts of a complete response: executor-reported `metadata["usage"]` first, then the
/// payload's own usage block.
pub fn tokens_from_response(format: Format, payload: &[u8], metadata: &Metadata) -> TokenUsage {
    if let Some(u @ Value::Object(_)) = metadata.get(META_USAGE) {
        let t = TokenUsage {
            input_tokens: int(u, "input_tokens"),
            output_tokens: int(u, "output_tokens"),
            reasoning_tokens: int(u, "reasoning_tokens"),
            cached_tokens: int(u, "cached_tokens"),
            total_tokens: int(u, "total_tokens"),
        };
        if t != TokenUsage::default() {
            return t;
        }
    }
    let v = cpa_json::parse(payload);
    if v.is_null() {
        return TokenUsage::default();
    }
    tokens_from_value(format, &v).unwrap_or_default()
}

/// Accumulates usage across stream chunks; later non-zero fields replace earlier ones.
#[derive(Debug, Default)]
pub struct StreamUsage {
    format: Option<Format>,
    pub tokens: TokenUsage,
}

impl StreamUsage {
    pub fn new(format: Format) -> Self {
        StreamUsage {
            format: Some(format),
            tokens: TokenUsage::default(),
        }
    }

    /// Observes one chunk (raw JSON or SSE frames) for usage.
    pub fn observe(&mut self, chunk: &[u8]) {
        let format = self.format.unwrap_or(Format::OpenAI);
        // Cheap precheck: skip chunks that cannot carry usage.
        if !contains(chunk, b"usage") && !contains(chunk, b"Usage") {
            return;
        }
        for line in chunk.split(|b| *b == b'\n') {
            let line = line.strip_prefix(b"data:").unwrap_or(line);
            let line = trim(line);
            if line.first() != Some(&b'{') {
                continue;
            }
            let v = cpa_json::parse(line);
            if let Some(t) = tokens_from_value(format, &v) {
                merge(&mut self.tokens, t);
            }
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(hay, needle).is_some()
}

fn trim(b: &[u8]) -> &[u8] {
    let s = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    let e = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(s, |e| e + 1);
    &b[s..e]
}

fn merge(dst: &mut TokenUsage, src: TokenUsage) {
    macro_rules! take {
        ($f:ident) => {
            if src.$f != 0 {
                dst.$f = src.$f;
            }
        };
    }
    take!(input_tokens);
    take!(output_tokens);
    take!(reasoning_tokens);
    take!(cached_tokens);
    take!(total_tokens);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_token_counts_saturate() {
        let t = tokens_from_response(
            Format::Claude,
            br#"{"usage":{"input_tokens":9223372036854775807,"output_tokens":9223372036854775807,"cache_read_input_tokens":5}}"#,
            &Metadata::new(),
        );
        assert_eq!(t.total_tokens, i64::MAX);
    }

    #[test]
    fn openai_chat_and_responses_usage() {
        let t = tokens_from_response(
            Format::OpenAI,
            br#"{"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}}"#,
            &Metadata::new(),
        );
        assert_eq!(
            (
                t.input_tokens,
                t.output_tokens,
                t.total_tokens,
                t.cached_tokens,
                t.reasoning_tokens
            ),
            (10, 5, 15, 4, 2)
        );
        let t = tokens_from_response(
            Format::OpenAIResponse,
            br#"{"usage":{"input_tokens":7,"output_tokens":3}}"#,
            &Metadata::new(),
        );
        assert_eq!(
            (t.input_tokens, t.output_tokens, t.total_tokens),
            (7, 3, 10)
        );
    }

    #[test]
    fn claude_cache_fields_are_independent() {
        let t = tokens_from_response(
            Format::Claude,
            br#"{"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}}"#,
            &Metadata::new(),
        );
        assert_eq!((t.total_tokens, t.cached_tokens), (135, 100));
    }

    #[test]
    fn gemini_metadata_and_executor_override() {
        let t = tokens_from_response(
            Format::Gemini,
            br#"{"response":{"usageMetadata":{"promptTokenCount":8,"candidatesTokenCount":2,"thoughtsTokenCount":1,"totalTokenCount":11}}}"#,
            &Metadata::new(),
        );
        assert_eq!(
            (
                t.input_tokens,
                t.output_tokens,
                t.reasoning_tokens,
                t.total_tokens
            ),
            (8, 2, 1, 11)
        );
        let mut md = Metadata::new();
        md.insert(
            META_USAGE.into(),
            serde_json::json!({"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}),
        );
        assert_eq!(
            tokens_from_response(Format::OpenAI, b"{}", &md).total_tokens,
            3
        );
    }

    #[test]
    fn stream_usage_merges_latest_fields() {
        let mut s = StreamUsage::new(Format::Claude);
        s.observe(b"event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n");
        s.observe(b"data: {\"type\":\"content_block_delta\"}\n\n");
        s.observe(b"event: message_delta\ndata: {\"usage\":{\"output_tokens\":9}}\n\n");
        assert_eq!((s.tokens.input_tokens, s.tokens.output_tokens), (12, 9));
    }

    #[test]
    fn record_marks_failure_and_alias() {
        let result = ExecResult {
            auth_id: "a".into(),
            provider: "claude".into(),
            model: "m".into(),
            route_model: "alias".into(),
            success: false,
            retry_after: None,
            credential_scope: false,
            error: Some(AuthError {
                message: "x".repeat(5000),
                http_status: 429,
                ..Default::default()
            }),
            options: crate::executor::Options::new(Format::OpenAI),
            skip_quota_observation: false,
            response_headers: Default::default(),
        };
        let facts = UsageFacts {
            upstream_model: "up".into(),
            requested_model: "alias".into(),
            ..Default::default()
        };
        let rec = build_usage_record(&result, None, &facts, Utc::now());
        assert!(rec.failed && rec.fail.status_code == 429 && rec.fail.body.len() == 2048);
        assert_eq!((rec.model.as_str(), rec.alias.as_str()), ("up", "alias"));
    }
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod report_tests;

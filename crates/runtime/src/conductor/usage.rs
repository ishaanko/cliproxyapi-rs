//! Usage record construction for completed upstream attempts.
//!
//! The Go executors publish usage themselves; here the conductor builds one
//! [`UsageRecord`](crate::usage::UsageRecord) per attempt from the execution facts and the token
//! counts it can read from the (already translated) response: either a `usage` object an
//! executor placed in `Response.metadata["usage"]`, or the usage block of the client-format
//! payload. Streams merge usage across chunks (latest value per field wins).

use std::time::Duration;

use chrono::{DateTime, Utc};
use cpa_auth::types::{Auth, AuthError};
use cpa_json::J;
use cpa_translator::Format;
use serde_json::Value;

use super::cooldown::ExecResult;
use crate::executor::{Metadata, meta};
use crate::usage::{TokenUsage, UsageFailure, UsageRecord};

/// Metadata key a client-facing layer may set with the downstream API key (sha-masked by the
/// usage tracker, not here).
pub const META_CLIENT_API_KEY: &str = "client_api_key";
/// Metadata key carrying the request id for correlation.
pub const META_REQUEST_ID: &str = "request_id";
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
        executor_type: result.provider.clone(),
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
    }
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
    hay.windows(needle.len()).any(|w| w == needle)
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

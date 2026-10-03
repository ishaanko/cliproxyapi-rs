//! Usage extraction per upstream wire format (Go: helps/usage_helpers.go, the parse half, plus
//! the stream merge helpers of plugin_executor_usage.go).
//!
//! `parse_*` take a complete JSON body; `parse_*_stream_usage` take one SSE line (or raw JSON
//! frame) and return `None` when the line carries no usage. [`StreamUsageBuffer`] keeps the
//! latest usage seen while forwarding a stream.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use cpa_json::{J, Res, Value};
use parking_lot::Mutex;

use super::accounting::{
    Detail, TokenBreakdown, TokenAccountingQuality, TOKEN_ACCOUNTING_SCHEMA_VERSION,
    new_independent_token_breakdown, new_partial_subset_token_breakdown,
    new_separate_reasoning_token_breakdown, new_subset_token_breakdown, new_unclassified_token_breakdown,
};
use crate::helps::response_model::{extract_claude_response_model_event, extract_generic_response_model_event};
use crate::helps::text::{contains, json_payload, trim_space};

/// True when `payload` is a well-formed JSON object with none of `keys` at its top level, so
/// every lookup rooted at those keys misses (stream lines are mostly such events; deciding this
/// from the key index avoids validating and parsing the whole frame).
fn lacks_top_level_keys(payload: &[u8], keys: &[&str]) -> bool {
    cpa_runtime::conductor::session::lazy::Doc::lazy(payload).is_some_and(|doc| !keys.iter().any(|k| doc.has(k)))
}

fn first_existing(root: &Value, paths: &[&str]) -> Option<Value> {
    paths.iter().find_map(|p| root.g(p).into_value())
}

fn exists(node: &Value, path: &str) -> bool {
    node.g(path).exists()
}

/// gjson `Result.Int()` of an already extracted node.
fn res_int(v: &Value) -> i64 {
    Res::of(v).int()
}

/// gjson `Result.String()` of an already extracted node.
fn res_str(v: &Value) -> String {
    Res::of(v).str()
}

/// Sum of non-negative values; `None` on a negative value or overflow.
fn safe_sum(values: &[i64]) -> Option<i64> {
    values.iter().try_fold(0i64, |total, &v| if v < 0 { None } else { total.checked_add(v) })
}

fn invalid_breakdown(total: i64) -> TokenBreakdown {
    let total = total.max(0);
    TokenBreakdown {
        schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
        quality: TokenAccountingQuality::Inconsistent,
        total_tokens: total,
        unclassified_tokens: total,
        ..Default::default()
    }
}

// ---------------------------------------------------------------- OpenAI style

fn has_openai_style_usage_token_fields(node: &Value) -> bool {
    node.is_object() && (exists(node, "total_tokens") || has_openai_style_bucket_fields(node))
}

fn has_openai_style_bucket_fields(node: &Value) -> bool {
    [
        "prompt_tokens",
        "input_tokens",
        "completion_tokens",
        "output_tokens",
        "prompt_tokens_details.cached_tokens",
        "input_tokens_details.cached_tokens",
        "prompt_tokens_details.cache_write_tokens",
        "prompt_tokens_details.cache_creation_tokens",
        "input_tokens_details.cache_write_tokens",
        "input_tokens_details.cache_creation_tokens",
        "completion_tokens_details.reasoning_tokens",
        "output_tokens_details.reasoning_tokens",
    ]
    .iter()
    .any(|p| exists(node, p))
}

fn parse_openai_style_usage_node(node: &Value) -> Detail {
    let input_node = if exists(node, "prompt_tokens") { node.g("prompt_tokens") } else { node.g("input_tokens") };
    let output_node = if exists(node, "completion_tokens") { node.g("completion_tokens") } else { node.g("output_tokens") };
    let mut detail = Detail {
        input_tokens: input_node.int(),
        output_tokens: output_node.int(),
        total_tokens: node.g("total_tokens").int(),
        ..Default::default()
    };
    let cached = if exists(node, "prompt_tokens_details.cached_tokens") {
        node.g("prompt_tokens_details.cached_tokens")
    } else {
        node.g("input_tokens_details.cached_tokens")
    };
    if cached.exists() {
        detail.cached_tokens = cached.int();
        detail.cache_read_tokens = cached.int();
    }
    let cache_creation = first_existing(
        node,
        &[
            "input_tokens_details.cache_creation_tokens",
            "input_tokens_details.cache_write_tokens",
            "prompt_tokens_details.cache_creation_tokens",
            "prompt_tokens_details.cache_write_tokens",
        ],
    );
    if let Some(c) = cache_creation {
        detail.cache_creation_tokens = res_int(&c);
    }
    let reasoning = if exists(node, "completion_tokens_details.reasoning_tokens") {
        node.g("completion_tokens_details.reasoning_tokens")
    } else {
        node.g("output_tokens_details.reasoning_tokens")
    };
    if reasoning.exists() {
        detail.reasoning_tokens = reasoning.int();
    }
    if has_openai_style_bucket_fields(node) {
        if input_node.exists() && output_node.exists() {
            detail.token_breakdown = new_subset_token_breakdown(
                detail.input_tokens,
                detail.cache_read_tokens,
                detail.cache_creation_tokens,
                detail.output_tokens,
                detail.reasoning_tokens,
                detail.total_tokens,
            );
        } else {
            let (mut cache_read, mut cache_creation) = (detail.cache_read_tokens, detail.cache_creation_tokens);
            if !input_node.exists() {
                cache_read = 0;
                cache_creation = 0;
            }
            let reasoning_tokens = if output_node.exists() { detail.reasoning_tokens } else { 0 };
            detail.token_breakdown = new_partial_subset_token_breakdown(
                detail.input_tokens,
                cache_read,
                cache_creation,
                detail.output_tokens,
                reasoning_tokens,
                detail.total_tokens,
            );
        }
    } else {
        detail.token_breakdown = new_unclassified_token_breakdown(detail.total_tokens);
    }
    if detail.total_tokens == 0 {
        detail.total_tokens = detail.token_breakdown.total_tokens;
    }
    detail
}

/// Response tier from `response.service_tier`, `service_tier`, `interaction.service_tier`.
fn extract_response_service_tier_of(v: &Value) -> String {
    for path in ["response.service_tier", "service_tier", "interaction.service_tier"] {
        let tier = v.g(path).str();
        if !tier.trim().is_empty() {
            return tier.trim().to_string();
        }
    }
    String::new()
}

fn extract_response_service_tier(payload: &[u8]) -> String {
    if payload.is_empty() || !cpa_json::valid(payload) {
        return String::new();
    }
    extract_response_service_tier_of(&cpa_json::parse(payload))
}

/// Usage of an OpenAI chat/responses-style body (`usage` object), plus the response service tier.
pub fn parse_openai_usage(data: &[u8]) -> Detail {
    let v = cpa_json::parse(data);
    let tier = extract_response_service_tier_of(&v);
    let node = v.g("usage").value();
    if !has_openai_style_usage_token_fields(&node) {
        return Detail { response_service_tier: tier, ..Default::default() };
    }
    let mut detail = parse_openai_style_usage_node(&node);
    detail.response_service_tier = tier;
    detail
}

/// One OpenAI chat stream line: usage (final chunk) and/or the response service tier.
pub fn parse_openai_stream_usage(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line)?;
    if !cpa_json::valid(payload) {
        return None;
    }
    let v = cpa_json::parse(payload);
    let tier = extract_response_service_tier_of(&v);
    let node = v.g("usage").value();
    if !has_openai_style_usage_token_fields(&node) {
        return (!tier.is_empty()).then(|| Detail { response_service_tier: tier, ..Default::default() });
    }
    let mut detail = parse_openai_style_usage_node(&node);
    detail.response_service_tier = tier;
    Some(detail)
}

/// Codex `response.completed` style event: `response.usage` and service tier.
pub fn parse_codex_usage(data: &[u8]) -> Option<Detail> {
    let v = cpa_json::parse(data);
    let tier = if data.is_empty() || !cpa_json::valid(data) { String::new() } else { extract_response_service_tier_of(&v) };
    let node = v.g("response.usage").value();
    if !has_openai_style_usage_token_fields(&node) {
        return (!tier.is_empty()).then(|| Detail { response_service_tier: tier, ..Default::default() });
    }
    let mut detail = parse_openai_style_usage_node(&node);
    detail.response_service_tier = tier;
    Some(detail)
}

/// Usage of the image generation tool in a Codex event (`response.tool_usage.image_gen`).
pub fn parse_codex_image_tool_usage(data: &[u8]) -> Option<Detail> {
    let node = cpa_json::parse(data).g("response.tool_usage.image_gen").value();
    has_openai_style_usage_token_fields(&node).then(|| parse_openai_style_usage_node(&node))
}

// ---------------------------------------------------------------- Claude

/// Usage of a Claude message body (`usage` object).
pub fn parse_claude_usage(data: &[u8]) -> Detail {
    match cpa_json::parse(data).g("usage").into_value() {
        Some(node) => parse_claude_usage_node(&node),
        None => Detail::default(),
    }
}

/// Usage from a Claude stream line (`usage` or `message.usage`).
pub fn parse_claude_stream_usage(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line)?;
    if lacks_top_level_keys(payload, &["usage", "message"]) {
        return None;
    }
    if !cpa_json::valid(payload) {
        return None;
    }
    let v = cpa_json::parse(payload);
    let node = first_existing(&v, &["usage", "message.usage"])?;
    Some(parse_claude_usage_node(&node))
}

fn parse_claude_usage_node(node: &Value) -> Detail {
    let cache_read = node.g("cache_read_input_tokens").int();
    let cache_creation = node.g("cache_creation_input_tokens").int();
    let raw_output = node.g("output_tokens").int();
    // Anthropic reports thinking as a subset of output_tokens: prefer the official nested field,
    // then legacy aliases used by some gateways.
    let reasoning = first_existing(
        node,
        &["output_tokens_details.thinking_tokens", "output_tokens_details.reasoning_tokens", "thinking_tokens"],
    )
    .map_or(0, |n| res_int(&n))
    .max(0);
    let non_reasoning_output = if reasoning > 0 && reasoning <= raw_output {
        raw_output - reasoning
    } else if reasoning > raw_output {
        // Keep OutputTokens authoritative and do not invent non-reasoning output for an
        // inconsistent upstream payload.
        0
    } else {
        raw_output
    };
    let mut detail = Detail {
        input_tokens: node.g("input_tokens").int(),
        output_tokens: raw_output,
        reasoning_tokens: reasoning,
        cached_tokens: cache_read,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        ..Default::default()
    };
    if detail.cached_tokens == 0 {
        detail.cached_tokens = detail.cache_creation_tokens;
    }
    // output_tokens already includes thinking; cache fields are independent of input_tokens.
    detail.total_tokens = detail.input_tokens + raw_output + detail.cache_read_tokens + detail.cache_creation_tokens;
    detail.token_breakdown = new_independent_token_breakdown(
        detail.input_tokens,
        detail.cache_read_tokens,
        detail.cache_creation_tokens,
        non_reasoning_output,
        detail.reasoning_tokens,
        detail.total_tokens,
    );
    detail
}

// ---------------------------------------------------------------- Gemini family

fn parse_gemini_family_usage_detail(node: &Value) -> Detail {
    let cached = node.g("cachedContentTokenCount").int();
    let tool_use = first_existing(node, &["toolUsePromptTokenCount", "tool_use_prompt_token_count"])
        .map_or(0, |n| res_int(&n));
    let input = safe_sum(&[node.g("promptTokenCount").int(), tool_use]);
    let mut detail = Detail {
        input_tokens: input.unwrap_or(0),
        output_tokens: node.g("candidatesTokenCount").int(),
        reasoning_tokens: node.g("thoughtsTokenCount").int(),
        total_tokens: node.g("totalTokenCount").int(),
        cached_tokens: cached,
        cache_read_tokens: cached,
        ..Default::default()
    };
    if input.is_none() {
        detail.token_breakdown = invalid_breakdown(detail.total_tokens);
        return detail;
    }
    if detail.total_tokens == 0 {
        match safe_sum(&[detail.input_tokens, detail.output_tokens, detail.reasoning_tokens]) {
            Some(total) => detail.total_tokens = total,
            None => {
                detail.total_tokens = 0;
                detail.token_breakdown = invalid_breakdown(0);
                return detail;
            }
        }
    }
    detail.token_breakdown = new_separate_reasoning_token_breakdown(
        detail.input_tokens,
        detail.cache_read_tokens,
        detail.cache_creation_tokens,
        detail.output_tokens,
        detail.reasoning_tokens,
        detail.total_tokens,
    );
    detail
}

fn parse_interactions_usage_detail(node: &Value) -> Detail {
    let int_of = |paths: &[&str]| first_existing(node, paths).map_or(0, |n| res_int(&n));
    let cache_read = first_existing(node, &["cache_read_tokens", "cacheReadTokens"]);
    let tool_use = int_of(&["tool_use_tokens", "total_tool_use_tokens", "toolUseTokens", "totalToolUseTokens"]);
    let input = safe_sum(&[int_of(&["input_tokens", "prompt_tokens", "total_input_tokens"]), tool_use]);
    let mut detail = Detail {
        input_tokens: input.unwrap_or(0),
        output_tokens: int_of(&["output_tokens", "completion_tokens", "total_output_tokens"]),
        reasoning_tokens: int_of(&["reasoning_tokens", "thoughtsTokenCount", "total_thought_tokens"]),
        total_tokens: int_of(&["total_tokens", "totalTokenCount"]),
        cached_tokens: int_of(&["cached_tokens", "cachedContentTokenCount", "total_cached_tokens"]),
        cache_read_tokens: cache_read.as_ref().map_or(0, res_int),
        cache_creation_tokens: int_of(&["cache_creation_tokens", "cacheCreationTokens", "cache_write_tokens", "cacheWriteTokens"]),
        ..Default::default()
    };
    if input.is_none() {
        detail.token_breakdown = invalid_breakdown(detail.total_tokens);
        return detail;
    }
    if cache_read.is_none() && detail.cached_tokens > 0 {
        detail.cache_read_tokens = detail.cached_tokens;
    }
    if detail.total_tokens == 0 {
        match safe_sum(&[detail.input_tokens, detail.output_tokens, detail.reasoning_tokens]) {
            Some(total) => detail.total_tokens = total,
            None => {
                detail.total_tokens = 0;
                detail.token_breakdown = invalid_breakdown(0);
                return detail;
            }
        }
    }
    detail.token_breakdown = new_separate_reasoning_token_breakdown(
        detail.input_tokens,
        detail.cache_read_tokens,
        detail.cache_creation_tokens,
        detail.output_tokens,
        detail.reasoning_tokens,
        detail.total_tokens,
    );
    detail
}

/// Usage of a Gemini Interactions response (or a Gemini-shaped `usageMetadata` inside one).
pub fn parse_interactions_usage(data: &[u8]) -> Detail {
    let root = cpa_json::parse(data);
    let Some(node) = first_existing(
        &root,
        &[
            "usage",
            "total_usage",
            "metadata.total_usage",
            "metadata.usage",
            "usageMetadata",
            "usage_metadata",
            "interaction.usage",
            "interaction.total_usage",
            "interaction.metadata.total_usage",
        ],
    ) else {
        return Detail::default();
    };
    let mut detail = if exists(&node, "promptTokenCount") || exists(&node, "candidatesTokenCount") {
        parse_gemini_family_usage_detail(&node)
    } else {
        parse_interactions_usage_detail(&node)
    };
    detail.response_service_tier = extract_response_service_tier(data);
    detail
}

/// One Interactions stream line; `None` when it carries no usage.
pub fn parse_interactions_stream_usage(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line).unwrap_or(line);
    if payload.is_empty() || !cpa_json::valid(payload) {
        return None;
    }
    let detail = parse_interactions_usage(payload);
    detail.has_token_usage().then_some(detail)
}

/// Usage of a Gemini `usageMetadata` / `usage_metadata` body.
pub fn parse_gemini_usage(data: &[u8]) -> Detail {
    match first_existing(&cpa_json::parse(data), &["usageMetadata", "usage_metadata"]) {
        Some(node) => parse_gemini_family_usage_detail(&node),
        None => Detail::default(),
    }
}

/// One Gemini stream line; `None` without non-zero usage metadata.
pub fn parse_gemini_stream_usage(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line)?;
    if lacks_top_level_keys(payload, &["usageMetadata", "usage_metadata"]) {
        return None;
    }
    if !cpa_json::valid(payload) {
        return None;
    }
    let node = first_existing(&cpa_json::parse(payload), &["usageMetadata", "usage_metadata"])?;
    let detail = parse_gemini_family_usage_detail(&node);
    detail.has_token_usage().then_some(detail)
}

/// Usage of an Antigravity body (`response.usageMetadata` envelope or bare `usageMetadata`).
pub fn parse_antigravity_usage(data: &[u8]) -> Detail {
    let v = cpa_json::parse(data);
    match first_existing(&v, &["response.usageMetadata", "usageMetadata", "usage_metadata"]) {
        Some(node) => parse_gemini_family_usage_detail(&node),
        None => Detail::default(),
    }
}

/// One Antigravity stream line; `None` when it has no usage metadata node.
pub fn parse_antigravity_stream_usage(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line)?;
    if lacks_top_level_keys(payload, &["response", "usageMetadata", "usage_metadata"]) {
        return None;
    }
    if !cpa_json::valid(payload) {
        return None;
    }
    let v = cpa_json::parse(payload);
    let node = first_existing(&v, &["response.usageMetadata", "usageMetadata", "usage_metadata"])?;
    Some(parse_gemini_family_usage_detail(&node))
}

// ---------------------------------------------------------------- stream buffer + merge

/// Keeps the latest usage detail observed in a stream.
#[derive(Debug, Default)]
pub struct StreamUsageBuffer {
    detail: Detail,
    ok: bool,
    response_model: String,
}

impl StreamUsageBuffer {
    /// Records `detail` when `ok`, letting the final stream usage win while a tier-only update
    /// keeps the previous counters.
    pub fn observe(&mut self, detail: Detail, ok: bool) {
        if !ok {
            return;
        }
        let tier = detail.response_service_tier.trim().to_string();
        if tier.is_empty() || detail.has_token_usage() {
            let preserved = std::mem::take(&mut self.detail.response_service_tier);
            self.detail = detail;
            if self.detail.response_service_tier.is_empty() {
                self.detail.response_service_tier = preserved;
            }
        } else {
            self.detail.response_service_tier = tier;
        }
        self.ok = true;
    }

    /// Records response-tier state and the latest usage from an OpenAI-style stream line while
    /// skipping JSON parsing for irrelevant chunks.
    pub fn observe_openai_stream(&mut self, line: &[u8]) {
        let Some(payload) = json_payload(line) else {
            return;
        };
        let has_usage_candidate = contains(payload, b"\"usage\"");
        let need_tier = self.detail.response_service_tier.is_empty() || has_usage_candidate;
        let has_tier_candidate = need_tier && contains(payload, b"\"service_tier\"");
        if self.response_model.is_empty() {
            let (model, _) = extract_generic_response_model_event(payload);
            if !model.is_empty() {
                self.response_model = model;
            }
        }
        if !has_usage_candidate && !has_tier_candidate {
            return;
        }
        if !cpa_json::valid(payload) {
            return;
        }
        let v = cpa_json::parse(payload);
        let mut detail = Detail::default();
        let mut usage_ok = false;
        if has_usage_candidate {
            let node = v.g("usage").value();
            if has_openai_style_usage_token_fields(&node) {
                detail = parse_openai_style_usage_node(&node);
                usage_ok = true;
            }
        }
        if has_tier_candidate {
            detail.response_service_tier = extract_response_service_tier_of(&v);
        }
        let observed = usage_ok || !detail.response_service_tier.is_empty();
        self.observe(detail, observed);
    }

    /// Records and merges usage from a Claude SSE line (`message_start` then `message_delta`).
    pub fn observe_claude_stream(&mut self, line: &[u8]) {
        if self.response_model.is_empty()
            && let Some(payload) = json_payload(line)
        {
            let (model, _) = extract_claude_response_model_event(payload);
            if !model.is_empty() {
                self.response_model = model;
            }
        }
        if let Some(detail) = parse_claude_stream_usage(line) {
            self.observe_merged(detail);
        }
    }

    /// Merges `update` into the buffered usage (Go: ObserveMergedStreamUsage).
    pub fn observe_merged(&mut self, update: Detail) {
        match self.detail_ref() {
            Some(existing) => {
                let merged = merge_stream_usage_detail(existing, &update);
                self.observe(merged, true);
            }
            None => self.observe(update, true),
        }
    }

    /// The latest observed usage, if any.
    pub fn detail_ref(&self) -> Option<&Detail> {
        self.ok.then_some(&self.detail)
    }

    /// Copy of the latest observed usage.
    pub fn detail(&self) -> Option<Detail> {
        self.detail_ref().cloned()
    }

    /// Model observed in the stream, "" when none.
    pub fn response_model(&self) -> &str {
        &self.response_model
    }
}

/// Merges earlier stream usage with a newer update: zero fields in the update keep the earlier
/// value, the total is at least input + output + cache, and the breakdown is recomputed
/// (independent semantics, as Claude streams split usage over several events).
pub fn merge_stream_usage_detail(existing: &Detail, update: &Detail) -> Detail {
    let mut merged = update.clone();
    macro_rules! keep {
        ($f:ident) => {
            if merged.$f == 0 && existing.$f > 0 {
                merged.$f = existing.$f;
            }
        };
    }
    keep!(input_tokens);
    keep!(cached_tokens);
    keep!(cache_read_tokens);
    keep!(cache_creation_tokens);
    keep!(output_tokens);
    keep!(reasoning_tokens);
    if merged.response_service_tier.is_empty() {
        merged.response_service_tier = existing.response_service_tier.clone();
    }
    let mut cached = merged.cache_read_tokens + merged.cache_creation_tokens;
    if cached == 0 {
        cached = merged.cached_tokens;
    }
    let calculated_total = merged.input_tokens + merged.output_tokens + cached;
    if merged.total_tokens == 0 || merged.total_tokens < calculated_total {
        merged.total_tokens = calculated_total;
    }
    let non_reasoning_output = (merged.output_tokens - merged.reasoning_tokens).max(0);
    merged.token_breakdown = new_independent_token_breakdown(
        merged.input_tokens,
        merged.cache_read_tokens,
        merged.cache_creation_tokens,
        non_reasoning_output,
        merged.reasoning_tokens,
        merged.total_tokens,
    );
    merged
}

// ---------------------------------------------------------------- SSE usage filtering

static STOP_WITHOUT_USAGE: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);
const STOP_WITHOUT_USAGE_TTL: Duration = Duration::from_secs(10 * 60);

fn remember_stop_without_usage(trace_id: &str) {
    let mut map = STOP_WITHOUT_USAGE.lock();
    let now = Instant::now();
    map.retain(|_, at| now.saturating_duration_since(*at) < STOP_WITHOUT_USAGE_TTL);
    map.insert(trace_id.to_string(), now);
}

fn take_stop_without_usage(trace_id: &str) -> bool {
    let mut map = STOP_WITHOUT_USAGE.lock();
    match map.remove(trace_id) {
        Some(at) => at.elapsed() < STOP_WITHOUT_USAGE_TTL,
        None => false,
    }
}

/// Removes `usageMetadata` from SSE events that are not terminal (no `finishReason`); terminal
/// chunks stay untouched. A stop chunk without usage is remembered by `traceId` so the usage
/// that arrives in a later chunk of the same trace is dropped. Shared by AI Studio and
/// Antigravity.
pub fn filter_sse_usage_metadata(payload: &[u8]) -> Vec<u8> {
    if payload.is_empty() {
        return payload.to_vec();
    }
    let mut lines: Vec<Vec<u8>> = payload.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
    let mut modified = false;
    let mut found_data = false;
    for idx in 0..lines.len() {
        let line = lines[idx].clone();
        let trimmed = trim_space(&line);
        if trimmed.is_empty() || !trimmed.starts_with(b"data:") {
            continue;
        }
        found_data = true;
        let Some(data_idx) = find(&line, b"data:") else {
            continue;
        };
        let raw_json = trim_space(&line[data_idx + 5..]);
        let chunk = Chunk::new(raw_json);
        let trace_id = chunk.value.g("traceId").str();
        if chunk.is_stop_without_usage() && !trace_id.is_empty() {
            remember_stop_without_usage(&trace_id);
            continue;
        }
        if !trace_id.is_empty() && chunk.has_usage_metadata() && take_stop_without_usage(&trace_id) {
            // Go drops the remembered entry and skips the line without altering it.
            continue;
        }
        let (cleaned, changed) = chunk.into_stripped(raw_json);
        if !changed {
            continue;
        }
        let mut rebuilt = line[..data_idx].to_vec();
        rebuilt.extend_from_slice(b"data:");
        if !cleaned.is_empty() {
            rebuilt.push(b' ');
            rebuilt.extend_from_slice(&cleaned);
        }
        lines[idx] = rebuilt;
        modified = true;
    }
    if !modified {
        if !found_data {
            // Raw JSON without an SSE `data:` prefix.
            let (cleaned, changed) = strip_usage_metadata_from_json(trim_space(payload));
            return if changed { cleaned } else { payload.to_vec() };
        }
        return payload.to_vec();
    }
    lines.join(&b'\n')
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// One SSE chunk parsed once for the filter's several questions (the JSON text, whether it is
/// valid JSON, and the value tree).
struct Chunk {
    value: std::sync::Arc<Value>,
    valid: bool,
}

impl Chunk {
    /// Parses through the executor parse memo, so an observer that already read the same frame
    /// (response model) and this filter share one parse and one validity scan.
    fn new(raw_json: &[u8]) -> Self {
        let json = trim_space(raw_json);
        Chunk { valid: !json.is_empty() && crate::helps::parse_cache::valid(json), value: crate::helps::parse_cache::parse(raw_json) }
    }

    fn has_usage_metadata(&self) -> bool {
        self.valid && (exists(&self.value, "usageMetadata") || exists(&self.value, "response.usageMetadata"))
    }

    fn is_terminal(&self) -> bool {
        let finish = first_existing(&self.value, &["candidates.0.finishReason", "response.candidates.0.finishReason"]);
        finish.is_some_and(|f| !res_str(&f).trim().is_empty())
    }

    fn is_stop_without_usage(&self) -> bool {
        self.valid && self.is_terminal() && !self.has_usage_metadata()
    }

    /// [`strip_usage_metadata_from_json`] on the already parsed chunk.
    fn into_stripped(self, raw_json: &[u8]) -> (Vec<u8>, bool) {
        if !self.valid || self.is_terminal() || !self.has_usage_metadata() {
            return (raw_json.to_vec(), false);
        }
        // Release our share first so the memo can hand its value over without a copy.
        drop(self.value);
        let mut changed = false;
        let out = crate::helps::parse_cache::edit(raw_json, |v| {
            if let Some(usage) = v.g("usageMetadata").into_value() {
                cpa_json::set(v, "cpaUsageMetadata", usage);
                cpa_json::delete(v, "usageMetadata");
                changed = true;
            }
            if let Some(usage) = v.g("response.usageMetadata").into_value() {
                cpa_json::set(v, "response.cpaUsageMetadata", usage);
                cpa_json::delete(v, "response.usageMetadata");
                changed = true;
            }
            changed
        });
        (out, changed)
    }
}

/// Renames `usageMetadata` to `cpaUsageMetadata` (also under `response.`) unless the chunk is
/// terminal (`candidates.0.finishReason` / `response.candidates.0.finishReason` non-empty).
/// Handles both AI Studio and Antigravity shapes. Returns the (possibly unchanged) JSON and
/// whether it changed.
pub fn strip_usage_metadata_from_json(raw_json: &[u8]) -> (Vec<u8>, bool) {
    let json_bytes = trim_space(raw_json);
    if json_bytes.is_empty() || !cpa_json::valid(json_bytes) {
        return (raw_json.to_vec(), false);
    }
    let mut v = cpa_json::parse(json_bytes);
    let finish = first_existing(&v, &["candidates.0.finishReason", "response.candidates.0.finishReason"]);
    let terminal = finish.is_some_and(|f| !res_str(&f).trim().is_empty());
    if terminal || (!exists(&v, "usageMetadata") && !exists(&v, "response.usageMetadata")) {
        return (raw_json.to_vec(), false);
    }
    let mut changed = false;
    if let Some(usage) = v.g("usageMetadata").into_value() {
        cpa_json::set(&mut v, "cpaUsageMetadata", usage);
        cpa_json::delete(&mut v, "usageMetadata");
        changed = true;
    }
    if let Some(usage) = v.g("response.usageMetadata").into_value() {
        cpa_json::set(&mut v, "response.cpaUsageMetadata", usage);
        cpa_json::delete(&mut v, "response.usageMetadata");
        changed = true;
    }
    (cpa_json::to_vec(&v), changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_usage_with_cache_and_reasoning() {
        let body = br#"{"service_tier":"default","usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,
            "prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":10}}}"#;
        let d = parse_openai_usage(body);
        assert_eq!((d.input_tokens, d.output_tokens, d.total_tokens), (100, 50, 150));
        assert_eq!((d.cached_tokens, d.cache_read_tokens, d.reasoning_tokens), (40, 40, 10));
        assert_eq!(d.response_service_tier, "default");
        assert_eq!(d.token_breakdown.input.uncached_tokens, 60);
        assert!(d.token_breakdown.valid());
        // Responses-style names and a usage-less body.
        let d = parse_openai_usage(br#"{"usage":{"input_tokens":7,"output_tokens":3}}"#);
        assert_eq!((d.input_tokens, d.output_tokens, d.total_tokens), (7, 3, 10));
        assert!(!parse_openai_usage(br#"{"id":"x"}"#).has_token_usage());
    }

    #[test]
    fn openai_stream_usage_lines() {
        assert!(parse_openai_stream_usage(b"data: [DONE]").is_none());
        assert!(parse_openai_stream_usage(br#"data: {"choices":[{"delta":{"content":"hi"}}]}"#).is_none());
        let d = parse_openai_stream_usage(br#"data: {"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#).unwrap();
        assert_eq!(d.total_tokens, 7);
        // A tier-only chunk reports just the tier; the buffer keeps earlier counters.
        let mut buf = StreamUsageBuffer::default();
        buf.observe_openai_stream(br#"data: {"model":"m","usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#);
        buf.observe_openai_stream(br#"data: {"service_tier":"flex"}"#);
        let d = buf.detail().unwrap();
        assert_eq!((d.total_tokens, d.response_service_tier.as_str()), (7, "flex"));
        assert_eq!(buf.response_model(), "m");
    }

    #[test]
    fn claude_usage_and_stream_merge() {
        let start = br#"data: {"type":"message_start","message":{"model":"c","usage":{"input_tokens":25,"cache_read_input_tokens":10,"cache_creation_input_tokens":5,"output_tokens":1}}}"#;
        let delta = br#"data: {"type":"message_delta","usage":{"output_tokens":15,"output_tokens_details":{"thinking_tokens":4}}}"#;
        let mut buf = StreamUsageBuffer::default();
        buf.observe_claude_stream(start);
        buf.observe_claude_stream(delta);
        let d = buf.detail().unwrap();
        assert_eq!(d.input_tokens, 25);
        assert_eq!(d.output_tokens, 15);
        assert_eq!(d.reasoning_tokens, 4);
        assert_eq!((d.cache_read_tokens, d.cache_creation_tokens, d.cached_tokens), (10, 5, 10));
        assert_eq!(d.total_tokens, 25 + 15 + 10 + 5);
        assert!(d.token_breakdown.valid());
        assert_eq!(buf.response_model(), "c");
        // Cached falls back to cache creation when nothing was read.
        let d = parse_claude_usage(br#"{"usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":9}}"#);
        assert_eq!((d.cached_tokens, d.total_tokens), (9, 12));
    }

    #[test]
    fn gemini_family_usage() {
        let d = parse_gemini_usage(br#"{"usageMetadata":{"promptTokenCount":10,"toolUsePromptTokenCount":2,"candidatesTokenCount":5,"thoughtsTokenCount":3,"cachedContentTokenCount":4}}"#);
        assert_eq!((d.input_tokens, d.output_tokens, d.reasoning_tokens, d.cached_tokens), (12, 5, 3, 4));
        assert_eq!(d.total_tokens, 20);
        assert!(d.token_breakdown.valid());
        let d = parse_antigravity_usage(br#"{"response":{"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}}"#);
        assert_eq!(d.total_tokens, 2);
        assert!(parse_gemini_stream_usage(br#"data: {"usageMetadata":{}}"#).is_none());
        assert!(parse_gemini_stream_usage(br#"data: {"candidates":[]}"#).is_none());
    }

    #[test]
    fn interactions_usage_shapes() {
        let d = parse_interactions_usage(br#"{"usage":{"total_input_tokens":8,"total_output_tokens":2,"total_thought_tokens":1,"total_tokens":11}}"#);
        assert_eq!((d.input_tokens, d.output_tokens, d.reasoning_tokens, d.total_tokens), (8, 2, 1, 11));
        let d = parse_interactions_usage(br#"{"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4}}"#);
        assert_eq!((d.input_tokens, d.total_tokens), (3, 7));
    }

    #[test]
    fn sse_usage_metadata_filter() {
        let mid = b"data: {\"traceId\":\"t1\",\"response\":{\"candidates\":[{\"content\":{}}],\"usageMetadata\":{\"totalTokenCount\":3}}}";
        let out = filter_sse_usage_metadata(mid);
        let line = String::from_utf8(out).unwrap();
        assert!(line.starts_with("data: {"));
        assert!(line.contains("cpaUsageMetadata") && !line.contains("\"usageMetadata\""));
        // Terminal chunks pass through untouched.
        let last = b"data: {\"response\":{\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"totalTokenCount\":3}}}";
        assert_eq!(filter_sse_usage_metadata(last), last.to_vec());
        // Raw JSON without a data: prefix is handled too.
        let raw = filter_sse_usage_metadata(br#"{"usageMetadata":{"a":1}}"#);
        assert_eq!(String::from_utf8(raw).unwrap(), r#"{"cpaUsageMetadata":{"a":1}}"#);
    }
}

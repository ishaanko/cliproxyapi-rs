//! Claude server-side web search as Responses `web_search_call` items
//! (Go: claude_openai-responses_web_search.go).
//!
//! Claude reports a search as a `server_tool_use` block plus a `web_search_tool_result` block;
//! Responses models it as one `web_search_call` item. The pair folds into one item on the way out
//! and expands back on the way in, so a replayed turn still shows the search and its hits.

use std::sync::LazyLock;

use cpa_json::{J, Res, Value};
use regex::Regex;

pub(super) const CLAUDE_WEB_SEARCH_TOOL_NAME: &str = "web_search";

/// Namespaces the Claude `server_tool_use` id inside the Responses item id.
const RESPONSES_WEB_SEARCH_ID_PREFIX: &str = "ws_";

/// Anthropic requires server tool ids to match `^srvtoolu_[a-zA-Z0-9_]+$`.
const CLAUDE_SERVER_TOOL_ID_PREFIX: &str = "srvtoolu_";

/// Characters Anthropic forbids in a server tool id (stricter than tool_use ids: no `-`).
static CLAUDE_SERVER_TOOL_ID_SANITIZER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-zA-Z0-9_]").expect("static regex"));

pub(super) fn responses_web_search_call_id(claude_tool_use_id: &str) -> String {
    format!("{RESPONSES_WEB_SEARCH_ID_PREFIX}{claude_tool_use_id}")
}

/// Recovers a valid Claude `server_tool_use` id from a Responses item id. The body is always
/// sanitized and re-prefixed, since a history may carry foreign ids like `ws_00112233aabb`;
/// genuine Claude ids pass through unchanged.
fn claude_web_search_tool_use_id(responses_item_id: &str) -> String {
    let trimmed = responses_item_id.trim();
    let body = trimmed.strip_prefix(RESPONSES_WEB_SEARCH_ID_PREFIX).unwrap_or(trimmed);
    let body = body.strip_prefix(CLAUDE_SERVER_TOOL_ID_PREFIX).unwrap_or(body);
    let body = CLAUDE_SERVER_TOOL_ID_SANITIZER.replace_all(body, "_");
    if body.is_empty() {
        return String::new();
    }
    format!("{CLAUDE_SERVER_TOOL_ID_PREFIX}{body}")
}

/// The query of a Claude `server_tool_use` input given as (possibly accumulated) JSON text.
pub(super) fn claude_web_search_query(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    cpa_json::parse_str(input).g("query").str().trim().to_string()
}

/// Converts the content of a `web_search_tool_result` block into the Responses `results`.
/// Entries ride through verbatim: Anthropic needs the genuine `encrypted_content` of each result
/// to replay it. `None` when the content is neither an object nor an array.
pub(super) fn claude_web_search_results_to_responses(content: &Res<'_>) -> Option<Value> {
    if content.is_object() {
        return Some(content.value());
    }
    if !content.is_array() {
        return None;
    }
    let results: Vec<Value> = content
        .array()
        .iter()
        .filter(|entry| {
            entry.g("type").str() == "web_search_tool_result_error" || !entry.g("url").str().trim().is_empty()
        })
        .map(Res::value)
        .collect();
    Some(Value::Array(results))
}

/// The Responses item for one Claude server-side search. `results` is `None` when the upstream
/// turn ended before the result block arrived.
pub(super) fn build_responses_web_search_call_item(
    claude_tool_use_id: &str,
    query: &str,
    results: Option<&Value>,
) -> Value {
    let mut item = cpa_json::parse_str(
        r#"{"id":"","type":"web_search_call","status":"completed","action":{"type":"search","query":""}}"#,
    );
    cpa_json::set(&mut item, "id", responses_web_search_call_id(claude_tool_use_id));
    cpa_json::set(&mut item, "action.query", query);
    if let Some(results) = results {
        cpa_json::set(&mut item, "results", results.clone());
    }
    item
}

/// Inverse of [`build_responses_web_search_call_item`]: the Claude block pair for a replayed
/// `web_search_call`, empty when the item id is unusable.
pub(super) fn convert_responses_web_search_call_to_claude_blocks(item: &Res<'_>) -> Vec<Value> {
    let tool_use_id = claude_web_search_tool_use_id(item.g("id").str().trim());
    if tool_use_id.is_empty() {
        return vec![];
    }

    let mut usage = cpa_json::parse_str(r#"{"type":"server_tool_use","id":"","name":"","input":{}}"#);
    cpa_json::set(&mut usage, "id", tool_use_id.as_str());
    cpa_json::set(&mut usage, "name", CLAUDE_WEB_SEARCH_TOOL_NAME);
    let query = responses_web_search_call_query(item);
    if !query.is_empty() {
        cpa_json::set(&mut usage, "input.query", query);
    }

    let mut result = cpa_json::parse_str(r#"{"type":"web_search_tool_result","tool_use_id":"","content":[]}"#);
    cpa_json::set(&mut result, "tool_use_id", tool_use_id);
    if let Some(content) = responses_web_search_results_to_claude(&item.g("results")) {
        cpa_json::set(&mut result, "content", content);
    }
    vec![usage, result]
}

/// The query of a Responses `web_search_call`, tolerating the `queries` array and the `open_page`
/// action shape OpenAI clients emit.
fn responses_web_search_call_query(item: &Res<'_>) -> String {
    for path in ["action.query", "action.queries.0", "action.url"] {
        let query = item.g(path).str().trim().to_string();
        if !query.is_empty() {
            return query;
        }
    }
    String::new()
}

/// Claude `web_search_tool_result` content for Responses `results`. Entries without the genuine
/// `encrypted_content` are dropped (Anthropic rejects forged or missing values); `None` when
/// nothing remains.
fn responses_web_search_results_to_claude(results: &Res<'_>) -> Option<Value> {
    if results.is_object() {
        return Some(results.value());
    }
    if !results.is_array() {
        return None;
    }
    let mut blocks: Vec<Value> = Vec::new();
    for entry in results.array() {
        if entry.g("type").str() == "web_search_tool_result_error" {
            blocks.push(entry.value());
            continue;
        }
        if entry.g("encrypted_content").str().trim().is_empty() {
            continue;
        }
        let mut block = entry.value();
        cpa_json::set(&mut block, "type", "web_search_result");
        blocks.push(block);
    }
    (!blocks.is_empty()).then_some(Value::Array(blocks))
}

/// Mirrors Responses `annotations` back onto a Claude text block as `citations`. Entries without
/// the mandatory `encrypted_index` cannot be replayed and are dropped.
pub(super) fn attach_claude_citations(mut text_block: Value, annotations: &Res<'_>) -> Value {
    if !annotations.is_array() {
        return text_block;
    }
    let citations: Vec<Value> = annotations
        .array()
        .iter()
        .filter(|a| !a.g("encrypted_index").str().trim().is_empty())
        .map(Res::value)
        .collect();
    if citations.is_empty() {
        return text_block;
    }
    cpa_json::set(&mut text_block, "citations", Value::Array(citations));
    text_block
}

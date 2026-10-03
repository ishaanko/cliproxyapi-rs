//! xAI Responses event and input normalization (Go: xai_executor_response.go).
//!
//! Covers the response side (hiding xAI's internal X Search traces, restoring namespaced and
//! aliased tool calls, mapping reasoning text events to reasoning summary events, rebuilding a
//! truncated `response.completed` output) and the request side input fixes (reasoning items,
//! encrypted content), plus upstream error classification.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use cpa_core::signature::inspect_grok_encrypted_content;
use cpa_json::{J, Kind, Value};
use cpa_runtime::executor::ExecError;

use super::tools::{ClientToolKey, NamespaceRefs, WEB_SEARCH_TOOL_TYPE, qualify_namespace_tool_name};
use super::util::{at, is_array_at, items, s, ts};
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::status::status_err;

/// The free-tier rolling window advertised by cli-chat-proxy.
const FREE_USAGE_EXHAUSTED_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);

// ---------------------------------------------------------------- internal X Search filter

/// Hides xAI's server-side x_search subtool traces, which it reports as client-style tool calls
/// that Responses clients would otherwise execute again.
pub struct InternalXSearchResponseFilter {
    enabled: bool,
    client_declared_tools: HashSet<ClientToolKey>,
    dropped_output_indexes: HashSet<i64>,
    dropped_item_ids: HashSet<String>,
}

fn is_internal_x_search_tool_name(name: &str) -> bool {
    matches!(name.trim(), "x_user_search" | "x_semantic_search" | "x_keyword_search" | "x_thread_fetch")
}

fn response_call_declared_type(item_type: &str) -> &'static str {
    match item_type.trim() {
        "function_call" => "function",
        "custom_tool_call" => "custom",
        _ => "",
    }
}

/// Go: xaiIsInternalXSearchCall. Client tools sharing a short name are kept only when the
/// call kind matches their effective declaration (custom normalizes to function); namespaced
/// calls and calls without the `xs_call` id prefix are judged by that rule.
pub fn is_internal_x_search_call(item: &Value, client_declared_tools: &HashSet<ClientToolKey>) -> bool {
    let declared_type = response_call_declared_type(&s(item, "type"));
    if declared_type.is_empty() {
        return false;
    }
    let name = ts(item, "name");
    if !is_internal_x_search_tool_name(&name) {
        return false;
    }
    let namespace = ts(item, "namespace");
    // Namespaced calls are restored client tools, never xAI internal X Search traces.
    if !namespace.is_empty() {
        return false;
    }
    if ts(item, "call_id").starts_with("xs_call") {
        return true;
    }
    let key = ClientToolKey { namespace, name, tool_type: declared_type.to_string() };
    !client_declared_tools.contains(&key)
}

impl InternalXSearchResponseFilter {
    pub fn new(enabled: bool, client_declared_tools: HashSet<ClientToolKey>) -> Self {
        Self {
            enabled,
            client_declared_tools,
            dropped_output_indexes: HashSet::new(),
            dropped_item_ids: HashSet::new(),
        }
    }

    /// Filters one event in place; false means the event must be dropped.
    pub fn apply(&mut self, event: &mut Value) -> bool {
        if !self.enabled || !event.is_object() {
            return true;
        }
        if let Some(item) = at(event, "item").cloned()
            && is_internal_x_search_call(&item, &self.client_declared_tools)
        {
            self.record_dropped_item(event, &item);
            return false;
        }
        self.filter_completed_output(event);
        if self.references_dropped_item(event) {
            return false;
        }
        self.compact_output_index(event);
        true
    }

    fn record_dropped_item(&mut self, event: &Value, item: &Value) {
        let output_index = event.g("output_index");
        if output_index.exists() {
            self.dropped_output_indexes.insert(output_index.int());
        }
        for path in ["id", "call_id"] {
            let id = ts(item, path);
            if !id.is_empty() {
                self.dropped_item_ids.insert(id);
            }
        }
    }

    fn references_dropped_item(&self, event: &Value) -> bool {
        let output_index = event.g("output_index");
        if output_index.exists() && self.dropped_output_indexes.contains(&output_index.int()) {
            return true;
        }
        ["item_id", "call_id"].iter().any(|path| {
            let id = ts(event, path);
            !id.is_empty() && self.dropped_item_ids.contains(&id)
        })
    }

    fn compact_output_index(&self, event: &mut Value) {
        let output_index = event.g("output_index");
        if !output_index.exists() {
            return;
        }
        let original = output_index.int();
        let removed_before = self.dropped_output_indexes.iter().filter(|d| **d < original).count() as i64;
        if removed_before == 0 {
            return;
        }
        cpa_json::set(event, "output_index", original - removed_before);
    }

    fn filter_completed_output(&self, event: &mut Value) {
        let Some(output) = at(event, "response.output").and_then(Value::as_array) else { return };
        let kept: Vec<Value> =
            output.iter().filter(|item| !is_internal_x_search_call(item, &self.client_declared_tools)).cloned().collect();
        if kept.len() == output.len() {
            return;
        }
        cpa_json::set(event, "response.output", Value::Array(kept));
    }
}

// ---------------------------------------------------------------- namespace restoration

/// Restores namespaced tool calls in upstream events: flat `ns__tool` names back to
/// `(namespace, tool)` and folded dispatcher calls back to the child call.
pub struct NamespaceRestorer {
    refs: NamespaceRefs,
    dispatcher_item_ids: HashMap<String, String>,
}

impl NamespaceRestorer {
    pub fn new(refs: NamespaceRefs) -> Self {
        Self { refs, dispatcher_item_ids: HashMap::new() }
    }

    pub fn restore(&mut self, data: &mut Value) {
        if self.refs.is_empty() || !data.is_object() {
            return;
        }
        match s(data, "type").as_str() {
            "response.output_item.added" => {
                let item = data.g("item").value();
                if s(&item, "type") == "function_call" {
                    let name = ts(&item, "name");
                    let item_id = ts(&item, "id");
                    if let Some(r) = self.refs.get(&name).filter(|r| r.is_dispatcher) {
                        let namespace = r.namespace.clone();
                        if !item_id.is_empty() {
                            self.dispatcher_item_ids.insert(item_id, namespace.clone());
                        }
                        cpa_json::set(data, "item.namespace", namespace);
                    }
                }
            }
            "response.function_call_arguments.done" => {
                let item_id = ts(data, "item_id");
                if let Some(namespace) = self.dispatcher_item_ids.get(&item_id) {
                    let raw_args = s(data, "arguments");
                    if let Some((_, child_args)) = unwrap_dispatcher_arguments(&raw_args, namespace, &self.refs) {
                        cpa_json::set(data, "arguments", child_args);
                    }
                }
            }
            _ => {
                self.restore_at_path(data, "item");
                let count = items(data, "response.output").len();
                for index in 0..count {
                    self.restore_at_path(data, &format!("response.output.{index}"));
                }
            }
        }
    }

    fn restore_at_path(&self, data: &mut Value, path: &str) {
        if s(data, &format!("{path}.type")) != "function_call" {
            return;
        }
        let qualified = ts(data, &format!("{path}.name"));
        let Some(r) = self.refs.get(&qualified) else { return };
        if r.is_dispatcher {
            let raw_args = s(data, &format!("{path}.arguments"));
            let unwrapped = unwrap_dispatcher_arguments(&raw_args, &r.namespace, &self.refs);
            let (mut child_name, child_args) = match &unwrapped {
                Some((name, args)) => (name.clone(), Some(args.clone())),
                None => (String::new(), None),
            };
            if unwrapped.is_none() && child_name.is_empty() {
                child_name = r.name.clone();
            }
            cpa_json::set(data, &format!("{path}.namespace"), r.namespace.clone());
            if !child_name.is_empty() {
                cpa_json::set(data, &format!("{path}.name"), child_name);
            }
            if let Some(args) = child_args.filter(|a| !a.is_empty()) {
                cpa_json::set(data, &format!("{path}.arguments"), args);
            }
            return;
        }
        cpa_json::set(data, &format!("{path}.name"), r.name.clone());
        cpa_json::set(data, &format!("{path}.namespace"), r.namespace.clone());
    }
}

/// Go: unwrapXAIDispatcherArguments. `(child name, child arguments)` of a folded dispatcher
/// call's `{"name": child, "arguments": ...}` wrapper.
pub fn unwrap_dispatcher_arguments(
    raw_args: &str,
    namespace_name: &str,
    refs: &NamespaceRefs,
) -> Option<(String, String)> {
    if !cpa_json::valid(raw_args.as_bytes()) {
        return None;
    }
    let parsed = cpa_json::parse_str(raw_args);
    let name_field = parsed.g("name");
    if !name_field.exists() || name_field.kind() != Kind::String {
        return None;
    }
    let child_name = name_field.str().trim().to_string();
    if child_name.is_empty() {
        return None;
    }
    if !namespace_name.is_empty() {
        let qualified = qualify_namespace_tool_name(namespace_name, &child_name);
        if refs.get(&qualified).is_some_and(|r| r.is_dispatcher) {
            return None;
        }
    } else {
        let is_child_of_dispatcher =
            refs.values().any(|r| r.is_dispatcher && (r.name == child_name || r.namespace == child_name));
        if !is_child_of_dispatcher && !parsed.g("arguments").exists() {
            return None;
        }
    }
    let args_field = parsed.g("arguments");
    let child_args = if args_field.exists() {
        if args_field.kind() == Kind::String { args_field.str() } else { args_field.raw() }
    } else {
        let mut cleaned = parsed.clone();
        cpa_json::delete(&mut cleaned, "name");
        let text = cpa_json::to_string(&cleaned);
        if !text.is_empty() && text != "{}" { text } else { "{}".to_string() }
    };
    let child_args = if child_args.is_empty() { "{}".to_string() } else { child_args };
    Some((child_name, child_args))
}

/// Go: restoreXAIClientWebSearchName. Maps the alias back to `web_search` on unnamespaced
/// items of an event.
pub fn restore_client_web_search_name(data: &mut Value, alias: &str) {
    if alias.is_empty() || !data.is_object() || !cpa_json::to_string(data).contains(alias) {
        return;
    }
    let restore = |data: &mut Value, base: &str| {
        let name_path = if base.is_empty() { "name".to_string() } else { format!("{base}.name") };
        let fn_path = if base.is_empty() { "function.name".to_string() } else { format!("{base}.function.name") };
        if ts(data, &name_path) == alias {
            cpa_json::set(data, &name_path, WEB_SEARCH_TOOL_TYPE);
        }
        if ts(data, &fn_path) == alias {
            cpa_json::set(data, &fn_path, WEB_SEARCH_TOOL_TYPE);
        }
    };
    // item (output_item.added / done)
    if ts(data, "item.namespace").is_empty() {
        restore(data, "item");
    }
    for prefix in ["response.output", "output"] {
        let count = items(data, prefix).len();
        for idx in 0..count {
            if !ts(data, &format!("{prefix}.{idx}.namespace")).is_empty() {
                continue;
            }
            restore(data, &format!("{prefix}.{idx}"));
        }
    }
    // top-level name only (not function.name)
    if ts(data, "namespace").is_empty() && ts(data, "name") == alias {
        cpa_json::set(data, "name", WEB_SEARCH_TOOL_TYPE);
    }
}

// ---------------------------------------------------------------- input reasoning items

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Null => "Null",
        Kind::False => "False",
        Kind::Number => "Number",
        Kind::String => "String",
        Kind::True => "True",
        Kind::Json => "JSON",
    }
}

/// Go: sanitizeXAIInputEncryptedContent. Drops compaction items and strips the
/// `encrypted_content` of reasoning items whose payload is not valid Grok content.
pub fn sanitize_input_encrypted_content(body: &mut Value) {
    let Some(input) = at(body, "input").and_then(Value::as_array) else { return };
    let mut out: Vec<Value> = Vec::with_capacity(input.len());
    let mut changed = false;
    let mut drop_count = 0usize;
    let mut first_reason = String::new();
    let mut first_item_type = String::new();
    for item in input {
        let item_type = ts(item, "type");
        if item_type != "reasoning" && item_type != "compaction" {
            out.push(item.clone());
            continue;
        }
        let encrypted = item.g("encrypted_content");
        if !encrypted.exists() {
            out.push(item.clone());
            continue;
        }
        let reason = match encrypted.kind() {
            Kind::String => match inspect_grok_encrypted_content(&encrypted.str()) {
                Ok(_) => String::new(),
                Err(err) => err.to_string(),
            },
            Kind::Null => "encrypted_content is null".to_string(),
            other => format!("encrypted_content must be a string, got {}", kind_name(other)),
        };
        if reason.is_empty() {
            out.push(item.clone());
            continue;
        }
        if item_type == "compaction" {
            changed = true;
            drop_count += 1;
            if first_reason.is_empty() {
                first_reason = reason;
                first_item_type = item_type;
            }
            continue;
        }
        let mut next = item.clone();
        cpa_json::delete(&mut next, "encrypted_content");
        out.push(next);
        changed = true;
        drop_count += 1;
        if first_reason.is_empty() {
            first_reason = reason;
            first_item_type = item_type;
        }
    }
    if !changed {
        return;
    }
    cpa_json::set(body, "input", Value::Array(out));
    if drop_count > 0 {
        tracing::debug!(
            component = "xai_encrypted_content_sanitizer",
            dropped = drop_count,
            first_item_type = %first_item_type,
            first_reason = %first_reason,
            "xai executor: removed invalid encrypted_content before upstream"
        );
    }
    merge_adjacent_input_reasoning_summaries(body);
}

/// Go: normalizeXAIInputReasoningItems. Removes null `content`/`encrypted_content` and merges
/// adjacent summary-only reasoning items.
pub fn normalize_input_reasoning_items(body: &mut Value) {
    let Some(input) = at(body, "input").and_then(Value::as_array) else { return };
    let mut deletions: Vec<String> = Vec::new();
    for (i, item) in input.iter().enumerate() {
        if s(item, "type") != "reasoning" {
            continue;
        }
        for field in ["content", "encrypted_content"] {
            if at(item, field).is_some_and(Value::is_null) {
                deletions.push(format!("input.{i}.{field}"));
            }
        }
    }
    for path in deletions {
        cpa_json::delete(body, &path);
    }
    merge_adjacent_input_reasoning_summaries(body);
}

fn can_merge_reasoning_summary(previous: &Value, current: &Value) -> bool {
    if s(previous, "type") != "reasoning" || s(current, "type") != "reasoning" {
        return false;
    }
    if !is_array_at(previous, "summary") || !is_array_at(current, "summary") {
        return false;
    }
    if items(current, "summary").is_empty() {
        return false;
    }
    current.as_object().is_some_and(|m| m.keys().all(|k| k == "type" || k == "summary"))
}

fn merge_adjacent_input_reasoning_summaries(body: &mut Value) {
    let Some(input) = at(body, "input").and_then(Value::as_array) else { return };
    let mut changed = false;
    let mut out: Vec<Value> = Vec::with_capacity(input.len());
    for item in input {
        if let Some(last) = out.last_mut()
            && can_merge_reasoning_summary(last, item)
            && let Some(Value::Array(summary)) = last.get_mut("summary") {
                summary.extend(items(item, "summary").iter().cloned());
                changed = true;
                continue;
            }
        out.push(item.clone());
    }
    if changed {
        cpa_json::set(body, "input", Value::Array(out));
    }
}

// ---------------------------------------------------------------- reasoning summary events

/// Go: xaiNormalizeReasoningSummaryEventName.
pub fn normalize_reasoning_summary_event_name(name: &str) -> String {
    match name {
        "response.reasoning_text.delta" => "response.reasoning_summary_text.delta".to_string(),
        "response.reasoning_text.done" => "response.reasoning_summary_part.done".to_string(),
        other => other.to_string(),
    }
}

/// Go: xaiNormalizeReasoningSummaryEventLine. `event_name` overrides the name carried by an
/// `event:` line; an empty result keeps the line as is.
pub fn normalize_reasoning_summary_event_line(line: &[u8], event_name: &str) -> Vec<u8> {
    let mut name = event_name.to_string();
    if name.is_empty()
        && let Some(rest) = line.strip_prefix(b"event:")
    {
        name = String::from_utf8_lossy(rest).trim().to_string();
    }
    let name = normalize_reasoning_summary_event_name(&name);
    if name.is_empty() {
        return line.to_vec();
    }
    format!("event: {name}").into_bytes()
}

fn normalize_reasoning_summary_index(event: &mut Value) {
    let content_index = event.g("content_index");
    if content_index.exists() && !content_index.raw().is_empty() && !event.g("summary_index").exists() {
        let raw = content_index.value();
        cpa_json::set(event, "summary_index", raw);
    }
    cpa_json::delete(event, "content_index");
}

/// Go: xaiNormalizeReasoningSummaryData. Maps `reasoning_text` events to the reasoning summary
/// shape Responses clients understand, including the items embedded in the event.
pub fn normalize_reasoning_summary_data(event: &mut Value) {
    if !event.is_object() {
        return;
    }
    match s(event, "type").as_str() {
        "response.reasoning_text.delta" => {
            cpa_json::set(event, "type", "response.reasoning_summary_text.delta");
            normalize_reasoning_summary_index(event);
        }
        "response.reasoning_text.done" => {
            cpa_json::set(event, "type", "response.reasoning_summary_part.done");
            cpa_json::set(event, "part.type", "summary_text");
            let text = event.g("text");
            if text.exists() {
                let text = text.str();
                cpa_json::set(event, "part.text", text);
            }
            cpa_json::delete(event, "text");
            normalize_reasoning_summary_index(event);
        }
        "response.content_part.added" => {
            if s(event, "part.type") == "reasoning_text" {
                cpa_json::set(event, "type", "response.reasoning_summary_part.added");
                cpa_json::set(event, "part.type", "summary_text");
                normalize_reasoning_summary_index(event);
            }
        }
        "response.content_part.done"
            if s(event, "part.type") == "reasoning_text" => {
                cpa_json::set(event, "type", "response.reasoning_summary_part.done");
                cpa_json::set(event, "part.type", "summary_text");
                normalize_reasoning_summary_index(event);
            }
        _ => {}
    }
    if let Some(item) = at(event, "item").filter(|i| i.is_object() || i.is_array()).cloned() {
        let mut updated = item.clone();
        if normalize_reasoning_output_item(&mut updated) {
            cpa_json::set(event, "item", updated);
        }
    }
    if let Some(output) = at(event, "response.output").and_then(Value::as_array) {
        let mut updated: Vec<Value> = output.clone();
        let mut changed = false;
        for item in updated.iter_mut() {
            changed |= normalize_reasoning_output_item(item);
        }
        if changed {
            cpa_json::set(event, "response.output", Value::Array(updated));
        }
    }
}

/// Go: xaiNormalizeReasoningSummaryDataEvents. A `reasoning_text.done` event expands into a
/// `reasoning_summary_text.done` followed by the part-done event.
pub fn normalize_reasoning_summary_data_events(event: Value) -> Vec<Value> {
    if !event.is_object() || s(&event, "type") != "response.reasoning_text.done" {
        let mut event = event;
        normalize_reasoning_summary_data(&mut event);
        return vec![event];
    }
    let mut text_done = event.clone();
    cpa_json::set(&mut text_done, "type", "response.reasoning_summary_text.done");
    normalize_reasoning_summary_index(&mut text_done);
    let mut part_done = event;
    normalize_reasoning_summary_data(&mut part_done);
    vec![text_done, part_done]
}

/// Converts `reasoning_text` parts of `items` to `summary_text`; true when any changed.
fn normalize_reasoning_summary_items(items: &mut [Value]) -> bool {
    let mut changed = false;
    for item in items.iter_mut() {
        if s(item, "type") == "reasoning_text" {
            cpa_json::set(item, "type", "summary_text");
            changed = true;
        }
    }
    changed
}

/// Go: xaiNormalizeReasoningOutputItem. Returns whether the item changed.
fn normalize_reasoning_output_item(item: &mut Value) -> bool {
    if !item.is_object() || s(item, "type") != "reasoning" {
        return false;
    }
    let mut changed = false;
    if let Some(summary) = at(item, "summary").and_then(Value::as_array) {
        let mut updated = summary.clone();
        if normalize_reasoning_summary_items(&mut updated) {
            cpa_json::set(item, "summary", Value::Array(updated));
            changed = true;
        }
    }
    let Some(content) = at(item, "content").and_then(Value::as_array) else { return changed };
    let mut summary_items: Vec<Value> =
        content.iter().filter(|part| s(part, "type") == "reasoning_text").cloned().collect();
    if summary_items.is_empty() {
        return changed;
    }
    normalize_reasoning_summary_items(&mut summary_items);
    cpa_json::set(item, "summary", Value::Array(summary_items));
    cpa_json::delete(item, "content");
    true
}

// ---------------------------------------------------------------- completed output

/// Go: xaiCollectOutputItemDone. Keeps finished output items so a completed event without
/// `output` can be rebuilt.
pub fn collect_output_item_done(
    event: &Value,
    by_index: &mut BTreeMap<i64, Value>,
    fallback: &mut Vec<Value>,
) {
    let Some(item) = at(event, "item").filter(|i| i.is_object() || i.is_array()) else { return };
    let output_index = event.g("output_index");
    if output_index.exists() {
        by_index.insert(output_index.int(), item.clone());
        return;
    }
    fallback.push(item.clone());
}

/// Go: xaiPatchCompletedOutput. Fills an empty `response.output` from the collected done items
/// and makes sure usage details exist.
pub fn patch_completed_output(event: &Value, by_index: &BTreeMap<i64, Value>, fallback: &[Value]) -> Value {
    let bytes = ensure_responses_usage_details(&cpa_json::to_vec(event));
    let mut event = cpa_json::parse(&bytes);
    let output_empty = !at(&event, "response.output").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
    if !(output_empty && (!by_index.is_empty() || !fallback.is_empty())) {
        return event;
    }
    let mut output: Vec<Value> = by_index.values().cloned().collect();
    output.extend(fallback.iter().cloned());
    cpa_json::set(&mut event, "response.output", Value::Array(output));
    event
}

// ---------------------------------------------------------------- errors

/// Go: xaiStatusErr. A 403 bad-credentials body becomes 401 (so refresh-after-unauthorized
/// runs); a 429 for exhausted free usage carries a 24h retry hint.
pub fn status_err_for_body(code: u16, body: &[u8]) -> ExecError {
    let mut err = status_err(code, String::from_utf8_lossy(body).into_owned());
    if body.is_empty() {
        return err;
    }
    if code == 403 && is_bad_credentials_body(body) {
        err.status = 401;
        return err;
    }
    if code != 429 {
        return err;
    }
    let parsed = cpa_json::parse(body);
    let code_str = s(&parsed, "code").to_lowercase();
    let mut msg = s(&parsed, "error").to_lowercase();
    if msg.is_empty() {
        msg = String::from_utf8_lossy(body).to_lowercase();
    }
    if code_str.contains("free-usage-exhausted")
        || msg.contains("free-usage-exhausted")
        || msg.contains("included free usage")
    {
        err.retry_after = Some(FREE_USAGE_EXHAUSTED_COOLDOWN);
    }
    err
}

/// Go: isXAIBadCredentialsBody. Nested and flat error shapes of HTTP and websocket payloads.
pub fn is_bad_credentials_body(body: &[u8]) -> bool {
    let parsed = cpa_json::parse(body);
    for path in ["code", "error.code", "body.error.code"] {
        if s(&parsed, path).to_lowercase().contains("bad-credentials") {
            return true;
        }
    }
    for path in ["error", "error.message", "message", "body.error", "body.error.message"] {
        if s(&parsed, path).to_lowercase().contains("access token could not be validated") {
            return true;
        }
    }
    let raw = String::from_utf8_lossy(body).to_lowercase();
    raw.contains("bad-credentials") || raw.contains("access token could not be validated")
}

/// Go: normalizeCodexInstructions (non-native). A missing or null `instructions` becomes "".
pub fn normalize_codex_instructions(body: &mut Value) {
    if !body.is_object() {
        return;
    }
    let instructions = body.g("instructions");
    if !instructions.exists() || instructions.is_null() {
        cpa_json::set(body, "instructions", "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expectations mirror Go's xai_status_err_test.go.
    #[test]
    fn status_err_classification() {
        let free = status_err_for_body(
            429,
            br#"{"code":"subscription:free-usage-exhausted","error":"You've used all the included free usage"}"#,
        );
        assert_eq!((free.status, free.retry_after), (429, Some(Duration::from_secs(86400))));
        assert_eq!(status_err_for_body(429, br#"{"code":"rate_limit","error":"too many requests"}"#).retry_after, None);
        assert_eq!(status_err_for_body(400, br#"{"error":"nope"}"#).status, 400);
        for body in [
            &br#"{"code":"unauthenticated:bad-credentials","error":"x"}"#[..],
            br#"{"error":"The OAuth2 access token could not be validated."}"#,
            br#"{"type":"error","status":403,"error":{"code":"unauthenticated:bad-credentials","message":"m"}}"#,
        ] {
            let err = status_err_for_body(403, body);
            assert_eq!(err.status, 401);
            assert_eq!(err.message.as_bytes(), body);
        }
        assert_eq!(status_err_for_body(403, br#"{"code":"permission_denied","error":"model access"}"#).status, 403);
        assert_eq!(status_err_for_body(403, b"").status, 403);
    }
}

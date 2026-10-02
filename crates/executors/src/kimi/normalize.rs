//! Kimi request normalization (Go: kimi_executor.go `normalizeKimi*` and helps/kimi_responses.go).
//!
//! These rewrite the translated upstream body so Moonshot accepts it: canonical model ids,
//! repaired tool message links, inlined tool schemas, strict temperatures, contiguous Responses
//! tool outputs, and the three upstream URL resolvers. Every function returns its input bytes
//! untouched when nothing needed to change.

use cpa_auth::Auth;
use cpa_auth::kimi::{KIMI_AI_API_BASE_URL, KIMI_API_BASE_URL, is_kimi_ai_auth};
use cpa_core::thinking::parse_suffix;
use cpa_core::util::inline_local_refs;
use cpa_json::{J, Res};
use serde_json::Value;

/// Reasoning stand-in for assistant tool-call turns that have none.
pub const KIMI_REASONING_UNAVAILABLE: &str = "[reasoning unavailable]";

// ---------------------------------------------------------------- model id

/// Go: stripKimiPrefix.
fn strip_kimi_prefix(model: &str) -> String {
    let model = model.trim();
    if model.len() >= 5 && model.is_char_boundary(5) && model[..5].eq_ignore_ascii_case("kimi-") {
        return model[5..].to_string();
    }
    model.to_string()
}

/// Canonical upstream model id: strips `kimi-` and a `[1m]` suffix, maps the K2.7/K2.8 code
/// aliases to `kimi-for-coding[-highspeed]`, and keeps a trailing thinking suffix (Go:
/// normalizeKimiUpstreamModel).
pub fn normalize_kimi_upstream_model(model: &str) -> String {
    let model = model.trim();
    let parsed = parse_suffix(model);
    let mut base = parsed.model_name.trim().to_lowercase();
    if let Some(stripped) = base.strip_suffix("[1m]") {
        base = stripped.to_string();
    }
    let normalized = match base.as_str() {
        "kimi-k2.8" | "k2.8" | "kimi-k2.8-code" | "k2.8-code" | "kimi-k2.8-preview" | "k2.8-preview"
        | "kimi-k2.7-code" | "k2.7-code" | "kimi-for-coding" | "for-coding" => "kimi-for-coding".to_string(),
        "kimi-k2.7-code-highspeed" | "k2.7-code-highspeed" | "kimi-for-coding-highspeed" | "for-coding-highspeed" => {
            "kimi-for-coding-highspeed".to_string()
        }
        _ => strip_kimi_prefix(&base),
    };
    if parsed.has_suffix {
        return format!("{normalized}({})", parsed.raw_suffix);
    }
    normalized
}

// ---------------------------------------------------------------- URLs

/// Upstream API base URL: `base_url` attribute, then metadata, then the domain default (Go:
/// ResolveKimiBaseURL).
pub fn resolve_kimi_base_url(auth: Option<&Auth>) -> String {
    if let Some(auth) = auth {
        let raw = auth.attributes.get("base_url").map(|v| v.trim().trim_end_matches('/')).unwrap_or("");
        if !raw.is_empty() {
            return raw.to_string();
        }
        let raw = auth.metadata.get("base_url").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if !raw.is_empty() {
            return raw.trim_end_matches('/').to_string();
        }
        if is_kimi_ai_auth(auth) {
            return KIMI_AI_API_BASE_URL.to_string();
        }
    }
    KIMI_API_BASE_URL.to_string()
}

fn with_v1_path(base: String, tail: &str) -> String {
    if base.ends_with("/v1") { format!("{base}/{tail}") } else { format!("{base}/v1/{tail}") }
}

/// Go: ResolveKimiResponsesURL.
pub fn resolve_kimi_responses_url(auth: Option<&Auth>) -> String {
    with_v1_path(resolve_kimi_base_url(auth), "responses")
}

/// Go: ResolveKimiChatURL.
pub fn resolve_kimi_chat_url(auth: Option<&Auth>) -> String {
    with_v1_path(resolve_kimi_base_url(auth), "chat/completions")
}

/// Base URL for the delegated Claude Messages path: the base minus a trailing `/v1` (Go:
/// ResolveKimiClaudeBaseURL).
pub fn resolve_kimi_claude_base_url(auth: Option<&Auth>) -> String {
    let base = resolve_kimi_base_url(auth);
    base.strip_suffix("/v1").map(str::to_string).unwrap_or(base)
}

// ---------------------------------------------------------------- tool message links

fn is_usable_kimi_reasoning(reasoning: &str) -> bool {
    let trimmed = reasoning.trim();
    !trimmed.is_empty() && trimmed != KIMI_REASONING_UNAVAILABLE
}

fn has_kimi_tool_calls(msg: &Res<'_>) -> bool {
    matches!(msg.g("tool_calls").v(), Some(Value::Array(a)) if !a.is_empty())
}

fn has_kimi_legacy_function_call(msg: &Res<'_>) -> bool {
    let function_call = msg.g("function_call");
    match function_call.v() {
        None | Some(Value::Null) => false,
        Some(v @ Value::Object(_)) => v.to_string().trim() != "{}",
        Some(v) => !v.to_string().trim().is_empty(),
    }
}

fn has_kimi_assistant_reasoning(msg: &Res<'_>) -> bool {
    let reasoning = msg.g("reasoning_content");
    reasoning.exists() && !reasoning.str().trim().is_empty()
}

fn is_kimi_assistant_content_part_empty(part: &Res<'_>) -> bool {
    match part.v() {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Object(_)) => {
            let text = part.g("text");
            if text.exists() {
                return text.str().trim().is_empty();
            }
            if part.g("type").str().trim() == "text" {
                return true;
            }
            part.raw().trim() == "{}"
        }
        Some(_) => false,
    }
}

fn is_kimi_assistant_content_empty(content: &Res<'_>) -> bool {
    match content.v() {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Array(items)) => items.iter().all(|p| is_kimi_assistant_content_part_empty(&Res::of(p))),
        Some(_) => false,
    }
}

fn should_drop_kimi_assistant_message(msg: &Res<'_>) -> bool {
    if msg.g("role").str().trim() != "assistant" {
        return false;
    }
    if has_kimi_tool_calls(msg) || has_kimi_legacy_function_call(msg) || has_kimi_assistant_reasoning(msg) {
        return false;
    }
    is_kimi_assistant_content_empty(&msg.g("content"))
}

/// Reasoning for an assistant tool-call turn that has none: the latest usable reasoning, else
/// the turn's own text, else the unavailable marker (Go: fallbackAssistantReasoning).
fn fallback_assistant_reasoning(msg: &Res<'_>, latest: Option<&str>) -> String {
    if let Some(latest) = latest
        && is_usable_kimi_reasoning(latest)
    {
        return latest.to_string();
    }
    let content = msg.g("content");
    match content.v() {
        Some(Value::String(s)) => {
            let text = s.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        }
        Some(Value::Array(items)) => {
            let parts: Vec<String> = items
                .iter()
                .map(|item| Res::of(item).g("text").str().trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            if !parts.is_empty() {
                return parts.join("\n");
            }
        }
        _ => {}
    }
    KIMI_REASONING_UNAVAILABLE.to_string()
}

/// One pending field write on a message.
struct MessagePatch {
    index: usize,
    path: &'static str,
    value: String,
}

/// Repairs assistant/tool message adjacency: drops empty assistant messages, fills missing
/// `tool_call_id` (from `call_id` or the single pending call), and gives assistant tool-call
/// turns usable `reasoning_content` (Go: normalizeKimiToolMessageLinks).
pub fn normalize_kimi_tool_message_links(body: &[u8]) -> Vec<u8> {
    if body.is_empty() || !cpa_json::valid(body) {
        return body.to_vec();
    }
    let root = cpa_json::parse(body);
    let Some(Value::Array(msgs)) = root.g("messages").v().cloned() else {
        return body.to_vec();
    };

    let mut dropped_messages = vec![false; msgs.len()];
    let mut patches: Vec<MessagePatch> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    let mut patched = 0usize;
    let mut patched_reasoning = 0usize;
    let mut ambiguous = 0usize;
    let mut latest_reasoning: Option<String> = None;

    for (msg_index, raw) in msgs.iter().enumerate() {
        let msg = Res::of(raw);
        if should_drop_kimi_assistant_message(&msg) {
            dropped_messages[msg_index] = true;
            dropped += 1;
            continue;
        }
        match msg.g("role").str().trim() {
            "assistant" => {
                let reasoning = msg.g("reasoning_content");
                if reasoning.exists() {
                    let text = reasoning.str();
                    if is_usable_kimi_reasoning(&text) {
                        latest_reasoning = Some(text);
                    }
                }
                if let Some(Value::Array(tool_calls)) = msg.g("tool_calls").v()
                    && !tool_calls.is_empty()
                {
                    if !reasoning.exists() || !is_usable_kimi_reasoning(&reasoning.str()) {
                        patches.push(MessagePatch {
                            index: msg_index,
                            path: "reasoning_content",
                            value: fallback_assistant_reasoning(&msg, latest_reasoning.as_deref()),
                        });
                        patched_reasoning += 1;
                    }
                    for call in tool_calls {
                        let id = Res::of(call).g("id").str().trim().to_string();
                        if !id.is_empty() {
                            pending.push(id);
                        }
                    }
                }
            }
            "tool" => {
                let mut tool_call_id = msg.g("tool_call_id").str().trim().to_string();
                if tool_call_id.is_empty() {
                    tool_call_id = msg.g("call_id").str().trim().to_string();
                    if !tool_call_id.is_empty() {
                        patches.push(MessagePatch { index: msg_index, path: "tool_call_id", value: tool_call_id.clone() });
                        patched += 1;
                    }
                }
                if tool_call_id.is_empty() {
                    match pending.len() {
                        1 => {
                            tool_call_id = pending[0].clone();
                            patches.push(MessagePatch { index: msg_index, path: "tool_call_id", value: tool_call_id.clone() });
                            patched += 1;
                        }
                        0 => {}
                        _ => ambiguous += 1,
                    }
                }
                if !tool_call_id.is_empty()
                    && let Some(pos) = pending.iter().position(|p| *p == tool_call_id)
                {
                    pending.remove(pos);
                }
            }
            _ => {}
        }
    }

    if dropped > 0 {
        tracing::debug!(dropped_assistant_messages = dropped, "kimi executor: dropped empty assistant messages");
    }
    if ambiguous > 0 {
        tracing::warn!(
            ambiguous_tool_messages = ambiguous,
            pending_tool_calls = pending.len(),
            "kimi executor: tool messages missing tool_call_id with ambiguous candidates"
        );
    }
    if dropped == 0 && patches.is_empty() {
        return body.to_vec();
    }

    let mut items: Vec<Value> = Vec::with_capacity(msgs.len() - dropped);
    let mut patch_index = 0;
    for (msg_index, mut msg) in msgs.into_iter().enumerate() {
        if dropped_messages[msg_index] {
            continue;
        }
        while patch_index < patches.len() && patches[patch_index].index == msg_index {
            let patch = &patches[patch_index];
            cpa_json::set(&mut msg, patch.path, patch.value.as_str());
            patch_index += 1;
        }
        items.push(msg);
    }
    let mut out = root;
    cpa_json::set(&mut out, "messages", Value::Array(items));
    if patched > 0 || patched_reasoning > 0 {
        tracing::debug!(
            patched_tool_messages = patched,
            patched_reasoning_messages = patched_reasoning,
            "kimi executor: normalized tool message fields"
        );
    }
    cpa_json::to_vec(&out)
}

// ---------------------------------------------------------------- tools and temperature

/// Inlines local `$ref`s, drops `$defs`/`definitions` and gives the schema root a `type` (Go:
/// normalizeKimiParametersSchema).
fn normalize_kimi_parameters_schema(params_raw: &str) -> String {
    if params_raw.trim().is_empty() {
        return params_raw.to_string();
    }
    let inlined = inline_local_refs(params_raw);
    let mut params = cpa_json::parse_str(&inlined);
    if params.g("$defs").exists() {
        cpa_json::delete(&mut params, "$defs");
    }
    if params.g("definitions").exists() {
        cpa_json::delete(&mut params, "definitions");
    }
    if !params.g("type").exists() {
        cpa_json::set(&mut params, "type", "object");
    }
    cpa_json::to_string(&params)
}

/// Normalizes the schemas under `tools` (`function.parameters` / `parameters`) or the legacy
/// `functions` array. Returns whether anything changed.
fn normalize_kimi_tool_list(root: &mut Value, array_key: &str, is_tools: bool) -> bool {
    let Some(Value::Array(items)) = root.g(array_key).v().cloned() else {
        return false;
    };
    if items.is_empty() {
        return false;
    }
    let mut changed = false;
    let mut updated = Vec::with_capacity(items.len());
    for mut item in items {
        let param_path = {
            let view = Res::of(&item);
            if is_tools && view.g("function.parameters").exists() {
                Some("function.parameters")
            } else if view.g("parameters").exists() {
                Some("parameters")
            } else {
                None
            }
        };
        if let Some(path) = param_path {
            let raw_params = item.g(path).value();
            if raw_params.is_object() {
                let raw = cpa_json::to_string(&raw_params);
                let normalized = normalize_kimi_parameters_schema(&raw);
                if normalized != raw && cpa_json::set_raw(&mut item, path, &normalized).is_ok() {
                    changed = true;
                }
            }
        }
        updated.push(item);
    }
    if changed {
        cpa_json::set(root, array_key, Value::Array(updated));
    }
    changed
}

/// Tool and legacy function parameter schemas in Moonshot's accepted form (Go:
/// normalizeKimiTools).
pub fn normalize_kimi_tools(body: &[u8]) -> Vec<u8> {
    if body.is_empty() {
        return body.to_vec();
    }
    let mut root = cpa_json::parse(body);
    let tools = normalize_kimi_tool_list(&mut root, "tools", true);
    let functions = normalize_kimi_tool_list(&mut root, "functions", false);
    if tools || functions { cpa_json::to_vec(&root) } else { body.to_vec() }
}

/// Upstream accepts only 0.6 with thinking disabled and 1.0 otherwise; any other temperature is
/// dropped (Go: normalizeKimiTemperature).
pub fn normalize_kimi_temperature(body: &[u8]) -> Vec<u8> {
    let mut root = cpa_json::parse(body);
    let temp = root.g("temperature");
    if !temp.exists() {
        return body.to_vec();
    }
    let value = temp.float();
    let want = if root.g("thinking.type").str().eq_ignore_ascii_case("disabled") { 0.6 } else { 1.0 };
    if value != want {
        cpa_json::delete(&mut root, "temperature");
        return cpa_json::to_vec(&root);
    }
    body.to_vec()
}

// ---------------------------------------------------------------- responses input

fn is_responses_tool_call(item: &Value) -> bool {
    matches!(Res::of(item).g("type").str().trim(), "function_call" | "custom_tool_call")
}

fn is_responses_tool_output(item: &Value) -> bool {
    matches!(Res::of(item).g("type").str().trim(), "function_call_output" | "custom_tool_call_output")
}

fn call_id_of(item: &Value) -> String {
    cpa_translator::common::extract_responses_call_id(&Res::of(item))
}

/// Keeps the tool outputs of a parallel function call batch contiguous right after the calls by
/// deferring intervening non-tool items (Go: NormalizeKimiResponsesInput).
pub fn normalize_kimi_responses_input(body: &[u8]) -> Vec<u8> {
    if body.is_empty() || !cpa_json::valid(body) {
        return body.to_vec();
    }
    let mut root = cpa_json::parse(body);
    let Some(Value::Array(items)) = root.g("input").v().cloned() else {
        return body.to_vec();
    };
    if items.is_empty() {
        return body.to_vec();
    }

    let mut reordered = false;
    let mut result: Vec<&Value> = Vec::with_capacity(items.len());
    let mut i = 0;
    while i < items.len() {
        if !is_responses_tool_call(&items[i]) {
            result.push(&items[i]);
            i += 1;
            continue;
        }

        let start_calls = i;
        let mut end_calls = i;
        let mut call_ids: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut call_id_count = 0usize;
        while end_calls < items.len() && is_responses_tool_call(&items[end_calls]) {
            let call_id = call_id_of(&items[end_calls]);
            if !call_id.is_empty() {
                *call_ids.entry(call_id).or_insert(0) += 1;
                call_id_count += 1;
            }
            end_calls += 1;
        }
        result.extend(&items[start_calls..end_calls]);
        if call_id_count == 0 {
            i = end_calls;
            continue;
        }

        let mut needed = call_ids.clone();
        let mut remaining_needed = call_id_count;
        let mut last_matching: Option<usize> = None;
        let mut j = end_calls;
        while j < items.len() && remaining_needed > 0 {
            let it = &items[j];
            if is_responses_tool_call(it) {
                break;
            }
            if is_responses_tool_output(it) {
                let cid = call_id_of(it);
                if let Some(count) = needed.get_mut(&cid)
                    && *count > 0
                {
                    *count -= 1;
                    remaining_needed -= 1;
                    last_matching = Some(j);
                }
            }
            j += 1;
        }

        match last_matching {
            Some(last) if remaining_needed == 0 => {
                let mut matching_outputs: Vec<&Value> = Vec::with_capacity(call_id_count);
                let mut intervening: Vec<&Value> = Vec::new();
                let mut consumed = call_ids;
                for it in &items[end_calls..=last] {
                    if is_responses_tool_output(it) {
                        let cid = call_id_of(it);
                        if let Some(count) = consumed.get_mut(&cid)
                            && *count > 0
                        {
                            *count -= 1;
                            matching_outputs.push(it);
                            continue;
                        }
                    }
                    intervening.push(it);
                }
                if !intervening.is_empty() {
                    reordered = true;
                }
                result.extend(matching_outputs);
                result.extend(intervening);
                i = last + 1;
            }
            _ => i = end_calls,
        }
    }

    if !reordered {
        return body.to_vec();
    }
    let rebuilt: Vec<Value> = result.into_iter().cloned().collect();
    cpa_json::set(&mut root, "input", Value::Array(rebuilt));
    cpa_json::to_vec(&root)
}

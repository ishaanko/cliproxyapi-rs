//! Interactions responses (Go: interactions_gemini_response.go): Interactions upstream to a Gemini
//! client, plus the interactions->interactions passthroughs.

use std::collections::HashMap;

use chrono::Utc;
use cpa_json::{json, Res, Value, J};

use super::shared::{
    first_non_empty_interaction_string, gemini_text_part_json, interactions_content_part_to_gemini_part,
};
use crate::common::{contains_json_ref, interactions_usage, set_gemini_function_response_result};
use crate::gemini::claude::{set_function_response_from_text, RawDoc};
use crate::registry::{Ctx, Param};

/// Per-stream state for Interactions events converted to Gemini chunks.
#[derive(Default)]
struct ToGeminiState {
    id: String,
    model: String,
    service_tier: String,
    step_names: HashMap<i64, String>,
    step_ids: HashMap<i64, String>,
    step_signatures: HashMap<i64, String>,
}

/// Interactions request to an Interactions upstream: unchanged.
pub fn convert_interactions_request_to_interactions(_model: &str, raw: &[u8], _stream: bool) -> Vec<u8> {
    raw.to_vec()
}

/// Forwards an Interactions stream chunk unchanged (empty chunks yield nothing).
pub fn convert_interactions_response_passthrough(
    _ctx: &Ctx,
    _model: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Vec<Vec<u8>> {
    if raw.is_empty() {
        return vec![];
    }
    vec![raw.to_vec()]
}

/// Forwards a complete Interactions response unchanged.
pub fn convert_interactions_response_passthrough_non_stream(
    _ctx: &Ctx,
    _model: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    Some(raw.to_vec())
}

/// Translates one Interactions stream event into Gemini chunks.
pub fn convert_interactions_response_to_gemini(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(|| ToGeminiState { model: model_name.to_string(), ..Default::default() });
    convert_interactions_event_to_gemini(model_name, raw, st)
}

/// Converts a complete Interactions response into a Gemini `generateContent` response.
pub fn convert_interactions_response_to_gemini_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw);
    let nested = root.g("interaction");
    let interaction = if nested.exists() { nested.value() } else { root.clone() };
    let fallback_id = format!("response_{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let st = ToGeminiState {
        id: first_non_empty_interaction_string(&[&interaction.g("id").str(), &root.g("id").str(), &fallback_id]),
        model: first_non_empty_interaction_string(&[&interaction.g("model").str(), &root.g("model").str(), model_name]),
        service_tier: first_non_empty_interaction_string(&[
            &interaction.g("service_tier").str(),
            &root.g("service_tier").str(),
        ]),
        ..Default::default()
    };
    let mut parts: Vec<Value> = Vec::new();
    let mut steps = interaction.g("steps");
    let mut steps_path = if nested.exists() { "interaction.steps" } else { "steps" };
    if !steps.exists() {
        steps = root.g("steps");
        steps_path = "steps";
    }
    let doc = RawDoc::new(raw);
    let mut step_index = 0;
    steps.for_each(|_, step| {
        parts.extend(step_to_gemini_parts(&step.value(), &doc, &format!("{steps_path}.{step_index}")));
        step_index += 1;
        true
    });
    let out = build_gemini_chunk(&st, model_name, parts, "STOP", &interactions_usage(&Res::of(&root)), true);
    Some(cpa_json::to_vec(&out))
}

/// SSE payload of a stream chunk: a bare JSON object, or the joined `data:` lines.
fn sse_payload(raw: &[u8]) -> Vec<u8> {
    let trimmed = raw.trim_ascii();
    if trimmed.is_empty() || trimmed == b"[DONE]" {
        return vec![];
    }
    if trimmed.starts_with(b"{") {
        return trimmed.to_vec();
    }
    let mut payload: Vec<u8> = Vec::new();
    for line in trimmed.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line).trim_ascii();
        let Some(data) = line.strip_prefix(b"data:") else { continue };
        let data = data.trim_ascii();
        if data.is_empty() || data == b"[DONE]" {
            continue;
        }
        if !payload.is_empty() {
            payload.push(b'\n');
        }
        payload.extend_from_slice(data);
    }
    payload
}

fn convert_interactions_event_to_gemini(model_name: &str, raw: &[u8], st: &mut ToGeminiState) -> Vec<Vec<u8>> {
    let payload = sse_payload(raw);
    if payload.is_empty() {
        return vec![];
    }
    let root = cpa_json::parse(&payload);
    match root.g("event_type").str().as_str() {
        "interaction.created" => {
            let interaction = root.g("interaction");
            st.id = first_non_empty_interaction_string(&[&st.id, &interaction.g("id").str()]);
            st.model = first_non_empty_interaction_string(&[&st.model, &interaction.g("model").str(), model_name]);
        }
        "step.start" => remember_step(&root, st),
        "step.delta" => {
            if let Some(chunk) = step_delta_to_gemini_chunk(model_name, &root, st) {
                return vec![cpa_json::to_vec(&chunk)];
            }
        }
        "interaction.completed" | "finish" => {
            let interaction = root.g("interaction");
            st.id = first_non_empty_interaction_string(&[&st.id, &interaction.g("id").str()]);
            st.model = first_non_empty_interaction_string(&[&st.model, &interaction.g("model").str(), model_name]);
            st.service_tier =
                first_non_empty_interaction_string(&[&st.service_tier, &interaction.g("service_tier").str()]);
            let chunk = build_gemini_chunk(st, model_name, vec![], "STOP", &interactions_usage(&Res::of(&root)), true);
            return vec![cpa_json::to_vec(&chunk)];
        }
        "response.failed" | "interaction.failed" => {
            let mut err_node = root.g("error");
            if !err_node.exists() {
                err_node = root.g("interaction.error");
            }
            let mut msg = err_node.g("message").str();
            if msg.is_empty() {
                msg = "upstream error occurred".to_string();
            }
            let code_val = first_non_empty_interaction_string(&[
                &err_node.g("code").str(),
                &root.g("code").str(),
                &err_node.g("status").str(),
            ]);
            let (status_code, status_text) = map_interactions_error_to_gemini(&code_val);
            let mut error_response = json!({ "error": { "code": 500, "message": "", "status": "INTERNAL" } });
            cpa_json::set(&mut error_response, "error.code", status_code);
            cpa_json::set(&mut error_response, "error.message", msg);
            cpa_json::set(&mut error_response, "error.status", status_text);
            return vec![cpa_json::to_vec(&error_response)];
        }
        _ => {}
    }
    vec![]
}

fn map_interactions_error_to_gemini(code: &str) -> (i64, &'static str) {
    let code = code.trim();
    match code.to_lowercase().as_str() {
        "400" | "invalid_argument" => (400, "INVALID_ARGUMENT"),
        "401" | "unauthenticated" => (401, "UNAUTHENTICATED"),
        "403" | "permission_denied" => (403, "PERMISSION_DENIED"),
        "404" | "not_found" => (404, "NOT_FOUND"),
        "429" | "resource_exhausted" | "rate_limit_exceeded" => (429, "RESOURCE_EXHAUSTED"),
        "499" | "canceled" | "cancelled" => (499, "CANCELLED"),
        "503" | "unavailable" => (503, "UNAVAILABLE"),
        "504" | "deadline_exceeded" => (504, "DEADLINE_EXCEEDED"),
        "500" | "internal" => (500, "INTERNAL"),
        _ => match code.parse::<i64>() {
            Ok(n) if (400..600).contains(&n) => (n, if n >= 500 { "INTERNAL" } else { "INVALID_ARGUMENT" }),
            _ => (500, "INTERNAL"),
        },
    }
}

fn remember_step(root: &Value, st: &mut ToGeminiState) {
    let index = root.g("index").int();
    let step = root.g("step");
    st.step_names.insert(index, step.g("name").str());
    st.step_ids.insert(
        index,
        first_non_empty_interaction_string(&[&step.g("call_id").str(), &step.g("id").str()]),
    );
    st.step_signatures.insert(
        index,
        first_non_empty_interaction_string(&[
            &step.g("signature").str(),
            &step.g("thoughtSignature").str(),
            &step.g("thought_signature").str(),
        ]),
    );
}

fn step_delta_to_gemini_chunk(model_name: &str, root: &Value, st: &mut ToGeminiState) -> Option<Value> {
    let index = root.g("index").int();
    let delta = root.g("delta");
    match delta.g("type").str().as_str() {
        "arguments_delta" => {
            let mut part = json!({ "functionCall": { "name": "", "args": {} } });
            let stored_name = st.step_names.get(&index).cloned().unwrap_or_default();
            cpa_json::set(
                &mut part,
                "functionCall.name",
                first_non_empty_interaction_string(&[&stored_name, &root.g("step.name").str()]),
            );
            if let Some(id) = st.step_ids.get(&index).filter(|id| !id.is_empty()) {
                cpa_json::set(&mut part, "functionCall.id", id.as_str());
            }
            if let Some(signature) = st.step_signatures.get(&index).filter(|s| !s.is_empty()) {
                cpa_json::set(&mut part, "thoughtSignature", signature.as_str());
            }
            let arguments = delta.g("arguments").str().trim().to_string();
            if !arguments.is_empty() && cpa_json::valid(arguments.as_bytes()) {
                let _ = cpa_json::set_raw(&mut part, "functionCall.args", &arguments);
            }
            Some(build_gemini_chunk(st, model_name, vec![part], "", &Res::NONE, false))
        }
        "text" => {
            let text = first_non_empty_interaction_string(&[&delta.g("text").str(), &delta.g("content.text").str()]);
            if text.is_empty() {
                return None;
            }
            Some(build_gemini_chunk(st, model_name, vec![gemini_text_part_json(&text, false)], "", &Res::NONE, false))
        }
        "thought_summary" => {
            let text = first_non_empty_interaction_string(&[&delta.g("content.text").str(), &delta.g("text").str()]);
            if text.is_empty() {
                return None;
            }
            Some(build_gemini_chunk(st, model_name, vec![gemini_text_part_json(&text, true)], "", &Res::NONE, false))
        }
        "thought_signature" => {
            let signature = first_non_empty_interaction_string(&[
                &delta.g("signature").str(),
                &delta.g("thought_signature").str(),
                &delta.g("thoughtSignature").str(),
            ]);
            if signature.is_empty() {
                return None;
            }
            st.step_signatures.insert(index, signature.clone());
            let mut part = gemini_text_part_json("", true);
            cpa_json::set(&mut part, "thoughtSignature", signature);
            Some(build_gemini_chunk(st, model_name, vec![part], "", &Res::NONE, false))
        }
        _ => None,
    }
}

/// `path` locates `step` in `doc`, for copying results as source text.
fn step_to_gemini_parts(step: &Value, doc: &RawDoc<'_>, path: &str) -> Vec<Value> {
    match step.g("type").str().as_str() {
        "function_call" => vec![function_call_step_to_gemini_part(step)],
        "function_result" => vec![function_response_step_to_gemini_part(step, doc, path)],
        "thought" => content_to_gemini_parts(&step.g("content"), true),
        _ => content_to_gemini_parts(&step.g("content"), false),
    }
}

fn content_to_gemini_parts(content: &Res<'_>, thought: bool) -> Vec<Value> {
    if !content.exists() {
        return vec![];
    }
    if let Some(text) = content.as_str() {
        return vec![gemini_text_part_json(text, thought)];
    }
    if content.is_object() {
        return interactions_content_part_to_gemini_part(&content.value(), thought).into_iter().collect();
    }
    if content.is_array() {
        return content
            .array()
            .iter()
            .filter_map(|item| interactions_content_part_to_gemini_part(&item.value(), thought))
            .collect();
    }
    vec![]
}

/// First existing value among `paths`.
fn first_existing<'a>(root: &'a Value, paths: &[&str]) -> Res<'a> {
    for path in paths {
        let value = root.g(path);
        if value.exists() {
            return value;
        }
    }
    Res::NONE
}

fn function_call_step_to_gemini_part(step: &Value) -> Value {
    let mut part = json!({ "functionCall": { "name": "", "args": {} } });
    cpa_json::set(&mut part, "functionCall.name", step.g("name").str());
    let id = first_non_empty_interaction_string(&[&step.g("call_id").str(), &step.g("id").str()]);
    if !id.is_empty() {
        cpa_json::set(&mut part, "functionCall.id", id);
    }
    let signature = first_non_empty_interaction_string(&[
        &step.g("signature").str(),
        &step.g("thoughtSignature").str(),
        &step.g("thought_signature").str(),
    ]);
    if !signature.is_empty() {
        cpa_json::set(&mut part, "thoughtSignature", signature);
    }
    set_raw_object(&mut part, "functionCall.args", &first_existing(step, &["arguments", "args"]));
    part
}

fn function_response_step_to_gemini_part(step: &Value, doc: &RawDoc<'_>, path: &str) -> Value {
    let mut part = json!({ "functionResponse": { "name": "", "response": {} } });
    cpa_json::set(&mut part, "functionResponse.name", step.g("name").str());
    let id = first_non_empty_interaction_string(&[&step.g("call_id").str(), &step.g("id").str()]);
    if !id.is_empty() {
        cpa_json::set(&mut part, "functionResponse.id", id);
    }
    let key = ["result", "response"].into_iter().find(|key| step.g(key).exists());
    let text = key.and_then(|key| doc.at(&format!("{path}.{key}")));
    set_function_response(&mut part, "functionResponse.response", &first_existing(step, &["result", "response"]), text);
    part
}

/// Sets `path` to `value`: a string holding valid JSON is parsed, anything else is kept as-is,
/// and a missing value becomes `{}`.
fn set_raw_object(out: &mut Value, path: &str, value: &Res<'_>) {
    if !value.exists() {
        cpa_json::set(out, path, json!({}));
        return;
    }
    if let Some(s) = value.as_str() {
        let raw = s.trim();
        if !raw.is_empty() && cpa_json::valid(raw.as_bytes()) {
            cpa_json::set(out, path, cpa_json::parse_str(raw));
            return;
        }
    }
    cpa_json::set(out, path, value.value());
}

/// Sets a functionResponse `response` via the shared helpers (which stringify `$ref` results).
fn set_function_response(out: &mut Value, path: &str, value: &Res<'_>, source_text: Option<&str>) {
    if !value.exists() {
        cpa_json::set(out, path, json!({}));
        return;
    }
    if let Some(s) = value.as_str() {
        let raw = s.trim();
        if !raw.is_empty() && cpa_json::valid(raw.as_bytes()) {
            set_function_response_from_text(out, path, raw);
            return;
        }
    }
    // A result with `$ref` is stored as its source text.
    if let Some(text) = source_text.filter(|_| contains_json_ref(value)) {
        set_function_response_from_text(out, path, text);
        return;
    }
    *out = cpa_json::parse(&set_gemini_function_response_result(&cpa_json::to_vec(out), path, value));
}

/// Builds a Gemini candidate chunk from `parts` (an empty single text part when
/// `include_empty_part` and no parts), with optional finish reason, model/id and usage.
fn build_gemini_chunk(
    st: &ToGeminiState,
    model_name: &str,
    mut parts: Vec<Value>,
    finish_reason: &str,
    usage: &Res<'_>,
    include_empty_part: bool,
) -> Value {
    let mut out = json!({ "candidates": [{ "content": { "parts": [], "role": "model" }, "index": 0 }] });
    if parts.is_empty() && include_empty_part {
        parts.push(gemini_text_part_json("", false));
    }
    if !parts.is_empty() {
        cpa_json::set(&mut out, "candidates.0.content.parts", Value::Array(parts));
    }
    if !finish_reason.is_empty() {
        cpa_json::set(&mut out, "candidates.0.finishReason", finish_reason);
    }
    let model = first_non_empty_interaction_string(&[&st.model, model_name]);
    if !model.is_empty() {
        cpa_json::set(&mut out, "modelVersion", model);
    }
    if !st.id.is_empty() {
        cpa_json::set(&mut out, "responseId", st.id.as_str());
    }
    if !st.service_tier.is_empty() {
        cpa_json::set(&mut out, "usageMetadata.serviceTier", st.service_tier.as_str());
    }
    set_gemini_usage_metadata(&mut out, usage);
    out
}

fn usage_int(usage: &Res<'_>, paths: &[&str]) -> Option<i64> {
    paths.iter().map(|p| usage.g(p)).find(|v| v.exists()).map(|v| v.int())
}

fn set_gemini_usage_metadata(out: &mut Value, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    let input_tokens = usage_int(usage, &["input_tokens", "total_input_tokens"]);
    let output_tokens = usage_int(usage, &["output_tokens", "total_output_tokens"]);
    let total_tokens = usage_int(usage, &["total_tokens"]);
    if let Some(input) = input_tokens {
        cpa_json::set(out, "usageMetadata.promptTokenCount", input);
        cpa_json::set(
            out,
            "usageMetadata.promptTokensDetails",
            json!([{ "modality": "TEXT", "tokenCount": input }]),
        );
    }
    if let Some(output) = output_tokens {
        cpa_json::set(out, "usageMetadata.candidatesTokenCount", output);
    }
    if let Some(total) = total_tokens {
        cpa_json::set(out, "usageMetadata.totalTokenCount", total);
    } else if input_tokens.is_some() || output_tokens.is_some() {
        cpa_json::set(
            out,
            "usageMetadata.totalTokenCount",
            input_tokens.unwrap_or(0) + output_tokens.unwrap_or(0),
        );
    }
    if let Some(thoughts) = usage_int(usage, &["reasoning_tokens", "total_thought_tokens"]) {
        cpa_json::set(out, "usageMetadata.thoughtsTokenCount", thoughts);
    }
    if let Some(cached) = usage_int(usage, &["cached_tokens", "total_cached_tokens"]) {
        cpa_json::set(out, "usageMetadata.cachedContentTokenCount", cached);
    }
}

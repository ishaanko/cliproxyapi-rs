//! Antigravity response -> Gemini response (Go: antigravity_gemini_response.go).

use cpa_core::util;
use cpa_json::{json, J, Value};

use crate::common;
use crate::registry::{Ctx, Param};

/// Per-stream state: enough to synthesize a faithful terminal chunk when upstream never emits
/// `finishReason`.
#[derive(Default)]
struct StreamState {
    saw_response: bool,
    saw_finish_reason: bool,
    model_version: String,
    response_id: String,
    /// Latest usage snapshot.
    usage_metadata: Option<Value>,
}

impl StreamState {
    fn observe(&mut self, raw: &Value) {
        if !self.saw_response {
            self.saw_response = has_response_payload(raw);
        }
        if !self.saw_finish_reason {
            self.saw_finish_reason = ["response.candidates", "candidates"]
                .iter()
                .any(|path| raw.g(path).array().iter().any(|c| !c.g("finishReason").str().is_empty()));
        }
        if self.model_version.is_empty() {
            self.model_version = first_string_value(raw, &["response.modelVersion", "modelVersion"]);
        }
        if self.response_id.is_empty() {
            self.response_id = first_string_value(raw, &["response.responseId", "responseId"]);
        }
        // FilterSSEUsageMetadata renames non-terminal usage to cpaUsageMetadata, so both
        // spellings are tracked.
        for path in ["response.usageMetadata", "response.cpaUsageMetadata", "usageMetadata", "cpaUsageMetadata"] {
            let usage = raw.g(path);
            if usage.exists() {
                self.usage_metadata = Some(usage.value());
                break;
            }
        }
    }

    /// The terminal chunk shape observed from upstream: a model-role candidate carrying an empty
    /// text part and finishReason, plus the last known usage, modelVersion and responseId.
    fn synthetic_terminal_chunk(&self) -> Value {
        let mut chunk = json!({"candidates": [{"content": {"role": "model", "parts": [{"text": ""}]}, "finishReason": "STOP"}]});
        if let Some(usage) = &self.usage_metadata {
            cpa_json::set(&mut chunk, "usageMetadata", usage.clone());
        }
        if !self.model_version.is_empty() {
            cpa_json::set(&mut chunk, "modelVersion", self.model_version.clone());
        }
        if !self.response_id.is_empty() {
            cpa_json::set(&mut chunk, "responseId", self.response_id.clone());
        }
        chunk
    }
}

/// Whether a chunk carries generated content or token accounting; an envelope such as `{}` or
/// `{"response":{"candidates":[]}}` must not count as a started stream.
fn has_response_payload(raw: &Value) -> bool {
    for path in ["response.candidates", "candidates"] {
        let candidates = raw.g(path);
        if candidates.is_array() && !candidates.array().is_empty() {
            return true;
        }
    }
    for path in ["response.usageMetadata", "response.cpaUsageMetadata", "usageMetadata", "cpaUsageMetadata"] {
        let usage = raw.g(path);
        if usage.is_object() && !usage.entries().is_empty() {
            return true;
        }
    }
    false
}

fn first_string_value(raw: &Value, paths: &[&str]) -> String {
    paths.iter().map(|p| raw.g(p).str()).find(|s| !s.is_empty()).unwrap_or_default()
}

/// Go: `ConvertAntigravityResponseToGemini`. Output depends on `ctx.alt`: without it nothing is
/// emitted, with an empty alt the `response` object of the chunk is returned, with a non-empty alt
/// the input is a JSON array and the output an array of each item's `response`.
pub fn convert_antigravity_response_to_gemini(
    ctx: &Ctx,
    _model: &str,
    original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let mut raw = raw_json;
    if raw.starts_with(b"data:") {
        raw = raw[5..].trim_ascii();
    }

    let state = param.state(StreamState::default);

    if raw == b"[DONE]" {
        // Never finalize a stream that produced no response at all.
        if !state.saw_response || state.saw_finish_reason {
            return vec![];
        }
        state.saw_finish_reason = true;
        let mut final_chunk = state.synthetic_terminal_chunk();
        if ctx.alt.as_deref().is_some_and(|a| !a.is_empty()) {
            final_chunk = Value::Array(vec![final_chunk]);
        }
        return vec![cpa_json::to_vec(&final_chunk)];
    }

    let parsed = cpa_json::parse(raw);
    state.observe(&parsed);

    let Some(alt) = ctx.alt.as_deref() else { return vec![] };
    let chunk = if alt.is_empty() {
        let response = parsed.g("response");
        if response.exists() {
            let chunk = restore_usage_metadata(response.value());
            restore_function_names(chunk, original_request_raw_json)
        } else {
            return vec![vec![]];
        }
    } else {
        let mut items: Vec<Value> = Vec::new();
        if parsed.is_array() {
            for item in parsed.as_array().into_iter().flatten() {
                let response = item.g("response");
                if response.exists() {
                    items.push(response.value());
                }
            }
        }
        Value::Array(items)
    };
    vec![cpa_json::to_vec(&chunk)]
}

/// Go: `ConvertAntigravityResponseToGeminiNonStream`.
pub fn convert_antigravity_response_to_gemini_non_stream(
    _ctx: &Ctx,
    _model: &str,
    original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let parsed = cpa_json::parse(raw_json);
    if parsed.is_null() && raw_json.trim_ascii() != b"null" {
        // Unparseable bodies pass through untouched.
        return Some(raw_json.to_vec());
    }
    let response = parsed.g("response");
    let mut chunk = if response.exists() {
        restore_function_names(restore_usage_metadata(response.value()), original_request_raw_json)
    } else if util::disambiguated_tool_name_map(original_request_raw_json).is_empty() {
        // Nothing to restore: the body passes through untouched.
        parsed
    } else {
        restore_function_names(parsed, original_request_raw_json)
    };
    let candidates = chunk.g("candidates");
    if candidates.is_array() {
        // Default every candidate, not just the first, so none is left unterminated.
        let missing: Vec<usize> = candidates
            .array()
            .iter()
            .enumerate()
            .filter(|(_, c)| c.g("finishReason").str().is_empty())
            .map(|(i, _)| i)
            .collect();
        for i in missing {
            cpa_json::set(&mut chunk, &format!("candidates.{i}.finishReason"), "STOP");
        }
    }
    Some(cpa_json::to_vec(&chunk))
}

/// Restores the client's tool names in functionCall/functionResponse parts of every candidate.
fn restore_function_names(mut chunk: Value, original_request_raw_json: &[u8]) -> Value {
    let name_map = util::disambiguated_tool_name_map(original_request_raw_json);
    if name_map.is_empty() {
        return chunk;
    }
    let candidates = chunk.g("candidates").array().len();
    for candidate_index in 0..candidates {
        let parts = chunk.g(&format!("candidates.{candidate_index}.content.parts")).array().len();
        for part_index in 0..parts {
            for field in ["functionCall", "functionResponse", "function_call", "function_response"] {
                let path = format!("candidates.{candidate_index}.content.parts.{part_index}.{field}.name");
                let name_result = chunk.g(&path);
                let name = name_result.str();
                if name.is_empty() {
                    continue;
                }
                let restored = util::restore_sanitized_tool_name(&name_map, &name);
                if name_result.is_string() && restored == name {
                    continue;
                }
                cpa_json::set(&mut chunk, &path, restored);
            }
        }
    }
    chunk
}

/// Renames `cpaUsageMetadata` back to `usageMetadata` (the executor renames usage in non-terminal
/// chunks to hide it from clients that do not expect it).
fn restore_usage_metadata(mut chunk: Value) -> Value {
    let cpa_usage = chunk.g("cpaUsageMetadata");
    if cpa_usage.exists() {
        let v = cpa_usage.value();
        cpa_json::set(&mut chunk, "usageMetadata", v);
        cpa_json::delete(&mut chunk, "cpaUsageMetadata");
    }
    chunk
}

/// Go: `GeminiTokenCount`.
pub fn gemini_token_count(_ctx: &Ctx, count: i64) -> Vec<u8> {
    common::gemini_token_count_json(count)
}

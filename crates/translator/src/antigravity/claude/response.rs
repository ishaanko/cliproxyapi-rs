//! Antigravity response -> Claude Messages response (Go: antigravity_claude_response.go).
//!
//! The streaming converter is a state machine over [`Params`]: one raw upstream chunk in, one
//! buffer of concatenated SSE events out.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use cpa_core::cache;
use cpa_core::signature::{self, b64, SignatureProvider};
use cpa_core::util;
use cpa_json::{json, J, Res, Value};

use super::signature_validation::{
    encode_gemini_claude_carrier_signature, CARRIER_ANY, CARRIER_FUNCTION, CARRIER_NEXT, CARRIER_PREVIOUS,
    CARRIER_STANDALONE, CARRIER_TEXT,
};
use super::web_search::{
    append_claude_web_search_stream_blocks, build_claude_web_search_content, grounding_metadata,
    new_web_search_tool_use_id, should_translate_grounding, text_content,
};
use crate::common;
use crate::registry::{Ctx, Param};

/// Decodes `R...` (2-layer base64) to `E...` (1-layer base64, Anthropic format). Empty when the
/// decoding fails (invalid signatures are skipped).
fn decode_signature(signature: &str) -> String {
    if signature.is_empty() {
        return String::new();
    }
    if signature.starts_with('R') {
        return match b64::std(signature).ok() {
            Some(decoded) => String::from_utf8_lossy(&decoded).into_owned(),
            None => {
                tracing::warn!("antigravity claude response: failed to decode signature, skipping");
                String::new()
            }
        };
    }
    signature.to_string()
}

fn format_gemini_claude_carrier_value(model: &str, signature: &str, direction: &str, target_kind: &str) -> String {
    if signature::signature_provider_from_model_name(model) == SignatureProvider::Gemini {
        return encode_gemini_claude_carrier_signature(signature, direction, target_kind);
    }
    format_claude_signature_value(model, signature)
}

/// Provider signatures are emitted as provider-native opaque values without CPA-specific
/// prefixes (such as claude#, gemini#, or gpt#).
fn format_claude_signature_value(model: &str, signature: &str) -> String {
    if cache::get_model_group(model) == "claude" {
        return decode_signature(signature);
    }
    signature.to_string()
}

/// Per-stream conversion state (Go: `Params`).
#[derive(Default)]
pub struct Params {
    has_first_response: bool,
    /// 0 none, 1 text, 2 thinking, 3 function.
    response_type: i32,
    response_index: i64,
    has_finish_reason: bool,
    finish_reason: String,
    has_usage_metadata: bool,
    prompt_token_count: i64,
    candidates_token_count: i64,
    thoughts_token_count: i64,
    total_token_count: i64,
    cached_token_count: i64,
    has_sent_final_events: bool,
    has_tool_use: bool,
    has_content: bool,
    has_semantic_content: bool,
    last_semantic_kind: String,
    has_web_search_tool: bool,
    web_search_requests: i64,
    web_search_text_buffer: String,
    /// Accumulates thinking text for signature caching.
    current_thinking_text: String,
    /// Whether the active thinking block already has its terminal signature.
    current_thinking_signed: bool,
    /// Sanitized Gemini function name -> original Claude tool name.
    tool_name_map: HashMap<String, String>,
    /// `model` of the translated request, fixed for the stream.
    model_name: std::sync::Arc<str>,
    /// Whether the request asked for translated web search grounding, fixed for the stream.
    web_search_stream_mode: bool,
}

static TOOL_USE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `args_raw` is the upstream text of `functionCall.args` (Go: `args.Raw`), hashed as sent.
fn claude_tool_use_id(model: &str, function_call: &Res<'_>, fallback: &str, args_raw: Option<&str>) -> String {
    if signature::signature_provider_from_model_name(model) == SignatureProvider::Gemini {
        let stable = util::gemini_claude_tool_use_id(
            &function_call.g("id").str(),
            &function_call.g("name").str(),
            &args_raw.map_or_else(|| function_call.g("args").raw(), str::to_string),
        );
        if !stable.is_empty() {
            return stable;
        }
    }
    util::sanitize_claude_tool_id(fallback)
}

/// Output buffer plus the shared state the Go closures captured.
struct Emitter<'a> {
    p: &'a mut Params,
    out: Vec<u8>,
    model: &'a str,
}

impl Emitter<'_> {
    fn event(&mut self, event: &str, payload: &str) {
        common::append_sse_event_string(&mut self.out, event, payload, 3);
    }

    fn event_value(&mut self, event: &str, payload: &Value) {
        self.event(event, &cpa_json::to_string(payload));
    }

    fn block_stop(&mut self) {
        self.event_value("content_block_stop", &json!({"type": "content_block_stop", "index": self.p.response_index}));
    }

    fn signature_delta(&mut self, sig_value: String) {
        let data = json!({"type": "content_block_delta", "index": self.p.response_index, "delta": {"type": "signature_delta", "signature": sig_value}});
        self.event_value("content_block_delta", &data);
    }

    fn text_delta(&mut self, text: &str) {
        let data = json!({"type": "content_block_delta", "index": self.p.response_index, "delta": {"type": "text_delta", "text": text}});
        self.event_value("content_block_delta", &data);
    }

    fn thinking_delta(&mut self, text: &str) {
        let data = json!({"type": "content_block_delta", "index": self.p.response_index, "delta": {"type": "thinking_delta", "thinking": text}});
        self.event_value("content_block_delta", &data);
    }

    fn append_thinking_signature(&mut self, signature: &str, direction: &str, target_kind: &str) {
        if signature.is_empty() || self.p.response_type != 2 {
            return;
        }
        if !self.p.current_thinking_text.is_empty() {
            cache::cache_signature_best_effort(self.model, &self.p.current_thinking_text, signature);
            self.p.current_thinking_text.clear();
        }
        let sig_value = format_gemini_claude_carrier_value(self.model, signature, direction, target_kind);
        self.signature_delta(sig_value);
        self.p.current_thinking_signed = true;
        self.p.has_content = true;
    }

    fn close_current_block(&mut self) {
        if self.p.response_type == 0 {
            return;
        }
        self.block_stop();
        self.p.response_index += 1;
        self.p.response_type = 0;
        self.p.current_thinking_signed = false;
    }

    fn start_empty_thinking_block(&mut self) {
        let data = json!({"type": "content_block_start", "index": self.p.response_index, "content_block": {"type": "thinking", "thinking": ""}});
        self.event_value("content_block_start", &data);
        self.p.response_type = 2;
        self.p.current_thinking_signed = false;
        self.p.has_content = true;
    }

    fn append_carrier_signature(&mut self, signature: &str, direction: &str, target_kind: &str) {
        if signature.is_empty() || self.p.response_type != 2 {
            return;
        }
        let sig_value = format_gemini_claude_carrier_value(self.model, signature, direction, target_kind);
        self.signature_delta(sig_value);
        self.p.current_thinking_signed = true;
        self.p.has_content = true;
    }

    /// Returns true when the signature opened its own carrier block (so the caller should close it
    /// after the visible text it targets).
    fn append_part_signature(&mut self, signature: &str, direction: &str, target_kind: &str) -> bool {
        if signature.is_empty() {
            return false;
        }
        if self.p.response_type == 2 && !self.p.current_thinking_signed {
            self.append_thinking_signature(signature, direction, target_kind);
            return false;
        }
        if direction == CARRIER_PREVIOUS && target_kind == CARRIER_TEXT {
            cache::cache_signature_best_effort(self.model, "", signature);
            return false;
        }
        self.close_current_block();
        self.start_empty_thinking_block();
        self.append_carrier_signature(signature, direction, target_kind);
        true
    }
}

/// Go: `ConvertAntigravityResponseToClaude`.
pub fn convert_antigravity_response_to_claude(
    _ctx: &Ctx,
    _model: &str,
    original_request_raw_json: &[u8],
    request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let params = param.state(|| {
        let request = cpa_json::parse(request_raw_json);
        let original = cpa_json::parse(original_request_raw_json);
        Params {
            tool_name_map: util::disambiguated_tool_name_map(original_request_raw_json),
            model_name: request.g("model").str().into(),
            web_search_stream_mode: should_translate_grounding(&original, &request),
            ..Default::default()
        }
    });
    let model_name = std::sync::Arc::clone(&params.model_name);
    let web_search_stream_mode = params.web_search_stream_mode;

    if raw_json == b"[DONE]" {
        let mut em = Emitter { p: params, out: Vec::with_capacity(256), model: &model_name };
        if em.p.has_first_response && !em.p.has_content {
            let data = json!({"type": "content_block_start", "index": em.p.response_index, "content_block": {"type": "text", "text": ""}});
            em.event_value("content_block_start", &data);
            em.p.response_type = 1;
            em.p.has_content = true;
        }
        if em.p.has_content {
            append_final_events(&mut em, true);
            em.event("message_stop", r#"{"type":"message_stop"}"#);
            return vec![em.out];
        }
        return vec![];
    }

    let root = cpa_json::parse(raw_json);
    let mut em = Emitter { p: params, out: Vec::with_capacity(1024), model: &model_name };

    // message_start is only sent for the very first chunk.
    if !em.p.has_first_response {
        let mut message_start = json!({
            "type": "message_start",
            "message": {
                "id": "msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY", "type": "message", "role": "assistant", "content": [],
                "model": "claude-3-5-sonnet-20241022", "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        // cpaUsageMetadata carries the usage for message_start.
        let prompt = root.g("response.cpaUsageMetadata.promptTokenCount");
        if prompt.exists() {
            cpa_json::set(&mut message_start, "message.usage.input_tokens", prompt.int());
        }
        let candidates = root.g("response.cpaUsageMetadata.candidatesTokenCount");
        if candidates.exists() && !web_search_stream_mode {
            cpa_json::set(&mut message_start, "message.usage.output_tokens", candidates.int());
        }
        let model_version = root.g("response.modelVersion");
        if model_version.exists() {
            cpa_json::set(&mut message_start, "message.model", model_version.str());
        }
        let response_id = root.g("response.responseId");
        if response_id.exists() {
            cpa_json::set(&mut message_start, "message.id", response_id.str());
        }
        em.event_value("message_start", &message_start);
        em.p.has_first_response = true;
    }

    let mut handled_web_search_grounding = false;
    if web_search_stream_mode && !em.p.has_web_search_tool
        && let Some(grounding) = grounding_metadata(&root) {
            let tool_use_id = new_web_search_tool_use_id();
            let text = std::mem::take(&mut em.p.web_search_text_buffer) + &text_content(&root);
            let mut out = std::mem::take(&mut em.out);
            let mut append = |event: &str, payload: String| common::append_sse_event_string(&mut out, event, &payload, 3);
            em.p.response_index = append_claude_web_search_stream_blocks(&mut append, em.p.response_index, &tool_use_id, &text, &grounding);
            em.out = out;
            em.p.has_web_search_tool = true;
            em.p.web_search_requests = 1;
            em.p.has_content = true;
            em.p.response_type = 0;
            handled_web_search_grounding = true;
        }

    // Each part can carry text, thinking, a thought signature or a function call.
    let parts_result = root.g("response.candidates.0.content.parts");
    if parts_result.is_array() && web_search_stream_mode && !em.p.has_web_search_tool && !handled_web_search_grounding {
        append_web_search_buffered_text(&parts_result, &mut em.p.web_search_text_buffer);
    } else if parts_result.is_array() && !handled_web_search_grounding {
        let part_raws = cpa_json::raw_children(raw_json, "response.candidates.0.content.parts");
        for (i, part) in parts_result.array().iter().enumerate() {
            // Go copies `args.Raw` into partial_json, so keep the upstream text as sent.
            let raw_args = crate::common::raw_in(part_raws.get(i), "functionCall.args");
            convert_part(&mut em, part, raw_args);
        }
    }

    let finish_reason = root.g("response.candidates.0.finishReason");
    if finish_reason.exists() {
        em.p.has_finish_reason = true;
        em.p.finish_reason = finish_reason.str();
    }

    let usage = root.g("response.usageMetadata");
    if usage.exists() {
        let p = &mut em.p;
        p.has_usage_metadata = true;
        p.cached_token_count = usage.g("cachedContentTokenCount").int();
        p.prompt_token_count = usage.g("promptTokenCount").int() - p.cached_token_count;
        p.candidates_token_count = usage.g("candidatesTokenCount").int();
        p.thoughts_token_count = usage.g("thoughtsTokenCount").int();
        p.total_token_count = usage.g("totalTokenCount").int();
        if p.candidates_token_count == 0 && p.total_token_count > 0 {
            p.candidates_token_count = (p.total_token_count - p.prompt_token_count - p.thoughts_token_count).max(0);
        }
    }

    if web_search_stream_mode && !em.p.has_web_search_tool && em.p.has_finish_reason && !em.p.web_search_text_buffer.is_empty() {
        append_buffered_web_search_text_block(&mut em);
    }

    if em.p.has_usage_metadata && em.p.has_finish_reason {
        append_final_events(&mut em, false);
    }

    vec![em.out]
}

fn convert_part(em: &mut Emitter<'_>, part: &Res<'_>, raw_args: Option<&str>) {
    let part_text = part.g("text");
    let function_call = part.g("functionCall");
    let mut thought_signature = part.g("thoughtSignature");
    if !thought_signature.exists() {
        thought_signature = part.g("thought_signature");
    }
    let signature = thought_signature.str();
    let has_thought_signature = thought_signature.exists() && !signature.is_empty() && !function_call.exists();

    if has_thought_signature && (!part_text.exists() || part_text.str().is_empty()) {
        let (direction, target_kind) = if em.p.has_semantic_content {
            (CARRIER_PREVIOUS, em.p.last_semantic_kind.clone())
        } else {
            (CARRIER_NEXT, CARRIER_ANY.to_string())
        };
        em.append_part_signature(&signature, direction, &target_kind);
        return;
    }

    if part_text.exists() {
        let text = part_text.str();
        if part.g("thought").bool() {
            if !text.is_empty() {
                em.p.has_semantic_content = true;
                em.p.last_semantic_kind = CARRIER_TEXT.to_string();
                if em.p.response_type == 2 && em.p.current_thinking_signed {
                    em.close_current_block();
                }
                if em.p.response_type == 2 {
                    em.p.current_thinking_text.push_str(&text);
                    em.thinking_delta(&text);
                    em.p.has_content = true;
                } else {
                    if em.p.response_type != 0 {
                        em.block_stop();
                        em.p.response_index += 1;
                    }
                    let start = json!({"type": "content_block_start", "index": em.p.response_index, "content_block": {"type": "thinking", "thinking": ""}});
                    em.event_value("content_block_start", &start);
                    em.p.current_thinking_signed = false;
                    em.thinking_delta(&text);
                    em.p.response_type = 2;
                    em.p.has_content = true;
                    em.p.current_thinking_text = text.clone();
                }
            }
            if has_thought_signature {
                em.append_thinking_signature(&signature, CARRIER_STANDALONE, CARRIER_TEXT);
            }
        } else {
            let mut signature_targets_visible_text = false;
            if has_thought_signature {
                signature_targets_visible_text = em.append_part_signature(&signature, CARRIER_NEXT, CARRIER_TEXT);
            }
            if em.p.response_type == 1 {
                em.text_delta(&text);
                em.p.has_content = true;
            } else if !text.is_empty() {
                if em.p.response_type != 0 {
                    em.block_stop();
                    em.p.response_index += 1;
                }
                let start = json!({"type": "content_block_start", "index": em.p.response_index, "content_block": {"type": "text", "text": ""}});
                em.event_value("content_block_start", &start);
                em.text_delta(&text);
                em.p.response_type = 1;
                em.p.has_content = true;
            }
            if !text.is_empty() {
                em.p.has_semantic_content = true;
                em.p.last_semantic_kind = CARRIER_TEXT.to_string();
                if signature_targets_visible_text {
                    em.close_current_block();
                }
            }
        }
    } else if function_call.exists() {
        let tool_signature = signature;
        let is_claude_model = cache::get_model_group(em.model) == "claude";
        if !is_claude_model {
            em.append_part_signature(&tool_signature, CARRIER_NEXT, CARRIER_FUNCTION);
        }
        em.p.has_tool_use = true;
        let fc_name = util::restore_sanitized_tool_name(&em.p.tool_name_map, &function_call.g("name").str());

        // Close any open function call block first, then any other open block.
        if em.p.response_type == 3 {
            em.block_stop();
            em.p.response_index += 1;
            em.p.response_type = 0;
        }
        if em.p.response_type != 0 {
            em.block_stop();
            em.p.response_index += 1;
        }

        let fallback_id = format!("{fc_name}-{}-{}", common::unix_nano_now(), TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1);
        let mut data = json!({"type": "content_block_start", "index": em.p.response_index, "content_block": {"type": "tool_use", "id": "", "name": "", "input": {}}});
        cpa_json::set(&mut data, "content_block.id", claude_tool_use_id(em.model, &function_call, &fallback_id, raw_args));
        cpa_json::set(&mut data, "content_block.name", fc_name);
        if is_claude_model && !tool_signature.is_empty() {
            cpa_json::set(&mut data, "content_block.signature", format_claude_signature_value(em.model, &tool_signature));
        }
        em.event_value("content_block_start", &data);

        let args = function_call.g("args");
        if args.exists() {
            let delta = json!({"type": "content_block_delta", "index": em.p.response_index, "delta": {"type": "input_json_delta", "partial_json": raw_args.map_or_else(|| args.raw(), str::to_string)}});
            em.event_value("content_block_delta", &delta);
        }
        em.p.response_type = 3;
        em.p.has_content = true;
        em.p.has_semantic_content = true;
        em.p.last_semantic_kind = CARRIER_FUNCTION.to_string();
    }
}

fn append_web_search_buffered_text(parts: &Res<'_>, buffer: &mut String) {
    for part in parts.array() {
        if part.g("thought").bool() || part.g("functionCall").exists() {
            continue;
        }
        let text = part.g("text");
        if text.exists() {
            buffer.push_str(&text.str());
        }
    }
}

fn append_buffered_web_search_text_block(em: &mut Emitter<'_>) {
    let text = std::mem::take(&mut em.p.web_search_text_buffer);
    if text.is_empty() {
        return;
    }
    let start = json!({"type": "content_block_start", "index": em.p.response_index, "content_block": {"type": "text", "text": ""}});
    em.event_value("content_block_start", &start);
    em.text_delta(&text);
    em.p.response_type = 1;
    em.p.has_content = true;
}

fn append_final_events(em: &mut Emitter<'_>, force: bool) {
    if em.p.has_sent_final_events {
        return;
    }
    if !em.p.has_usage_metadata && !force {
        return;
    }
    // Final events only follow actual output.
    if !em.p.has_content {
        return;
    }
    if em.p.response_type != 0 {
        em.block_stop();
        em.p.response_type = 0;
    }

    let p = &*em.p;
    let stop_reason = resolve_stop_reason(p);
    let mut usage_output_tokens = p.candidates_token_count + p.thoughts_token_count;
    if usage_output_tokens == 0 && p.total_token_count > 0 {
        usage_output_tokens = (p.total_token_count - p.prompt_token_count).max(0);
    }
    let mut delta = json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
        "usage": {"input_tokens": p.prompt_token_count, "output_tokens": usage_output_tokens}
    });
    if p.web_search_requests > 0 {
        cpa_json::set(&mut delta, "usage.server_tool_use.web_search_requests", p.web_search_requests);
    }
    // cache_read_input_tokens signals that prompt caching is working.
    if p.cached_token_count > 0 {
        cpa_json::set(&mut delta, "usage.cache_read_input_tokens", p.cached_token_count);
    }
    em.event_value("message_delta", &delta);
    em.p.has_sent_final_events = true;
}

fn resolve_stop_reason(params: &Params) -> &'static str {
    if params.has_tool_use {
        return "tool_use";
    }
    match params.finish_reason.as_str() {
        "MAX_TOKENS" => "max_tokens",
        _ => "end_turn",
    }
}

/// Go: `ConvertAntigravityResponseToClaudeNonStream`.
pub fn convert_antigravity_response_to_claude_non_stream(
    _ctx: &Ctx,
    _model: &str,
    original_request_raw_json: &[u8],
    request_raw_json: &[u8],
    raw_json: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let tool_name_map = util::disambiguated_tool_name_map(original_request_raw_json);
    let request = cpa_json::parse(request_raw_json);
    let model_name = request.g("model").str();

    let root = cpa_json::parse(raw_json);
    let prompt_tokens = root.g("response.usageMetadata.promptTokenCount").int();
    let candidate_tokens = root.g("response.usageMetadata.candidatesTokenCount").int();
    let thought_tokens = root.g("response.usageMetadata.thoughtsTokenCount").int();
    let total_tokens = root.g("response.usageMetadata.totalTokenCount").int();
    let cached_tokens = root.g("response.usageMetadata.cachedContentTokenCount").int();
    let mut output_tokens = candidate_tokens + thought_tokens;
    if output_tokens == 0 && total_tokens > 0 {
        output_tokens = (total_tokens - prompt_tokens).max(0);
    }

    let mut response = json!({
        "id": "", "type": "message", "role": "assistant", "model": "", "content": [],
        "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0}
    });
    cpa_json::set(&mut response, "id", root.g("response.responseId").str());
    cpa_json::set(&mut response, "model", root.g("response.modelVersion").str());
    cpa_json::set(&mut response, "usage.input_tokens", prompt_tokens);
    cpa_json::set(&mut response, "usage.output_tokens", output_tokens);
    if cached_tokens > 0 {
        cpa_json::set(&mut response, "usage.cache_read_input_tokens", cached_tokens);
    }

    let original = cpa_json::parse(original_request_raw_json);
    if should_translate_grounding(&original, &request)
        && let Some(grounding) = grounding_metadata(&root) {
            let tool_use_id = new_web_search_tool_use_id();
            cpa_json::set(&mut response, "content", build_claude_web_search_content(&tool_use_id, &text_content(&root), &grounding));
            cpa_json::set(&mut response, "stop_reason", "end_turn");
            cpa_json::set(&mut response, "usage.server_tool_use.web_search_requests", 1);
            return Some(cpa_json::to_vec(&response));
        }

    let mut blocks: Vec<Value> = Vec::new();
    let parts = root.g("response.candidates.0.content.parts");
    let mut text_builder = String::new();
    let mut thinking_builder = String::new();
    let mut thinking_signature = String::new();
    let mut thinking_signature_direction = CARRIER_STANDALONE.to_string();
    let mut thinking_signature_target_kind = CARRIER_TEXT.to_string();
    let mut tool_id_counter = 0;
    let mut has_tool_call = false;
    let mut has_semantic_content = false;
    let mut last_semantic_kind = CARRIER_ANY.to_string();

    macro_rules! flush_text {
        () => {
            if !text_builder.is_empty() {
                blocks.push(json!({"type": "text", "text": std::mem::take(&mut text_builder)}));
            }
        };
    }
    // `flush_thinking!()` emits the buffered thinking block and resets the signature carrier
    // fields; `flush_thinking!(last)` skips the reset when nothing reads them afterwards.
    macro_rules! flush_thinking {
        (@emit $($reset:ident)?) => {
            if !(thinking_builder.is_empty() && thinking_signature.is_empty()) {
                let mut block = json!({"type": "thinking", "thinking": std::mem::take(&mut thinking_builder)});
                if !thinking_signature.is_empty() {
                    let sig_value = format_gemini_claude_carrier_value(&model_name, &thinking_signature, &thinking_signature_direction, &thinking_signature_target_kind);
                    cpa_json::set(&mut block, "signature", sig_value);
                }
                blocks.push(block);
                thinking_signature.clear();
                $( flush_thinking!(@$reset); )?
            }
        };
        (@reset) => {
            thinking_signature_direction = CARRIER_STANDALONE.to_string();
            thinking_signature_target_kind = CARRIER_TEXT.to_string();
        };
        () => {
            flush_thinking!(@emit reset)
        };
        (last) => {
            flush_thinking!(@emit)
        };
    }
    macro_rules! append_signature_carrier {
        ($signature:expr, $direction:expr, $kind:expr) => {
            if !$signature.is_empty() {
                blocks.push(json!({
                    "type": "thinking", "thinking": "",
                    "signature": format_gemini_claude_carrier_value(&model_name, $signature, $direction, $kind)
                }));
            }
        };
    }

    if parts.is_array() {
        let part_raws = cpa_json::raw_children(raw_json, "response.candidates.0.content.parts");
        for (part_index, part) in parts.array().into_iter().enumerate() {
            let args_text = crate::common::raw_in(part_raws.get(part_index), "functionCall.args");
            let mut sig = part.g("thoughtSignature");
            if !sig.exists() {
                sig = part.g("thought_signature");
            }
            let signature = if sig.exists() { sig.str() } else { String::new() };

            let function_call = part.g("functionCall");
            if function_call.exists() {
                let mut signature_attached_to_thought = false;
                let is_claude_target = cache::get_model_group(&model_name) == "claude";
                if !is_claude_target && !signature.is_empty() && !thinking_builder.is_empty() && thinking_signature.is_empty() {
                    thinking_signature = signature.clone();
                    thinking_signature_direction = CARRIER_NEXT.to_string();
                    thinking_signature_target_kind = CARRIER_FUNCTION.to_string();
                    signature_attached_to_thought = true;
                }
                flush_thinking!();
                flush_text!();
                has_tool_call = true;

                let name = util::restore_sanitized_tool_name(&tool_name_map, &function_call.g("name").str());
                tool_id_counter += 1;
                if !is_claude_target && !signature.is_empty() && !signature_attached_to_thought {
                    append_signature_carrier!(&signature, CARRIER_NEXT, CARRIER_FUNCTION);
                }
                let mut tool_block = json!({"type": "tool_use", "id": "", "name": "", "input": {}});
                cpa_json::set(&mut tool_block, "id", claude_tool_use_id(&model_name, &function_call, &format!("tool_{tool_id_counter}"), args_text));
                cpa_json::set(&mut tool_block, "name", name);
                if is_claude_target && !signature.is_empty() {
                    cpa_json::set(&mut tool_block, "signature", format_claude_signature_value(&model_name, &signature));
                }
                let args = function_call.g("args");
                if args.exists() && args.is_object() {
                    // Go: SetRawBytes(args.Raw) when the upstream text is valid JSON.
                    let args_raw = args_text.map_or_else(|| args.raw(), str::to_string);
                    if !args_raw.is_empty() && cpa_json::valid(args_raw.as_bytes()) {
                        let _ = cpa_json::set_raw(&mut tool_block, "input", &args_raw);
                    }
                }
                blocks.push(tool_block);
                has_semantic_content = true;
                last_semantic_kind = CARRIER_FUNCTION.to_string();
                continue;
            }

            let text = part.g("text");
            let text_str = text.str();
            if part.g("thought").bool() {
                flush_text!();
                if !thinking_signature.is_empty() {
                    flush_thinking!();
                }
                if text.exists() && !text_str.is_empty() {
                    thinking_builder.push_str(&text_str);
                    has_semantic_content = true;
                    last_semantic_kind = CARRIER_TEXT.to_string();
                }
                if !signature.is_empty() {
                    if !thinking_builder.is_empty() {
                        thinking_signature = signature.clone();
                        thinking_signature_direction = CARRIER_STANDALONE.to_string();
                        thinking_signature_target_kind = CARRIER_TEXT.to_string();
                        flush_thinking!();
                    } else if has_semantic_content && last_semantic_kind == CARRIER_TEXT {
                        cache::cache_signature_best_effort(&model_name, "", &signature);
                    } else if has_semantic_content {
                        append_signature_carrier!(&signature, CARRIER_PREVIOUS, &last_semantic_kind);
                    } else {
                        append_signature_carrier!(&signature, CARRIER_NEXT, CARRIER_ANY);
                    }
                }
                continue;
            }

            let mut visible_signature_carrier = false;
            if !signature.is_empty() {
                if !thinking_builder.is_empty() && thinking_signature.is_empty() {
                    thinking_signature = signature.clone();
                    thinking_signature_direction = CARRIER_NEXT.to_string();
                    thinking_signature_target_kind = CARRIER_TEXT.to_string();
                    flush_thinking!();
                } else {
                    flush_thinking!();
                    flush_text!();
                    if text.exists() && !text_str.is_empty() {
                        append_signature_carrier!(&signature, CARRIER_NEXT, CARRIER_TEXT);
                        visible_signature_carrier = true;
                    } else if has_semantic_content && last_semantic_kind == CARRIER_TEXT {
                        cache::cache_signature_best_effort(&model_name, "", &signature);
                    } else if has_semantic_content {
                        append_signature_carrier!(&signature, CARRIER_PREVIOUS, &last_semantic_kind);
                    } else {
                        append_signature_carrier!(&signature, CARRIER_NEXT, CARRIER_ANY);
                    }
                }
            }
            if text.exists() && !text_str.is_empty() {
                flush_thinking!();
                text_builder.push_str(&text_str);
                has_semantic_content = true;
                last_semantic_kind = CARRIER_TEXT.to_string();
                if visible_signature_carrier {
                    flush_text!();
                }
            }
        }
    }

    flush_thinking!(last);
    flush_text!();

    if !blocks.is_empty() {
        cpa_json::set(&mut response, "content", Value::Array(blocks));
    }

    let mut stop_reason = "end_turn";
    if has_tool_call {
        stop_reason = "tool_use";
    } else {
        let finish = root.g("response.candidates.0.finishReason");
        if finish.exists() && finish.str() == "MAX_TOKENS" {
            stop_reason = "max_tokens";
        }
    }
    cpa_json::set(&mut response, "stop_reason", stop_reason);

    if prompt_tokens == 0 && output_tokens == 0 && !root.g("response.usageMetadata").exists() {
        cpa_json::delete(&mut response, "usage");
    }

    Some(cpa_json::to_vec(&response))
}

/// Go: `ClaudeTokenCount`.
pub fn claude_token_count(_ctx: &Ctx, count: i64) -> Vec<u8> {
    common::claude_input_tokens_json(count)
}

//! Claude Messages request -> Antigravity request (Go: antigravity_claude_request.go).

use cpa_core::cache;
use cpa_core::signature::{
    self, SignatureBlockKind, SignatureProvider, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
};
use cpa_core::thinking::get_thinking_text;
use cpa_core::util;
use cpa_json::{json, J, Res, Value};

use super::signature_validation::{
    carrier_matches_adjacent, decode_gemini_claude_carrier_signature, normalize_claude_bypass_signature,
    CARRIER_ANY, CARRIER_FUNCTION, CARRIER_NEXT, CARRIER_PREVIOUS, CARRIER_STANDALONE, CARRIER_TEXT,
};
use super::web_search::{
    build_web_search_request, is_claude_typed_web_search_tool_type, should_build_web_search_request,
};
use crate::antigravity::function_response;
use crate::common;
use crate::gemini::common::attach_default_safety_settings;

fn resolve_thinking_signature(model: &str, thinking_text: &str, raw_signature: &str) -> String {
    let target = signature::signature_provider_from_model_name(model);
    if target == SignatureProvider::Gemini {
        let c = decode_gemini_claude_carrier_signature(raw_signature);
        if !c.ok {
            return String::new();
        }
        let block_kind = if c.marked && c.target_kind == CARRIER_FUNCTION {
            SignatureBlockKind::GeminiFunctionCall
        } else {
            SignatureBlockKind::GeminiModelPart
        };
        return resolve_provider_compatible_signature(target, &c.signature, block_kind);
    }
    if cache::signature_cache_enabled() {
        return resolve_cache_mode_signature(model, thinking_text, raw_signature);
    }
    let sig = resolve_provider_compatible_signature(target, raw_signature, SignatureBlockKind::Unknown);
    if !sig.is_empty() {
        return sig;
    }
    resolve_bypass_mode_signature_for_provider(target, raw_signature)
}

fn resolve_cache_mode_signature(model: &str, thinking_text: &str, raw_signature: &str) -> String {
    let target = signature::signature_provider_from_model_name(model);
    // A client-carried signature that is incompatible or invalid never falls back to the cache.
    if !raw_signature.is_empty() {
        return resolve_provider_compatible_signature(target, raw_signature, SignatureBlockKind::Unknown);
    }
    if !thinking_text.is_empty() {
        let cached = cache::get_cached_signature(model, thinking_text);
        if !cached.is_empty() {
            if target == SignatureProvider::Claude {
                return signature::compatible_antigravity_claude_thinking_signature(&cached).unwrap_or_default();
            }
            return cached;
        }
    }
    String::new()
}

fn resolve_bypass_mode_signature_for_provider(target: SignatureProvider, raw_signature: &str) -> String {
    if raw_signature.is_empty() {
        return String::new();
    }
    if target != SignatureProvider::Claude && target != SignatureProvider::Unknown {
        return String::new();
    }
    if target == SignatureProvider::Claude {
        return signature::compatible_antigravity_claude_thinking_signature(raw_signature).unwrap_or_default();
    }
    normalize_claude_bypass_signature(raw_signature).unwrap_or_default()
}

fn has_resolved_thinking_signature(model: &str, signature: &str) -> bool {
    let target = signature::signature_provider_from_model_name(model);
    if target == SignatureProvider::Claude {
        return signature::compatible_antigravity_claude_thinking_signature(signature).is_some();
    }
    if signature::compatible_signature_for_provider(target, signature).is_some() {
        return true;
    }
    if cache::signature_cache_enabled() {
        return cache::has_valid_signature(model, signature);
    }
    !signature.is_empty()
}

fn resolve_provider_compatible_signature(target: SignatureProvider, raw_signature: &str, block_kind: SignatureBlockKind) -> String {
    if raw_signature.is_empty() {
        return String::new();
    }
    if target == SignatureProvider::Claude {
        return signature::compatible_antigravity_claude_thinking_signature(raw_signature).unwrap_or_default();
    }
    signature::compatible_signature_for_provider_block(target, raw_signature, block_kind).unwrap_or_default()
}

const TOOL_USE_SIGNATURE_PATHS: [&str; 3] = ["signature", "thought_signature", "extra_content.google.thought_signature"];

fn resolve_tool_use_thought_signature(model: &str, content: &Value, allow_synthetic_fallback: bool) -> String {
    let target = signature::signature_provider_from_model_name(model);
    if target == SignatureProvider::Gemini {
        for path in TOOL_USE_SIGNATURE_PATHS {
            let sig = content.g(path);
            if sig.exists() {
                let s = resolve_provider_compatible_signature(target, &sig.str(), SignatureBlockKind::GeminiFunctionCall);
                if !s.is_empty() {
                    return s;
                }
            }
        }
        return if allow_synthetic_fallback { GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string() } else { String::new() };
    }
    for path in TOOL_USE_SIGNATURE_PATHS {
        let sig = content.g(path);
        if sig.exists() {
            let s = resolve_provider_compatible_signature(target, &sig.str(), SignatureBlockKind::Unknown);
            if !s.is_empty() {
                return s;
            }
        }
    }
    if target == SignatureProvider::Claude {
        return String::new();
    }
    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string()
}

/// The parts of one model/user turn plus the "pending detached signature" state machine used to
/// move opaque Gemini signatures onto neighbouring text/function parts.
#[derive(Default)]
struct TurnParts {
    items: Vec<Value>,
    pending_signature: String,
    pending_target_kind: String,
}

impl TurnParts {
    fn append_detached_carrier(&mut self, signature: &str) {
        self.items.push(json!({"text": "", "thoughtSignature": signature}));
    }

    fn clear_pending(&mut self) {
        self.pending_signature.clear();
        self.pending_target_kind.clear();
    }

    fn set_pending(&mut self, signature: &str, target_kind: &str) {
        if !self.pending_signature.is_empty() {
            let pending = std::mem::take(&mut self.pending_signature);
            self.append_detached_carrier(&pending);
        }
        self.pending_signature = signature.to_string();
        self.pending_target_kind = target_kind.to_string();
    }

    /// Whether the pending signature may attach to a part of `kind` (`text` or `function`).
    fn pending_matches(&self, kind: &str) -> bool {
        self.pending_target_kind.is_empty() || self.pending_target_kind == CARRIER_ANY || self.pending_target_kind == kind
    }
}

fn claude_content(role: &str, parts: Vec<Value>) -> Value {
    json!({"role": role, "parts": parts})
}

fn inline_data_from_source(mime_type: &str, data: &str) -> Value {
    let mut inline = json!({});
    if !mime_type.is_empty() {
        cpa_json::set(&mut inline, "mimeType", mime_type);
    }
    if !data.is_empty() {
        cpa_json::set(&mut inline, "data", data);
    }
    inline
}

fn is_base64_image(item: &Res<'_>) -> bool {
    item.g("type").str() == "image" && item.g("source.type").str() == "base64"
}

fn image_part(item: &Res<'_>) -> Value {
    let mut part = json!({});
    cpa_json::set(
        &mut part,
        "inlineData",
        inline_data_from_source(&item.g("source.media_type").str(), &item.g("source.data").str()),
    );
    part
}

/// The original text of a tool_result's `content`, so a stringified (`$ref`) result can keep its
/// original whitespace.
#[derive(Clone, Copy)]
struct RawSource<'a> {
    text: &'a str,
}

impl<'a> RawSource<'a> {
    /// Original text of every element of an array `content`, in one pass.
    fn items(&self) -> Vec<&'a str> {
        cpa_json::raw_children(self.text.as_bytes(), "")
    }
}

fn build_function_response(
    tool_call_id: &str,
    func_name: &str,
    result: &Res<'_>,
    function_name_map: &std::collections::HashMap<String, String>,
    raw_src: Option<RawSource<'_>>,
) -> Value {
    let mut fr = json!({});
    cpa_json::set(&mut fr, "id", tool_call_id);
    cpa_json::set(&mut fr, "name", util::map_sanitized_function_name(function_name_map, func_name));
    let result_path = "response.result";

    if result.is_string() {
        cpa_json::set(&mut fr, result_path, result.str());
    } else if result.is_array() {
        let items = result.array();
        let item_raws = raw_src.map(|r| r.items()).unwrap_or_default();
        let mut non_image: Vec<(Res<'_>, Option<&str>)> = Vec::with_capacity(items.len());
        let mut image_parts: Vec<Value> = Vec::new();
        for (k, item) in items.iter().enumerate() {
            if is_base64_image(item) {
                image_parts.push(image_part(item));
                continue;
            }
            non_image.push((item.clone(), item_raws.get(k).copied()));
        }
        match non_image.len() {
            0 => {
                cpa_json::set(&mut fr, result_path, "");
            }
            1 => function_response::set_function_response_result(&mut fr, result_path, &non_image[0].0, non_image[0].1),
            _ => {
                let joined: Vec<Value> = non_image.iter().map(|(item, _)| item.value()).collect();
                let joined_text = format!(
                    "[{}]",
                    non_image.iter().map(|(item, raw)| raw.map_or_else(|| item.raw(), str::to_string)).collect::<Vec<_>>().join(",")
                );
                function_response::set_function_response_result(&mut fr, result_path, &Res::owned(Value::Array(joined)), Some(&joined_text));
            }
        }
        // Image data goes inside functionResponse.parts rather than as sibling parts, to keep the
        // base64 payload out of the text context.
        if !image_parts.is_empty() {
            cpa_json::set(&mut fr, "parts", Value::Array(image_parts));
        }
    } else if result.is_object() {
        if is_base64_image(result) {
            cpa_json::set(&mut fr, "parts", Value::Array(vec![image_part(result)]));
            cpa_json::set(&mut fr, result_path, "");
        } else {
            function_response::set_function_response_result(&mut fr, result_path, result, raw_src.map(|r| r.text));
        }
    } else if result.exists() {
        function_response::set_function_response_result(&mut fr, result_path, result, raw_src.map(|r| r.text));
    } else {
        cpa_json::set(&mut fr, result_path, "");
    }
    fr
}

/// Function call args as valid JSON: objects verbatim, JSON-object strings parsed, other strings
/// kept as JSON strings, null -> `{}`, anything else verbatim. `None` when `input` is absent.
fn function_call_args(args: &Res<'_>) -> Option<Value> {
    if args.is_object() {
        return Some(args.value());
    }
    if !args.exists() {
        return None;
    }
    Some(match args.v() {
        Some(Value::String(s)) => {
            if s.trim_start().starts_with('{') {
                match cpa_json::parse_str(s) {
                    v @ Value::Object(_) => v,
                    _ => args.value(),
                }
            } else {
                args.value()
            }
        }
        Some(Value::Null) => json!({}),
        _ => args.value(),
    })
}

/// Reorders model parts: thought parts first, regular content second, function calls and trailing
/// signature carriers last. Returns `None` when the order is already fine.
fn reorder_model_parts(items: &[Value]) -> Option<Vec<Value>> {
    let mut thinking_parts = vec![];
    let mut regular_parts = vec![];
    let mut trailing_parts = vec![];
    let mut needs_reorder = false;
    let mut previous_category: i32 = -1;
    let mut seen_function_call = false;
    for part in items {
        let mut category = 1;
        let text = part.g("text");
        let is_signature_carrier = text.exists() && text.str().is_empty() && !part.g("thoughtSignature").str().trim().is_empty();
        let is_function_tail_carrier = is_signature_carrier && seen_function_call;
        if part.g("thought").bool() {
            category = 0;
            thinking_parts.push(part.clone());
        } else if part.g("functionCall").exists() || is_function_tail_carrier {
            category = 2;
            trailing_parts.push(part.clone());
            seen_function_call = seen_function_call || part.g("functionCall").exists();
        } else {
            regular_parts.push(part.clone());
        }
        needs_reorder = needs_reorder || category < previous_category;
        previous_category = category;
    }
    if !needs_reorder {
        return None;
    }
    let mut out = thinking_parts;
    out.extend(regular_parts);
    out.extend(trailing_parts);
    Some(out)
}

/// Handles one assistant `thinking` block. Returns false when the block disabled thought
/// translation (unsigned thinking on a non-Gemini target).
fn convert_thinking_block(
    model: &str,
    parts: &mut TurnParts,
    blocks: &[&Value],
    j: usize,
    enable_thought_translate: &mut bool,
) {
    let content = blocks[j];
    let thinking_text = get_thinking_text(content);
    let raw_signature = content.g("signature").str();
    let mut signature = resolve_thinking_signature(model, &thinking_text, &raw_signature);
    if !signature.is_empty() && !parts.pending_signature.is_empty() {
        if parts.pending_signature != signature {
            let pending = parts.pending_signature.clone();
            parts.append_detached_carrier(&pending);
        }
        parts.clear_pending();
    }
    let mut signature_from_pending_carrier = false;
    if signature.is_empty() && !thinking_text.is_empty() && !parts.pending_signature.is_empty() {
        if parts.pending_matches(CARRIER_TEXT) {
            signature = parts.pending_signature.clone();
            signature_from_pending_carrier = true;
        } else {
            let pending = parts.pending_signature.clone();
            parts.append_detached_carrier(&pending);
        }
        parts.clear_pending();
    }

    let is_gemini_signature = signature::signature_provider_from_model_name(model) == SignatureProvider::Gemini;

    // Unsigned thinking is dropped (never turned into text) for non-Gemini targets: Claude requires
    // assistant turns to start with thinking blocks when thinking is enabled.
    let is_unsigned = !has_resolved_thinking_signature(model, &signature);
    if is_unsigned && !is_gemini_signature {
        *enable_thought_translate = false;
        return;
    }

    let mut next_accepts_detached_signature = false;
    let mut next_target_kind = CARRIER_ANY;
    if j + 1 < blocks.len() {
        match blocks[j + 1].g("type").str().as_str() {
            "text" => {
                next_accepts_detached_signature = true;
                next_target_kind = CARRIER_TEXT;
            }
            "tool_use" => {
                next_accepts_detached_signature = true;
                next_target_kind = CARRIER_FUNCTION;
            }
            _ => {}
        }
    }
    let c = decode_gemini_claude_carrier_signature(&raw_signature);

    // Gemini places the signature on the visible text/function part that follows hidden thought
    // text. Keep the thought text, but defer its opaque signature to that native neighbouring part.
    if !thinking_text.is_empty() {
        let mut part = json!({});
        cpa_json::set(&mut part, "thought", true);
        cpa_json::set(&mut part, "text", thinking_text);
        if signature_from_pending_carrier {
            cpa_json::set(&mut part, "thoughtSignature", signature);
        } else if c.marked {
            let carrier_targets_next = c.target_kind == CARRIER_ANY || c.target_kind == next_target_kind;
            if c.ok && c.direction == CARRIER_STANDALONE && (c.target_kind == CARRIER_TEXT || c.target_kind == CARRIER_ANY) {
                cpa_json::set(&mut part, "thoughtSignature", signature);
            } else if c.ok && c.direction == CARRIER_NEXT && next_accepts_detached_signature && carrier_targets_next {
                parts.set_pending(&signature, &c.target_kind);
            }
        } else if is_gemini_signature && next_accepts_detached_signature {
            parts.set_pending(&signature, next_target_kind);
        } else if !signature.is_empty() {
            cpa_json::set(&mut part, "thoughtSignature", signature);
        }
        parts.items.push(part);
        return;
    }

    if !is_gemini_signature {
        return;
    }
    if c.marked && !c.ok {
        return;
    }
    if c.marked && c.direction == CARRIER_NEXT {
        if carrier_matches_adjacent(blocks, j, &c.direction, &c.target_kind) {
            parts.set_pending(&signature, &c.target_kind);
        }
        return;
    }
    if c.marked && c.direction == CARRIER_STANDALONE {
        parts.append_detached_carrier(&signature);
        return;
    }

    // Tagged trailing carriers bind backward even when another semantic block follows. Untagged
    // legacy carriers retain adjacency behavior.
    let bind_backward = c.marked && c.direction == CARRIER_PREVIOUS;
    if bind_backward && !carrier_matches_adjacent(blocks, j, &c.direction, &c.target_kind) {
        return;
    }
    if !bind_backward && next_accepts_detached_signature {
        parts.set_pending(&signature, next_target_kind);
        return;
    }
    let mut attached = false;
    let mut found_semantic_part = false;
    for part_index in (0..parts.items.len()).rev() {
        let part = &parts.items[part_index];
        let part_target_kind = if part.g("functionCall").exists() {
            CARRIER_FUNCTION
        } else if part.g("text").exists() && !part.g("text").str().is_empty() {
            CARRIER_TEXT
        } else {
            continue;
        };
        found_semantic_part = true;
        if c.marked && c.target_kind != CARRIER_ANY && c.target_kind != part_target_kind {
            break;
        }
        let part_signature = part.g("thoughtSignature").str().trim().to_string();
        let replace_fallback = bind_backward && part_target_kind == CARRIER_FUNCTION && part_signature == GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR;
        if part_signature.is_empty() || replace_fallback {
            cpa_json::set(&mut parts.items[part_index], "thoughtSignature", signature.clone());
            attached = true;
        }
        break;
    }
    if !attached && (found_semantic_part || bind_backward) {
        parts.append_detached_carrier(&signature);
    } else if !attached {
        parts.set_pending(&signature, &c.target_kind);
    }
}

pub fn convert_claude_request_to_antigravity(model: &str, input_raw_json: &[u8], _stream: bool) -> Vec<u8> {
    let mut enable_thought_translate = true;
    let raw = cpa_json::parse(input_raw_json);
    if should_build_web_search_request(model, &raw) {
        return build_web_search_request(model, &raw);
    }
    let function_name_map = util::sanitized_function_name_map(input_raw_json);

    // system instruction
    let mut system_parts: Vec<Value> = Vec::with_capacity(2);
    let system = raw.g("system");
    if system.is_array() {
        for system_prompt in system.array() {
            let t = system_prompt.g("type");
            if t.is_string() && t.str() == "text" {
                let text = system_prompt.g("text").str();
                if util::is_claude_code_attribution_system_text(&text) {
                    continue;
                }
                let mut part = json!({});
                if !text.is_empty() {
                    cpa_json::set(&mut part, "text", text);
                }
                system_parts.push(part);
            }
        }
    } else if system.is_string() && !util::is_claude_code_attribution_system_text(&system.str()) {
        system_parts.push(json!({"text": system.str()}));
    }

    // contents
    let mut content_items: Vec<Vec<u8>> = Vec::new();

    // tool_use id -> tool name, filled while walking the messages: a tool_result references its
    // tool_use by id, while Gemini requires functionResponse.name.
    let mut tool_name_by_id: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut pending_tool_use_ids: Vec<String> = Vec::new();

    let messages = raw.g("messages");
    if messages.is_array() {
        // Source text of each message, resolved on first use (only tool_results need it).
        let message_raws: std::cell::OnceCell<Vec<&str>> = std::cell::OnceCell::new();
        for (message_index, message) in messages.array().iter().enumerate() {
            let role_result = message.g("role");
            if !role_result.is_string() {
                continue;
            }
            let original_role = role_result.str();
            let mut preceding_tool_use_ids: Vec<String> = Vec::new();
            if original_role != "system" && original_role != "developer" {
                preceding_tool_use_ids = std::mem::take(&mut pending_tool_use_ids);
            }
            let role = match original_role.as_str() {
                "assistant" => "model",
                "system" | "developer" => "user",
                other => other,
            };
            let mut parts = TurnParts::default();
            let contents_result = message.g("content");
            if original_role == "system" || original_role == "developer" {
                if let Some(reminder) = common::claude_message_system_reminder_text(&contents_result) {
                    parts.items.push(json!({"text": reminder}));
                    content_items.push(cpa_json::to_vec(&claude_content(role, parts.items)));
                }
                continue;
            }
            if contents_result.is_array() {
                // Alignment may reorder tool_result blocks; keep the original order to locate raw text.
                let original_content = message.g("content");
                let original_blocks = original_content.array();
                let block_raws: std::cell::OnceCell<Vec<&str>> = std::cell::OnceCell::new();
                let contents_result = if original_role == "user" {
                    common::align_claude_tool_results(contents_result, &preceding_tool_use_ids)
                } else {
                    contents_result
                };
                let block_results = contents_result.array();
                let blocks: Vec<&Value> = block_results.iter().filter_map(|r| r.v()).collect();
                for j in 0..blocks.len() {
                    let content = blocks[j];
                    let content_type = content.g("type");
                    let ctype = if content_type.is_string() { content_type.str() } else { String::new() };
                    match ctype.as_str() {
                        "thinking" => {
                            if original_role != "assistant" {
                                continue;
                            }
                            convert_thinking_block(model, &mut parts, &blocks, j, &mut enable_thought_translate);
                        }
                        "text" => {
                            let prompt = content.g("text").str();
                            // Empty text parts are skipped: Gemini rejects "required oneof field
                            // 'data' must have one initialized field".
                            if prompt.is_empty() {
                                continue;
                            }
                            let mut part = json!({});
                            cpa_json::set(&mut part, "text", prompt);
                            if !parts.pending_signature.is_empty() {
                                if parts.pending_matches(CARRIER_TEXT) {
                                    cpa_json::set(&mut part, "thoughtSignature", parts.pending_signature.clone());
                                } else {
                                    let pending = parts.pending_signature.clone();
                                    parts.append_detached_carrier(&pending);
                                }
                                parts.clear_pending();
                            }
                            parts.items.push(part);
                        }
                        "tool_use" => {
                            // Dummy thinking blocks are never injected: Antigravity validates signatures.
                            let original_function_name = content.g("name").str();
                            let function_name = util::map_sanitized_function_name(&function_name_map, &original_function_name);
                            let function_id = content.g("id").str();
                            if !function_id.is_empty() && !original_function_name.is_empty() {
                                tool_name_by_id.insert(function_id.clone(), original_function_name.clone());
                            }
                            let Some(args) = function_call_args(&content.g("input")) else { continue };
                            let mut part = json!({});
                            let mut signature = resolve_tool_use_thought_signature(model, content, true);
                            if !parts.pending_signature.is_empty() {
                                if parts.pending_matches(CARRIER_FUNCTION)
                                    && (signature.is_empty() || signature == GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR)
                                {
                                    signature = parts.pending_signature.clone();
                                } else {
                                    let pending = parts.pending_signature.clone();
                                    parts.append_detached_carrier(&pending);
                                }
                                parts.clear_pending();
                            }
                            if !signature.is_empty() {
                                cpa_json::set(&mut part, "thoughtSignature", signature);
                            }
                            if !function_id.is_empty() {
                                cpa_json::set(&mut part, "functionCall.id", function_id.clone());
                            }
                            cpa_json::set(&mut part, "functionCall.name", function_name);
                            cpa_json::set(&mut part, "functionCall.args", args);
                            parts.items.push(part);
                            if original_role == "assistant" {
                                pending_tool_use_ids.push(function_id);
                            }
                        }
                        "tool_result" => {
                            let tool_call_id = content.g("tool_use_id").str();
                            if tool_call_id.is_empty() {
                                continue;
                            }
                            let func_name = match tool_name_by_id.get(&tool_call_id) {
                                Some(n) => n.clone(),
                                None => {
                                    // Derive a semantic name by stripping the last two dash separated
                                    // segments ("get_weather-call-123" -> "get_weather"); the raw id is
                                    // the last resort.
                                    let segments: Vec<&str> = tool_call_id.split('-').collect();
                                    let mut name = if segments.len() > 2 { segments[..segments.len() - 2].join("-") } else { String::new() };
                                    if name.is_empty() {
                                        name = tool_call_id.clone();
                                    }
                                    tracing::warn!(
                                        "antigravity claude request: tool_result references unknown tool_use_id={tool_call_id}, derived function name={name}"
                                    );
                                    name
                                }
                            };
                            let original_index = original_blocks.iter().position(|b| b.v() == Some(content));
                            let raw_src = original_index
                                .and_then(|j0| {
                                    let blocks = block_raws.get_or_init(|| {
                                        let messages = message_raws.get_or_init(|| cpa_json::raw_children(input_raw_json, "messages"));
                                        messages.get(message_index).map(|m| cpa_json::raw_children(m.as_bytes(), "content")).unwrap_or_default()
                                    });
                                    crate::common::raw_in(blocks.get(j0), "content")
                                })
                                .map(|text| RawSource { text });
                            let fr = build_function_response(&tool_call_id, &func_name, &content.g("content"), &function_name_map, raw_src);
                            parts.items.push(json!({"functionResponse": fr}));
                        }
                        "image" => {
                            let source = content.g("source");
                            if source.g("type").str() == "base64" {
                                let inline = inline_data_from_source(&source.g("media_type").str(), &source.g("data").str());
                                parts.items.push(json!({"inlineData": inline}));
                            }
                        }
                        _ => {}
                    }
                }
                if !parts.pending_signature.is_empty() {
                    let pending = parts.pending_signature.clone();
                    parts.append_detached_carrier(&pending);
                    parts.clear_pending();
                }

                if parts.items.is_empty() {
                    continue;
                }
                let mut client_content = claude_content(role, parts.items.clone());
                if role == "model" && parts.items.len() > 1
                    && let Some(new_parts) = reorder_model_parts(&parts.items) {
                        cpa_json::set(&mut client_content, "parts", Value::Array(new_parts));
                    }
                content_items.push(cpa_json::to_vec(&client_content));
            } else if contents_result.is_string() {
                let mut part = json!({});
                let prompt = contents_result.str();
                if !prompt.is_empty() {
                    cpa_json::set(&mut part, "text", prompt);
                }
                content_items.push(cpa_json::to_vec(&claude_content(role, vec![part])));
            }
        }
    }

    // tools
    let mut tools_json: Option<Value> = None;
    let mut tool_decl_count = 0usize;
    const ALLOWED_TOOL_KEYS: [&str; 7] = ["name", "description", "behavior", "parameters", "parametersJsonSchema", "response", "responseJsonSchema"];
    let tools = raw.g("tools");
    if tools.is_array() {
        let mut function_declarations: Vec<Vec<u8>> = Vec::new();
        for tool_result in tools.array() {
            if is_claude_typed_web_search_tool_type(&tool_result.g("type").str()) {
                continue;
            }
            let input_schema = tool_result.g("input_schema");
            if input_schema.exists() && input_schema.is_object() {
                // Sanitize the input schema for Antigravity API compatibility.
                let cleaned = util::clean_json_schema_for_antigravity(&input_schema.raw());
                let mut tool = tool_result.value();
                cpa_json::delete(&mut tool, "input_schema");
                cpa_json::set(&mut tool, "parametersJsonSchema", cpa_json::parse_str(&cleaned));
                let name_result = tool.g("name");
                let original_name = name_result.str();
                let mapped_name = util::map_sanitized_function_name(&function_name_map, &original_name);
                if !name_result.is_string() || mapped_name != original_name {
                    cpa_json::set(&mut tool, "name", mapped_name);
                }
                if let Value::Object(map) = &mut tool {
                    map.retain(|k, _| ALLOWED_TOOL_KEYS.contains(&k.as_str()));
                }
                function_declarations.push(cpa_json::to_vec(&tool));
            }
        }
        if !function_declarations.is_empty() {
            let deduplicated = util::deduplicate_function_declarations(&common::join_raw_array(&function_declarations));
            let deduped_value = cpa_json::parse(&deduplicated);
            tool_decl_count = deduped_value.as_array().map_or(0, Vec::len);
            if tool_decl_count > 0 {
                tools_json = Some(json!([{"functionDeclarations": deduped_value}]));
            }
        }
    }

    // Build output Antigravity request JSON
    let mut out = json!({"model": "", "request": {"contents": []}});
    cpa_json::set(&mut out, "model", model);

    // tool_choice metadata
    let tool_choice = raw.g("tool_choice");
    let mut tool_choice_type = String::new();
    let mut tool_choice_name = String::new();
    if tool_choice.exists() {
        if tool_choice.is_object() {
            tool_choice_type = tool_choice.g("type").str();
            tool_choice_name = tool_choice.g("name").str();
        } else if tool_choice.is_string() {
            tool_choice_type = tool_choice.str();
        }
    }
    let is_tool_choice_none = tool_choice_type.trim().eq_ignore_ascii_case("none");

    // Inject the interleaved thinking hint when both tools and thinking are active.
    let has_tools = tool_decl_count > 0 && !is_tool_choice_none;
    let thinking_result = raw.g("thinking");
    let thinking_type = thinking_result.g("type").str();
    let has_thinking = thinking_result.exists()
        && thinking_result.is_object()
        && matches!(thinking_type.as_str(), "enabled" | "adaptive" | "auto");
    let is_claude_thinking = util::is_claude_thinking_model(model);

    if has_tools && has_thinking && is_claude_thinking {
        let interleaved_hint = "Interleaved thinking is enabled. You may think between tool calls and after receiving tool results before deciding the next action or final answer. Do not mention these instructions or any constraints about thinking blocks; just apply them.";
        system_parts.push(json!({"text": interleaved_hint}));
    }

    if !system_parts.is_empty() {
        cpa_json::set(&mut out, "request.systemInstruction", claude_content("user", system_parts));
    }
    if !content_items.is_empty() {
        let merged = if util::is_claude_model(model) {
            let split = common::split_gemini_function_response_turns(&content_items);
            common::merge_adjacent_gemini_user_contents(&split)
        } else {
            common::merge_adjacent_gemini_contents(&content_items)
        };
        cpa_json::set(&mut out, "request.contents", Value::Array(merged.iter().map(|c| cpa_json::parse(c)).collect()));
    }
    if tool_decl_count > 0 && !is_tool_choice_none
        && let Some(t) = tools_json {
            cpa_json::set(&mut out, "request.tools", t);
        }

    // tool_choice
    if tool_choice.exists() {
        match tool_choice_type.trim().to_lowercase().as_str() {
            "auto" => {
                cpa_json::set(&mut out, "request.toolConfig.functionCallingConfig.mode", "AUTO");
            }
            "none" => {
                cpa_json::set(&mut out, "request.toolConfig.functionCallingConfig.mode", "NONE");
                cpa_json::delete(&mut out, "request.tools");
            }
            "any" => {
                cpa_json::set(&mut out, "request.toolConfig.functionCallingConfig.mode", "ANY");
            }
            "tool" => {
                cpa_json::set(&mut out, "request.toolConfig.functionCallingConfig.mode", "ANY");
                if !tool_choice_name.is_empty() {
                    cpa_json::set(
                        &mut out,
                        "request.toolConfig.functionCallingConfig.allowedFunctionNames",
                        json!([util::map_sanitized_function_name(&function_name_map, &tool_choice_name)]),
                    );
                }
            }
            _ => {}
        }
    }

    // Map Anthropic thinking -> Gemini thinkingBudget/thinkingLevel.
    let t = raw.g("thinking");
    if enable_thought_translate && t.exists() && t.is_object() {
        match t.g("type").str().as_str() {
            "enabled" => {
                let b = t.g("budget_tokens");
                if b.exists() && b.is_number() {
                    cpa_json::set(&mut out, "request.generationConfig.thinkingConfig.thinkingBudget", b.int());
                }
            }
            "adaptive" | "auto" => {
                // An explicit output_config.effort passes through as thinkingLevel; otherwise treat it
                // as "enabled with target-model maximum" and emit high. ApplyThinking clamps later.
                let mut effort = String::new();
                let v = raw.g("output_config.effort");
                if v.exists() && v.is_string() {
                    effort = v.str().trim().to_lowercase();
                }
                let level = if effort.is_empty() { "high".to_string() } else { effort };
                cpa_json::set(&mut out, "request.generationConfig.thinkingConfig.thinkingLevel", level);
            }
            _ => {}
        }
    }
    for (src, dst) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
        ("max_tokens", "maxOutputTokens"),
    ] {
        let v = raw.g(src);
        if v.exists() && v.is_number() {
            cpa_json::set(&mut out, &format!("request.generationConfig.{dst}"), cpa_json::num_f64(v.float()));
        }
    }

    let mut out_bytes = attach_default_safety_settings(&cpa_json::to_vec(&out), "request.safetySettings");
    if signature::signature_provider_from_model_name(model) == SignatureProvider::Gemini {
        out_bytes = signature::sanitize_gemini_request_thought_signatures(&out_bytes, "request.contents");
    }
    out_bytes
}

//! Codex Responses -> OpenAI Chat Completions response (Go: codex_openai_response.go).

use std::collections::HashMap;

use cpa_core::applypatch;
use cpa_core::util::{collect_responses_tool_winners, qualify_responses_namespace_tool_name};
use cpa_json::{json, Res, Value, J};
use sha2::{Digest, Sha256};

use super::request::{build_short_name_map_for, collect_request_tool_names};
use crate::codex::util::{mime_type_from_output_format, reverse_map, unix_now};
use crate::registry::{Ctx, Param};

/// Streaming state of one tool call.
#[derive(Default)]
struct ToolCallState {
    index: i64,
    arguments_emitted: bool,
    patch: bool,
    input_started: bool,
    input_closed: bool,
    done: bool,
}

/// Per-stream state (Go: ConvertCliToOpenAIParams). Tool call states live in an arena so the
/// lookup tables and `current` can share them by index.
struct State {
    service_tier: String,
    response_id: String,
    created_at: i64,
    model: String,
    function_call_index: i64,
    states: Vec<ToolCallState>,
    tool_call_states: HashMap<String, usize>,
    current_tool_call: Option<usize>,
    last_image_hash_by_item_id: HashMap<String, [u8; 32]>,
}

/// Go: ConvertCodexResponseToOpenAI. One `data:` line in, at most one chunk out.
pub fn convert_codex_response_to_openai(
    _ctx: &Ctx,
    model_name: &str,
    original_request: &[u8],
    _request: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let p = param.state(|| State {
        service_tier: String::new(),
        response_id: String::new(),
        created_at: 0,
        model: model_name.to_string(),
        function_call_index: -1,
        states: Vec::new(),
        tool_call_states: HashMap::new(),
        current_tool_call: None,
        last_image_hash_by_item_id: HashMap::new(),
    });

    if !raw.starts_with(b"data:") {
        return vec![];
    }
    let raw = raw[5..].trim_ascii();

    let mut template = cpa_json::parse_str(
        r#"{"id":"","object":"chat.completion.chunk","created":12345,"model":"model","choices":[{"index":0,"delta":{},"finish_reason":null,"native_finish_reason":null}]}"#,
    );
    let root = cpa_json::parse(raw);

    let tier = response_service_tier(&root.g("response"));
    if !tier.is_empty() {
        p.service_tier = tier;
    } else {
        let tier = response_service_tier(&Res::of(&root));
        if !tier.is_empty() {
            p.service_tier = tier;
        }
    }
    if !p.service_tier.is_empty() {
        cpa_json::set(&mut template, "service_tier", p.service_tier.clone());
    }

    let data_type = root.g("type").str();
    if data_type == "response.created" {
        p.response_id = root.g("response.id").str();
        p.created_at = root.g("response.created_at").int();
        p.model = root.g("response.model").str();
        return vec![];
    }

    // Model version: the event's own, else the cached one, else the requested one.
    let model_result = root.g("model");
    if model_result.exists() {
        cpa_json::set(&mut template, "model", model_result.str());
    } else if !p.model.is_empty() {
        cpa_json::set(&mut template, "model", p.model.clone());
    } else if !model_name.is_empty() {
        cpa_json::set(&mut template, "model", model_name);
    }

    cpa_json::set(&mut template, "created", p.created_at);
    cpa_json::set(&mut template, "id", p.response_id.clone());

    let usage = root.g("response.usage");
    if usage.exists() {
        set_usage(&mut template, &usage);
    }

    if data_type == "response.reasoning_summary_text.delta" || data_type == "response.reasoning_text.delta" {
        let delta = root.g("delta");
        if delta.exists() {
            cpa_json::set(&mut template, "choices.0.delta.role", "assistant");
            cpa_json::set(&mut template, "choices.0.delta.reasoning_content", delta.str());
        }
    } else if data_type == "response.reasoning_summary_text.done" || data_type == "response.reasoning_text.done" {
        cpa_json::set(&mut template, "choices.0.delta.role", "assistant");
        cpa_json::set(&mut template, "choices.0.delta.reasoning_content", "\n\n");
    } else if data_type == "response.output_text.delta" {
        let delta = root.g("delta");
        if delta.exists() {
            cpa_json::set(&mut template, "choices.0.delta.role", "assistant");
            cpa_json::set(&mut template, "choices.0.delta.content", delta.str());
        }
    } else if data_type == "response.image_generation_call.partial_image" {
        let item_id = root.g("item_id").str();
        let b64 = root.g("partial_image_b64").str();
        if b64.is_empty() {
            return vec![];
        }
        if is_duplicate_image(&mut p.last_image_hash_by_item_id, &item_id, &b64) {
            return vec![];
        }
        let mime_type = mime_type_from_output_format(&root.g("output_format").str());
        append_image(&mut template, &mime_type, &b64);
    } else if data_type == "response.completed" || data_type == "response.incomplete" {
        let mut finish_reason = "stop".to_string();
        let mut native_finish_reason = finish_reason.clone();
        if data_type == "response.incomplete" {
            native_finish_reason = root.g("response.incomplete_details.reason").str();
            match native_finish_reason.as_str() {
                "max_tokens" | "max_output_tokens" => finish_reason = "length".into(),
                "content_filter" => finish_reason = "content_filter".into(),
                _ => {}
            }
        } else if p.function_call_index != -1 {
            finish_reason = "tool_calls".into();
            native_finish_reason = finish_reason.clone();
        }
        cpa_json::set(&mut template, "choices.0.finish_reason", finish_reason);
        cpa_json::set(&mut template, "choices.0.native_finish_reason", native_finish_reason);
    } else if data_type == "response.output_item.added" {
        let item = root.g("item");
        if !item.exists() || !is_tool_call_type(&item.g("type").str()) {
            return vec![];
        }

        // A new tool call item gets the next index.
        p.function_call_index += 1;
        let state = ToolCallState {
            index: p.function_call_index,
            patch: is_original_custom_patch(original_request, &item),
            ..Default::default()
        };
        let index = state.index;
        register_tool_call_state(p, &root, &item, state);

        let name = restore_tool_name(original_request, &item.g("name").str());
        let call = json!({"index": index, "id": item.g("call_id").str(), "type": "function", "function": {"name": name, "arguments": ""}});
        cpa_json::set(&mut template, "choices.0.delta.role", "assistant");
        cpa_json::set(&mut template, "choices.0.delta.tool_calls", json!([call]));
    } else if data_type == "response.function_call_arguments.delta" || data_type == "response.custom_tool_call_input.delta" {
        let Some(si) = find_tool_call_state(p, &root, &Res::NONE) else { return vec![] };
        let mut delta_value = root.g("delta").str();
        let state = &mut p.states[si];
        if state.done || delta_value.is_empty() {
            return vec![];
        }
        state.arguments_emitted = true;
        if state.patch {
            delta_value = applypatch::escape_input_fragment(&delta_value);
            if !state.input_started {
                delta_value = format!("{{\"input\":\"{delta_value}");
                state.input_started = true;
            }
        }
        set_arguments_chunk(&mut template, state.index, &delta_value);
    } else if data_type == "response.function_call_arguments.done" || data_type == "response.custom_tool_call_input.done" {
        let Some(si) = find_tool_call_state(p, &root, &Res::NONE) else { return vec![] };
        let state = &mut p.states[si];
        if state.done || state.input_closed || (state.arguments_emitted && !state.patch) {
            return vec![];
        }

        // Fallback: no delta events were received, emit the full arguments as a single chunk.
        let full_args_field = if data_type == "response.custom_tool_call_input.done" { "input" } else { "arguments" };
        state.arguments_emitted = true;
        let mut full_args = root.g(full_args_field).str();
        if state.patch {
            full_args = finish_patch_chat_arguments(state, &full_args);
        }
        if full_args.is_empty() {
            return vec![];
        }
        set_arguments_chunk(&mut template, state.index, &full_args);
    } else if data_type == "response.output_item.done" {
        let item = root.g("item");
        if !item.exists() {
            return vec![];
        }
        let item_type = item.g("type").str();
        if item_type == "image_generation_call" {
            let item_id = item.g("id").str();
            let b64 = item.g("result").str();
            if b64.is_empty() {
                return vec![];
            }
            if is_duplicate_image(&mut p.last_image_hash_by_item_id, &item_id, &b64) {
                return vec![];
            }
            let mime_type = mime_type_from_output_format(&item.g("output_format").str());
            append_image(&mut template, &mime_type, &b64);
            return vec![cpa_json::to_vec(&template)];
        }
        if !is_tool_call_type(&item_type) {
            return vec![];
        }

        if let Some(si) = find_tool_call_state(p, &root, &item) {
            let state = &mut p.states[si];
            if state.done {
                return vec![];
            }
            state.done = true;
            if state.arguments_emitted && (!state.patch || state.input_closed) {
                return vec![];
            }

            // The tool was announced but no argument event arrived. Emit only the completed
            // arguments so the id and name are not duplicated.
            state.arguments_emitted = true;
            let mut full_args = tool_call_arguments(&item);
            if state.patch {
                full_args = finish_patch_chat_arguments(state, &full_args);
            }
            if full_args.is_empty() {
                return vec![];
            }
            set_arguments_chunk(&mut template, state.index, &full_args);
            return vec![cpa_json::to_vec(&template)];
        }

        // Fallback: the model skipped output_item.added, so emit the complete tool call now.
        p.function_call_index += 1;
        let mut state = ToolCallState {
            index: p.function_call_index,
            arguments_emitted: true,
            done: true,
            patch: is_original_custom_patch(original_request, &item),
            ..Default::default()
        };
        let index = state.index;
        let name = restore_tool_name(original_request, &item.g("name").str());
        let mut full_args = tool_call_arguments(&item);
        if state.patch {
            full_args = finish_patch_chat_arguments(&mut state, &full_args);
        }
        register_tool_call_state(p, &root, &item, state);
        let call = json!({"index": index, "id": item.g("call_id").str(), "type": "function", "function": {"name": name, "arguments": full_args}});
        cpa_json::set(&mut template, "choices.0.delta.tool_calls", json!([]));
        cpa_json::set(&mut template, "choices.0.delta.role", "assistant");
        cpa_json::set(&mut template, "choices.0.delta.tool_calls.-1", call);
    } else {
        return vec![];
    }

    vec![cpa_json::to_vec(&template)]
}

/// Sets the response usage block from a Codex `usage` object.
fn set_usage(template: &mut Value, usage: &Res<'_>) {
    let output = usage.g("output_tokens");
    if output.exists() {
        cpa_json::set(template, "usage.completion_tokens", output.int());
    }
    let total = usage.g("total_tokens");
    if total.exists() {
        cpa_json::set(template, "usage.total_tokens", total.int());
    }
    let input = usage.g("input_tokens");
    if input.exists() {
        cpa_json::set(template, "usage.prompt_tokens", input.int());
    }
    let cached = usage.g("input_tokens_details.cached_tokens");
    if cached.exists() {
        cpa_json::set(template, "usage.prompt_tokens_details.cached_tokens", cached.int());
    }
    set_cache_write_tokens(template, usage);
    let reasoning = usage.g("output_tokens_details.reasoning_tokens");
    if reasoning.exists() {
        cpa_json::set(template, "usage.completion_tokens_details.reasoning_tokens", reasoning.int());
    }
}

/// Records the image hash for `item_id`; true when the same image was already emitted.
fn is_duplicate_image(hashes: &mut HashMap<String, [u8; 32]>, item_id: &str, b64: &str) -> bool {
    if item_id.is_empty() {
        return false;
    }
    let hash: [u8; 32] = Sha256::digest(b64.as_bytes()).into();
    if hashes.get(item_id) == Some(&hash) {
        return true;
    }
    hashes.insert(item_id.to_string(), hash);
    false
}

/// Appends an image delta (`delta.images`) to a fresh chunk template.
fn append_image(template: &mut Value, mime_type: &str, b64: &str) {
    let image_url = format!("data:{mime_type};base64,{b64}");
    cpa_json::set(template, "choices.0.delta.images", json!([]));
    let payload = json!({"type": "image_url", "image_url": {"url": image_url}, "index": 0});
    cpa_json::set(template, "choices.0.delta.role", "assistant");
    cpa_json::set(template, "choices.0.delta.images.-1", payload);
}

fn set_arguments_chunk(template: &mut Value, index: i64, arguments: &str) {
    let call = json!({"index": index, "function": {"arguments": arguments}});
    cpa_json::set(template, "choices.0.delta.tool_calls", json!([]));
    cpa_json::set(template, "choices.0.delta.tool_calls.-1", call);
}

/// Go: ConvertCodexResponseToOpenAINonStream.
pub fn convert_codex_response_to_openai_non_stream(
    _ctx: &Ctx,
    _model_name: &str,
    original_request: &[u8],
    _request: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw);
    let response_type = root.g("type").str();
    if response_type != "response.completed" && response_type != "response.incomplete" {
        return Some(Vec::new());
    }

    let unix_timestamp = unix_now();
    let response = root.g("response");

    let mut template = cpa_json::parse_str(
        r#"{"id":"","object":"chat.completion","created":123456,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}"#,
    );

    let tier = response_service_tier(&response);
    if !tier.is_empty() {
        cpa_json::set(&mut template, "service_tier", tier);
    } else {
        let tier = response_service_tier(&Res::of(&root));
        if !tier.is_empty() {
            cpa_json::set(&mut template, "service_tier", tier);
        }
    }

    let model = response.g("model");
    if model.exists() {
        cpa_json::set(&mut template, "model", model.str());
    }
    let created_at = response.g("created_at");
    if created_at.exists() {
        cpa_json::set(&mut template, "created", created_at.int());
    } else {
        cpa_json::set(&mut template, "created", unix_timestamp);
    }
    let id = response.g("id");
    if id.exists() {
        cpa_json::set(&mut template, "id", id.str());
    }

    let usage = response.g("usage");
    if usage.exists() {
        set_usage(&mut template, &usage);
    }

    // Output array: content, reasoning, tool calls, images.
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut images: Vec<Value> = Vec::new();
    let output = response.g("output");
    if output.is_array() {
        let mut content_text = String::new();
        let mut reasoning_text = String::new();

        for item in output.array() {
            match item.g("type").str().as_str() {
                "reasoning" => {
                    let summary = item.g("summary");
                    if summary.is_array() {
                        for summary_item in summary.array() {
                            if summary_item.g("type").str() == "summary_text" {
                                reasoning_text.push_str(&summary_item.g("text").str());
                                break;
                            }
                        }
                    }
                    let content = item.g("content");
                    if content.is_array() {
                        for content_item in content.array() {
                            if content_item.g("type").str() == "reasoning_text" {
                                reasoning_text.push_str(&content_item.g("text").str());
                            }
                        }
                    }
                }
                "message" => {
                    let content = item.g("content");
                    if content.is_array() {
                        for content_item in content.array() {
                            if content_item.g("type").str() == "output_text" {
                                content_text.push_str(&content_item.g("text").str());
                                break;
                            }
                        }
                    }
                }
                "function_call" | "custom_tool_call" => {
                    let mut call = json!({"id": "", "type": "function", "function": {"name": "", "arguments": ""}});
                    let call_id = item.g("call_id");
                    if call_id.exists() {
                        cpa_json::set(&mut call, "id", call_id.str());
                    }
                    let name = item.g("name");
                    if name.exists() {
                        cpa_json::set(&mut call, "function.name", restore_tool_name(original_request, &name.str()));
                    }
                    let mut full_args = tool_call_arguments(&item);
                    if is_original_custom_patch(original_request, &item) {
                        full_args = applypatch::wrap_input(&full_args);
                    }
                    cpa_json::set(&mut call, "function.arguments", full_args);
                    tool_calls.push(call);
                }
                "image_generation_call" => {
                    let b64 = item.g("result").str();
                    if b64.is_empty() {
                        continue;
                    }
                    let mime_type = mime_type_from_output_format(&item.g("output_format").str());
                    let image_url = format!("data:{mime_type};base64,{b64}");
                    images.push(json!({"type": "image_url", "image_url": {"url": image_url}, "index": images.len()}));
                }
                _ => {}
            }
        }

        if !content_text.is_empty() {
            cpa_json::set(&mut template, "choices.0.message.content", content_text);
        }
        if !reasoning_text.is_empty() {
            cpa_json::set(&mut template, "choices.0.message.reasoning_content", reasoning_text);
        }
        if !tool_calls.is_empty() {
            cpa_json::set(&mut template, "choices.0.message.tool_calls", Value::Array(tool_calls.clone()));
        }
        if !images.is_empty() {
            cpa_json::set(&mut template, "choices.0.message.images", Value::Array(images));
        }
    }

    // Finish reason from status.
    let status_result = response.g("status");
    if status_result.exists() {
        let mut finish_reason = String::new();
        let mut native_finish_reason = String::new();
        match status_result.str().as_str() {
            "completed" => {
                finish_reason = "stop".into();
                native_finish_reason = finish_reason.clone();
                if !tool_calls.is_empty() {
                    finish_reason = "tool_calls".into();
                    native_finish_reason = finish_reason.clone();
                }
            }
            "incomplete" => {
                native_finish_reason = response.g("incomplete_details.reason").str();
                finish_reason = match native_finish_reason.as_str() {
                    "max_tokens" | "max_output_tokens" => "length",
                    "content_filter" => "content_filter",
                    _ => "stop",
                }
                .into();
            }
            _ => {}
        }
        if !finish_reason.is_empty() {
            cpa_json::set(&mut template, "choices.0.finish_reason", finish_reason);
            cpa_json::set(&mut template, "choices.0.native_finish_reason", native_finish_reason);
        }
    }

    Some(cpa_json::to_vec(&template))
}

fn register_tool_call_state(p: &mut State, event: &Value, item: &Res<'_>, state: ToolCallState) {
    let si = p.states.len();
    p.states.push(state);
    let item_id = event.g("item_id").str();
    if !item_id.is_empty() {
        p.tool_call_states.insert(format!("item:{item_id}"), si);
    }
    let item_id = item.g("id").str();
    if !item_id.is_empty() {
        p.tool_call_states.insert(format!("item:{item_id}"), si);
    }
    let output_index = event.g("output_index");
    if output_index.exists() {
        p.tool_call_states.insert(format!("output:{}", output_index.raw()), si);
    }
    p.current_tool_call = Some(si);
}

fn find_tool_call_state(p: &State, event: &Value, item: &Res<'_>) -> Option<usize> {
    let item_id = event.g("item_id").str();
    if !item_id.is_empty()
        && let Some(&si) = p.tool_call_states.get(&format!("item:{item_id}"))
    {
        return Some(si);
    }
    let item_id = item.g("id").str();
    if !item_id.is_empty()
        && let Some(&si) = p.tool_call_states.get(&format!("item:{item_id}"))
    {
        return Some(si);
    }
    let output_index = event.g("output_index");
    if output_index.exists()
        && let Some(&si) = p.tool_call_states.get(&format!("output:{}", output_index.raw()))
    {
        return Some(si);
    }
    p.current_tool_call
}

fn is_tool_call_type(item_type: &str) -> bool {
    item_type == "function_call" || item_type == "custom_tool_call"
}

fn tool_call_arguments(item: &Res<'_>) -> String {
    if item.g("type").str() == "custom_tool_call" {
        return item.g("input").str();
    }
    item.g("arguments").str()
}

/// Restores a shortened tool name using the original request's tool names.
fn restore_tool_name(original: &[u8], name: &str) -> String {
    let rev = build_reverse_map_from_original(original);
    rev.get(name).cloned().unwrap_or_else(|| name.to_string())
}

/// Shortened tool name -> original name, from the original request.
fn build_reverse_map_from_original(original: &[u8]) -> HashMap<String, String> {
    let names = collect_request_tool_names(&cpa_json::parse(original));
    if names.is_empty() {
        return HashMap::new();
    }
    reverse_map(build_short_name_map_for(&names))
}

/// Only an actual nonempty upstream tier.
fn response_service_tier(response: &Res<'_>) -> String {
    match response.g("service_tier").as_str() {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => String::new(),
    }
}

/// Preserves the upstream integer without float conversion; non-integers are ignored.
fn set_cache_write_tokens(template: &mut Value, usage: &Res<'_>) {
    let value = usage.g("input_tokens_details.cache_write_tokens");
    if !value.exists() || value.is_null() {
        return;
    }
    let raw = value.raw();
    let valid = value.is_number() && !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit());
    if !valid {
        tracing::warn!(field = "usage.input_tokens_details.cache_write_tokens", "Ignoring invalid Codex cache write token count");
        return;
    }
    cpa_json::set(template, "usage.prompt_tokens_details.cache_write_tokens", value.value());
    cpa_json::set(template, "usage.prompt_tokens_details.cached_creation_tokens", value.value());
}

/// A custom `apply_patch` call is bridged only when the original request declared it as a
/// custom tool; an ordinary same-name function is never promoted.
fn is_original_custom_patch(original: &[u8], item: &Res<'_>) -> bool {
    if item.g("type").str() != "custom_tool_call" {
        return false;
    }
    let name = qualify_responses_namespace_tool_name(&item.g("namespace").str(), &item.g("name").str());
    let root = cpa_json::parse(original);
    // Chat Completions prefers ordinary functions for ambiguous names, regardless of order.
    let tools = root.g("tools");
    for tool in tools.array() {
        if tool.g("type").str() == "function" && tool.g("function.name").str() == name {
            return false;
        }
    }
    collect_responses_tool_winners(&root)
        .get(&name)
        .is_some_and(|winner| applypatch::is_custom_tool(&winner.tool))
}

/// Closes the apply_patch input string: `"}` once started, otherwise the whole wrapped input.
fn finish_patch_chat_arguments(state: &mut ToolCallState, input: &str) -> String {
    if state.input_closed {
        return String::new();
    }
    state.input_closed = true;
    if state.input_started {
        return "\"}".into();
    }
    applypatch::wrap_input(input)
}

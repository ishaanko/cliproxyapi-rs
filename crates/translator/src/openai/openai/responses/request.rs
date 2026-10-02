//! OpenAI Responses request -> OpenAI Chat Completions request
//! (Go: openai_openai-responses_request.go).

use std::collections::{HashMap, HashSet};

use cpa_core::util::go_json_string;
use cpa_json::{Res, Value, J};

use super::tools::{responses_tool_output_text, ToolIndex};
use super::{go_any, RawSrc};
use crate::common;

/// Writes a tool output into a Chat Completions message; the source locates `output` in the request.
type SetContent = fn(&mut Value, &Res<'_>, &RawSrc<'_>);

const REASONING_UNAVAILABLE: &str = "[reasoning unavailable]";

fn tpl(s: &str) -> Value {
    cpa_json::parse_str(s)
}

/// Message assembly state while walking the Responses `input` items.
struct Conv {
    messages: Vec<Value>,
    pending_tool_calls: Vec<Value>,
    pending_tool_call_ids: Vec<String>,
    pending_reasoning_content: String,
    latest_reasoning_content: String,
    awaiting_tool_outputs: HashSet<String>,
    output_counts: HashMap<String, i32>,
    duplicate_output_ids: HashSet<String>,
    /// Index of a trailing assistant message that a following function_call may merge into.
    mergeable_assistant_index: Option<usize>,
    has_reasoning_in_session: bool,
}

impl Conv {
    fn fallback_tool_reasoning(&self) -> String {
        if !self.latest_reasoning_content.is_empty() {
            return self.latest_reasoning_content.clone();
        }
        if self.has_reasoning_in_session {
            return REASONING_UNAVAILABLE.to_string();
        }
        String::new()
    }

    fn take_pending_reasoning_content(&mut self) -> String {
        std::mem::take(&mut self.pending_reasoning_content)
    }

    /// Emits buffered function calls as one assistant message (or merges them into the preceding
    /// plain assistant message) and starts waiting for their outputs.
    fn flush_pending_tool_calls(&mut self) {
        if self.pending_tool_calls.is_empty() {
            return;
        }

        let reasoning_content = self.take_pending_reasoning_content();
        let mut merged_into_assistant = false;
        if let Some(idx) = self.mergeable_assistant_index.filter(|&i| i + 1 == self.messages.len()) {
            let assistant = &self.messages[idx];
            if assistant.g("role").str() == "assistant" && !assistant.g("tool_calls").exists() {
                let existing_reasoning = assistant.g("reasoning_content").str();
                let combined = combine_reasoning(&existing_reasoning, &reasoning_content);
                let fallback = if combined.is_empty() { self.fallback_tool_reasoning() } else { String::new() };
                let updated = &mut self.messages[idx];
                cpa_json::set(updated, "tool_calls", Value::Array(self.pending_tool_calls.clone()));
                if !combined.is_empty() {
                    cpa_json::set(updated, "reasoning_content", combined.clone());
                    if is_usable_reasoning(&combined) {
                        self.latest_reasoning_content = combined;
                    }
                } else if !fallback.is_empty() {
                    cpa_json::set(updated, "reasoning_content", fallback);
                }
                merged_into_assistant = true;
            }
        }
        if !merged_into_assistant {
            let mut assistant = tpl(r#"{"role":"assistant","tool_calls":[]}"#);
            cpa_json::set(&mut assistant, "tool_calls", Value::Array(self.pending_tool_calls.clone()));
            if !reasoning_content.is_empty() {
                cpa_json::set(&mut assistant, "reasoning_content", reasoning_content.clone());
                if is_usable_reasoning(&reasoning_content) {
                    self.latest_reasoning_content = reasoning_content;
                }
            } else {
                let fallback = self.fallback_tool_reasoning();
                if !fallback.is_empty() {
                    cpa_json::set(&mut assistant, "reasoning_content", fallback);
                }
            }
            self.messages.push(assistant);
        }
        for id in &self.pending_tool_call_ids {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                self.awaiting_tool_outputs.insert(trimmed.to_string());
            }
        }
        self.pending_tool_calls.clear();
        self.pending_tool_call_ids.clear();
        self.mergeable_assistant_index = None;
    }

    /// Appends the message and returns its index.
    fn append_regular_message(&mut self, message: Value) -> usize {
        self.messages.push(message);
        self.messages.len() - 1
    }

    /// Turns buffered reasoning with no tool call or assistant message to attach to into an
    /// assistant message of its own.
    fn append_pending_reasoning_message(&mut self) {
        let reasoning_content = self.take_pending_reasoning_content();
        if reasoning_content.is_empty() {
            return;
        }
        if is_usable_reasoning(&reasoning_content) {
            self.latest_reasoning_content = reasoning_content.clone();
        }
        let mut message = tpl(r#"{"role":"assistant","content":"","reasoning_content":""}"#);
        cpa_json::set(&mut message, "reasoning_content", reasoning_content);
        self.append_regular_message(message);
    }

    /// Records a tool output id; ids seen more than once are ambiguous for message alignment.
    fn count_output(&mut self, call_id: &str) {
        if call_id.is_empty() {
            return;
        }
        let count = self.output_counts.entry(call_id.to_string()).or_insert(0);
        *count += 1;
        if *count > 1 {
            self.duplicate_output_ids.insert(call_id.to_string());
        }
    }

    /// Tool output item: a tool message when its call is awaiting output, otherwise user text
    /// (orphan outputs, e.g. Codex send_message_to_thread cards, must not become tool messages).
    fn append_tool_output(&mut self, item: &Res<'_>, call_id: &str, set_content: SetContent, item_src: &RawSrc<'_>) {
        self.mergeable_assistant_index = None;
        self.count_output(call_id);
        let output = item.g("output");
        let output_src = item_src.child("output");
        if !self.awaiting_tool_outputs.remove(call_id) {
            self.append_standalone_tool_output_as_user(&output, set_content, &output_src);
        } else {
            let mut tool_message = tpl(r#"{"role":"tool","tool_call_id":"","content":""}"#);
            cpa_json::set(&mut tool_message, "tool_call_id", call_id);
            if output.exists() {
                set_content(&mut tool_message, &output, &output_src);
            }
            self.messages.push(tool_message);
        }
    }

    fn append_standalone_tool_output_as_user(&mut self, output: &Res<'_>, set_content: SetContent, output_src: &RawSrc<'_>) {
        let mut user_message = tpl(r#"{"role":"user","content":""}"#);
        if output.exists() {
            set_content(&mut user_message, output, output_src);
        }
        let content = user_message.g("content");
        if !content.exists() {
            return;
        }
        if content.is_string() && content.str().trim().is_empty() {
            return;
        }
        if content.is_array() && !content.g("0").exists() {
            return;
        }
        let _ = content;
        self.append_regular_message(user_message);
    }
}

/// Converts an OpenAI Responses request (instructions plus input items) into a Chat Completions
/// request: instructions become a system message, input items become messages, and tools, tool
/// choice and generation parameters are mapped.
pub fn convert_openai_responses_request_to_openai_chat_completions(model_name: &str, input_bytes: &[u8], stream: bool) -> Vec<u8> {
    let mut out = tpl(r#"{"model":"","messages":[],"stream":false}"#);

    let root = cpa_json::parse(input_bytes);
    let tool_index = ToolIndex::new(&root);
    let root_src = RawSrc::new(input_bytes, String::new());

    cpa_json::set(&mut out, "model", model_name);
    cpa_json::set(&mut out, "stream", stream);

    // Responses text format -> Chat Completions response format.
    let text_format = root.g("text.format");
    if text_format.exists()
        && let Some(response_format) = convert_text_format_to_chat_response_format(&text_format)
    {
        cpa_json::set(&mut out, "response_format", response_format);
    }

    let max_tokens = root.g("max_output_tokens");
    if max_tokens.exists() {
        cpa_json::set(&mut out, "max_tokens", max_tokens.value());
    }

    let mut conv = Conv {
        messages: Vec::new(),
        pending_tool_calls: Vec::new(),
        pending_tool_call_ids: Vec::new(),
        pending_reasoning_content: String::new(),
        latest_reasoning_content: String::new(),
        awaiting_tool_outputs: HashSet::new(),
        output_counts: HashMap::new(),
        duplicate_output_ids: HashSet::new(),
        mergeable_assistant_index: None,
        has_reasoning_in_session: false,
    };

    let instructions = root.g("instructions");
    if instructions.exists() {
        let mut system_message = tpl(r#"{"role":"system","content":""}"#);
        cpa_json::set(&mut system_message, "content", root_src.child("instructions").string(&instructions));
        conv.messages.push(system_message);
    }

    let input = root.g("input");
    if input.is_array() {
        let raw_input_array = input.array();
        let mut explicit_output_counts: HashMap<String, i32> = HashMap::new();
        let mut missing_id_outputs_count = 0;
        for item in &raw_input_array {
            let item_type = item.g("type").str();
            if item_type == "function_call_output" || item_type == "custom_tool_call_output" {
                let id = common::extract_responses_call_id(item);
                if id.is_empty() {
                    missing_id_outputs_count += 1;
                } else {
                    *explicit_output_counts.entry(id).or_insert(0) += 1;
                }
            }
        }

        let mut unclaimed_calls: HashSet<String> = HashSet::new();
        for item in &raw_input_array {
            let item_type = item.g("type").str();
            if item_type == "function_call" || item_type == "custom_tool_call" {
                let id = common::extract_responses_call_id(item);
                if !id.is_empty() && explicit_output_counts.get(&id).copied().unwrap_or(0) == 0 {
                    unclaimed_calls.insert(id);
                }
            }
        }

        let mut input_items = common::normalize_responses_tool_call_outputs(&raw_input_array);
        if missing_id_outputs_count > 1 || (missing_id_outputs_count > 0 && unclaimed_calls.len() > 1) {
            for idx in 0..input_items.len() {
                let item_type = input_items[idx].g("type").str();
                if (item_type == "function_call_output" || item_type == "custom_tool_call_output")
                    && idx < raw_input_array.len()
                    && common::extract_responses_call_id(&raw_input_array[idx]).is_empty()
                {
                    let mut raw = input_items[idx].value();
                    for key in ["call_id", "tool_call_id", "callId"] {
                        cpa_json::delete(&mut raw, key);
                    }
                    input_items[idx] = Res::owned(raw);
                }
            }
        }

        let is_off = |effort: &str| matches!(effort, "" | "none" | "0" | "false");
        let effort_res = root.g("reasoning.effort");
        let reasoning_effort = root.g("reasoning_effort");
        let reasoning_obj = root.g("reasoning");
        if effort_res.exists() {
            if !is_off(&effort_res.str().trim().to_lowercase()) {
                conv.has_reasoning_in_session = true;
            }
        } else if reasoning_effort.exists() {
            if !is_off(&reasoning_effort.str().trim().to_lowercase()) {
                conv.has_reasoning_in_session = true;
            }
        } else if reasoning_obj.exists() {
            // `String()` of an object is its raw text, whitespace included.
            let raw = root_src.child("reasoning").string(&reasoning_obj);
            let reasoning_raw = raw.trim().to_lowercase();
            if !matches!(reasoning_raw.as_str(), "" | "none" | "false" | "{}") {
                conv.has_reasoning_in_session = true;
            }
        }
        if !conv.has_reasoning_in_session {
            conv.has_reasoning_in_session = raw_input_array
                .iter()
                .any(|item| item.g("type").str() == "reasoning" || item.g("reasoning_content").exists());
        }

        // Normalization keeps item order, so item `idx` is `input.<idx>` in the original request.
        let aligned_with_request = input_items.len() == raw_input_array.len();
        for (item_index, item) in input_items.iter().enumerate() {
            let item_src = if aligned_with_request {
                RawSrc::new(input_bytes, format!("input.{item_index}"))
            } else {
                RawSrc::none()
            };
            let mut item_type = item.g("type").str();
            if item_type.is_empty() && !item.g("role").str().is_empty() {
                item_type = "message".into();
            }
            if item_type != "function_call" && item_type != "custom_tool_call" {
                conv.flush_pending_tool_calls();
            }

            match item_type.as_str() {
                "message" | "" => {
                    let mut role = item.g("role").str();
                    if role == "developer" {
                        role = "user".into();
                    }
                    conv.mergeable_assistant_index = None;
                    if role != "assistant" {
                        conv.append_pending_reasoning_message();
                        conv.latest_reasoning_content.clear();
                    }
                    let mut message = tpl(r#"{"role":"","content":[]}"#);
                    cpa_json::set(&mut message, "role", role.clone());

                    let content = item.g("content");
                    if content.is_array() {
                        let content_items: Vec<Value> =
                            content.array().iter().filter_map(convert_message_content_part).collect();
                        if !content_items.is_empty() {
                            cpa_json::set(&mut message, "content", Value::Array(content_items));
                        }
                    } else if content.is_string() {
                        cpa_json::set(&mut message, "content", content.str());
                    }

                    if role == "assistant" {
                        let pending = conv.take_pending_reasoning_content();
                        let reasoning_content = combine_reasoning(&pending, &item.g("reasoning_content").str());
                        if !reasoning_content.is_empty() {
                            cpa_json::set(&mut message, "reasoning_content", reasoning_content.clone());
                            if is_usable_reasoning(&reasoning_content) {
                                conv.latest_reasoning_content = reasoning_content;
                            }
                        }
                    }

                    let message_index = conv.append_regular_message(message);
                    if role == "assistant" {
                        conv.mergeable_assistant_index = Some(message_index);
                    }
                }

                "reasoning" => {
                    let reasoning_content = collect_reasoning_content(item);
                    conv.pending_reasoning_content = combine_reasoning(&conv.pending_reasoning_content, &reasoning_content);
                    if is_usable_reasoning(&reasoning_content) {
                        conv.latest_reasoning_content = reasoning_content;
                    }
                }

                "function_call" => {
                    let rc = item.g("reasoning_content").str();
                    conv.pending_reasoning_content = combine_reasoning(&conv.pending_reasoning_content, &rc);
                    if is_usable_reasoning(&rc) {
                        conv.latest_reasoning_content = rc;
                    }
                    // Consecutive function calls are buffered and emitted as one assistant message.
                    // Go marshals tool calls through map[string]any: sorted keys.
                    let mut tool_call = tpl(r#"{"function":{"arguments":"","name":""},"id":"","type":"function"}"#);

                    let call_id = common::extract_responses_call_id(item);
                    if !call_id.is_empty() {
                        cpa_json::set(&mut tool_call, "id", call_id.clone());
                    }

                    let name = item.g("name");
                    if name.exists() {
                        let mut function_name = name.str();
                        let namespace = item.g("namespace").str().trim().to_string();
                        function_name = if namespace.is_empty() {
                            tool_index.canonical_name(&function_name)
                        } else {
                            tool_index.namespace_name(&namespace, &function_name)
                        };
                        cpa_json::set(&mut tool_call, "function.name", function_name);
                    }

                    let arguments = item.g("arguments");
                    if arguments.exists() {
                        cpa_json::set(&mut tool_call, "function.arguments", item_src.child("arguments").string(&arguments));
                    }
                    conv.pending_tool_calls.push(tool_call);
                    if !call_id.is_empty() {
                        conv.pending_tool_call_ids.push(call_id);
                    }
                }

                "function_call_output" => {
                    let call_id = common::extract_responses_call_id(item);
                    conv.append_tool_output(item, &call_id, set_function_call_output_content, &item_src);
                }

                "custom_tool_call" => {
                    let rc = item.g("reasoning_content").str();
                    conv.pending_reasoning_content = combine_reasoning(&conv.pending_reasoning_content, &rc);
                    if is_usable_reasoning(&rc) {
                        conv.latest_reasoning_content = rc;
                    }
                    // Codex freeform tool call replay: wrap the raw input to match the
                    // {"input": string} function shape used for converted custom tool definitions.
                    let call_id = common::extract_responses_call_id(item);
                    let mut tool_call = tpl(r#"{"function":{"arguments":"","name":""},"id":"","type":"function"}"#);
                    cpa_json::set(&mut tool_call, "id", call_id.clone());
                    let mut function_name = item.g("name").str();
                    let namespace = item.g("namespace").str();
                    function_name = if namespace.is_empty() {
                        tool_index.canonical_name(&function_name)
                    } else {
                        tool_index.namespace_name(&namespace, &function_name)
                    };
                    cpa_json::set(&mut tool_call, "function.name", function_name);
                    let wrapped_args = format!(r#"{{"input":{}}}"#, sjson_string(&item_src.child("input").string(&item.g("input"))));
                    cpa_json::set(&mut tool_call, "function.arguments", wrapped_args);
                    conv.pending_tool_calls.push(tool_call);
                    if !call_id.is_empty() {
                        conv.pending_tool_call_ids.push(call_id);
                    }
                }

                "custom_tool_call_output" => {
                    let call_id = common::extract_responses_call_id(item);
                    conv.append_tool_output(item, &call_id, set_custom_tool_call_output_content, &item_src);
                }

                _ => conv.mergeable_assistant_index = None,
            }
        }
        conv.flush_pending_tool_calls();
        conv.append_pending_reasoning_message();
    } else if input.is_string() {
        let mut msg = tpl("{}");
        cpa_json::set(&mut msg, "role", "user");
        cpa_json::set(&mut msg, "content", input.str());
        conv.messages.push(msg);
    }

    if !conv.messages.is_empty() {
        let extra_ambiguous: Vec<&str> = conv.duplicate_output_ids.iter().map(String::as_str).collect();
        let raw_messages: Vec<Vec<u8>> = conv.messages.iter().map(cpa_json::to_vec).collect();
        let aligned = common::align_openai_tool_call_messages(&raw_messages, &extra_ambiguous);
        let messages: Vec<Value> = aligned.iter().map(|raw| cpa_json::parse(raw)).collect();
        cpa_json::set(&mut out, "messages", Value::Array(messages));
    }

    // Tools come from both the top-level `tools` field and Codex Desktop (Responses Lite)
    // `additional_tools` input items.
    let chat_completions_tools: Vec<Value> = tool_index.chat_tools().into_iter().map(go_any).collect();
    if !chat_completions_tools.is_empty() {
        cpa_json::set(&mut out, "tools", Value::Array(chat_completions_tools));
        let parallel_tool_calls = root.g("parallel_tool_calls");
        if parallel_tool_calls.exists() {
            cpa_json::set(&mut out, "parallel_tool_calls", parallel_tool_calls.bool());
        }
        let tool_choice = root.g("tool_choice");
        if tool_choice.exists() {
            cpa_json::set(&mut out, "tool_choice", convert_tool_choice_with_index(&tool_choice, &tool_index));
        }
    }

    let reasoning_effort = root.g("reasoning.effort");
    if reasoning_effort.exists() {
        let effort = reasoning_effort.str().trim().to_lowercase();
        if !effort.is_empty() {
            cpa_json::set(&mut out, "reasoning_effort", effort);
        }
    }

    cpa_json::to_vec(&out)
}

/// A string as sjson writes it: verbatim between quotes unless it holds control, non-ASCII, quote
/// or backslash bytes, in which case it is `json.Marshal`ed (escaping `<`, `>`, `&`, U+2028).
fn sjson_string(s: &str) -> String {
    if s.bytes().any(|b| !(0x20..=0x7f).contains(&b) || b == b'"' || b == b'\\') {
        go_json_string(s)
    } else {
        format!("\"{s}\"")
    }
}

/// Chat Completions content part for one Responses message content item (unknown types are
/// dropped).
fn convert_message_content_part(content_item: &Res<'_>) -> Option<Value> {
    let mut content_type = content_item.g("type").str();
    if content_type.is_empty() {
        content_type = "input_text".into();
    }
    match content_type.as_str() {
        "input_text" | "output_text" => {
            let mut part = tpl(r#"{"type":"text","text":""}"#);
            cpa_json::set(&mut part, "text", content_item.g("text").str());
            Some(part)
        }
        "input_video" | "video_url" => {
            // Malformed video parts are preserved for upstream validation instead of silently
            // turning a video request into a text-only request.
            let mut part = tpl(r#"{"type":"video_url","video_url":{}}"#);
            let video_url = content_item.g("video_url");
            if video_url.is_object() {
                cpa_json::set(&mut part, "video_url", video_url.value());
            } else if video_url.exists() {
                cpa_json::set(&mut part, "video_url.url", video_url.value());
            }
            let processing = content_item.g("processing");
            if processing.exists() {
                cpa_json::set(&mut part, "video_url.processing", processing.value());
            }
            Some(part)
        }
        "input_image" => {
            let mut part = tpl(r#"{"type":"image_url","image_url":{"url":""}}"#);
            cpa_json::set(&mut part, "image_url.url", content_item.g("image_url").str());
            if let Some(detail) = normalize_chat_image_detail(&content_item.g("detail")).filter(|d| !d.is_empty()) {
                cpa_json::set(&mut part, "image_url.detail", detail);
            }
            Some(part)
        }
        _ => None,
    }
}

/// tool_choice for the Chat Completions request: function/custom choices are re-named through the
/// tool index, anything else passes through.
fn convert_tool_choice_with_index(tool_choice: &Res<'_>, tool_index: &ToolIndex) -> Value {
    if !tool_choice.is_object() {
        return tool_choice.value();
    }

    let choice_type = tool_choice.g("type").str();
    if choice_type != "function" && choice_type != "custom" {
        return tool_choice.value();
    }

    let mut name = tool_choice.g("function.name").str();
    if name.is_empty() {
        name = tool_choice.g("custom.name").str();
    }
    if name.is_empty() {
        name = tool_choice.g("name").str();
    }
    if name.is_empty() {
        return tool_choice.value();
    }

    let mut namespace = tool_choice.g("namespace").str().trim().to_string();
    if namespace.is_empty() {
        namespace = tool_choice.g("function.namespace").str().trim().to_string();
    }
    if namespace.is_empty() {
        namespace = tool_choice.g("custom.namespace").str().trim().to_string();
    }
    name = if namespace.is_empty() {
        tool_index.canonical_name(&name)
    } else {
        tool_index.namespace_name(&namespace, &name)
    };

    let mut converted = tpl(r#"{"type":"function","function":{"name":""}}"#);
    cpa_json::set(&mut converted, "function.name", name);
    converted
}

/// Responses `text.format` -> Chat Completions `response_format`; `None` for unknown formats.
fn convert_text_format_to_chat_response_format(text_format: &Res<'_>) -> Option<Value> {
    let format_type = text_format.g("type").str();
    match format_type.as_str() {
        "text" | "json_object" => {
            let mut response_format = tpl(r#"{"type":""}"#);
            cpa_json::set(&mut response_format, "type", format_type);
            Some(response_format)
        }
        "json_schema" => {
            let mut response_format = tpl(r#"{"type":"json_schema","json_schema":{}}"#);
            for field in ["name", "description", "strict"] {
                let value = text_format.g(field);
                if value.exists() {
                    cpa_json::set(&mut response_format, &format!("json_schema.{field}"), go_any(value.value()));
                }
            }
            let schema = text_format.g("schema");
            if schema.exists() {
                cpa_json::set(&mut response_format, "json_schema.schema", schema.value());
            }
            Some(response_format)
        }
        _ => None,
    }
}

/// Tool message content for a function_call_output: an image-bearing structured output becomes
/// content parts, everything else its text form.
fn set_function_call_output_content(tool_message: &mut Value, output: &Res<'_>, src: &RawSrc<'_>) {
    let mut structured = output.clone();
    let mut structured_src = src.clone();
    if output.is_string() {
        let text = output.str();
        if !cpa_json::valid(text.as_bytes()) {
            cpa_json::set(tool_message, "content", text);
            return;
        }
        structured = Res::owned(cpa_json::parse_str(&text));
        // A JSON string output is its own document.
        structured_src = RawSrc::owned(text.into_bytes());
    }

    if has_chat_tool_output_image_part(&structured) {
        let content_items: Vec<Value> = structured
            .array()
            .iter()
            .enumerate()
            .map(|(k, item)| chat_tool_output_content_part(item, &structured_src.child(k)))
            .collect();
        cpa_json::set(tool_message, "content", Value::Array(content_items));
        return;
    }

    cpa_json::set(tool_message, "content", src.string(output));
}

fn set_custom_tool_call_output_content(tool_message: &mut Value, output: &Res<'_>, src: &RawSrc<'_>) {
    let mut structured = output.clone();
    if output.is_string() && cpa_json::valid(output.str().as_bytes()) {
        structured = Res::owned(cpa_json::parse_str(&output.str()));
    }
    if has_chat_tool_output_image_part(&structured) {
        set_function_call_output_content(tool_message, output, src);
        return;
    }

    cpa_json::set(tool_message, "content", responses_tool_output_text(output, src));
}

fn chat_tool_output_content_part(item: &Res<'_>, src: &RawSrc<'_>) -> Value {
    match item.g("type").str().as_str() {
        "text" | "input_text" | "output_text" => {
            let mut part = tpl(r#"{"type":"text","text":""}"#);
            cpa_json::set(&mut part, "text", item.g("text").str());
            part
        }
        "image_url" | "input_image" => {
            let Some((image_url, detail)) = chat_tool_output_image_fields(item) else {
                return chat_tool_output_fallback_part(item, src);
            };
            let mut part = tpl(r#"{"type":"image_url","image_url":{"url":""}}"#);
            cpa_json::set(&mut part, "image_url.url", image_url);
            if !detail.is_empty() {
                cpa_json::set(&mut part, "image_url.detail", detail);
            }
            part
        }
        _ => chat_tool_output_fallback_part(item, src),
    }
}

fn has_chat_tool_output_image_part(content: &Res<'_>) -> bool {
    if !content.is_array() {
        return false;
    }

    let mut has_image = false;
    for item in content.array() {
        let item_type = item.g("type");
        if !item_type.is_string() {
            continue;
        }
        match item_type.str().as_str() {
            "text" | "input_text" | "output_text" => {
                if !item.g("text").is_string() {
                    return false;
                }
            }
            "image_url" | "input_image" => {
                if chat_tool_output_image_fields(&item).is_none() {
                    return false;
                }
                has_image = true;
            }
            _ => {}
        }
    }
    has_image
}

/// (image url, normalized detail) of an image tool-output part; `None` when malformed.
fn chat_tool_output_image_fields(item: &Res<'_>) -> Option<(String, String)> {
    let (image_url_value, detail_value) = match item.g("type").str().as_str() {
        "image_url" => (item.g("image_url.url"), item.g("image_url.detail")),
        "input_image" => (item.g("image_url"), item.g("detail")),
        _ => return None,
    };

    if !image_url_value.is_string() {
        return None;
    }
    let image_url = image_url_value.str().trim().to_string();
    if image_url.is_empty() {
        return None;
    }
    let detail = normalize_chat_image_detail(&detail_value)?;
    Some((image_url, detail))
}

/// Normalized image detail: `Some("")` for absent or unknown values, `None` when not a string.
fn normalize_chat_image_detail(detail_value: &Res<'_>) -> Option<String> {
    if !detail_value.exists() {
        return Some(String::new());
    }
    if !detail_value.is_string() {
        return None;
    }
    Some(match detail_value.str().trim().to_lowercase().as_str() {
        d @ ("auto" | "low" | "high") => d.to_string(),
        // Chat Completions does not support Codex's original detail value.
        "original" => "high".to_string(),
        _ => String::new(),
    })
}

fn chat_tool_output_fallback_part(item: &Res<'_>, src: &RawSrc<'_>) -> Value {
    let mut text = src.raw(item);
    if item.is_string() || text.is_empty() {
        text = item.str();
    }
    let mut part = tpl(r#"{"type":"text","text":""}"#);
    cpa_json::set(&mut part, "text", text);
    part
}

/// Summary text of a Responses reasoning item, or a placeholder when it has none.
fn collect_reasoning_content(item: &Res<'_>) -> String {
    let mut reasoning_text = String::new();
    let summary = item.g("summary");
    if summary.is_array() {
        for summary_item in summary.array() {
            if summary_item.g("type").str() == "summary_text" {
                reasoning_text.push_str(&summary_item.g("text").str());
            }
        }
    }
    if reasoning_text.is_empty() {
        return REASONING_UNAVAILABLE.to_string();
    }
    reasoning_text
}

fn combine_reasoning(existing: &str, incoming: &str) -> String {
    let existing_trimmed = existing.trim();
    let incoming_trimmed = incoming.trim();

    if existing_trimmed.is_empty() {
        incoming.to_string()
    } else if incoming_trimmed.is_empty() {
        existing.to_string()
    } else if existing_trimmed == REASONING_UNAVAILABLE {
        incoming.to_string()
    } else if incoming_trimmed == REASONING_UNAVAILABLE || existing_trimmed == incoming_trimmed {
        existing.to_string()
    } else {
        format!("{existing}\n\n{incoming}")
    }
}

fn is_usable_reasoning(reasoning: &str) -> bool {
    let trimmed = reasoning.trim();
    !trimmed.is_empty() && trimmed != REASONING_UNAVAILABLE
}

//! OpenAI Responses request to Claude Messages request
//! (Go: claude/openai/responses/claude_openai-responses_request.go).

use std::collections::{BTreeMap, HashMap, HashSet};

use cpa_core::applypatch;
use cpa_core::registry::lookup_model_info;
use cpa_core::signature::{self, SignatureProvider};
use cpa_core::util::{normalize_claude_tool_input_schema, sanitize_claude_function_name, sanitize_claude_tool_id};
use cpa_json::{J, Kind, Res, Value};

use super::response::CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX;
use super::tool_names::{
    ToolDescriptor, build_claude_tool_names, is_unsupported_openai_builtin_tool_type,
    qualify_responses_namespace_tool_name, responses_tool_description, responses_tool_descriptors, responses_tool_name,
    responses_tool_name_map, responses_tool_parameters, responses_tool_winners,
};
use super::web_search::{attach_claude_citations, convert_responses_web_search_call_to_claude_blocks};
use crate::claude::openai::chat_completions::{apply_reasoning_effort, structured_output_instruction};
use crate::common;

const DEFAULT_CLAUDE_RESPONSES_MAX_TOKENS: i64 = 32000;
const DEFAULT_FABLE_RESPONSES_MAX_TOKENS: i64 = 64000;

/// Transforms an OpenAI Responses API request into a Claude Messages API request: instructions
/// and system/developer input items become top-level system blocks, messages and tool calls
/// become Claude messages, and tools (including `additional_tools`) become Claude tools.
pub fn convert_openai_responses_request_to_claude(model_name: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    convert(model_name, raw, stream, false)
}

/// Like [`convert_openai_responses_request_to_claude`], but keeps reasoning items whose encrypted
/// content is not a Claude signature (as the raw signature) for compatibility endpoints.
pub fn convert_openai_responses_request_to_claude_with_compat(model_name: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    convert(model_name, raw, stream, true)
}

fn text_block(text: &str) -> Value {
    let mut v = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
    cpa_json::set(&mut v, "text", text);
    v
}

/// `block` with the valid `cache_control` of `src` attached (common helper over `Value`).
fn with_cache_control(block: Value, src: &Res<'_>) -> Value {
    cpa_json::parse(&common::attach_cache_control(&cpa_json::to_vec(&block), src))
}

fn set_choice(out: &mut Value, json: &str) {
    cpa_json::set(out, "tool_choice", cpa_json::parse_str(json));
}

fn type_of(v: &Value) -> String {
    v.g("type").str()
}

/// Assembles Claude messages from consecutive same-role parts. Client tool calls are held back
/// and appended after the other parts of an assistant turn.
#[derive(Default)]
struct MessageBuilder {
    messages: Vec<Value>,
    pending_role: String,
    pending_parts: Vec<Value>,
    pending_tool_use_parts: Vec<Value>,
}

impl MessageBuilder {
    fn flush(&mut self) {
        if self.pending_role.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.pending_parts);
        let tool_uses = std::mem::take(&mut self.pending_tool_use_parts);
        if self.pending_role == "assistant" && !tool_uses.is_empty() {
            if let Some(separator) = claude_thinking_separator_for_tool_use(&parts) {
                parts.push(separator);
            }
            parts.extend(tool_uses);
        }
        if !parts.is_empty() {
            let mut msg = cpa_json::parse_str(r#"{"role":"","content":[]}"#);
            cpa_json::set(&mut msg, "role", self.pending_role.as_str());
            set_message_content(&mut msg, parts);
            self.messages.push(msg);
        }
        self.pending_role.clear();
    }

    fn append_parts(&mut self, role: &str, parts: Vec<Value>) {
        if role.is_empty() || parts.is_empty() {
            return;
        }
        if !self.pending_role.is_empty() && self.pending_role != role {
            self.flush();
        }
        self.pending_role = role.to_string();
        self.pending_parts.extend(parts);
    }

    fn append_tool_use(&mut self, tool_use: Value) {
        if !self.pending_role.is_empty() && self.pending_role != "assistant" {
            self.flush();
        }
        self.pending_role = "assistant".to_string();
        self.pending_tool_use_parts.push(tool_use);
    }

    fn append_reasoning(&mut self, reasoning_part: Option<Value>) {
        let Some(reasoning_part) = reasoning_part else { return };
        if !self.pending_role.is_empty() && self.pending_role != "assistant" {
            self.flush();
        }
        self.pending_role = "assistant".to_string();

        // Client tool calls normally stay at the end of an assistant message, but a later
        // reasoning item makes them a real separator between thinking blocks.
        if !self.pending_tool_use_parts.is_empty() {
            let held = std::mem::take(&mut self.pending_tool_use_parts);
            self.pending_parts.extend(held);
        }

        // A later thinking block replaces an adjacent earlier one.
        if type_of(&reasoning_part) == "thinking"
            && let Some(last) = self.pending_parts.last_mut()
            && type_of(last) == "thinking"
        {
            *last = reasoning_part;
            return;
        }
        self.pending_parts.push(reasoning_part);
    }
}

/// Sets `content` to the lone text as a plain string when it carries nothing else, else to the
/// array of parts.
fn set_message_content(msg: &mut Value, parts: Vec<Value>) {
    if let [part] = parts.as_slice()
        && type_of(part) == "text"
        && !part.g("cache_control").exists()
        && !part.g("citations").exists()
    {
        let text = part.g("text").str();
        cpa_json::set(msg, "content", text);
        return;
    }
    cpa_json::set(msg, "content", Value::Array(parts));
}

fn default_claude_responses_max_tokens_for_model(model_name: &str) -> i64 {
    let mut max_tokens = DEFAULT_CLAUDE_RESPONSES_MAX_TOKENS;
    if model_name.trim().to_lowercase().contains("fable") {
        max_tokens = DEFAULT_FABLE_RESPONSES_MAX_TOKENS;
    }
    if let Some(info) = lookup_model_info(model_name, Some("claude"))
        && info.max_completion_tokens > 0
        && info.max_completion_tokens < max_tokens
    {
        return info.max_completion_tokens;
    }
    max_tokens
}

/// Whether an input item carries system-level authority (system or developer).
fn is_responses_system_level_role(role: &str) -> bool {
    matches!(role.trim().to_lowercase().as_str(), "system" | "developer")
}

/// `data:<media>;base64,<data>` split into media type and data; a missing separator leaves the
/// default media type and empty data.
fn split_base64_data_url(url: &str) -> (String, String) {
    let trimmed = url.strip_prefix("data:").unwrap_or(url);
    let mut media_type = "application/octet-stream".to_string();
    let mut data = String::new();
    if let Some((media, rest)) = trimmed.split_once(";base64,") {
        if !media.is_empty() {
            media_type = media.to_string();
        }
        data = rest.to_string();
    }
    (media_type, data)
}

/// A base64 document block from `input_file.file_data` (a data URL or bare base64).
fn document_block(file_data: &str) -> Value {
    let mut media_type = "application/octet-stream".to_string();
    let mut data = file_data.to_string();
    if file_data.starts_with("data:") {
        let trimmed = file_data.strip_prefix("data:").unwrap_or(file_data);
        if let Some((media, rest)) = trimmed.split_once(";base64,") {
            if !media.is_empty() {
                media_type = media.to_string();
            }
            data = rest.to_string();
        }
    }
    let mut part = cpa_json::parse_str(r#"{"type":"document","source":{"type":"base64","media_type":"","data":""}}"#);
    cpa_json::set(&mut part, "source.media_type", media_type);
    cpa_json::set(&mut part, "source.data", data);
    part
}

/// An image block from `input_image` (data URL or remote URL); `None` for a data URL without data.
fn image_block(url: &str) -> Option<Value> {
    if url.starts_with("data:") {
        let (media_type, data) = split_base64_data_url(url);
        if data.is_empty() {
            return None;
        }
        let mut part = cpa_json::parse_str(r#"{"type":"image","source":{"type":"base64","media_type":"","data":""}}"#);
        cpa_json::set(&mut part, "source.media_type", media_type);
        cpa_json::set(&mut part, "source.data", data);
        return Some(part);
    }
    let mut part = cpa_json::parse_str(r#"{"type":"image","source":{"type":"url","url":""}}"#);
    cpa_json::set(&mut part, "source.url", url);
    Some(part)
}

/// `image_url`, falling back to `url`.
fn input_image_url(part: &Res<'_>) -> String {
    let url = part.g("image_url").str();
    if url.is_empty() { part.g("url").str() } else { url }
}

fn convert(model_name: &str, input_raw: &[u8], stream: bool, preserve_empty_thinking_blocks: bool) -> Vec<u8> {
    let raw_json = normalize_codex_agent_messages(input_raw);

    let user_id = common::derive_claude_user_id(&raw_json);

    let mut out = cpa_json::parse_str(r#"{"model":"","max_tokens":32000,"messages":[],"metadata":{}}"#);
    cpa_json::set(&mut out, "metadata.user_id", user_id);
    cpa_json::set(&mut out, "max_tokens", default_claude_responses_max_tokens_for_model(model_name));

    let root = cpa_json::parse(&raw_json);

    // reasoning.effort -> Claude thinking config.
    let v = root.g("reasoning.effort");
    if v.exists() {
        apply_reasoning_effort(&mut out, model_name, &v.str());
    }

    cpa_json::set(&mut out, "model", model_name);

    let mot = root.g("max_output_tokens");
    if mot.exists() && !mot.is_null() {
        let mut val = mot.int();
        if let Some(info) = lookup_model_info(model_name, Some("claude"))
            && info.max_completion_tokens > 0
            && val > info.max_completion_tokens
        {
            val = info.max_completion_tokens;
        }
        cpa_json::set(&mut out, "max_tokens", val);
    }

    cpa_json::set(&mut out, "stream", stream);

    // service_tier priority -> fast speed.
    let st = root.g("service_tier");
    if st.is_string() && st.str() == "priority" {
        cpa_json::set(&mut out, "speed", "fast");
    }

    // System-level inputs become top-level Claude system blocks in source order: instructions
    // first, then every input item whose role is system or developer. Each source block stays a
    // separate Claude block; this layer must not merge, trim or downgrade them.
    let mut system_blocks: Vec<Value> = Vec::with_capacity(4);
    fn append_system_text(blocks: &mut Vec<Value>, text: &str, cache_source: Option<&Res<'_>>) {
        if text.is_empty() {
            return;
        }
        let block = text_block(text);
        blocks.push(match cache_source {
            Some(src) if src.exists() => with_cache_control(block, src),
            _ => block,
        });
    }
    let instr = root.g("instructions");
    if instr.is_string() {
        append_system_text(&mut system_blocks, &instr.str(), None);
    }
    let input = root.g("input");
    if input.is_array() {
        for item in input.array() {
            if !is_responses_system_level_role(&item.g("role").str()) {
                continue;
            }
            let start_idx = system_blocks.len();
            let content = item.g("content");
            if content.is_string() {
                append_system_text(&mut system_blocks, &content.str(), None);
            } else if content.is_array() {
                for part in content.array() {
                    match part.g("type").str().as_str() {
                        "input_text" | "output_text" | "text" => {
                            append_system_text(&mut system_blocks, &part.g("text").str(), Some(&part))
                        }
                        _ => {
                            if let Some(block) = responses_system_unsupported_block(&part) {
                                system_blocks.push(block);
                            }
                        }
                    }
                }
            }
            // Item-level cache_control applies to the last block this item produced.
            if item.g("cache_control").exists() && system_blocks.len() > start_idx {
                let last_idx = system_blocks.len() - 1;
                if !system_blocks[last_idx].g("cache_control").exists() {
                    let last = std::mem::take(&mut system_blocks[last_idx]);
                    system_blocks[last_idx] = with_cache_control(last, &item);
                }
            }
        }
    }

    let (format_path, format_result) = match root.g("text.format") {
        f if f.exists() => ("text.format", f),
        _ => ("response_format", root.g("response_format")),
    };
    let format_instruction = structured_output_instruction(&raw_json, format_path, &format_result);
    if !format_instruction.is_empty() {
        append_system_text(&mut system_blocks, &format_instruction, None);
    }

    let names = build_claude_tool_names(&root);

    let mut builder = MessageBuilder::default();

    let mut input_items: Vec<Res<'_>> = Vec::new();
    if input.exists() {
        if input.is_array() {
            input_items = common::normalize_responses_tool_call_outputs(&input.array());
        } else if input.is_string() {
            builder.append_parts("user", vec![text_block(&input.str())]);
        }
    }

    let mut last_tool_result: HashMap<String, &Res<'_>> = HashMap::new();
    for item in &input_items {
        if matches!(item.g("type").str().as_str(), "function_call_output" | "custom_tool_call_output") {
            let raw_id = common::extract_responses_call_id(item);
            if !raw_id.is_empty() {
                last_tool_result.insert(raw_id, item);
            }
        }
    }
    let mut emitted_tool_results: HashSet<String> = HashSet::new();
    let mut emitted_raw_tool_uses: HashSet<String> = HashSet::new();
    let mut unmapped_item_types: BTreeMap<String, usize> = BTreeMap::new();

    for item in &input_items {
        // System-level items already became top-level system blocks.
        if is_responses_system_level_role(&item.g("role").str()) {
            continue;
        }
        let mut typ = item.g("type").str();
        if typ.is_empty() && !item.g("role").str().is_empty() {
            typ = "message".to_string();
        }
        match typ.as_str() {
            "message" => convert_message_item(item, &mut builder),

            "web_search_call" => {
                // Rebuild the Claude server-side search pair so the replayed turn still shows
                // the search and its hits.
                let blocks = convert_responses_web_search_call_to_claude_blocks(item);
                builder.append_parts("assistant", blocks);
            }

            "reasoning" => builder
                .append_reasoning(convert_responses_reasoning_to_claude_thinking(item, preserve_empty_thinking_blocks)),

            "function_call" | "custom_tool_call" => {
                // Freeform custom input is wrapped in an object because Claude tool_use input
                // must be a JSON object.
                let raw_call_id = common::extract_responses_call_id(item);
                let mut call_id = raw_call_id.clone();
                if call_id.is_empty() {
                    call_id = common::generate_claude_tool_call_id();
                }
                let call_id = sanitize_claude_tool_id(&call_id);
                if !raw_call_id.is_empty() {
                    emitted_raw_tool_uses.insert(raw_call_id);
                }
                let mut name = item.g("name").str();
                let namespace_name = item.g("namespace").str();
                if !namespace_name.trim().is_empty() {
                    // Rebuild the qualified name emitted by the previous Responses turn.
                    name = qualify_responses_namespace_tool_name(namespace_name.trim(), &name);
                }

                let mut tool_use = cpa_json::parse_str(r#"{"type":"tool_use","id":"","name":"","input":{}}"#);
                cpa_json::set(&mut tool_use, "id", call_id);
                cpa_json::set(&mut tool_use, "name", names.claude_name(&name));
                if typ == "custom_tool_call" {
                    cpa_json::set(&mut tool_use, "input.input", item.g("input").str());
                } else {
                    let args_str = item.g("arguments").str();
                    if !args_str.is_empty() && cpa_json::valid(args_str.as_bytes()) {
                        let args_json = cpa_json::parse_str(&args_str);
                        if args_json.is_object() {
                            cpa_json::set(&mut tool_use, "input", args_json);
                        }
                    }
                }
                builder.append_tool_use(tool_use);
            }

            "function_call_output" | "custom_tool_call_output" => {
                let raw_id = common::extract_responses_call_id(item);
                if !raw_id.is_empty() && !emitted_tool_results.insert(raw_id.clone()) {
                    continue;
                }
                let mut output = item.g("output");
                if !raw_id.is_empty()
                    && let Some(last_item) = last_tool_result.get(&raw_id)
                {
                    output = last_item.g("output");
                }
                // Standalone outputs (no call_id, or one that never paired with a function_call
                // in this input) have no tool_use to attach to. Claude rejects orphan
                // tool_result blocks, so they become plain user text. Pairing is decided on the
                // raw id so distinct ids that sanitize to the same Claude id are not mistaken
                // for the same call.
                if raw_id.is_empty() || !emitted_raw_tool_uses.contains(&raw_id) {
                    builder.append_parts("user", convert_responses_standalone_tool_output_to_claude_text(&output));
                    continue;
                }
                let call_id = sanitize_claude_tool_id(&raw_id);
                let mut tool_result = cpa_json::parse_str(r#"{"type":"tool_result","tool_use_id":"","content":""}"#);
                cpa_json::set(&mut tool_result, "tool_use_id", call_id);
                apply_responses_tool_result_content(&mut tool_result, &output);
                builder.append_parts("user", vec![tool_result]);
            }

            other => {
                // additional_tools is consumed later when building tools[]; anything else is
                // dropped turn content worth surfacing in the log.
                if !other.is_empty() && other != "additional_tools" {
                    *unmapped_item_types.entry(other.to_string()).or_default() += 1;
                }
            }
        }
    }
    builder.flush();
    if !unmapped_item_types.is_empty() {
        tracing::warn!(
            "responses->claude: dropped input items of unmapped types {unmapped_item_types:?} (model={model_name})"
        );
    }
    let mut message_blocks = builder.messages;
    let had_messages = !message_blocks.is_empty();
    if !preserve_empty_thinking_blocks {
        strip_trailing_claude_thinking_blocks(&mut message_blocks);
    }
    // Answer dangling tool_use blocks before the prefill check so an interrupted turn ends with
    // a synthesized user tool_result instead of a rejected assistant prefill.
    let mut message_blocks = repair_claude_tool_pairing(message_blocks);
    if !preserve_empty_thinking_blocks {
        drop_unsupported_claude_assistant_prefill(model_name, &mut message_blocks);
    }
    let problems = claude_message_invariant_problems(&message_blocks);
    if !problems.is_empty() {
        tracing::warn!(
            "responses->claude: message invariants violated after repair (model={model_name}): {}",
            problems.join("; ")
        );
    }
    // Keep a minimal conversational turn for system-only inputs or emptied messages so
    // downstream validation still sees a Claude-shaped request.
    if message_blocks.is_empty() && (!system_blocks.is_empty() || had_messages) {
        message_blocks.push(cpa_json::parse_str(r#"{"role":"user","content":[{"type":"text","text":""}]}"#));
    }
    if !message_blocks.is_empty() {
        cpa_json::set(&mut out, "messages", Value::Array(message_blocks));
    }
    if !system_blocks.is_empty() {
        cpa_json::set(&mut out, "system", Value::Array(system_blocks));
    }

    let mut included_tool_names: HashSet<String> = HashSet::new();

    // Responses Lite puts tool definitions in input[].additional_tools. One winner is selected
    // for each final name; surviving tools keep their original order.
    let mut tool_items: Vec<Value> = Vec::new();
    let winners = responses_tool_winners(&root);
    for descriptor in responses_tool_descriptors(&root) {
        if winners.get(&descriptor.name).is_none_or(|w| w.order != descriptor.order) {
            continue;
        }
        let claude_name = names.claude_name(&descriptor.name);
        let Some(t_json) = convert_responses_tool_descriptor_to_claude(&descriptor, &claude_name) else {
            continue;
        };
        let tool_name = t_json.g("name").str();
        if !tool_name.is_empty() {
            included_tool_names.insert(descriptor.name.clone());
            included_tool_names.insert(tool_name);
        }
        tool_items.push(t_json);
    }
    let tool_name_map = responses_tool_name_map(&root, &included_tool_names);
    if !tool_items.is_empty() {
        cpa_json::set(&mut out, "tools", Value::Array(tool_items));
    }

    // tool_choice, like the Chat Completions translator.
    let tool_choice = root.g("tool_choice");
    match tool_choice.kind() {
        _ if !tool_choice.exists() => {}
        Kind::String => match tool_choice.str().as_str() {
            "auto" => set_choice(&mut out, r#"{"type":"auto"}"#),
            // none leaves the choice unset (implies no tools)
            "required" if !included_tool_names.is_empty() => set_choice(&mut out, r#"{"type":"any"}"#),
            _ => {}
        },
        Kind::Json => {
            let choice_type = tool_choice.g("type").str();
            if choice_type == "function" || choice_type == "custom" {
                let mut fnn = tool_choice.g("function.name").str();
                if fnn.is_empty() {
                    fnn = tool_choice.g("custom.name").str();
                }
                if fnn.is_empty() {
                    fnn = tool_choice.g("name").str();
                }
                let mut namespace_name = tool_choice.g("namespace").str();
                if namespace_name.is_empty() {
                    namespace_name = tool_choice.g("function.namespace").str();
                }
                if namespace_name.is_empty() {
                    namespace_name = tool_choice.g("custom.namespace").str();
                }
                if !namespace_name.is_empty() {
                    fnn = qualify_responses_namespace_tool_name(&namespace_name, &fnn);
                }
                if let Some(mapped) = tool_name_map.get(&fnn)
                    && !mapped.is_empty()
                {
                    fnn = mapped.clone();
                }
                if included_tool_names.contains(&fnn) {
                    let mut choice = cpa_json::parse_str(r#"{"name":"","type":"tool"}"#);
                    cpa_json::set(&mut choice, "name", names.claude_name(&fnn));
                    cpa_json::set(&mut out, "tool_choice", choice);
                }
            }
        }
        _ => {}
    }

    crate::common::apply_translated_summary_to_claude(&cpa_json::to_vec(&out), &raw_json, "openai-response", model_name)
}

/// Converts one `message` input item into Claude parts and queues them on `builder`.
fn convert_message_item(item: &Res<'_>, builder: &mut MessageBuilder) {
    let mut role = String::new();
    let mut parts_json: Vec<Value> = Vec::new();
    let parts = item.g("content");
    if parts.is_array() {
        for part in parts.array() {
            let ptype = part.g("type").str();
            match ptype.as_str() {
                "input_text" | "output_text" => {
                    let t = part.g("text");
                    if t.exists() {
                        let mut content_part = text_block(&t.str());
                        content_part = attach_claude_citations(content_part, &part.g("annotations"));
                        content_part = with_cache_control(content_part, &part);
                        parts_json.push(content_part);
                    }
                    role = if ptype == "input_text" { "user" } else { "assistant" }.to_string();
                }
                "refusal" => {
                    // Claude has no refusal block; the text keeps the turn intact.
                    let t = part.g("refusal");
                    if t.exists() && !t.str().is_empty() {
                        parts_json.push(with_cache_control(text_block(&t.str()), &part));
                    }
                    role = "assistant".to_string();
                }
                "input_image" => {
                    let url = input_image_url(&part);
                    if !url.is_empty()
                        && let Some(content_part) = image_block(&url)
                    {
                        parts_json.push(with_cache_control(content_part, &part));
                        if role.is_empty() {
                            role = "user".to_string();
                        }
                    }
                }
                "input_file" => {
                    let file_data = part.g("file_data").str();
                    if !file_data.is_empty() {
                        parts_json.push(with_cache_control(document_block(&file_data), &part));
                        if role.is_empty() {
                            role = "user".to_string();
                        }
                    }
                }
                _ => {}
            }
        }
    } else if parts.is_string() && !parts.str().is_empty() {
        parts_json.push(text_block(&parts.str()));
    }

    // Fall back to the item's own role when the content types are not decisive.
    if role.is_empty() {
        role = match item.g("role").str().as_str() {
            "assistant" => "assistant".to_string(),
            _ => "user".to_string(),
        };
    }

    if let Some(last) = parts_json.last_mut()
        && !last.g("cache_control").exists()
    {
        let taken = std::mem::take(last);
        *last = with_cache_control(taken, item);
    }
    builder.append_parts(&role, parts_json);
}

/// A typed marker for a system-level content part Claude cannot carry. Anthropic accepts only
/// text in `system`, so images, files and unknown parts have no lossless mapping; the marker lets
/// the Claude executor fail the request naming the offending type instead of silently dropping
/// operator instructions.
fn responses_system_unsupported_block(part: &Res<'_>) -> Option<Value> {
    let part_type = part.g("type").str().trim().to_string();
    if part_type.is_empty() {
        return None;
    }
    let mut block = cpa_json::parse_str(r#"{"type":""}"#);
    cpa_json::set(&mut block, "type", part_type);
    Some(block)
}

/// Removes a trailing assistant message for Claude families that reject assistant prefill.
fn drop_unsupported_claude_assistant_prefill(model_name: &str, messages: &mut Vec<Value>) {
    if !claude_model_rejects_assistant_prefill(model_name) {
        return;
    }
    if messages.last().is_some_and(|last| last.g("role").str().trim().eq_ignore_ascii_case("assistant")) {
        messages.pop();
    }
}

/// Removes trailing thinking and redacted_thinking blocks from the final assistant message
/// (Anthropic rejects a final thinking block); the message goes when nothing remains.
fn strip_trailing_claude_thinking_blocks(messages: &mut Vec<Value>) {
    let Some(last) = messages.last() else { return };
    if !last.g("role").str().trim().eq_ignore_ascii_case("assistant") {
        return;
    }
    let content = last.g("content");
    if !content.is_array() {
        return;
    }
    let parts = content.array();
    let mut end = parts.len();
    while end > 0 {
        let part_type = parts[end - 1].g("type").str();
        if matches!(part_type.trim(), "thinking" | "redacted_thinking") {
            end -= 1;
        } else {
            break;
        }
    }
    if end == parts.len() {
        return;
    }
    if end == 0 {
        messages.pop();
        return;
    }
    let remaining: Vec<Value> = parts[..end].iter().map(Res::value).collect();
    drop(parts);
    let idx = messages.len() - 1;
    set_message_content(&mut messages[idx], remaining);
}

/// Whether a Claude model family disallows trailing assistant prefill.
fn claude_model_rejects_assistant_prefill(model_name: &str) -> bool {
    let normalized = model_name.trim().to_lowercase();
    ["fable", "opus-5", "sonnet-4-6"].iter().any(|family| normalized.contains(family))
}

/// Rebuilds one Claude thinking block from a Responses reasoning item. Anthropic requires a
/// signature on every thinking block, so an item whose `encrypted_content` is missing or belongs
/// to another provider is dropped; compat mode keeps the raw value as the signature instead.
fn convert_responses_reasoning_to_claude_thinking(item: &Res<'_>, preserve_empty: bool) -> Option<Value> {
    let encrypted = item.g("encrypted_content").str();
    if let Some(data) = responses_redacted_thinking_data(&encrypted) {
        if data.is_empty() {
            return None;
        }
        let mut redacted = cpa_json::parse_str(r#"{"type":"redacted_thinking","data":""}"#);
        cpa_json::set(&mut redacted, "data", data);
        return Some(redacted);
    }

    let signature = match signature::compatible_signature_for_provider(SignatureProvider::Claude, &encrypted) {
        Some(sig) => sig,
        None if preserve_empty => encrypted,
        None => return None,
    };

    let mut thinking_part = cpa_json::parse_str(r#"{"type":"thinking","thinking":"","signature":""}"#);
    cpa_json::set(&mut thinking_part, "thinking", responses_reasoning_text(item));
    cpa_json::set(&mut thinking_part, "signature", signature);
    Some(thinking_part)
}

/// The payload of an Anthropic redacted_thinking carrier in `encrypted_content`, if it is one.
fn responses_redacted_thinking_data(encrypted_content: &str) -> Option<String> {
    let trimmed = encrypted_content.trim();
    let rest = trimmed.strip_prefix(CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX)?;
    Some(rest.trim().to_string())
}

/// The reasoning text of a Responses item: `summary[]` parts, else `content[]` parts (so a client
/// mirroring the text into both arrays does not replay it twice).
fn responses_reasoning_text(item: &Res<'_>) -> String {
    let text = responses_reasoning_parts_text(&item.g("summary"));
    if !text.is_empty() {
        return text;
    }
    responses_reasoning_parts_text(&item.g("content"))
}

fn responses_reasoning_parts_text(parts: &Res<'_>) -> String {
    if !parts.is_array() {
        return String::new();
    }
    let mut text = String::new();
    for part in parts.array() {
        let t = part.g("text");
        if t.exists() {
            text.push_str(&t.str());
        } else if part.is_string() {
            text.push_str(&part.str());
        }
    }
    text
}

/// The most recent thinking block when the buffered assistant content ends with a server tool
/// result, so the tool_use run that follows keeps a thinking block of its own (upstreams that
/// enforce Anthropic's replay rules reject a tool_use glued onto a web_search_tool_result).
fn claude_thinking_separator_for_tool_use(parts: &[Value]) -> Option<Value> {
    if type_of(parts.last()?) != "web_search_tool_result" {
        return None;
    }
    parts.iter().rev().find(|p| type_of(p) == "thinking").cloned()
}

/// Sets the tool_result `content` from a Responses tool output: converted parts, a lone text
/// collapsed to a string, or the output text.
fn apply_responses_tool_result_content(tool_result: &mut Value, output: &Res<'_>) {
    if output.is_array() {
        let mut parts_json: Vec<Value> = Vec::new();
        let (mut has_image, mut has_file) = (false, false);
        for part in output.array() {
            if let Some(part_json) = convert_responses_content_part_to_claude(&part) {
                match type_of(&part_json).as_str() {
                    "image" => has_image = true,
                    "document" => has_file = true,
                    _ => {}
                }
                parts_json.push(part_json);
            }
        }
        if parts_json.is_empty() {
            cpa_json::set(tool_result, "content", output.raw());
            return;
        }
        if parts_json.len() == 1 && !has_image && !has_file && type_of(&parts_json[0]) == "text" {
            cpa_json::set(tool_result, "content", parts_json[0].g("text").str());
            return;
        }
        cpa_json::delete(tool_result, "content");
        cpa_json::set(tool_result, "content", Value::Array(parts_json));
        return;
    }
    cpa_json::set(tool_result, "content", output.str());
}

/// Reports shapes Anthropic rejects. Descriptive only (logged): `repair_claude_tool_pairing`
/// already fixes the cases it knows about.
fn claude_message_invariant_problems(messages: &[Value]) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(first) = messages.first()
        && first.g("role").str() != "user"
    {
        problems.push("first message is not user".to_string());
    }
    for (i, msg) in messages.iter().enumerate() {
        let role = msg.g("role").str();
        let content = msg.g("content");
        match role.as_str() {
            "assistant" => {
                let tool_use_ids: Vec<String> = content_blocks(&content)
                    .iter()
                    .filter(|b| b.g("type").str() == "tool_use")
                    .map(|b| b.g("id").str())
                    .collect();
                if tool_use_ids.is_empty() {
                    continue;
                }
                if messages.get(i + 1).is_none_or(|next| next.g("role").str() != "user") {
                    problems.push(format!("messages[{i}] tool_use has no following user message"));
                    continue;
                }
                let next = messages[i + 1].g("content");
                let answered: HashSet<String> = content_blocks(&next)
                    .iter()
                    .filter(|b| b.g("type").str() == "tool_result")
                    .map(|b| b.g("tool_use_id").str())
                    .collect();
                for id in &tool_use_ids {
                    if !answered.contains(id) {
                        problems.push(format!("messages[{i}] tool_use {id} has no tool_result in messages[{}]", i + 1));
                    }
                }
            }
            "user" => {
                let previous = if i > 0 { messages[i - 1].g("content") } else { Res::NONE };
                let tool_uses: HashSet<String> = content_blocks(&previous)
                    .iter()
                    .filter(|b| b.g("type").str() == "tool_use")
                    .map(|b| b.g("id").str())
                    .collect();
                let mut leading = true;
                for block in content_blocks(&content) {
                    if block.g("type").str() == "tool_result" {
                        if !leading {
                            problems.push(format!("messages[{i}] tool_result after non-tool_result block"));
                        }
                        if !tool_uses.contains(&block.g("tool_use_id").str()) {
                            problems.push(format!(
                                "messages[{i}] tool_result {} has no tool_use in the previous message",
                                block.g("tool_use_id").str()
                            ));
                        }
                    } else {
                        leading = false;
                    }
                }
            }
            _ => {}
        }
    }
    problems
}

/// gjson `ForEach` over message content: array elements, object values, or the lone scalar.
fn content_blocks<'a>(content: &'a Res<'_>) -> Vec<Res<'a>> {
    let mut blocks = Vec::new();
    content.for_each(|_, v| {
        blocks.push(Res::owned(v.value()));
        true
    });
    blocks
}

/// Enforces the Anthropic invariant that every assistant tool_use is answered by a tool_result at
/// the start of the next user message and every tool_result references a tool_use of the
/// preceding assistant message. Missing results are synthesized as errors and orphan results fold
/// into plain text so the model can carry on.
fn repair_claude_tool_pairing(mut messages: Vec<Value>) -> Vec<Value> {
    if messages.is_empty() {
        return messages;
    }
    let mut prev_tool_use_ids: HashSet<String> = HashSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    for i in 0..messages.len() {
        let role = messages[i].g("role").str();

        if role == "user" {
            // Fold tool_result blocks that do not answer a tool_use of the preceding assistant
            // message into plain text and move the real ones ahead of any other content.
            if let Some(rebuilt) = normalize_claude_tool_result_message(&messages[i], &prev_tool_use_ids) {
                messages[i] = rebuilt;
            }
        }

        out.push(messages[i].clone());

        prev_tool_use_ids = HashSet::new();
        if role == "assistant" {
            let mut tool_use_ids: Vec<String> = Vec::new();
            for block in content_blocks(&messages[i].g("content")) {
                if block.g("type").str() == "tool_use" {
                    let id = block.g("id").str();
                    if !id.is_empty() {
                        tool_use_ids.push(id.clone());
                        prev_tool_use_ids.insert(id);
                    }
                }
            }
            if tool_use_ids.is_empty() {
                continue;
            }

            let has_next_user = messages.get(i + 1).is_some_and(|next| next.g("role").str() == "user");
            let mut answered: HashSet<String> = HashSet::new();
            if has_next_user {
                for block in content_blocks(&messages[i + 1].g("content")) {
                    if block.g("type").str() == "tool_result" {
                        answered.insert(block.g("tool_use_id").str());
                    }
                }
            }

            let mut synthesized: Vec<Value> = Vec::new();
            for id in &tool_use_ids {
                if answered.contains(id) {
                    continue;
                }
                let mut part = cpa_json::parse_str(
                    r#"{"type":"tool_result","tool_use_id":"","is_error":true,"content":"Tool call was interrupted before any output was recorded."}"#,
                );
                cpa_json::set(&mut part, "tool_use_id", id.as_str());
                synthesized.push(part);
            }
            if synthesized.is_empty() {
                continue;
            }

            let mut user_msg = cpa_json::parse_str(r#"{"role":"user","content":[]}"#);
            if has_next_user {
                // Prepend the missing results so tool_result blocks still lead the user message.
                let next_content = messages[i + 1].g("content");
                let mut parts = synthesized;
                if next_content.is_array() {
                    parts.extend(next_content.array().iter().map(Res::value));
                } else if next_content.is_string() {
                    parts.push(text_block(&next_content.str()));
                }
                drop(next_content);
                cpa_json::set(&mut user_msg, "content", Value::Array(parts));
                messages[i + 1] = user_msg;
            } else {
                cpa_json::set(&mut user_msg, "content", Value::Array(synthesized));
                out.push(user_msg);
            }
        }
    }
    out
}

/// Rebuilds a user message so tool_result blocks lead the content and any tool_result that does
/// not answer a tool_use of the preceding assistant message folds into plain text. `None` when
/// nothing changes.
fn normalize_claude_tool_result_message(msg: &Value, answered_ids: &HashSet<String>) -> Option<Value> {
    let content = msg.g("content");
    if !content.is_array() {
        return None;
    }
    let mut result_parts: Vec<Value> = Vec::new();
    let mut other_parts: Vec<Value> = Vec::new();
    let mut seen_other = false;
    let mut changed = false;
    for block in content.array() {
        if block.g("type").str() == "tool_result" {
            if !answered_ids.contains(&block.g("tool_use_id").str()) {
                changed = true;
                seen_other = true;
                let text_parts = tool_result_text_parts(&block);
                if text_parts.is_empty() {
                    // An empty orphan result folds to nothing; keep a marker instead of an
                    // empty user message, which Anthropic also rejects.
                    other_parts.push(text_block("Tool result was empty."));
                } else {
                    other_parts.extend(text_parts);
                }
                continue;
            }
            result_parts.push(block.value());
            if seen_other {
                changed = true;
            }
            continue;
        }
        seen_other = true;
        other_parts.push(block.value());
    }
    if !changed {
        return None;
    }
    result_parts.extend(other_parts);
    let mut user_msg = cpa_json::parse_str(r#"{"role":"user","content":[]}"#);
    cpa_json::set(&mut user_msg, "content", Value::Array(result_parts));
    Some(user_msg)
}

/// Folds an orphan tool_result into plain text parts. Empty text parts are filtered out; nothing
/// visible left returns an empty list so the caller can substitute a marker.
fn tool_result_text_parts(block: &Res<'_>) -> Vec<Value> {
    let content = block.g("content");
    if content.is_array() {
        let mut parts = Vec::new();
        for part in content.array() {
            let mut raw = part.value();
            if part.g("type").str().is_empty() {
                cpa_json::set(&mut raw, "type", "text");
            }
            if content_part_has_visible_content(&raw) {
                parts.push(raw);
            }
        }
        return parts;
    }
    let text = content.str();
    if text.trim().is_empty() {
        return vec![];
    }
    vec![text_block(&text)]
}

/// Renders a tool output that has no matching tool_use as ordinary user content blocks.
fn convert_responses_standalone_tool_output_to_claude_text(output: &Res<'_>) -> Vec<Value> {
    if output.is_array() {
        // Empty text parts are dropped so a mixed array never emits blocks Anthropic rejects;
        // images and documents always count.
        let parts_json: Vec<Value> = output
            .array()
            .iter()
            .filter_map(convert_responses_content_part_to_claude)
            .filter(content_part_has_visible_content)
            .collect();
        if !parts_json.is_empty() {
            return parts_json;
        }
    }
    let text = output.str();
    if output.is_array() || text.trim().is_empty() {
        // An empty standalone output still needs a block so the user message it joins never
        // degenerates to an empty content array. The array guard keeps the raw array dump from
        // leaking in as literal text when every converted part was empty.
        return vec![text_block("Tool result was empty.")];
    }
    vec![text_block(&text)]
}

/// Whether a converted Claude block carries visible payload (non-empty text, image, document).
fn content_part_has_visible_content(part: &Value) -> bool {
    match type_of(part).as_str() {
        "text" => !part.g("text").str().trim().is_empty(),
        _ => true,
    }
}

fn convert_responses_content_part_to_claude(part: &Res<'_>) -> Option<Value> {
    match part.g("type").str().as_str() {
        "input_text" | "output_text" => {
            let t = part.g("text");
            t.exists().then(|| text_block(&t.str()))
        }
        "input_image" => {
            let url = input_image_url(part);
            if url.is_empty() {
                return None;
            }
            image_block(&url)
        }
        "input_file" => {
            let file_data = part.g("file_data").str();
            (!file_data.is_empty()).then(|| document_block(&file_data))
        }
        _ => None,
    }
}

fn convert_responses_tool_descriptor_to_claude(descriptor: &ToolDescriptor, claude_name: &str) -> Option<Value> {
    let override_name =
        if claude_name.is_empty() && !descriptor.direct { descriptor.name.as_str() } else { claude_name };
    match descriptor.tool_type.as_str() {
        "function" => convert_responses_function_tool_to_claude(&descriptor.tool, override_name),
        "custom" => convert_responses_custom_tool_to_claude(&descriptor.tool, override_name),
        "web_search" => convert_responses_web_search_tool_to_claude(&descriptor.tool),
        other => {
            if is_unsupported_openai_builtin_tool_type(other) || descriptor.tool.g("name").str().is_empty() {
                return None;
            }
            Some(descriptor.tool.clone())
        }
    }
}

fn convert_responses_function_tool_to_claude(tool: &Value, override_name: &str) -> Option<Value> {
    let tool_res = Res::of(tool);
    let mut name = override_name.trim().to_string();
    if name.is_empty() {
        name = sanitize_claude_function_name(&responses_tool_name(&tool_res));
    }
    if name.is_empty() {
        return None;
    }

    let mut t_json =
        cpa_json::parse_str(r#"{"name":"","description":"","input_schema":{"type":"object","properties":{}}}"#);
    cpa_json::set(&mut t_json, "name", name);
    let d = responses_tool_description(&tool_res);
    if !d.is_empty() {
        cpa_json::set(&mut t_json, "description", d);
    }
    let schema = normalize_claude_tool_input_schema(responses_tool_parameters(&tool_res).raw().as_bytes());
    cpa_json::set(&mut t_json, "input_schema", cpa_json::parse(&schema));
    t_json = with_cache_control(t_json, &tool_res);
    if !t_json.g("cache_control").exists() {
        t_json = with_cache_control(t_json, &tool_res.g("function"));
    }
    Some(t_json)
}

fn convert_responses_custom_tool_to_claude(tool: &Value, override_name: &str) -> Option<Value> {
    let tool_res = Res::of(tool);
    let mut name = override_name.trim().to_string();
    if name.is_empty() {
        name = sanitize_claude_function_name(&responses_tool_name(&tool_res));
    }
    if name.is_empty() {
        return None;
    }

    let mut t_json = cpa_json::parse_str(
        r#"{"name":"","description":"","input_schema":{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}}"#,
    );
    cpa_json::set(&mut t_json, "name", name);
    let description = responses_tool_description(&tool_res);
    if !description.is_empty() {
        cpa_json::set(&mut t_json, "description", description);
    }
    if applypatch::is_custom_tool(tool) {
        cpa_json::set(&mut t_json, "description", applypatch::description(tool));
        cpa_json::set(&mut t_json, "input_schema", cpa_json::parse(&applypatch::parameters()));
    }
    Some(with_cache_control(t_json, &tool_res))
}

fn convert_responses_web_search_tool_to_claude(tool: &Value) -> Option<Value> {
    let external = tool.g("external_web_access");
    if external.exists() && !external.bool() {
        return None;
    }

    let mut name = tool.g("name").str().trim().to_string();
    if name.is_empty() {
        name = "web_search".to_string();
    }
    let mut t_json = cpa_json::parse_str(r#"{"type":"web_search_20250305","name":""}"#);
    cpa_json::set(&mut t_json, "name", name);
    let max_uses = tool.g("max_uses");
    if max_uses.exists() {
        cpa_json::set(&mut t_json, "max_uses", max_uses.int());
    }
    let allowed_domains = tool.g("filters.allowed_domains");
    if allowed_domains.is_array() {
        cpa_json::set(&mut t_json, "allowed_domains", allowed_domains.value());
    }
    let user_location = tool.g("user_location");
    if user_location.is_object() {
        cpa_json::set(&mut t_json, "user_location", user_location.value());
    }
    Some(t_json)
}

/// Rewrites Codex multi-agent v2 `agent_message` input items into plain user `message` items so
/// the delegated task text survives; encrypted content parts surface as `input_text`.
fn normalize_codex_agent_messages(payload: &[u8]) -> Vec<u8> {
    let mut root = cpa_json::parse(payload);
    let input = root.g("input");
    if !input.is_array() {
        return payload.to_vec();
    }
    // (item index, part indexes with their encrypted text)
    let mut edits: Vec<(usize, Vec<(usize, String)>)> = Vec::new();
    for (item_index, item) in input.array().iter().enumerate() {
        if item.g("type").str().trim() != "agent_message" {
            continue;
        }
        let mut parts = Vec::new();
        let content = item.g("content");
        if content.is_array() {
            for (part_index, part) in content.array().iter().enumerate() {
                if part.g("type").str().trim() != "encrypted_content" {
                    continue;
                }
                let enc = part.g("encrypted_content");
                if !enc.is_string() {
                    continue;
                }
                parts.push((part_index, enc.str()));
            }
        }
        edits.push((item_index, parts));
    }
    drop(input);
    if edits.is_empty() {
        return payload.to_vec();
    }
    for (item_index, parts) in edits {
        let item_path = format!("input.{item_index}");
        for (part_index, text) in parts {
            let part_path = format!("{item_path}.content.{part_index}");
            cpa_json::set(&mut root, &format!("{part_path}.type"), "input_text");
            cpa_json::set(&mut root, &format!("{part_path}.text"), text);
            cpa_json::delete(&mut root, &format!("{part_path}.encrypted_content"));
        }
        cpa_json::set(&mut root, &format!("{item_path}.role"), "user");
        cpa_json::set(&mut root, &format!("{item_path}.type"), "message");
    }
    cpa_json::to_vec(&root)
}

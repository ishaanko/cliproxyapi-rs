//! OpenAI Chat Completions request to Claude Messages request
//! (Go: claude/openai/chat-completions/claude_openai_request.go).

use std::collections::{HashMap, HashSet};

use cpa_core::registry::lookup_model_info;
use cpa_core::thinking::{self, level};
use cpa_core::util::{normalize_claude_tool_input_schema, sanitize_claude_function_name, sanitize_claude_tool_id};
use cpa_json::{J, Kind, Res, Value};

use crate::common;

/// Transforms an OpenAI Chat Completions request into a Claude Code API request.
pub fn convert_openai_request_to_claude(model_name: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    convert(model_name, raw, stream, false)
}

/// Like [`convert_openai_request_to_claude`], but keeps assistant `reasoning_content` as an
/// unsigned thinking block for configured compatibility endpoints.
pub fn convert_openai_request_to_claude_with_compat(model_name: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    convert(model_name, raw, stream, true)
}

/// `{"type":"text","text":<text>}` as JSON bytes.
fn text_block(text: &str) -> Vec<u8> {
    let mut v = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
    cpa_json::set(&mut v, "text", text);
    cpa_json::to_vec(&v)
}

fn set_bytes(raw: &[u8], path: &str, val: impl Into<Value>) -> Vec<u8> {
    let mut v = cpa_json::parse(raw);
    cpa_json::set(&mut v, path, val);
    cpa_json::to_vec(&v)
}

fn set_choice(out: &mut Value, json: &str) {
    cpa_json::set(out, "tool_choice", cpa_json::parse_str(json));
}

/// Writes Claude `thinking` / `output_config.effort` for an OpenAI reasoning effort string.
/// Adaptive models (those with thinking levels) get `thinking.type` plus `output_config.effort`;
/// other models get a manual `budget_tokens`. Shared with the Responses translator.
pub(crate) fn apply_reasoning_effort(out: &mut Value, model_name: &str, effort_raw: &str) {
    let mut effort = effort_raw.trim().to_lowercase();
    if effort.is_empty() {
        return;
    }
    let mi = lookup_model_info(model_name, Some("claude"));
    let levels = mi.as_ref().and_then(|m| m.thinking.as_ref()).map(|t| t.levels.as_slice()).unwrap_or(&[]);
    let supports_adaptive = !levels.is_empty();
    let supports_max = supports_adaptive && thinking::has_level(levels, level::MAX);

    if supports_adaptive {
        match effort.as_str() {
            "none" => {
                cpa_json::set(out, "thinking.type", "disabled");
                cpa_json::delete(out, "thinking.budget_tokens");
                cpa_json::delete(out, "output_config.effort");
            }
            "auto" => {
                cpa_json::set(out, "thinking.type", "adaptive");
                cpa_json::delete(out, "thinking.budget_tokens");
                cpa_json::delete(out, "output_config.effort");
            }
            _ => {
                if let Some(mapped) = thinking::map_to_claude_effort(&effort, supports_max) {
                    effort = mapped.to_string();
                }
                cpa_json::set(out, "thinking.type", "adaptive");
                cpa_json::delete(out, "thinking.budget_tokens");
                cpa_json::set(out, "output_config.effort", effort);
            }
        }
    } else if let Some(budget) = thinking::convert_level_to_budget(&effort) {
        // Legacy/manual thinking (budget_tokens).
        match budget {
            0 => {
                cpa_json::set(out, "thinking.type", "disabled");
            }
            -1 => {
                cpa_json::set(out, "thinking.type", "enabled");
            }
            b if b > 0 => {
                cpa_json::set(out, "thinking.type", "enabled");
                cpa_json::set(out, "thinking.budget_tokens", b);
            }
            _ => {}
        }
    }
}

fn convert(model_name: &str, raw: &[u8], stream: bool, preserve_empty_thinking_blocks: bool) -> Vec<u8> {
    let user_id = common::derive_claude_user_id(raw);

    // Base template with the default max_tokens.
    let mut out = cpa_json::parse_str(r#"{"model":"","max_tokens":32000,"messages":[],"metadata":{}}"#);
    cpa_json::set(&mut out, "metadata.user_id", user_id);

    let root = cpa_json::parse(raw);

    // reasoning_effort -> Claude thinking config.
    let v = root.g("reasoning_effort");
    if v.exists() {
        apply_reasoning_effort(&mut out, model_name, &v.str());
    }

    cpa_json::set(&mut out, "model", model_name);

    // max_tokens, falling back to the newer max_completion_tokens spelling.
    let max_tokens = first_existing([root.g("max_tokens"), root.g("max_completion_tokens")]);
    if max_tokens.exists() {
        cpa_json::set(&mut out, "max_tokens", max_tokens.int());
    }

    let top_p = root.g("top_p");
    if top_p.exists() {
        cpa_json::set(&mut out, "top_p", cpa_json::num_f64(top_p.float()));
    }

    let stop = root.g("stop");
    if stop.exists() {
        if stop.is_array() {
            let seqs: Vec<Value> = stop.array().iter().map(|s| Value::String(s.str())).collect();
            if !seqs.is_empty() {
                cpa_json::set(&mut out, "stop_sequences", Value::Array(seqs));
            }
        } else {
            cpa_json::set(&mut out, "stop_sequences", Value::Array(vec![Value::String(stop.str())]));
        }
    }

    cpa_json::set(&mut out, "stream", stream);

    let mut system_blocks: Vec<Vec<u8>> = Vec::new();
    let mut message_blocks: Vec<Vec<u8>> = Vec::new();

    let messages = root.g("messages");
    if messages.is_array() {
        let msgs = messages.array();
        // Duplicate tool results for one tool_call_id collapse onto the first position but use
        // the content of the last message.
        let mut last_tool_message: HashMap<String, &Res<'_>> = HashMap::new();
        for message in &msgs {
            if message.g("role").str() == "tool" {
                let raw_id = message.g("tool_call_id").str();
                if !raw_id.is_empty() {
                    last_tool_message.insert(raw_id, message);
                }
            }
        }
        let mut emitted_tool_results: HashSet<String> = HashSet::new();

        let mut accumulator = common::ClaudeMessageAccumulator::new(msgs.len());
        for message in &msgs {
            let role = message.g("role").str();
            let content = message.g("content");

            match role.as_str() {
                // Developer messages rank with system messages, so both become top-level
                // Claude system blocks.
                "system" | "developer" => {
                    let system_start = system_blocks.len();
                    if content.as_str().is_some_and(|s| !s.is_empty()) {
                        let text_part = text_block(&content.str());
                        system_blocks.push(common::attach_cache_control(&text_part, message));
                    } else if content.is_array() {
                        for part in content.array() {
                            if part.g("type").str() == "text" {
                                let text_part = text_block(&part.g("text").str());
                                system_blocks.push(common::attach_cache_control(&text_part, &part));
                            }
                        }
                        // Message-level cache_control applies to the last system block from this message.
                        if message.g("cache_control").exists() && system_blocks.len() > system_start {
                            let last = system_blocks.len() - 1;
                            if !cpa_json::parse(&system_blocks[last]).g("cache_control").exists() {
                                system_blocks[last] = common::attach_cache_control(&system_blocks[last], message);
                            }
                        }
                    }
                }
                "user" | "assistant" => {
                    let mut content_blocks: Vec<Vec<u8>> = Vec::with_capacity(4);
                    if preserve_empty_thinking_blocks && role == "assistant" {
                        let rc = message.g("reasoning_content");
                        if rc.as_str().is_some_and(|s| !s.trim().is_empty()) {
                            let mut part = cpa_json::parse_str(r#"{"type":"thinking","thinking":"","signature":""}"#);
                            cpa_json::set(&mut part, "thinking", rc.str());
                            content_blocks.push(cpa_json::to_vec(&part));
                        }
                    }

                    if content.as_str().is_some_and(|s| !s.is_empty()) {
                        content_blocks.push(text_block(&content.str()));
                    } else if content.is_array() {
                        for part in content.array() {
                            if let Some(claude_part) = convert_content_part(&part) {
                                content_blocks.push(claude_part);
                            }
                        }
                    }

                    // Tool calls (assistant only).
                    let tool_calls = message.g("tool_calls");
                    if tool_calls.is_array() && role == "assistant" {
                        for tool_call in tool_calls.array() {
                            if tool_call.g("type").str() != "function" {
                                continue;
                            }
                            let mut tool_call_id = tool_call.g("id").str();
                            if tool_call_id.is_empty() {
                                tool_call_id = common::generate_claude_tool_call_id();
                            }
                            let tool_call_id = sanitize_claude_tool_id(&tool_call_id);

                            let function = tool_call.g("function");
                            let mut tool_use =
                                cpa_json::parse_str(r#"{"type":"tool_use","id":"","name":"","input":{}}"#);
                            cpa_json::set(&mut tool_use, "id", tool_call_id);
                            cpa_json::set(
                                &mut tool_use,
                                "name",
                                sanitize_claude_function_name(&function.g("name").str()),
                            );

                            // Arguments are kept only when they are a valid JSON object.
                            let args = function.g("arguments");
                            if args.exists() {
                                let args_str = args.str();
                                if !args_str.is_empty() && cpa_json::valid(args_str.as_bytes()) {
                                    let args_json = cpa_json::parse_str(&args_str);
                                    if args_json.is_object() {
                                        cpa_json::set(&mut tool_use, "input", args_json);
                                    }
                                }
                            }
                            content_blocks.push(cpa_json::to_vec(&tool_use));
                        }
                    }

                    let mut msg = cpa_json::parse_str(r#"{"role":"","content":[]}"#);
                    cpa_json::set(&mut msg, "role", role.as_str());
                    cpa_json::set(&mut msg, "content", cpa_json::parse(&common::join_raw_array(&content_blocks)));
                    let msg = common::attach_message_cache_control(&cpa_json::to_vec(&msg), message);
                    accumulator.append(&msg);
                }
                "tool" => {
                    let raw_id = message.g("tool_call_id").str();
                    let tool_call_id = sanitize_claude_tool_id(&raw_id);
                    if !raw_id.is_empty() && !emitted_tool_results.insert(raw_id.clone()) {
                        continue;
                    }

                    let target = if raw_id.is_empty() {
                        message
                    } else {
                        last_tool_message.get(&raw_id).copied().unwrap_or(message)
                    };
                    let tool_content = target.g("content");

                    let mut msg = cpa_json::parse_str(
                        r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"","content":""}]}"#,
                    );
                    cpa_json::set(&mut msg, "content.0.tool_use_id", tool_call_id);
                    cpa_json::set(&mut msg, "content.0.content", convert_tool_result_content(&tool_content));
                    // Anthropic rejects cache_control inside tool_result.content, so it is
                    // hoisted onto the tool_result block itself.
                    let msg = common::attach_tool_message_cache_control(&cpa_json::to_vec(&msg), target);
                    accumulator.append(&msg);
                }
                _ => {}
            }
        }

        message_blocks = accumulator.messages();
    }

    let format_instruction =
        structured_output_instruction(raw, "response_format", &root.g("response_format"));
    if !format_instruction.is_empty() {
        system_blocks.push(text_block(&format_instruction));
    }

    // System-only inputs keep a minimal conversational turn.
    if message_blocks.is_empty() && !system_blocks.is_empty() {
        message_blocks.push(br#"{"role":"user","content":[{"type":"text","text":""}]}"#.to_vec());
    }

    if !system_blocks.is_empty() {
        cpa_json::set(&mut out, "system", cpa_json::parse(&common::join_raw_array(&system_blocks)));
    }
    if !message_blocks.is_empty() {
        cpa_json::set(&mut out, "messages", cpa_json::parse(&common::join_raw_array(&message_blocks)));
    }

    // Tools: OpenAI tools -> Claude tools.
    let mut allowed_tool_names: HashSet<String> = HashSet::new();
    let mut is_allowed_tools = false;
    let mut allowed_mode = "auto".to_string();
    let tool_choice = root.g("tool_choice");
    if tool_choice.is_object() && tool_choice.g("type").str() == "allowed_tools" {
        is_allowed_tools = true;
        let nested = tool_choice.g("allowed_tools.tools");
        let flat = tool_choice.g("tools");
        let mut tool_list = nested.array();
        if tool_list.is_empty() {
            tool_list = flat.array();
        }
        for t in &tool_list {
            let mut fn_name = t.g("function.name").str().trim().to_string();
            if fn_name.is_empty() {
                fn_name = t.g("name").str().trim().to_string();
            }
            if !fn_name.is_empty() {
                allowed_tool_names.insert(sanitize_claude_function_name(&fn_name));
                allowed_tool_names.insert(fn_name);
            }
        }
        let mut mode_val = tool_choice.g("allowed_tools.mode").str().trim().to_lowercase();
        if mode_val.is_empty() {
            mode_val = tool_choice.g("mode").str().trim().to_lowercase();
        }
        if !mode_val.is_empty() {
            allowed_mode = mode_val;
        }
    }

    let mut anthropic_tools: Vec<Vec<u8>> = Vec::new();
    let tools = root.g("tools");
    if tools.is_array() && !tools.array().is_empty() {
        for tool in tools.array() {
            if tool.g("type").str() != "function" {
                continue;
            }
            let function = tool.g("function");
            let fn_name = function.g("name").str();
            let sanitized_fn_name = sanitize_claude_function_name(&fn_name);
            if is_allowed_tools
                && !allowed_tool_names.contains(&fn_name)
                && !allowed_tool_names.contains(&sanitized_fn_name)
            {
                continue;
            }
            let mut anthropic_tool = cpa_json::parse_str(r#"{"name":"","description":""}"#);
            cpa_json::set(&mut anthropic_tool, "name", sanitized_fn_name);
            cpa_json::set(&mut anthropic_tool, "description", function.g("description").str());

            let mut parameters = function.g("parameters");
            if !parameters.exists() {
                parameters = function.g("parametersJsonSchema");
            }
            let schema_raw = if parameters.exists() { parameters.raw().into_bytes() } else { Vec::new() };
            let schema = normalize_claude_tool_input_schema(&schema_raw);
            cpa_json::set(&mut anthropic_tool, "input_schema", cpa_json::parse(&schema));

            let mut tool_bytes = common::attach_cache_control(&cpa_json::to_vec(&anthropic_tool), &tool);
            if !cpa_json::parse(&tool_bytes).g("cache_control").exists() {
                tool_bytes = common::attach_cache_control(&tool_bytes, &function);
            }
            let mut strict = function.g("strict");
            if !strict.exists() {
                strict = tool.g("strict");
            }
            match strict.kind() {
                Kind::True => tool_bytes = set_bytes(&tool_bytes, "strict", true),
                Kind::False => tool_bytes = set_bytes(&tool_bytes, "strict", false),
                _ => {}
            }
            anthropic_tools.push(tool_bytes);
        }

        if anthropic_tools.is_empty() {
            cpa_json::delete(&mut out, "tools");
        } else {
            cpa_json::set(&mut out, "tools", cpa_json::parse(&common::join_raw_array(&anthropic_tools)));
        }
    }

    // Tool choice mapping.
    if is_allowed_tools {
        let choice = if anthropic_tools.is_empty() {
            r#"{"type":"none"}"#
        } else if allowed_mode == "required" {
            r#"{"type":"any"}"#
        } else {
            r#"{"type":"auto"}"#
        };
        set_choice(&mut out, choice);
    } else if tool_choice.exists() && !tool_choice.is_null() {
        match tool_choice.kind() {
            Kind::String => match tool_choice.str().as_str() {
                "none" => set_choice(&mut out, r#"{"type":"none"}"#),
                "auto" => set_choice(&mut out, r#"{"type":"auto"}"#),
                "required" => set_choice(&mut out, r#"{"type":"any"}"#),
                _ => {}
            },
            Kind::Json => match tool_choice.g("type").str().as_str() {
                "none" => set_choice(&mut out, r#"{"type":"none"}"#),
                "auto" => set_choice(&mut out, r#"{"type":"auto"}"#),
                "required" | "any" => set_choice(&mut out, r#"{"type":"any"}"#),
                "function" => {
                    let mut function_name = tool_choice.g("function.name").str();
                    if function_name.is_empty() {
                        function_name = tool_choice.g("name").str();
                    }
                    if function_name.is_empty() {
                        set_choice(&mut out, r#"{"type":"none"}"#);
                    } else {
                        let mut choice = cpa_json::parse_str(r#"{"type":"tool","name":""}"#);
                        cpa_json::set(&mut choice, "name", sanitize_claude_function_name(&function_name));
                        cpa_json::set(&mut out, "tool_choice", choice);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    if root.g("parallel_tool_calls").kind() == Kind::False {
        if out.g("tool_choice").exists() {
            if out.g("tool_choice.type").str() != "none" {
                cpa_json::set(&mut out, "tool_choice.disable_parallel_tool_use", true);
            }
        } else if out.g("tools").exists() {
            set_choice(&mut out, r#"{"type":"auto","disable_parallel_tool_use":true}"#);
        }
    }

    thinking::apply_translated_summary_to_claude(&cpa_json::to_vec(&out), raw, "openai", model_name)
}

/// Converts an OpenAI content part to a Claude block without cache_control; `None` when the part
/// type is unsupported or its data is unusable.
fn convert_content_part_raw(part: &Res<'_>) -> Option<Vec<u8>> {
    match part.g("type").str().as_str() {
        "text" => Some(text_block(&part.g("text").str())),
        "image_url" => convert_image_url(&part.g("image_url.url").str()),
        "file" => {
            let file_data = part.g("file.file_data").str();
            if file_data.starts_with("data:")
                && let (Some(semicolon), Some(comma)) = (file_data.find(';'), file_data.find(','))
                && comma > semicolon
            {
                let media_type = file_data[..semicolon].strip_prefix("data:").unwrap_or(&file_data[..semicolon]);
                let mut doc =
                    cpa_json::parse_str(r#"{"type":"document","source":{"type":"base64","media_type":"","data":""}}"#);
                cpa_json::set(&mut doc, "source.media_type", media_type);
                cpa_json::set(&mut doc, "source.data", &file_data[comma + 1..]);
                return Some(cpa_json::to_vec(&doc));
            }
            None
        }
        _ => None,
    }
}

/// [`convert_content_part_raw`] plus the part's own cache_control.
fn convert_content_part(part: &Res<'_>) -> Option<Vec<u8>> {
    let claude_part = convert_content_part_raw(part)?;
    Some(common::attach_cache_control(&claude_part, part))
}

fn convert_image_url(image_url: &str) -> Option<Vec<u8>> {
    if image_url.is_empty() {
        return None;
    }
    if image_url.starts_with("data:") {
        let (head, data) = image_url.split_once(',')?;
        let media_type_part = head.split(';').next().unwrap_or(head);
        let mut media_type = media_type_part.strip_prefix("data:").unwrap_or(media_type_part);
        if media_type.is_empty() {
            media_type = "application/octet-stream";
        }
        let mut image = cpa_json::parse_str(r#"{"type":"image","source":{"type":"base64","media_type":"","data":""}}"#);
        cpa_json::set(&mut image, "source.media_type", media_type);
        cpa_json::set(&mut image, "source.data", data);
        return Some(cpa_json::to_vec(&image));
    }
    let mut image = cpa_json::parse_str(r#"{"type":"image","source":{"type":"url","url":""}}"#);
    cpa_json::set(&mut image, "source.url", image_url);
    Some(cpa_json::to_vec(&image))
}

/// The Claude `tool_result.content` for an OpenAI tool message content: a string, or an array of
/// Claude parts when the content converts. Absent content becomes an empty string.
fn convert_tool_result_content(content: &Res<'_>) -> Value {
    if !content.exists() {
        return Value::String(String::new());
    }
    if let Some(s) = content.as_str() {
        return Value::String(s.to_string());
    }
    if content.is_array() {
        let items = content.array();
        let mut claude_parts: Vec<Vec<u8>> = Vec::with_capacity(4);
        for part in &items {
            if let Some(s) = part.as_str() {
                claude_parts.push(text_block(s));
            } else if let Some(p) = convert_content_part_raw(part) {
                claude_parts.push(p);
            }
        }
        if !claude_parts.is_empty() || items.is_empty() {
            return cpa_json::parse(&common::join_raw_array(&claude_parts));
        }
    } else if content.is_object()
        && let Some(p) = convert_content_part_raw(content)
    {
        return cpa_json::parse(&common::join_raw_array(&[p]));
    }
    Value::String(content.raw())
}

/// The first result that exists, or a missing result.
fn first_existing<'a>(values: impl IntoIterator<Item = Res<'a>>) -> Res<'a> {
    values.into_iter().find(Res::exists).unwrap_or(Res::NONE)
}

/// The structured output instruction (Go: `common.BuildClaudeStructuredOutputInstruction`) with
/// the schema embedded as the client sent it. `format` is the value at `format_path` of `body`.
pub(crate) fn structured_output_instruction(body: &[u8], format_path: &str, format: &Res<'_>) -> String {
    let schema_path = if format.g("json_schema.schema").exists() { "json_schema.schema" } else { "schema" };
    let schema_raw = cpa_json::raw_at(body, &format!("{format_path}.{schema_path}"));
    common::build_claude_structured_output_instruction(format, schema_raw)
}

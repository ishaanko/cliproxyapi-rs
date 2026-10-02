//! OpenAI Chat Completions request -> Codex Responses request (Go: codex_openai_request.go).

use std::collections::{HashMap, HashSet};

use cpa_core::applypatch;
use cpa_json::{J, Res, Value, json};

use crate::codex::raw::raw_at;
use crate::codex::util::{build_short_name_map, truncate_bytes};

/// One assistant tool call awaiting its `tool` message.
struct PendingToolCall {
    call_id: String,
    source_call_id: String,
    call_type: &'static str,
    consumed: bool,
}

/// Go: ConvertOpenAIRequestToCodex.
pub fn convert_openai_request_to_codex(model_name: &str, raw_json: &[u8], stream: bool) -> Vec<u8> {
    let root = cpa_json::parse(raw_json);
    let tools = root.g("tools");
    let tool_results = tools.array();
    let mut out = cpa_json::parse_str(r#"{"instructions":""}"#);

    cpa_json::set(&mut out, "stream", stream);

    // Codex does not support temperature, top_p, top_k or token limits, so they are dropped.
    let effort = root.g("reasoning_effort");
    if effort.exists() {
        cpa_json::set(&mut out, "reasoning.effort", effort.value());
    } else {
        cpa_json::set(&mut out, "reasoning.effort", "medium");
    }
    if let Some(tier) = normalize_service_tier(&root.g("service_tier")) {
        cpa_json::set(&mut out, "service_tier", tier);
    }
    cpa_json::set(&mut out, "parallel_tool_calls", true);
    // reasoning.summary is left to the source request's canonical summary intent.
    cpa_json::set(&mut out, "include", json!(["reasoning.encrypted_content"]));
    cpa_json::set(&mut out, "model", model_name);

    // Request-local tool metadata and name shortening map.
    let mut custom_tool_names: HashSet<String> = HashSet::new();
    let mut function_tool_names: HashSet<String> = HashSet::new();
    let mut original_tool_name_map: HashMap<String, String> = HashMap::new();
    if tools.is_array() && !tool_results.is_empty() {
        for tool in &tool_results {
            match tool.g("type").str().as_str() {
                "function" => {
                    function_tool_names.insert(tool.g("function.name").str());
                }
                "custom" => {
                    custom_tool_names.insert(tool.g("name").str());
                }
                _ => {}
            }
        }
        // A normalized function envelope cannot disambiguate declarations that share a name, so
        // ambiguous names keep function behavior.
        for name in &function_tool_names {
            custom_tool_names.remove(name);
        }
    }
    let all_names = collect_request_tool_names(&root);
    if !all_names.is_empty() {
        original_tool_name_map = build_short_name_map_for(&all_names);
    }
    let short_name = |name: &str| -> String {
        original_tool_name_map
            .get(name)
            .cloned()
            .unwrap_or_else(|| shorten_name_if_needed(name))
    };

    // Returns (call type, name, input) for function and custom calls; None for anything else.
    let resolve_tool_call = |tool_call: &Res<'_>| -> Option<(&'static str, String, String)> {
        match tool_call.g("type").str().as_str() {
            "custom" => Some((
                "custom",
                tool_call.g("custom.name").str(),
                tool_call.g("custom.input").str(),
            )),
            "function" => {
                let name = tool_call.g("function.name").str();
                let call_type = if custom_tool_names.contains(&name) {
                    "custom"
                } else {
                    "function"
                };
                let mut input = tool_call.g("function.arguments").str();
                if call_type == "custom"
                    && name.trim() == "apply_patch"
                    // Only normalized function history carries the JSON envelope; explicit
                    // custom input is raw.
                    && let Ok(unwrapped) = applypatch::unwrap_input(&input)
                {
                    input = unwrapped;
                }
                Some((call_type, name, input))
            }
            _ => None,
        }
    };

    let messages = root.g("messages");
    let mut pending_tool_calls: Vec<PendingToolCall> = Vec::new();
    let mut ambiguous_tool_call_ids: HashSet<String> = HashSet::new();

    let mut input_items: Vec<Value> = Vec::new();
    if messages.is_array() {
        for (i, m) in messages.array().iter().enumerate() {
            let role = m.g("role").str();

            if role == "tool" {
                // Tool responses become top-level tool call output items.
                let mut tool_call_id = m.g("tool_call_id").str();
                if !tool_call_id.is_empty() && ambiguous_tool_call_ids.contains(&tool_call_id) {
                    continue;
                }
                let pending_index = pending_tool_calls.iter().position(|p| {
                    !p.consumed
                        && (tool_call_id.is_empty()
                            || p.source_call_id == tool_call_id
                            || p.call_id == tool_call_id)
                });
                let Some(pending_index) = pending_index else {
                    continue;
                };
                let pending = &mut pending_tool_calls[pending_index];
                pending.consumed = true;
                tool_call_id = pending.call_id.clone();
                let output_type = if pending.call_type == "custom" {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };

                let mut tool_output = json!({"type": output_type, "call_id": tool_call_id});
                set_tool_call_output_content(
                    &mut tool_output,
                    &m.g("content"),
                    raw_json,
                    &format!("messages.{i}.content"),
                );
                input_items.push(tool_output);
                continue;
            }

            // A new conversational message starts a new tool-call batch.
            pending_tool_calls.clear();
            ambiguous_tool_call_ids.clear();

            let mut msg = json!({"type": "message"});
            cpa_json::set(
                &mut msg,
                "role",
                if role == "system" {
                    "developer"
                } else {
                    role.as_str()
                },
            );

            let mut content_items: Vec<Value> = Vec::new();
            let c = m.g("content");
            let text_part_type = if role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };
            if c.exists() && c.is_string() && !c.str().is_empty() {
                content_items.push(json!({"type": text_part_type, "text": c.str()}));
            } else if c.exists() && c.is_array() {
                for it in c.array() {
                    match it.g("type").str().as_str() {
                        "text" => content_items
                            .push(json!({"type": text_part_type, "text": it.g("text").str()})),
                        // Image, file and audio inputs are user-only.
                        "image_url" if role == "user" => {
                            let mut part = json!({"type": "input_image"});
                            let u = it.g("image_url.url");
                            if u.exists() {
                                cpa_json::set(&mut part, "image_url", u.str());
                            }
                            content_items.push(part);
                        }
                        "file" if role == "user" => {
                            let file_data = it.g("file.file_data").str();
                            let filename = it.g("file.filename").str();
                            if !file_data.is_empty() {
                                let mut part =
                                    json!({"type": "input_file", "file_data": file_data});
                                if !filename.is_empty() {
                                    cpa_json::set(&mut part, "filename", filename);
                                }
                                content_items.push(part);
                            }
                        }
                        "input_audio" if role == "user" => {
                            let data = it.g("input_audio.data").str();
                            let format = it.g("input_audio.format").str();
                            if !data.is_empty() {
                                let mut part = json!({"type": "input_audio", "data": data});
                                if !format.is_empty() {
                                    cpa_json::set(&mut part, "format", format);
                                }
                                content_items.push(part);
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Don't emit empty assistant messages when only tool_calls are present: the
            // Responses API needs function_call items directly or call_id matching fails.
            if role != "assistant" || !content_items.is_empty() {
                cpa_json::set(&mut msg, "content", Value::Array(content_items));
                input_items.push(msg);
            }

            // Assistant tool calls become separate top-level items.
            if role != "assistant" {
                continue;
            }
            let tool_calls = m.g("tool_calls");
            if !(tool_calls.exists() && tool_calls.is_array()) {
                continue;
            }
            let tool_calls_arr = tool_calls.array();
            let mut call_id_counts: HashMap<String, usize> = HashMap::new();
            let mut used_call_ids: HashSet<String> = HashSet::new();
            for tc in &tool_calls_arr {
                let valid = resolve_tool_call(tc).is_some();
                let call_id = tc.g("id").str();
                if valid && !call_id.is_empty() {
                    *call_id_counts.entry(call_id.clone()).or_default() += 1;
                    used_call_ids.insert(call_id);
                }
            }
            for (call_id, count) in call_id_counts {
                if count > 1 {
                    ambiguous_tool_call_ids.insert(call_id);
                }
            }

            for (j, tc) in tool_calls_arr.iter().enumerate() {
                let Some((call_type, call_name, call_input)) = resolve_tool_call(tc) else {
                    continue;
                };
                let source_call_id = tc.g("id").str();
                if !source_call_id.is_empty() && ambiguous_tool_call_ids.contains(&source_call_id) {
                    continue;
                }
                let mut call_id = source_call_id.clone();
                if call_id.is_empty() {
                    let base = format!("call_missing_{i}_{j}");
                    call_id = base.clone();
                    let mut suffix = 1;
                    while used_call_ids.contains(&call_id) {
                        call_id = format!("{base}_{suffix}");
                        suffix += 1;
                    }
                    used_call_ids.insert(call_id.clone());
                }
                pending_tool_calls.push(PendingToolCall {
                    call_id: call_id.clone(),
                    source_call_id,
                    call_type,
                    consumed: false,
                });

                let name = short_name(&call_name);
                if call_type == "function" {
                    input_items.push(json!({"type": "function_call", "call_id": call_id, "name": name, "arguments": call_input}));
                } else {
                    input_items.push(json!({"type": "custom_tool_call", "call_id": call_id, "name": name, "input": call_input}));
                }
            }
        }
    }
    cpa_json::set(&mut out, "input", Value::Array(input_items));

    // response_format and text settings -> Responses text.format.
    let rf = root.g("response_format");
    let text = root.g("text");
    if rf.exists() {
        if !out.g("text").exists() {
            cpa_json::set(&mut out, "text", json!({}));
        }
        match rf.g("type").str().as_str() {
            "text" => {
                cpa_json::set(&mut out, "text.format.type", "text");
            }
            "json_schema" => {
                let js = rf.g("json_schema");
                if js.exists() {
                    cpa_json::set(&mut out, "text.format.type", "json_schema");
                    let v = js.g("name");
                    if v.exists() {
                        cpa_json::set(&mut out, "text.format.name", v.value());
                    }
                    let v = js.g("strict");
                    if v.exists() {
                        cpa_json::set(&mut out, "text.format.strict", v.value());
                    }
                    let v = js.g("schema");
                    if v.exists() {
                        cpa_json::set(&mut out, "text.format.schema", v.value());
                    }
                }
            }
            _ => {}
        }
        if text.exists() {
            let v = text.g("verbosity");
            if v.exists() {
                cpa_json::set(&mut out, "text.verbosity", v.value());
            }
        }
    } else if text.exists() {
        let v = text.g("verbosity");
        if v.exists() {
            if !out.g("text").exists() {
                cpa_json::set(&mut out, "text", json!({}));
            }
            cpa_json::set(&mut out, "text.verbosity", v.value());
        }
    }

    // Tools: flatten function fields.
    if tools.is_array() && !tool_results.is_empty() {
        let mut tool_items: Vec<Value> = Vec::with_capacity(tool_results.len());
        for t in &tool_results {
            let tool_type = t.g("type").str();
            if tool_type == "custom" {
                let mut item = t.value();
                cpa_json::set(&mut item, "name", short_name(&t.g("name").str()));
                tool_items.push(item);
                continue;
            }
            // Built-in tools (e.g. {"type":"web_search"}) pass through; only function and
            // custom tools need structural conversion.
            if !tool_type.is_empty() && tool_type != "function" && t.is_object() {
                tool_items.push(t.value());
                continue;
            }
            if tool_type == "function" {
                let mut item = json!({"type": "function"});
                let f = t.g("function");
                if f.exists() {
                    let v = f.g("name");
                    if v.exists() {
                        cpa_json::set(&mut item, "name", short_name(&v.str()));
                    }
                    let v = f.g("description");
                    if v.exists() {
                        cpa_json::set(&mut item, "description", v.value());
                    }
                    let v = f.g("parameters");
                    if v.exists() {
                        cpa_json::set(&mut item, "parameters", v.value());
                    }
                    let v = f.g("strict");
                    if v.exists() {
                        cpa_json::set(&mut item, "strict", v.value());
                    } else {
                        // Chat Completions defaults strict to false while Responses defaults
                        // to true, so an omitted value must be forwarded explicitly.
                        cpa_json::set(&mut item, "strict", false);
                    }
                }
                tool_items.push(item);
            }
        }
        cpa_json::set(&mut out, "tools", Value::Array(tool_items));
    }

    // tool_choice: strings pass through; named choices flatten to {"type","name"}.
    let tc = root.g("tool_choice");
    if tc.exists() {
        if tc.is_string() {
            cpa_json::set(&mut out, "tool_choice", tc.str());
        } else if tc.is_object() {
            let mut tc_type = tc.g("type").str();
            if tc_type == "function" || tc_type == "custom" {
                let mut name = tc.g("name").str();
                if tc_type == "function" {
                    name = tc.g("function.name").str();
                    if custom_tool_names.contains(&name) {
                        tc_type = "custom".into();
                    }
                }
                if !name.is_empty() {
                    name = short_name(&name);
                }
                let mut choice = json!({});
                cpa_json::set(&mut choice, "type", tc_type);
                if !name.is_empty() {
                    cpa_json::set(&mut choice, "name", name);
                }
                cpa_json::set(&mut out, "tool_choice", choice);
            } else if !tc_type.is_empty() {
                // Built-in tool choices are already Responses-compatible.
                cpa_json::set(&mut out, "tool_choice", tc.value());
            }
        }
    }

    cpa_json::set(&mut out, "store", false);
    cpa_json::to_vec(&out)
}

/// Sets `output` of a tool output item from a Chat Completions tool message `content`. `src` and
/// `path` locate `content` in the source text for the verbatim fallbacks.
fn set_tool_call_output_content(
    func_output: &mut Value,
    content: &Res<'_>,
    src: &[u8],
    path: &str,
) {
    if content.is_string() {
        // A string holding a JSON array with image parts is unpacked into content parts.
        let s = content.str();
        let structured = cpa_json::parse_str(&s);
        if has_tool_output_image_part(&Res::of(&structured)) {
            set_tool_call_output_content(func_output, &Res::of(&structured), s.as_bytes(), "");
            return;
        }
        cpa_json::set(func_output, "output", s);
    } else if content.is_array() {
        let items: Vec<Value> = content
            .array()
            .iter()
            .enumerate()
            .map(|(k, item)| tool_output_content_part(item, src, &join_path(path, k)))
            .collect();
        cpa_json::set(func_output, "output", Value::Array(items));
    } else {
        let mut fallback = if content.exists() {
            raw_at(src, path).unwrap_or_else(|| content.raw())
        } else {
            String::new()
        };
        if fallback.is_empty() {
            fallback = content.str();
        }
        cpa_json::set(func_output, "output", fallback);
    }
}

fn join_path(path: &str, index: usize) -> String {
    if path.is_empty() {
        index.to_string()
    } else {
        format!("{path}.{index}")
    }
}

fn tool_output_content_part(item: &Res<'_>, src: &[u8], path: &str) -> Value {
    let item_type = item.g("type").str();
    match item_type.as_str() {
        "text" | "input_text" | "output_text" => {
            json!({"type": "input_text", "text": item.g("text").str()})
        }
        "image_url" | "input_image" => {
            let input_image = item_type == "input_image";
            let (image_url, file_id) = if input_image {
                (item.g("image_url").str(), item.g("file_id").str())
            } else {
                (
                    item.g("image_url.url").str(),
                    item.g("image_url.file_id").str(),
                )
            };
            if image_url.is_empty() && file_id.is_empty() {
                return tool_output_fallback_part(item, src, path);
            }
            let mut part = json!({"type": "input_image"});
            if !image_url.is_empty() {
                cpa_json::set(&mut part, "image_url", image_url);
            }
            if !file_id.is_empty() {
                cpa_json::set(&mut part, "file_id", file_id);
            }
            let detail = if input_image {
                item.g("detail").str()
            } else {
                item.g("image_url.detail").str()
            };
            if !detail.is_empty() {
                cpa_json::set(&mut part, "detail", detail);
            }
            part
        }
        "file" => {
            let file_id = item.g("file.file_id").str();
            let file_data = item.g("file.file_data").str();
            let file_url = item.g("file.file_url").str();
            if file_id.is_empty() && file_data.is_empty() && file_url.is_empty() {
                return tool_output_fallback_part(item, src, path);
            }
            let mut part = json!({"type": "input_file"});
            if !file_id.is_empty() {
                cpa_json::set(&mut part, "file_id", file_id);
            }
            if !file_data.is_empty() {
                cpa_json::set(&mut part, "file_data", file_data);
            }
            if !file_url.is_empty() {
                cpa_json::set(&mut part, "file_url", file_url);
            }
            let filename = item.g("file.filename").str();
            if !filename.is_empty() {
                cpa_json::set(&mut part, "filename", filename);
            }
            part
        }
        _ => tool_output_fallback_part(item, src, path),
    }
}

fn has_tool_output_image_part(content: &Res<'_>) -> bool {
    if !content.is_array() {
        return false;
    }
    content
        .array()
        .iter()
        .any(|item| match item.g("type").str().as_str() {
            "image_url" => {
                !item.g("image_url.url").str().is_empty()
                    || !item.g("image_url.file_id").str().is_empty()
            }
            "input_image" => {
                !item.g("image_url").str().is_empty() || !item.g("file_id").str().is_empty()
            }
            _ => false,
        })
}

/// Unsupported tool output parts are forwarded as their source text.
fn tool_output_fallback_part(item: &Res<'_>, src: &[u8], path: &str) -> Value {
    let mut text = if item.exists() {
        raw_at(src, path).unwrap_or_else(|| item.raw())
    } else {
        String::new()
    };
    if text.is_empty() {
        text = item.str();
    }
    json!({"type": "input_text", "text": text})
}

/// Replaces every character outside `[a-zA-Z0-9_-]` with `_` (one per character).
fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Sanitizes, then applies the 64 byte shortening rule: keeps the `mcp__` prefix and last
/// segment when possible, otherwise truncates.
pub(super) fn shorten_name_if_needed(name: &str) -> String {
    const LIMIT: usize = 64;
    let sanitized = sanitize_tool_name(name);
    if sanitized.len() <= LIMIT {
        return sanitized;
    }
    if sanitized.starts_with("mcp__")
        && let Some(idx) = sanitized.rfind("__")
        && idx > 0
    {
        let candidate = format!("mcp__{}", &sanitized[idx + 2..]);
        if candidate.len() > LIMIT {
            return truncate_bytes(&candidate, LIMIT);
        }
        return candidate;
    }
    truncate_bytes(&sanitized, LIMIT)
}

/// Unique shortened names for the request's tool names (original -> short).
pub(super) fn build_short_name_map_for(names: &[String]) -> HashMap<String, String> {
    build_short_name_map(names, shorten_name_if_needed)
}

/// Unique tool names across declarations, tool_choice and assistant tool_calls, in order.
pub(super) fn collect_request_tool_names(root: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |name: String| {
        if !name.is_empty() && seen.insert(name.clone()) {
            names.push(name);
        }
    };

    let tools = root.g("tools");
    if tools.is_array() {
        for tool in tools.array() {
            match tool.g("type").str().as_str() {
                "function" => add(tool.g("function.name").str()),
                "custom" => add(tool.g("name").str()),
                _ => {}
            }
        }
    }

    let tc = root.g("tool_choice");
    if tc.is_object() {
        match tc.g("type").str().as_str() {
            "function" => {
                let mut name = tc.g("function.name").str();
                if name.is_empty() {
                    name = tc.g("name").str();
                }
                add(name);
            }
            "custom" => add(tc.g("name").str()),
            _ => {}
        }
    }

    let messages = root.g("messages");
    if messages.is_array() {
        for msg in messages.array() {
            if msg.g("role").str() != "assistant" {
                continue;
            }
            let tool_calls = msg.g("tool_calls");
            if tool_calls.is_array() {
                for tc in tool_calls.array() {
                    let name = tc.g("function.name").str();
                    if name.is_empty() {
                        add(tc.g("custom.name").str());
                    } else {
                        add(name);
                    }
                }
            }
        }
    }
    names
}

fn normalize_service_tier(result: &Res<'_>) -> Option<&'static str> {
    match result.as_str()?.trim().to_lowercase().as_str() {
        "fast" | "priority" => Some("priority"),
        "ultrafast" => Some("ultrafast"),
        _ => None,
    }
}

//! Gemini request -> Codex Responses request (Go: codex_gemini_request.go).

use std::collections::HashMap;

use cpa_core::thinking;
use cpa_core::util::walk;
use cpa_json::{J, Res, Value, json};

use crate::codex::util::{
    build_short_name_map, file_name_from_mime, input_audio_format_from_mime, shorten_name_if_needed,
};
use crate::common::is_gemini_thought_part;
use cpa_json::raw_at;

/// Go: ConvertGeminiRequestToCodex. Maps system instruction, contents (text, media, function
/// calls and responses paired through a FIFO of call ids), tools, tool config and thinking
/// config onto a Codex Responses request.
pub fn convert_gemini_request_to_codex(
    model_name: &str,
    raw_json: &[u8],
    _stream: bool,
) -> Vec<u8> {
    let mut out = cpa_json::parse_str(r#"{"model":"","instructions":"","input":[]}"#);
    let root = cpa_json::parse(raw_json);
    let mut input_items: Vec<Value> = Vec::new();

    let short_map = short_name_map_from_tools(&root);

    // Gemini pairs functionResponses with calls in order, so keep a FIFO of generated call ids.
    let mut pending_call_ids: Vec<String> = Vec::new();
    let mut call_counter = 0u32;

    out_set(&mut out, "model", model_name);
    if let Some(tier) = normalize_service_tier(&root.g("service_tier")) {
        out_set(&mut out, "service_tier", tier);
    }

    // System instruction -> developer message with input_text parts.
    let mut sys_parts = root.g("system_instruction.parts");
    if !sys_parts.exists() {
        sys_parts = root.g("systemInstruction.parts");
    }
    if sys_parts.is_array() {
        let mut content_items = Vec::new();
        for p in sys_parts.array() {
            if is_gemini_thought_part(&p) {
                continue;
            }
            let t = p.g("text");
            if t.exists() {
                content_items.push(json!({"type": "input_text", "text": t.str()}));
            }
        }
        if !content_items.is_empty() {
            input_items
                .push(json!({"type": "message", "role": "developer", "content": content_items}));
        }
    }

    // Contents -> messages and function calls/results.
    let contents = root.g("contents");
    if contents.is_array() {
        for (ci, item) in contents.array().iter().enumerate() {
            let mut role = item.g("role").str();
            if role == "model" {
                role = "assistant".into();
            }
            let parts = item.g("parts");
            if !parts.is_array() {
                continue;
            }
            for (pj, p) in parts.array().iter().enumerate() {
                if is_gemini_thought_part(p) {
                    continue;
                }
                // Source text of this part, for values Go copies verbatim into strings.
                let part_path = format!("contents.{ci}.parts.{pj}");

                let t = p.g("text");
                if t.exists() {
                    let part_type = if role == "assistant" {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    input_items.push(message_with_part(
                        &role,
                        json!({"type": part_type, "text": t.str()}),
                    ));
                    continue;
                }
                if let Some(part) = content_part_from_inline_data(p) {
                    input_items.push(message_with_part(&role, part));
                    continue;
                }
                if let Some(part) = content_part_from_file_data(p) {
                    input_items.push(message_with_part(&role, part));
                    continue;
                }

                // Function call from the model.
                let fc = p.g("functionCall");
                if fc.exists() {
                    let mut f = json!({"type": "function_call"});
                    let name = fc.g("name");
                    if name.exists() {
                        let n = name.str();
                        let n = short_map
                            .get(&n)
                            .cloned()
                            .unwrap_or_else(|| shorten_name_if_needed(&n));
                        cpa_json::set(&mut f, "name", n);
                    }
                    let args = fc.g("args");
                    if args.exists() {
                        let raw_args = raw_at(raw_json, &format!("{part_path}.functionCall.args"))
                            .map_or_else(|| args.raw(), str::to_string);
                        cpa_json::set(&mut f, "arguments", raw_args);
                    }
                    // Reuse gateway-provided ids when present, otherwise generate one for pairing.
                    let mut id = gemini_call_id(&fc);
                    if id.is_empty() {
                        call_counter += 1;
                        id = format!("call_gemini_{call_counter:016}");
                    }
                    cpa_json::set(&mut f, "call_id", id.clone());
                    pending_call_ids.push(id);
                    input_items.push(f);
                    continue;
                }

                // Function response from the user.
                let fr = p.g("functionResponse");
                if fr.exists() {
                    let mut fno = json!({"type": "function_call_output"});
                    // Prefer a string result if present; otherwise embed the raw response.
                    let res = fr.g("response.result");
                    let resp = fr.g("response");
                    if res.exists() {
                        cpa_json::set(&mut fno, "output", res.str());
                    } else if resp.exists() {
                        let raw_resp =
                            raw_at(raw_json, &format!("{part_path}.functionResponse.response"))
                                .map_or_else(|| resp.raw(), str::to_string);
                        cpa_json::set(&mut fno, "output", raw_resp);
                    }
                    // Pair with the oldest queued call id; generate one if the queue is empty.
                    let custom_id = gemini_call_id(&fr);
                    let id = if !custom_id.is_empty() {
                        if let Some(idx) = pending_call_ids.iter().position(|x| *x == custom_id) {
                            pending_call_ids.remove(idx);
                        }
                        custom_id
                    } else if !pending_call_ids.is_empty() {
                        pending_call_ids.remove(0)
                    } else {
                        call_counter += 1;
                        format!("call_gemini_{call_counter:016}")
                    };
                    cpa_json::set(&mut fno, "call_id", id);
                    input_items.push(fno);
                }
            }
        }
    }

    if !input_items.is_empty() {
        out_set(&mut out, "input", Value::Array(input_items));
    }

    // Tools: Gemini functionDeclarations -> Codex function tools.
    let tools = root.g("tools");
    if tools.is_array() {
        let mut tool_items = Vec::new();
        out_set(&mut out, "tool_choice", "auto");
        for td in tools.array() {
            let fns = td.g("functionDeclarations");
            if !fns.is_array() {
                continue;
            }
            for f in fns.array() {
                let mut tool = json!({"type": "function"});
                let v = f.g("name");
                if v.exists() {
                    let name = v.str();
                    let name = short_map
                        .get(&name)
                        .cloned()
                        .unwrap_or_else(|| shorten_name_if_needed(&name));
                    cpa_json::set(&mut tool, "name", name);
                }
                let v = f.g("description");
                if v.exists() {
                    cpa_json::set(&mut tool, "description", v.str());
                }
                let mut prm = f.g("parameters");
                if !prm.exists() {
                    prm = f.g("parametersJsonSchema");
                }
                if prm.exists() {
                    cpa_json::set(&mut tool, "parameters", clean_tool_parameters(&prm));
                }
                cpa_json::set(&mut tool, "strict", false);
                tool_items.push(tool);
            }
        }
        out_set(&mut out, "tools", Value::Array(tool_items));
    }

    // Fixed flags aligning with Codex expectations.
    out_set(&mut out, "parallel_tool_calls", true);
    set_tool_choice_from_tool_config(&mut out, &root.g("toolConfig.functionCallingConfig"));

    // Gemini thinkingConfig -> reasoning.effort. The official Python SDK sends snake_case.
    let mut effort_set = false;
    let gen_config = root.g("generationConfig");
    if gen_config.exists() {
        let mut thinking_level = gen_config.g("thinkingLevel");
        if !thinking_level.exists() {
            thinking_level = gen_config.g("thinking_level");
        }
        let thinking_config = gen_config.g("thinkingConfig");
        if thinking_level.exists() {
            let effort = thinking_level.str().trim().to_lowercase();
            if !effort.is_empty() {
                out_set(&mut out, "reasoning.effort", effort);
                effort_set = true;
            }
        } else if thinking_config.exists() && thinking_config.is_object() {
            let mut tl = thinking_config.g("thinkingLevel");
            if !tl.exists() {
                tl = thinking_config.g("thinking_level");
            }
            if tl.exists() {
                let effort = tl.str().trim().to_lowercase();
                if !effort.is_empty() {
                    out_set(&mut out, "reasoning.effort", effort);
                    effort_set = true;
                }
            } else {
                let mut budget = thinking_config.g("thinkingBudget");
                if !budget.exists() {
                    budget = thinking_config.g("thinking_budget");
                }
                if budget.exists()
                    && let Some(effort) = thinking::convert_budget_to_level(budget.int())
                {
                    out_set(&mut out, "reasoning.effort", effort);
                    effort_set = true;
                }
            }
        }
    }
    if !effort_set {
        out_set(&mut out, "reasoning.effort", "medium");
    }
    // reasoning.summary is left to the source request's canonical summary intent.
    out_set(&mut out, "stream", true);
    out_set(&mut out, "store", false);
    out_set(&mut out, "include", json!(["reasoning.encrypted_content"]));

    // Tool schema `type` values are lowercased (Gemini schemas use STRING/OBJECT).
    let mut paths_to_lower = Vec::new();
    if let Some(tools) = out.g("tools").v() {
        walk(tools, "", "type", &mut paths_to_lower);
    }
    for p in paths_to_lower {
        let full_path = format!("tools.{p}");
        let type_value = out.g(&full_path);
        let Some(current) = type_value.as_str() else {
            continue;
        };
        let normalized = current.to_lowercase();
        if normalized == current {
            continue;
        }
        out_set(&mut out, &full_path, normalized);
    }

    cpa_json::to_vec(&out)
}

fn out_set(out: &mut Value, path: &str, val: impl Into<Value>) {
    cpa_json::set(out, path, val);
}

/// Shortened names for declared functionDeclarations (original -> short).
pub(super) fn short_name_map_from_tools(root: &Value) -> HashMap<String, String> {
    let tools = root.g("tools");
    if !tools.is_array() {
        return HashMap::new();
    }
    let mut names = Vec::new();
    for t in tools.array() {
        let fns = t.g("functionDeclarations");
        if !fns.is_array() {
            continue;
        }
        for f in fns.array() {
            let v = f.g("name");
            if v.exists() {
                names.push(v.str());
            }
        }
    }
    if names.is_empty() {
        return HashMap::new();
    }
    build_short_name_map(&names, shorten_name_if_needed)
}

fn gemini_call_id(value: &Res<'_>) -> String {
    let id = value.g("id").str().trim().to_string();
    if !id.is_empty() {
        return id;
    }
    value.g("call_id").str().trim().to_string()
}

fn set_tool_choice_from_tool_config(out: &mut Value, cfg: &Res<'_>) {
    if !cfg.exists() {
        return;
    }
    match cfg.g("mode").str().as_str() {
        "NONE" => out_set(out, "tool_choice", "none"),
        "AUTO" => {
            let current = out.g("tool_choice");
            if current.as_str() != Some("auto") {
                out_set(out, "tool_choice", "auto");
            }
        }
        "ANY" => {
            let allowed = cfg.g("allowedFunctionNames");
            let items = allowed.array();
            if allowed.is_array() && items.len() == 1 {
                let choice =
                    json!({"type": "function", "name": shorten_name_if_needed(&items[0].str())});
                out_set(out, "tool_choice", choice);
            } else {
                out_set(out, "tool_choice", "required");
            }
        }
        _ => {}
    }
}

/// Drops `$schema` and forces `additionalProperties: false` unless already false.
fn clean_tool_parameters(parameters: &Res<'_>) -> Value {
    let mut cleaned = parameters.value();
    if parameters.g("$schema").exists() {
        cpa_json::delete(&mut cleaned, "$schema");
    }
    if parameters.g("additionalProperties").v() != Some(&Value::Bool(false)) {
        cpa_json::set(&mut cleaned, "additionalProperties", false);
    }
    cleaned
}

fn message_with_part(role: &str, part: Value) -> Value {
    json!({"type": "message", "role": role, "content": [part]})
}

fn normalize_service_tier(service_tier: &Res<'_>) -> Option<&'static str> {
    let s = service_tier.as_str()?;
    matches!(s.trim().to_lowercase().as_str(), "priority" | "fast").then_some("priority")
}

fn content_part_from_inline_data(part: &Res<'_>) -> Option<Value> {
    let mut inline = part.g("inlineData");
    if !inline.exists() {
        inline = part.g("inline_data");
    }
    if !inline.exists() {
        return None;
    }
    let mut mime_type = inline.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = inline.g("mime_type").str();
    }
    let data = inline.g("data").str();
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    let lower = mime_type.to_lowercase();
    Some(if lower.starts_with("image/") {
        json!({"type": "input_image", "image_url": format!("data:{mime_type};base64,{data}")})
    } else if lower.starts_with("audio/") {
        json!({"type": "input_audio", "input_audio": {"data": data, "format": input_audio_format_from_mime(&mime_type)}})
    } else {
        json!({"type": "input_file", "file_data": data, "filename": file_name_from_mime(&mime_type)})
    })
}

fn content_part_from_file_data(part: &Res<'_>) -> Option<Value> {
    let mut file = part.g("fileData");
    if !file.exists() {
        file = part.g("file_data");
    }
    if !file.exists() {
        return None;
    }
    let mut uri = file.g("fileUri").str();
    if uri.is_empty() {
        uri = file.g("file_uri").str();
    }
    if uri.is_empty() {
        return None;
    }
    let mut mime_type = file.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = file.g("mime_type").str();
    }
    let lower = mime_type.to_lowercase();
    if lower.starts_with("image/") {
        return Some(json!({"type": "input_image", "image_url": uri}));
    }
    if lower.starts_with("video/")
        || lower.starts_with("application/")
        || lower.starts_with("text/")
    {
        return Some(
            json!({"type": "input_file", "file_url": uri, "filename": file_name_from_mime(&mime_type)}),
        );
    }
    let mut info = format!("File: {uri}");
    if !mime_type.is_empty() {
        info.push_str(&format!(" (Type: {mime_type})"));
    }
    Some(json!({"type": "input_text", "text": info}))
}

//! OpenAI Responses request -> Codex Responses request (Go: codex_openai-responses_request.go).

use cpa_json::{json, Value, J};

/// Go: ConvertOpenAIResponsesRequestToCodex. The Responses body is already Codex shaped; this
/// forces the fields Codex requires and strips the ones it rejects.
pub fn convert_openai_responses_request_to_codex(_model: &str, input_raw_json: &[u8], _stream: bool) -> Vec<u8> {
    let mut root = cpa_json::parse(input_raw_json);

    let input = root.g("input");
    if input.is_string() {
        let mut msg = cpa_json::parse_str(r#"[{"type":"message","role":"user","content":[{"type":"input_text","text":""}]}]"#);
        cpa_json::set(&mut msg, "0.content.0.text", input.str());
        cpa_json::set(&mut root, "input", msg);
    }

    set_required_bool(&mut root, "stream", true);
    set_required_bool(&mut root, "store", false);
    set_required_bool(&mut root, "parallel_tool_calls", true);
    set_required_include(&mut root);
    // Codex Responses rejects token limit fields, so strip them out before forwarding.
    delete_fields(&mut root, &["max_output_tokens", "max_completion_tokens", "temperature", "top_p"]);
    let service_tier = root.g("service_tier");
    if service_tier.exists() {
        if service_tier.is_string() {
            let tier = service_tier.str();
            match tier.trim().to_lowercase().as_str() {
                "priority" | "fast" => {
                    if tier != "priority" {
                        cpa_json::set(&mut root, "service_tier", "priority");
                    }
                }
                "ultrafast" => {
                    if tier != "ultrafast" {
                        cpa_json::set(&mut root, "service_tier", "ultrafast");
                    }
                }
                _ => delete_fields(&mut root, &["service_tier"]),
            }
        } else {
            delete_fields(&mut root, &["service_tier"]);
        }
    }

    delete_fields(&mut root, &["truncation", "prompt_cache_options", "prompt_cache_retention"]);
    strip_cache_breakpoints(&mut root);
    // Codex /responses rejects context_management.
    delete_fields(&mut root, &["context_management"]);

    // Delete the user field as it is not supported by the Codex upstream.
    delete_fields(&mut root, &["user"]);

    convert_system_role_to_developer(&mut root);
    normalize_builtin_tools(&mut root);
    normalize_empty_function_call_arguments(&mut root);

    cpa_json::to_vec(&root)
}

/// Blank string `arguments` on history function_call items become "{}": parameter-less calls
/// serialized as "" are rejected upstream. Non-blank strings pass through untouched.
fn normalize_empty_function_call_arguments(root: &mut Value) {
    let Some(Value::Array(items)) = cpa_json::get_mut(root, "input") else { return };
    for item in items {
        if item.is_object()
            && item.g("type").str() == "function_call"
            && item.g("arguments").as_str().is_some_and(|a| a.trim().is_empty())
        {
            cpa_json::set(item, "arguments", "{}");
        }
    }
}

fn set_required_bool(root: &mut Value, path: &str, value: bool) {
    if root.g(path).v() != Some(&Value::Bool(value)) {
        cpa_json::set(root, path, value);
    }
}

fn set_required_include(root: &mut Value) {
    let current = root.g("include");
    if let Some(Value::Array(a)) = current.v()
        && a.len() == 1
        && a[0].as_str() == Some("reasoning.encrypted_content")
    {
        return;
    }
    cpa_json::set(root, "include", json!(["reasoning.encrypted_content"]));
}

fn delete_fields(root: &mut Value, paths: &[&str]) {
    for path in paths {
        if root.g(path).exists() {
            cpa_json::delete(root, path);
        }
    }
}

/// Removes `prompt_cache_breakpoint` from input items and from their `content` / `output`
/// parts; Codex rejects it outright.
fn strip_cache_breakpoints(root: &mut Value) {
    let Some(Value::Array(items)) = cpa_json::get_mut(root, "input") else { return };
    for item in items {
        for array_path in ["content", "output"] {
            if let Some(Value::Array(parts)) = cpa_json::get_mut(item, array_path) {
                for part in parts {
                    if part.g("prompt_cache_breakpoint").exists() {
                        cpa_json::delete(part, "prompt_cache_breakpoint");
                    }
                }
            }
        }
        if item.g("prompt_cache_breakpoint").exists() {
            cpa_json::delete(item, "prompt_cache_breakpoint");
        }
    }
}

/// Codex does not accept the "system" role in input; rewrite it to "developer".
fn convert_system_role_to_developer(root: &mut Value) {
    let Some(Value::Array(items)) = cpa_json::get_mut(root, "input") else { return };
    for item in items {
        if item.is_object() && item.g("role").str() == "system" {
            cpa_json::set(item, "role", "developer");
        }
    }
}

/// Rewrites legacy/preview built-in tool types to the stable names Codex expects.
fn normalize_builtin_tools(root: &mut Value) {
    normalize_builtin_tool_array(root, "tools");
    normalize_builtin_tool_at_path(root, "tool_choice.type");
    normalize_builtin_tool_array(root, "tool_choice.tools");
}

fn normalize_builtin_tool_array(root: &mut Value, path: &str) {
    let Some(Value::Array(tools)) = cpa_json::get_mut(root, path) else { return };
    for tool in tools {
        if let Some(normalized) = normalize_builtin_tool_type(&tool.g("type").str()) {
            cpa_json::set(tool, "type", normalized);
        }
    }
}

fn normalize_builtin_tool_at_path(root: &mut Value, path: &str) {
    if let Some(normalized) = normalize_builtin_tool_type(&root.g(path).str()) {
        cpa_json::set(root, path, normalized);
    }
}

fn normalize_builtin_tool_type(tool_type: &str) -> Option<&'static str> {
    match tool_type {
        "web_search_preview" | "web_search_preview_2025_03_11" => Some("web_search"),
        _ => None,
    }
}

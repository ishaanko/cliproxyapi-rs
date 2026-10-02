//! OpenAI Responses tool declarations: identity, precedence and Gemini conversion
//! (Go: util/responses_tools.go).
//!
//! Go passes gjson results; here the request root is a `&Value` and tool values are owned clones.

use std::collections::{HashMap, HashSet};

use cpa_json::{J, Value};

use super::gemini_schema::clean_json_schema_for_gemini_json_schema;
use super::sanitize_function_name;
use super::translator::sanitize_unique_names;
use crate::applypatch;

/// Resolved identity of a tool in OpenAI Responses format.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResponsesToolIdentity {
    pub name: String,
    pub namespace: String,
    pub custom: bool,
    /// Resolved from the winning original declaration, never from the upstream name.
    pub apply_patch: bool,
}

/// A tool declaration found in a Responses request.
#[derive(Debug, Clone)]
pub struct ResponsesToolDescriptor {
    /// Qualified name (`functions__exec` or `exec`).
    pub name: String,
    /// Name without namespace (`exec`).
    pub local_name: String,
    /// Namespace, if any (`functions`).
    pub namespace: String,
    /// `function` or `custom`.
    pub tool_type: String,
    pub tool: Value,
    /// 0 for top-level `tools`, 1 for `additional_tools` input items.
    pub source_priority: i32,
    /// True when declared directly, false when declared as a namespace child.
    pub direct: bool,
    /// Original discovery order.
    pub order: usize,
}

/// Qualifies a child tool name with its namespace: `ns__child` (no double separators; `mcp__`
/// names and names already carrying the namespace are left alone).
pub fn qualify_responses_namespace_tool_name(namespace_name: &str, child_name: &str) -> String {
    let child_name = child_name.trim();
    let namespace_name = namespace_name.trim();
    if child_name.is_empty() || namespace_name.is_empty() || child_name.starts_with("mcp__") {
        return child_name.to_string();
    }
    if child_name == namespace_name || child_name.starts_with(&format!("{namespace_name}__")) {
        return child_name.to_string();
    }
    if namespace_name.ends_with("__") {
        return format!("{namespace_name}{child_name}");
    }
    format!("{namespace_name}__{child_name}")
}

/// Tool arrays of a request with their source priority: `tools` (0), then each
/// `additional_tools` input item's `tools` (1).
fn responses_tool_sources(root: &Value) -> Vec<(&Vec<Value>, i32)> {
    let mut sources = Vec::new();
    if let Some(Value::Array(tools)) = root.get("tools") {
        sources.push((tools, 0));
    }
    if let Some(Value::Array(input)) = root.get("input") {
        for item in input {
            if item.g("type").str() == "additional_tools"
                && let Some(Value::Array(tools)) = item.get("tools")
            {
                sources.push((tools, 1));
            }
        }
    }
    sources
}

fn responses_tool_name(tool: &Value) -> String {
    let name = tool.g("name").str().trim().to_string();
    if !name.is_empty() {
        return name;
    }
    tool.g("function.name").str().trim().to_string()
}

/// Description of a tool or function object (`description`, else `function.description`).
pub fn responses_tool_description(tool: &Value) -> String {
    let description = tool.g("description").str();
    if !description.is_empty() {
        return description;
    }
    tool.g("function.description").str()
}

/// Schema/parameters of a tool or function object: the first existing of `parameters`,
/// `parametersJsonSchema`, `input_schema`, `function.parameters`, `function.parametersJsonSchema`.
pub fn responses_tool_parameters(tool: &Value) -> Option<&Value> {
    ["parameters", "parametersJsonSchema", "input_schema"]
        .into_iter()
        .find_map(|key| tool.get(key))
        .or_else(|| {
            let function = tool.get("function")?;
            function
                .get("parameters")
                .or_else(|| function.get("parametersJsonSchema"))
        })
}

/// All tool descriptors of a Responses request root, in discovery order. Function and custom
/// tools are collected directly; `namespace` tools contribute their `tools`/`children`.
pub fn collect_responses_tool_descriptors(root: &Value) -> Vec<ResponsesToolDescriptor> {
    let mut descriptors: Vec<ResponsesToolDescriptor> = Vec::new();
    let mut append = |tool: &Value,
                      name: String,
                      local_name: String,
                      namespace: &str,
                      tool_type: &str,
                      source_priority: i32,
                      direct: bool| {
        if name.is_empty() {
            return;
        }
        let order = descriptors.len();
        descriptors.push(ResponsesToolDescriptor {
            name,
            local_name,
            namespace: namespace.to_string(),
            tool_type: tool_type.to_string(),
            tool: tool.clone(),
            source_priority,
            direct,
            order,
        });
    };

    for (tools, priority) in responses_tool_sources(root) {
        for tool in tools {
            match tool.g("type").str().trim() {
                "" | "function" => {
                    let name = responses_tool_name(tool);
                    append(tool, name.clone(), name, "", "function", priority, true);
                }
                "custom" => {
                    let name = responses_tool_name(tool);
                    append(tool, name.clone(), name, "", "custom", priority, true);
                }
                "namespace" => {
                    let namespace_name = tool.g("name").str().trim().to_string();
                    let children = match tool.get("tools") {
                        Some(Value::Array(c)) => Some(c),
                        _ => match tool.get("children") {
                            Some(Value::Array(c)) => Some(c),
                            _ => None,
                        },
                    };
                    for child in children.into_iter().flatten() {
                        let child_name = responses_tool_name(child);
                        if child_name.is_empty() {
                            continue;
                        }
                        let qualified =
                            qualify_responses_namespace_tool_name(&namespace_name, &child_name);
                        match child.g("type").str().trim() {
                            "" | "function" => append(
                                child,
                                qualified,
                                child_name,
                                &namespace_name,
                                "function",
                                priority,
                                false,
                            ),
                            "custom" => append(
                                child,
                                qualified,
                                child_name,
                                &namespace_name,
                                "custom",
                                priority,
                                false,
                            ),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
    descriptors
}

fn descriptor_precedes(left: &ResponsesToolDescriptor, right: &ResponsesToolDescriptor) -> bool {
    if left.source_priority != right.source_priority {
        return left.source_priority < right.source_priority;
    }
    if left.direct != right.direct {
        return left.direct;
    }
    left.order < right.order
}

/// The winning descriptor per qualified tool name: top-level before `additional_tools`, direct
/// before namespace children, then first declared.
pub fn collect_responses_tool_winners(root: &Value) -> HashMap<String, ResponsesToolDescriptor> {
    let mut winners: HashMap<String, ResponsesToolDescriptor> = HashMap::new();
    for descriptor in collect_responses_tool_descriptors(root) {
        match winners.get(&descriptor.name) {
            Some(current) if !descriptor_precedes(&descriptor, current) => {}
            _ => {
                winners.insert(descriptor.name.clone(), descriptor);
            }
        }
    }
    winners
}

/// Gemini function declarations for a Responses request root, plus the forward map (request tool
/// name -> Gemini name) and the reverse identity map (Gemini or qualified name -> identity).
/// Declarations are `{"name","description","parametersJsonSchema"}` objects, apply_patch custom
/// tools get the function wrapper schema, other custom tools a single string `input`.
pub fn build_gemini_function_declarations(
    root: &Value,
) -> (
    Vec<Value>,
    HashMap<String, String>,
    HashMap<String, ResponsesToolIdentity>,
) {
    let descriptors = collect_responses_tool_descriptors(root);
    let winners = collect_responses_tool_winners(root);

    let mut seen_names: HashSet<&str> = HashSet::new();
    let mut winning_list: Vec<&ResponsesToolDescriptor> = Vec::new();
    for descriptor in &descriptors {
        let Some(winner) = winners.get(&descriptor.name) else {
            continue;
        };
        if winner.order != descriptor.order || !seen_names.insert(&descriptor.name) {
            continue;
        }
        winning_list.push(descriptor);
    }
    if winning_list.is_empty() {
        return (Vec::new(), HashMap::new(), HashMap::new());
    }

    let sanitized_map =
        sanitize_unique_names(winning_list.iter().map(|d| d.name.clone()).collect());

    let mut forward_map: HashMap<String, String> = HashMap::with_capacity(winning_list.len() * 2);
    let mut reverse_map: HashMap<String, ResponsesToolIdentity> =
        HashMap::with_capacity(winning_list.len() * 2);
    let mut declarations = Vec::with_capacity(winning_list.len());

    for desc in winning_list {
        let gemini_name = match sanitized_map.get(&desc.name) {
            Some(mapped) if !mapped.is_empty() => mapped.clone(),
            _ => sanitize_function_name(&desc.name),
        };

        forward_map.insert(desc.name.clone(), gemini_name.clone());
        if !desc.local_name.is_empty() && desc.local_name != desc.name {
            forward_map
                .entry(desc.local_name.clone())
                .or_insert_with(|| gemini_name.clone());
        }

        let is_apply_patch = applypatch::is_custom_tool(&desc.tool);
        let identity = ResponsesToolIdentity {
            name: desc.local_name.clone(),
            namespace: desc.namespace.clone(),
            custom: desc.tool_type == "custom",
            apply_patch: is_apply_patch,
        };
        reverse_map.insert(gemini_name.clone(), identity.clone());
        if desc.name != gemini_name {
            reverse_map.insert(desc.name.clone(), identity);
        }

        let mut func_decl =
            cpa_json::parse_str(r#"{"name":"","description":"","parametersJsonSchema":{}}"#);
        cpa_json::set(&mut func_decl, "name", gemini_name.as_str());
        let description = responses_tool_description(&desc.tool);
        if !description.is_empty() {
            cpa_json::set(&mut func_decl, "description", description);
        }

        if is_apply_patch {
            cpa_json::set(
                &mut func_decl,
                "description",
                applypatch::description(&desc.tool),
            );
            cpa_json::set(
                &mut func_decl,
                "parametersJsonSchema",
                cpa_json::parse(&applypatch::parameters()),
            );
        } else if desc.tool_type == "custom" {
            cpa_json::set(
                &mut func_decl,
                "parametersJsonSchema",
                cpa_json::parse_str(
                    r#"{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}"#,
                ),
            );
        } else if let Some(params) = responses_tool_parameters(&desc.tool) {
            let cleaned = clean_json_schema_for_gemini_json_schema(&params.to_string());
            cpa_json::set(
                &mut func_decl,
                "parametersJsonSchema",
                cpa_json::parse_str(&cleaned),
            );
        }
        declarations.push(func_decl);
    }

    (declarations, forward_map, reverse_map)
}

/// Gemini function name -> identity map for a Responses request body (a Codex-style envelope with
/// a nested `request` is unwrapped first). Empty for invalid or tool-less bodies.
pub fn responses_tool_reverse_identity_map(
    raw_json: &[u8],
) -> HashMap<String, ResponsesToolIdentity> {
    if raw_json.is_empty() || !cpa_json::valid(raw_json) {
        return HashMap::new();
    }
    let root = cpa_json::parse(raw_json);
    let mut target = &root;
    if let Some(req) = root.get("request")
        && (req.get("model").is_some() || req.get("input").is_some() || req.get("tools").is_some())
    {
        target = req;
    }
    build_gemini_function_declarations(target).2
}

/// Gemini name for a request tool name via the forward map; the plain sanitized name when unmapped.
pub fn map_responses_tool_name(forward_map: &HashMap<String, String>, name: &str) -> String {
    match forward_map.get(name) {
        Some(mapped) if !mapped.is_empty() => mapped.clone(),
        _ => sanitize_function_name(name),
    }
}

/// Translates a Responses `tool_choice` (`None` when absent) into Gemini `functionCallingConfig`
/// JSON (`{"mode": ..., "allowedFunctionNames": [...]}`); `None` when it maps to nothing.
pub fn convert_responses_tool_choice_to_gemini(
    tool_choice: Option<&Value>,
    forward_map: &HashMap<String, String>,
) -> Option<Value> {
    let tool_choice = tool_choice?;
    let mut mode = "";
    let mut allowed_names: Vec<String> = Vec::new();
    match tool_choice {
        Value::String(s) => {
            mode = match s.trim().to_lowercase().as_str() {
                "none" => "NONE",
                "auto" => "AUTO",
                "required" | "any" => "ANY",
                _ => "",
            };
        }
        Value::Object(_) => match tool_choice.g("type").str().trim().to_lowercase().as_str() {
            "none" => mode = "NONE",
            "auto" => mode = "AUTO",
            "required" | "any" => mode = "ANY",
            "function" | "custom" | "tool" | "" => {
                mode = "ANY";
                let first_non_empty = |paths: [&str; 3]| {
                    paths
                        .into_iter()
                        .map(|p| tool_choice.g(p).str().trim().to_string())
                        .find(|v| !v.is_empty())
                        .unwrap_or_default()
                };
                let mut name = first_non_empty(["name", "function.name", "custom.name"]);
                let namespace =
                    first_non_empty(["namespace", "function.namespace", "custom.namespace"]);
                if !namespace.is_empty() {
                    name = qualify_responses_namespace_tool_name(&namespace, &name);
                }
                if !name.is_empty() {
                    allowed_names.push(map_responses_tool_name(forward_map, &name));
                }
            }
            _ => {}
        },
        _ => {}
    }
    if mode.is_empty() {
        return None;
    }
    let mut cfg = cpa_json::parse_str(r#"{"mode":""}"#);
    cpa_json::set(&mut cfg, "mode", mode);
    if !allowed_names.is_empty() {
        cpa_json::set(&mut cfg, "allowedFunctionNames", allowed_names);
    }
    Some(cfg)
}

/// Raw input string from custom tool arguments: the `input` field (strings verbatim, other values
/// as compact JSON), a bare JSON string, or the arguments text itself. `{}`/blank give "".
pub fn unwrap_responses_custom_tool_input(arguments: &str) -> String {
    let arguments = arguments.trim();
    if arguments.is_empty() || arguments == "{}" {
        return String::new();
    }
    if cpa_json::valid(arguments.as_bytes()) {
        let parsed = cpa_json::parse_str(arguments);
        if let Some(v) = parsed.get("input") {
            return match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
        if let Value::String(s) = parsed {
            return s;
        }
    }
    arguments.to_string()
}

//! Tool declaration and tool choice normalization for xAI's Responses API (Go:
//! xai_executor_request.go and the schema helpers of xai_executor_response.go).
//!
//! xAI accepts a narrower tool dialect than Codex Responses: no `namespace` wrappers (flattened
//! or folded into dispatcher functions when the 200 tool cap would be exceeded), no `custom`
//! tools, no `tool_search`, strict object-only function schemas, and a specific `tool_choice`
//! shape. Every function edits the request body `Value` in place.

use std::collections::{HashMap, HashSet};

use cpa_core::thinking::parse_suffix;
use cpa_json::{J, Kind, Value};
use serde_json::json;

use super::util::{at, compact, exists, is_array_at, items, s, ts};

pub const MAX_TOOLS: usize = 200;
pub const FUNCTION_TOOL_TYPE: &str = "function";
pub const CUSTOM_TOOL_TYPE: &str = "custom";
pub const IMAGE_GENERATION_TOOL_TYPE: &str = "image_generation";
pub const NAMESPACE_TOOL_TYPE: &str = "namespace";
pub const TOOL_SEARCH_TYPE: &str = "tool_search";
pub const WEB_SEARCH_TOOL_TYPE: &str = "web_search";
pub const CLIENT_WEB_SEARCH_ALIAS: &str = "clientfn_web_search";
pub const X_SEARCH_TOOL_TYPE: &str = "x_search";
const CODEX_APP_NAMESPACE_NAME: &str = "codex_app";
const AUTOMATION_UPDATE_TOOL_NAME: &str = "automation_update";
/// Permissive placeholder schema: keeps the tool callable without the upstream hang the real
/// Codex Desktop automation schema causes.
const SAFE_FUNCTION_PARAMETERS: &str = r#"{"type":"object","properties":{},"additionalProperties":true}"#;

/// A namespace tool as the executor flattened it (Go: xaiNamespaceToolRef).
#[derive(Debug, Clone)]
pub struct NamespaceToolRef {
    pub namespace: String,
    pub name: String,
    pub is_dispatcher: bool,
}

pub type NamespaceRefs = HashMap<String, NamespaceToolRef>;

/// Identity of a client-declared callable tool after normalization (Go: xaiClientToolKey).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientToolKey {
    pub namespace: String,
    pub name: String,
    pub tool_type: String,
}

// ---------------------------------------------------------------- native image generation

/// Go: xaiSupportsNativeImageGeneration. grok-4.6 and later accept the hosted tool; grok-4.20
/// is an older line whose dotted minor is not comparable.
pub fn supports_native_image_generation(model: &str) -> bool {
    let mut name = parse_suffix(model).model_name.trim().to_lowercase();
    if let Some(idx) = name.rfind('/') {
        name = name[idx + 1..].to_string();
    }
    let Some(rest) = name.strip_prefix("grok-") else { return false };
    if rest == "4.20" || rest.starts_with("4.20-") {
        return false;
    }
    let Some((major, minor)) = parse_grok_version_prefix(rest) else { return false };
    (major, minor.max(0)) >= (4, 6)
}

fn parse_grok_version_prefix(rest: &str) -> Option<(i64, i64)> {
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let major: i64 = rest[..i].parse().ok()?;
    if i == bytes.len() || bytes[i] != b'.' {
        return Some((major, -1));
    }
    let mut j = i + 1;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    if j == i + 1 {
        return Some((major, -1));
    }
    let minor: i64 = rest[i + 1..j].parse().ok()?;
    Some((major, minor))
}

// ---------------------------------------------------------------- x_search

/// Go: xaiRequestHasNativeXSearch (top-level tools or any additional_tools item).
pub fn request_has_native_x_search(body: &Value) -> bool {
    if items(body, "tools").iter().any(|t| s(t, "type") == X_SEARCH_TOOL_TYPE) {
        return true;
    }
    items(body, "input")
        .iter()
        .filter(|item| s(item, "type") == "additional_tools")
        .any(|item| items(item, "tools").iter().any(|t| s(t, "type") == X_SEARCH_TOOL_TYPE))
}

/// Go: ensureXAINativeXSearchTool. Appends `{"type":"x_search"}` when absent and mirrors it
/// into an `allowed_tools` choice.
pub fn ensure_native_x_search_tool(body: &mut Value) {
    if !body.is_object() {
        return;
    }
    if !request_has_native_x_search(body) {
        if is_array_at(body, "tools") {
            cpa_json::set(body, "tools.-1", json!({"type": "x_search"}));
        } else {
            cpa_json::set(body, "tools", json!([{"type": "x_search"}]));
        }
    }
    ensure_native_x_search_allowed_tools(body);
}

fn ensure_native_x_search_allowed_tools(body: &mut Value) {
    let choice = body.g("tool_choice");
    if !choice.is_object() || choice.g("type").str() != "allowed_tools" {
        return;
    }
    let allowed = choice.g("tools");
    if !allowed.is_array() {
        cpa_json::set(body, "tool_choice.tools", json!([{"type": "x_search"}]));
        return;
    }
    if allowed.array().iter().any(|t| t.g("type").str().trim() == X_SEARCH_TOOL_TYPE) {
        return;
    }
    cpa_json::set(body, "tool_choice.tools.-1", json!({"type": "x_search"}));
}

// ---------------------------------------------------------------- client web_search alias

/// Go: xaiHasClientWebSearchFunction. A client function/custom tool named `web_search` that is
/// not a folded namespace dispatcher.
pub fn has_client_web_search_function(body: &Value, namespace_tools: &NamespaceRefs) -> bool {
    items(body, "tools").iter().any(|tool| {
        let tool_type = ts(tool, "type");
        let name = ts(tool, "name");
        (tool_type == FUNCTION_TOOL_TYPE || tool_type == CUSTOM_TOOL_TYPE)
            && name == WEB_SEARCH_TOOL_TYPE
            && !namespace_tools.contains_key(&name)
    })
}

/// Go: xaiBodyHasToolNamed (tools, nested namespace tools and input items).
fn body_has_tool_named(body: &Value, name: &str) -> bool {
    for tool in items(body, "tools") {
        if ts(tool, "name") == name {
            return true;
        }
        if items(tool, "tools").iter().any(|child| ts(child, "name") == name) {
            return true;
        }
    }
    items(body, "input").iter().any(|item| ts(item, "name") == name)
}

/// Go: xaiResolveClientWebSearchAlias, an alias that collides with no client tool.
pub fn resolve_client_web_search_alias(body: &Value) -> String {
    let candidate = CLIENT_WEB_SEARCH_ALIAS.to_string();
    if !body_has_tool_named(body, &candidate) {
        return candidate;
    }
    let mut i = 1;
    loop {
        let next = format!("{CLIENT_WEB_SEARCH_ALIAS}_{i}");
        if !body_has_tool_named(body, &next) {
            return next;
        }
        i += 1;
    }
}

/// Go: aliasXAIClientWebSearchInput. Renames replayed `web_search` calls to the alias.
pub fn alias_client_web_search_input(body: &mut Value, alias: &str, namespace_tools: &NamespaceRefs) {
    if !body.is_object() || alias.is_empty() || namespace_tools.contains_key(WEB_SEARCH_TOOL_TYPE) {
        return;
    }
    let renames: Vec<usize> = items(body, "input")
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            let item_type = ts(item, "type");
            (item_type == "function_call" || item_type == "custom_tool_call" || item_type == "function_call_output")
                && ts(item, "name") == WEB_SEARCH_TOOL_TYPE
                && ts(item, "namespace").is_empty()
        })
        .map(|(idx, _)| idx)
        .collect();
    for idx in renames {
        cpa_json::set(body, &format!("input.{idx}.name"), alias);
    }
}

/// Go: aliasXAIClientWebSearchFunction. Keeps xAI from hijacking a client `web_search`
/// function into hosted search by renaming it in tools, tool_choice and input history.
pub fn alias_client_web_search_function(body: &mut Value, alias: &str, namespace_tools: &NamespaceRefs) {
    if !body.is_object() || alias.is_empty() {
        return;
    }
    let is_namespace = namespace_tools.contains_key(WEB_SEARCH_TOOL_TYPE);
    // 1. tools (namespace dispatchers excluded)
    let tool_renames: Vec<usize> = items(body, "tools")
        .iter()
        .enumerate()
        .filter(|(_, tool)| {
            let tool_type = ts(tool, "type");
            let name = ts(tool, "name");
            (tool_type == FUNCTION_TOOL_TYPE || tool_type == CUSTOM_TOOL_TYPE)
                && name == WEB_SEARCH_TOOL_TYPE
                && !namespace_tools.contains_key(&name)
        })
        .map(|(i, _)| i)
        .collect();
    for idx in tool_renames {
        cpa_json::set(body, &format!("tools.{idx}.name"), alias);
    }
    // 2. tool_choice (only when unnamespaced and not a namespace dispatcher)
    let choice = body.g("tool_choice").value();
    if choice.is_object() {
        let fn_name = choice.g("function.name");
        if fn_name.exists()
            && fn_name.str().trim() == WEB_SEARCH_TOOL_TYPE
            && ts(&choice, "function.namespace").is_empty()
            && !is_namespace
        {
            cpa_json::set(body, "tool_choice.function.name", alias);
        }
        let name = choice.g("name");
        if name.exists() && name.str().trim() == WEB_SEARCH_TOOL_TYPE && ts(&choice, "namespace").is_empty() {
            let choice_type = ts(&choice, "type");
            if (choice_type == FUNCTION_TOOL_TYPE || choice_type == "tool") && !is_namespace {
                cpa_json::set(body, "tool_choice.name", alias);
            }
        }
        for (idx, allowed) in items(&choice, "tools").iter().enumerate() {
            if !ts(allowed, "namespace").is_empty() || is_namespace {
                continue;
            }
            let allowed_type = ts(allowed, "type");
            let allowed_name = ts(allowed, "name");
            if ((allowed_type == FUNCTION_TOOL_TYPE || allowed_type == "tool") && allowed_name == WEB_SEARCH_TOOL_TYPE)
                || (allowed_name == WEB_SEARCH_TOOL_TYPE && allowed_type != WEB_SEARCH_TOOL_TYPE)
            {
                cpa_json::set(body, &format!("tool_choice.tools.{idx}.name"), alias);
            }
        }
    }
    // 3. input history (only when unnamespaced)
    alias_client_web_search_input(body, alias, namespace_tools);
}

// ---------------------------------------------------------------- hosted tool choice

/// Go: normalizeXAIForcedWebSearchToolChoice.
pub fn normalize_forced_web_search_tool_choice(body: &mut Value) {
    normalize_forced_hosted_tool_choice(body, WEB_SEARCH_TOOL_TYPE);
}

/// Go: normalizeXAIForcedImageGenerationToolChoice.
pub fn normalize_forced_image_generation_tool_choice(body: &mut Value) {
    normalize_forced_hosted_tool_choice(body, IMAGE_GENERATION_TOOL_TYPE);
}

/// Go: normalizeXAIForcedHostedToolChoice. `{type: <hosted>}` becomes `"required"` with the
/// tools list reduced to that hosted tool; an `allowed_tools` list naming only the hosted tool
/// becomes its original mode, mixed lists just drop the hosted entry.
fn normalize_forced_hosted_tool_choice(body: &mut Value, tool_type: &str) {
    let choice = body.g("tool_choice").value();
    if !choice.is_object() {
        return;
    }
    let choice_type = ts(&choice, "type");
    if choice_type == tool_type {
        keep_only_hosted_tools(body, tool_type);
        set_tool_choice_string(body, "required");
        return;
    }
    if choice_type != "allowed_tools" {
        return;
    }
    let Some(allowed) = at(&choice, "tools").and_then(Value::as_array) else { return };
    let filtered: Vec<Value> = allowed.iter().filter(|t| ts(t, "type") != tool_type).cloned().collect();
    if filtered.len() == allowed.len() {
        return;
    }
    if filtered.is_empty() {
        let mode = ts(&choice, "mode");
        let mode = if mode == "auto" { "auto" } else { "required" };
        keep_only_hosted_tools(body, tool_type);
        set_tool_choice_string(body, mode);
        return;
    }
    cpa_json::set(body, "tool_choice.tools", Value::Array(filtered));
}

fn keep_only_hosted_tools(body: &mut Value, tool_type: &str) {
    let Some(tools) = at(body, "tools").and_then(Value::as_array) else { return };
    let kept: Vec<Value> = tools.iter().filter(|t| ts(t, "type") == tool_type).cloned().collect();
    if kept.is_empty() || kept.len() == tools.len() {
        return;
    }
    cpa_json::set(body, "tools", Value::Array(kept));
}

fn tool_choice_requires_hosted_tool_only(body: &Value, tool_type: &str) -> bool {
    let choice = body.g("tool_choice");
    if choice.kind() != Kind::String {
        return false;
    }
    if !matches!(choice.str().as_str(), "required" | "auto") {
        return false;
    }
    let tools = items(body, "tools");
    !tools.is_empty() && tools.iter().all(|t| ts(t, "type") == tool_type)
}

/// Go: xaiToolChoiceRequiresHostedToolOnlyAny.
pub fn tool_choice_requires_hosted_tool_only_any(body: &Value) -> bool {
    tool_choice_requires_hosted_tool_only(body, IMAGE_GENERATION_TOOL_TYPE)
        || tool_choice_requires_hosted_tool_only(body, WEB_SEARCH_TOOL_TYPE)
}

fn set_tool_choice_string(body: &mut Value, value: &str) {
    cpa_json::set(body, "tool_choice", value);
}

// ---------------------------------------------------------------- orphaned tool choice

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolChoiceKey {
    tool_type: String,
    name: String,
}

/// Go: pruneXAIOrphanedToolChoice. Drops choices that point at tools normalization removed.
pub fn prune_orphaned_tool_choice(body: &mut Value) {
    if !body.is_object() {
        return;
    }
    let choice = body.g("tool_choice");
    if !choice.exists() {
        return;
    }
    let available = collect_available_tool_choice_keys(body);
    // auto / none / required are not tool references.
    if choice.kind() == Kind::String || !choice.is_object() {
        return;
    }
    let choice = choice.value();
    let choice_type = ts(&choice, "type");
    if choice_type == "allowed_tools" {
        prune_allowed_tools_choice(body, &available);
        return;
    }
    if choice_type.is_empty() {
        return;
    }
    if tool_choice_matches_available(&choice, &available) {
        return;
    }
    cpa_json::delete(body, "tool_choice");
}

fn prune_allowed_tools_choice(body: &mut Value, available: &HashSet<ToolChoiceKey>) {
    let Some(allowed) = at(body, "tool_choice.tools").and_then(Value::as_array) else {
        cpa_json::delete(body, "tool_choice");
        return;
    };
    let filtered: Vec<Value> = allowed.iter().filter(|t| tool_choice_matches_available(t, available)).cloned().collect();
    if filtered.len() == allowed.len() {
        return;
    }
    if filtered.is_empty() {
        cpa_json::delete(body, "tool_choice");
        return;
    }
    cpa_json::set(body, "tool_choice.tools", Value::Array(filtered));
}

fn collect_available_tool_choice_keys(body: &Value) -> HashSet<ToolChoiceKey> {
    let mut keys = HashSet::new();
    let mut collect = |tools: &[Value]| {
        for tool in tools {
            let tool_type = ts(tool, "type");
            if tool_type.is_empty() {
                continue;
            }
            let mut key = ToolChoiceKey { tool_type: tool_type.clone(), name: String::new() };
            if tool_type == FUNCTION_TOOL_TYPE || tool_type == CUSTOM_TOOL_TYPE {
                key.name = ts(tool, "name");
                if key.name.is_empty() {
                    continue;
                }
            }
            keys.insert(key);
        }
    };
    collect(items(body, "tools"));
    for item in items(body, "input") {
        if s(item, "type") == "additional_tools" {
            collect(items(item, "tools"));
        }
    }
    keys
}

fn tool_choice_matches_available(choice: &Value, available: &HashSet<ToolChoiceKey>) -> bool {
    let tool_type = ts(choice, "type");
    if tool_type.is_empty() {
        return false;
    }
    let mut key = ToolChoiceKey { tool_type: tool_type.clone(), name: String::new() };
    if tool_type == FUNCTION_TOOL_TYPE || tool_type == CUSTOM_TOOL_TYPE {
        key.name = ts(choice, "name");
        if key.name.is_empty() {
            return false;
        }
    }
    available.contains(&key)
}

// ---------------------------------------------------------------- tool counts and folding

fn count_flattened_tools(tools: &[Value]) -> usize {
    tools
        .iter()
        .map(|tool| match s(tool, "type").as_str() {
            NAMESPACE_TOOL_TYPE => match at(tool, "tools").and_then(Value::as_array) {
                Some(nested) => nested.len(),
                None => 1,
            },
            // Tool search is stripped by normalize_tool.
            TOOL_SEARCH_TYPE => 0,
            _ => 1,
        })
        .sum()
}

fn total_flattened_tools_count(body: &Value, will_inject_x_search: bool) -> usize {
    let mut count = count_flattened_tools(items(body, "tools"));
    for item in items(body, "input") {
        if s(item, "type") == "additional_tools" {
            count += count_flattened_tools(items(item, "tools"));
        }
    }
    if will_inject_x_search && !request_has_native_x_search(body) && !tool_choice_requires_hosted_tool_only_any(body) {
        count += 1;
    }
    count
}

/// Go: xaiShouldFoldNamespaceTools. Folding turns each namespace into one dispatcher function
/// when flattening would exceed the upstream cap.
pub fn should_fold_namespace_tools(body: &Value, will_inject_x_search: bool) -> bool {
    total_flattened_tools_count(body, will_inject_x_search) > MAX_TOOLS
}

/// Go: buildXAINamespaceDispatcherTool. Keys are inserted in sorted order, as Go's map
/// marshalling emits them.
fn build_namespace_dispatcher_tool(tool: &Value) -> Option<Value> {
    let namespace_name = ts(tool, "name");
    if namespace_name.is_empty() {
        return None;
    }
    let description = ts(tool, "description");
    let mut tool_names: Vec<String> = Vec::new();
    let mut tool_descriptions: Vec<String> = Vec::new();
    for child in items(tool, "tools") {
        let child_name = ts(child, "name");
        if child_name.is_empty() {
            continue;
        }
        tool_names.push(child_name.clone());
        let child_desc = ts(child, "description");
        let mut params = child.g("parameters");
        if !params.exists() {
            params = child.g("input_schema");
        }
        let mut param_str = String::new();
        if params.exists() {
            let raw_params = params.raw().trim().to_string();
            if !raw_params.is_empty() && raw_params != "{}" && raw_params != r#"{"type":"object","properties":{}}"# {
                let inlined = cpa_core::util::inline_local_refs(&raw_params);
                if cpa_json::valid(inlined.as_bytes()) {
                    let mut cleaned = cpa_json::parse_str(&inlined);
                    cpa_json::delete(&mut cleaned, "$defs");
                    cpa_json::delete(&mut cleaned, "definitions");
                    param_str = compact(&cleaned);
                } else {
                    param_str = inlined;
                }
            }
        }
        let entry = match (child_desc.is_empty(), param_str.is_empty()) {
            (false, false) => format!("- {child_name}: {child_desc}\n  Parameters: {param_str}"),
            (false, true) => format!("- {child_name}: {child_desc}"),
            (true, false) => format!("- {child_name}\n  Parameters: {param_str}"),
            (true, true) => format!("- {child_name}"),
        };
        tool_descriptions.push(entry);
    }
    let mut full_description = description;
    if !tool_descriptions.is_empty() {
        let catalog = format!("Available tools in this namespace:\n{}", tool_descriptions.join("\n"));
        if !full_description.is_empty() {
            full_description.push_str("\n\n");
            full_description.push_str(&catalog);
        } else {
            full_description = format!("Tools in namespace {namespace_name}.\n\n{catalog}");
        }
    } else if full_description.is_empty() {
        full_description = format!("Tools in namespace {namespace_name}.");
    }
    let mut name_prop = json!({
        "description": format!("Child tool name to execute in namespace {namespace_name}"),
    });
    if !tool_names.is_empty() {
        name_prop["enum"] = json!(tool_names);
    }
    name_prop["type"] = json!("string");
    Some(json!({
        "description": full_description,
        "name": namespace_name,
        "parameters": {
            "properties": {
                "arguments": {
                    "additionalProperties": true,
                    "description": "Arguments object matching the parameter schema of the selected child tool",
                    "type": "object",
                },
                "name": name_prop,
            },
            "required": ["name"],
            "type": "object",
        },
        "type": FUNCTION_TOOL_TYPE,
    }))
}

// ---------------------------------------------------------------- tool normalization

/// Outcome of normalizing one declaration: `None` aborts the whole pass (Go's `ok == false`).
type ToolOutcome = Option<(Option<Value>, bool)>;

/// Go: normalizeXAITool.
fn normalize_tool(tool: &Value, namespace_name: &str, keep_image_generation: bool) -> ToolOutcome {
    let mut tool_type = s(tool, "type");
    let mut changed = false;
    if tool_type == TOOL_SEARCH_TYPE {
        return Some((None, true));
    }
    if tool_type == IMAGE_GENERATION_TOOL_TYPE && !keep_image_generation {
        return Some((None, true));
    }
    let mut raw = tool.clone();
    let mut schema_tool = tool.clone();
    if tool_type == FUNCTION_TOOL_TYPE || tool_type == CUSTOM_TOOL_TYPE {
        if let Some(params) = at(&schema_tool, "parameters") {
            let raw_params = compact(params);
            let inlined = cpa_core::util::inline_local_refs(&raw_params);
            if inlined != raw_params && cpa_json::set_raw(&mut raw, "parameters", &inlined).is_ok() {
                cpa_json::delete(&mut raw, "parameters.$defs");
                cpa_json::delete(&mut raw, "parameters.definitions");
                schema_tool = raw.clone();
                changed = true;
            }
        }
        let schema_changed = normalize_object_root_union_branch_types(&mut raw);
        if schema_changed {
            schema_tool = raw.clone();
            changed = true;
            tracing::debug!(
                "xai: added object types to root union branches for tool {namespace_name}.{}",
                s(tool, "name")
            );
        }
    }
    if tool_type == CUSTOM_TOOL_TYPE {
        cpa_json::set(&mut raw, "type", FUNCTION_TOOL_TYPE);
        tool_type = FUNCTION_TOOL_TYPE.to_string();
        changed = true;
    }
    if tool_type == WEB_SEARCH_TOOL_TYPE && exists(tool, "external_web_access") {
        cpa_json::delete(&mut raw, "external_web_access");
        changed = true;
    }
    if tool_type == FUNCTION_TOOL_TYPE && !exists(&schema_tool, "parameters") {
        cpa_json::set(&mut raw, "parameters", json!({"type": "object", "properties": {}}));
        changed = true;
    }
    // Simplify the Codex Desktop automation schema and root unions xAI rejects because
    // function parameters must resolve exclusively to objects.
    if tool_type == FUNCTION_TOOL_TYPE && function_parameters_need_simplification(&schema_tool, namespace_name) {
        let _ = cpa_json::set_raw(&mut raw, "parameters", SAFE_FUNCTION_PARAMETERS);
        if tool.g("strict").exists() && tool.g("strict").bool() {
            cpa_json::set(&mut raw, "strict", false);
        }
        changed = true;
        tracing::debug!(
            "xai: simplified parameters for tool {namespace_name}.{} to avoid upstream schema rejection or hang",
            s(tool, "name")
        );
    }
    if tool_type == FUNCTION_TOOL_TYPE && !namespace_name.trim().is_empty() {
        let qualified = qualify_namespace_tool_name(namespace_name, &s(tool, "name"));
        if qualified.is_empty() {
            return None;
        }
        cpa_json::set(&mut raw, "name", qualified);
        changed = true;
    }
    Some((Some(raw), changed))
}

/// Go: normalizeXAIObjectRootUnionBranchTypes. Makes untyped root union branches object-only
/// when the parameter root already is. Returns whether anything changed.
fn normalize_object_root_union_branch_types(tool: &mut Value) -> bool {
    let parameters = tool.g("parameters").value();
    let root_type = parameters.g("type");
    if root_type.kind() != Kind::String || root_type.str() != "object" {
        return false;
    }
    let mut changed = false;
    for union_name in ["anyOf", "oneOf"] {
        let Some(branches) = at(&parameters, union_name).and_then(Value::as_array) else { continue };
        for (index, branch) in branches.iter().enumerate() {
            if !branch.is_object() || exists(branch, "type") || exists(branch, "$ref") {
                continue;
            }
            cpa_json::set(tool, &format!("parameters.{union_name}.{index}.type"), "object");
            changed = true;
        }
    }
    changed
}

fn schema_type_is_object_only(schema_type: &cpa_json::Res<'_>) -> bool {
    if schema_type.kind() == Kind::String {
        return schema_type.str().trim().eq_ignore_ascii_case("object");
    }
    if !schema_type.is_array() {
        return false;
    }
    let types = schema_type.array();
    if types.is_empty() {
        return false;
    }
    types.iter().all(|t| t.kind() == Kind::String && t.str().trim().eq_ignore_ascii_case("object"))
}

fn is_codex_app_automation_update(tool_name: &str, namespace_name: &str) -> bool {
    let namespace = namespace_name.trim();
    let tool = tool_name.trim();
    let clean_namespace = namespace.strip_prefix("mcp__").unwrap_or(namespace);
    let clean_tool = tool.strip_prefix("mcp__").unwrap_or(tool);
    if clean_tool.eq_ignore_ascii_case(AUTOMATION_UPDATE_TOOL_NAME)
        && (clean_namespace.eq_ignore_ascii_case(CODEX_APP_NAMESPACE_NAME) || clean_namespace.eq_ignore_ascii_case("codex_apps"))
    {
        return true;
    }
    clean_tool.eq_ignore_ascii_case(&format!("{CODEX_APP_NAMESPACE_NAME}__{AUTOMATION_UPDATE_TOOL_NAME}"))
        || clean_tool.eq_ignore_ascii_case(&format!("codex_apps__{AUTOMATION_UPDATE_TOOL_NAME}"))
}

/// Go: xaiFunctionParametersNeedSimplification.
fn function_parameters_need_simplification(tool: &Value, namespace_name: &str) -> bool {
    let tool_type = ts(tool, "type");
    let is_function = tool_type.eq_ignore_ascii_case(FUNCTION_TOOL_TYPE);
    let is_normalized_custom = tool_type.eq_ignore_ascii_case(CUSTOM_TOOL_TYPE);
    if !is_function && !is_normalized_custom {
        return false;
    }
    if is_function && is_codex_app_automation_update(&ts(tool, "name"), namespace_name) {
        return true;
    }
    let parameters = tool.g("parameters").value();
    for union_name in ["anyOf", "oneOf"] {
        for branch in items(&parameters, union_name) {
            if exists(branch, "$ref") || !schema_type_is_object_only(&branch.g("type")) {
                return true;
            }
        }
    }
    false
}

/// Go: qualifyXAINamespaceToolName (`<namespace>__<tool>`, MCP tools stay as they are).
pub fn qualify_namespace_tool_name(namespace_name: &str, tool_name: &str) -> String {
    let namespace_name = namespace_name.trim();
    let tool_name = tool_name.trim();
    if namespace_name.is_empty() || tool_name.is_empty() || tool_name.starts_with("mcp__") {
        return tool_name.to_string();
    }
    let mut prefix = namespace_name.to_string();
    if !prefix.ends_with("__") {
        prefix.push_str("__");
    }
    if tool_name.starts_with(&prefix) {
        return tool_name.to_string();
    }
    format!("{prefix}{tool_name}")
}

/// Go: normalizeXAIToolArray. `None` aborts; `Some(None)` leaves the array alone.
fn normalize_tool_array(
    tools: &[Value],
    keep_image_generation: bool,
    should_fold: bool,
) -> Option<Option<Vec<Value>>> {
    let mut filtered: Vec<Value> = Vec::with_capacity(tools.len());
    let mut changed = false;
    for tool in tools {
        if s(tool, "type") == NAMESPACE_TOOL_TYPE {
            changed = true;
            if should_fold {
                if let Some(dispatcher) = build_namespace_dispatcher_tool(tool) {
                    filtered.push(dispatcher);
                }
                continue;
            }
            let namespace_name = s(tool, "name");
            for nested in items(tool, "tools") {
                let (raw, nested_changed) = normalize_tool(nested, &namespace_name, keep_image_generation)?;
                changed = changed || nested_changed;
                if let Some(raw) = raw {
                    filtered.push(raw);
                }
            }
            continue;
        }
        let (raw, tool_changed) = normalize_tool(tool, "", keep_image_generation)?;
        changed = changed || tool_changed;
        if let Some(raw) = raw {
            filtered.push(raw);
        }
    }
    Some(changed.then_some(filtered))
}

/// Go: normalizeXAIToolsWithFold. Normalizes `tools` and every `additional_tools` item; any
/// aborted pass leaves the body untouched.
pub fn normalize_tools_with_fold(body: &mut Value, should_fold: bool) {
    if !body.is_object() {
        return;
    }
    let keep_image_generation = supports_native_image_generation(&s(body, "model"));
    let mut updates: Vec<(String, Vec<Value>)> = Vec::new();
    if let Some(tools) = at(body, "tools").and_then(Value::as_array) {
        match normalize_tool_array(tools, keep_image_generation, should_fold) {
            None => return,
            Some(None) => {}
            Some(Some(filtered)) => updates.push(("tools".to_string(), filtered)),
        }
    }
    for (index, item) in items(body, "input").iter().enumerate() {
        if s(item, "type") != "additional_tools" {
            continue;
        }
        let Some(tools) = at(item, "tools").and_then(Value::as_array) else { continue };
        match normalize_tool_array(tools, keep_image_generation, should_fold) {
            None => return,
            Some(None) => {}
            Some(Some(filtered)) => updates.push((format!("input.{index}.tools"), filtered)),
        }
    }
    for (path, filtered) in updates {
        cpa_json::set(body, &path, Value::Array(filtered));
    }
}

/// Go: xaiHasFunctionToolNamed.
fn has_function_tool_named(body: &Value, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let is_match = |tool: &Value| s(tool, "type") == FUNCTION_TOOL_TYPE && s(tool, "name") == name;
    if items(body, "tools").iter().any(is_match) {
        return true;
    }
    items(body, "input")
        .iter()
        .filter(|item| s(item, "type") == "additional_tools")
        .any(|item| items(item, "tools").iter().any(is_match))
}

/// Go: clampXAIToolsLimit. Keeps dispatcher tools first, then regular ones up to the cap.
pub fn clamp_tools_limit(body: &mut Value, max_tools: usize, refs: &NamespaceRefs) {
    let Some(tools) = at(body, "tools").and_then(Value::as_array) else { return };
    if tools.len() <= max_tools {
        return;
    }
    let (mut dispatchers, mut regular) = (Vec::new(), Vec::new());
    for tool in tools {
        let name = ts(tool, "name");
        if refs.get(&name).is_some_and(|r| r.is_dispatcher) {
            dispatchers.push(tool.clone());
        } else {
            regular.push(tool.clone());
        }
    }
    let mut capped: Vec<Value> = Vec::with_capacity(max_tools);
    if dispatchers.len() >= max_tools {
        capped.extend(dispatchers.into_iter().take(max_tools));
    } else {
        let remaining = max_tools - dispatchers.len();
        capped.extend(dispatchers);
        capped.extend(regular.into_iter().take(remaining));
    }
    cpa_json::set(body, "tools", Value::Array(capped));
    prune_orphaned_tool_choice(body);
    normalize_tool_choice_for_tools(body);
}

/// Go: promoteXAIAdditionalTools. xAI has no `additional_tools` input items, so their tools
/// move to the top-level array.
pub fn promote_additional_tools(body: &mut Value) {
    if !body.is_object() {
        return;
    }
    let Some(input) = at(body, "input").and_then(Value::as_array) else { return };
    let mut remaining: Vec<Value> = Vec::with_capacity(input.len());
    let mut promoted: Vec<Value> = Vec::new();
    for item in input {
        if s(item, "type") != "additional_tools" {
            remaining.push(item.clone());
            continue;
        }
        promoted.extend(items(item, "tools").iter().cloned());
    }
    if remaining.len() == input.len() {
        return;
    }
    cpa_json::set(body, "input", Value::Array(remaining));
    if promoted.is_empty() {
        return;
    }
    let mut tools: Vec<Value> = items(body, "tools").to_vec();
    tools.extend(promoted);
    cpa_json::set(body, "tools", Value::Array(tools));
}

/// Go: normalizeXAIToolChoiceForTools. xAI rejects `tool_choice` (and `parallel_tool_calls`)
/// when no tools are defined.
pub fn normalize_tool_choice_for_tools(body: &mut Value) {
    let mut has_tools = !items(body, "tools").is_empty();
    if !has_tools {
        has_tools = items(body, "input")
            .iter()
            .any(|item| s(item, "type") == "additional_tools" && !items(item, "tools").is_empty());
    }
    if has_tools {
        return;
    }
    if exists(body, "tools") {
        cpa_json::delete(body, "tools");
    }
    if exists(body, "tool_choice") {
        cpa_json::delete(body, "tool_choice");
    }
    if exists(body, "parallel_tool_calls") {
        cpa_json::delete(body, "parallel_tool_calls");
    }
}

/// Go: normalizeXAINamespaceToolChoiceWithFold. Qualifies namespaced function choices with the
/// names sent in the flattened tools list (xAI has no `namespace` field on choices).
pub fn normalize_namespace_tool_choice_with_fold(body: &mut Value, should_fold: bool) {
    if !body.is_object() {
        return;
    }
    let mut paths = vec!["tool_choice".to_string()];
    let count = items(body, "tool_choice.tools").len();
    paths.extend((0..count).map(|i| format!("tool_choice.tools.{i}")));
    for path in paths {
        let Some(choice) = at(body, &path).filter(|c| c.is_object()).cloned() else { continue };
        if s(&choice, "type") != FUNCTION_TOOL_TYPE {
            continue;
        }
        let namespace_name = ts(&choice, "namespace");
        let tool_name = ts(&choice, "name");
        if namespace_name.is_empty() {
            continue;
        }
        let qualified = qualify_namespace_tool_name(&namespace_name, &tool_name);
        let target = if has_function_tool_named(body, &namespace_name) {
            namespace_name.clone()
        } else if has_function_tool_named(body, &qualified) {
            qualified
        } else if should_fold {
            namespace_name.clone()
        } else {
            qualified
        };
        if target.is_empty() {
            continue;
        }
        cpa_json::set(body, &format!("{path}.name"), target);
        cpa_json::delete(body, &format!("{path}.namespace"));
    }
}

/// Go: collectXAINamespaceToolRefsWithFold.
pub fn collect_namespace_tool_refs_with_fold(body: &Value, should_fold: bool) -> NamespaceRefs {
    let mut refs = NamespaceRefs::new();
    let mut collect = |tools: &[Value]| {
        for tool in tools {
            if s(tool, "type") != NAMESPACE_TOOL_TYPE {
                continue;
            }
            let namespace_name = ts(tool, "name");
            if namespace_name.is_empty() {
                continue;
            }
            if should_fold {
                refs.insert(
                    namespace_name.clone(),
                    NamespaceToolRef { namespace: namespace_name.clone(), name: String::new(), is_dispatcher: true },
                );
            }
            for nested in items(tool, "tools") {
                let tool_name = ts(nested, "name");
                let qualified = qualify_namespace_tool_name(&namespace_name, &tool_name);
                if qualified.is_empty() {
                    continue;
                }
                refs.insert(
                    qualified,
                    NamespaceToolRef { namespace: namespace_name.clone(), name: tool_name, is_dispatcher: false },
                );
            }
        }
    };
    collect(items(body, "tools"));
    for item in items(body, "input") {
        if s(item, "type") == "additional_tools" {
            collect(items(item, "tools"));
        }
    }
    refs
}

/// Go: collectXAIClientDeclaredToolKeys. Must run before normalization flattens namespaces.
pub fn collect_client_declared_tool_keys(body: &Value) -> HashSet<ClientToolKey> {
    let mut keys = HashSet::new();
    let effective = |tool_type: &str| {
        let t = tool_type.trim();
        if t == CUSTOM_TOOL_TYPE { FUNCTION_TOOL_TYPE.to_string() } else { t.to_string() }
    };
    let mut collect = |tools: &[Value]| {
        for tool in tools {
            let tool_type = ts(tool, "type");
            match tool_type.as_str() {
                NAMESPACE_TOOL_TYPE => {
                    let namespace_name = ts(tool, "name");
                    if namespace_name.is_empty() {
                        continue;
                    }
                    for nested in items(tool, "tools") {
                        let nested_type = ts(nested, "type");
                        if nested_type != FUNCTION_TOOL_TYPE && nested_type != CUSTOM_TOOL_TYPE {
                            continue;
                        }
                        let tool_name = ts(nested, "name");
                        if tool_name.is_empty() {
                            continue;
                        }
                        keys.insert(ClientToolKey {
                            namespace: namespace_name.clone(),
                            name: tool_name,
                            tool_type: effective(&nested_type),
                        });
                    }
                }
                FUNCTION_TOOL_TYPE | CUSTOM_TOOL_TYPE => {
                    let tool_name = ts(tool, "name");
                    if tool_name.is_empty() {
                        continue;
                    }
                    keys.insert(ClientToolKey {
                        namespace: String::new(),
                        name: tool_name,
                        tool_type: effective(&tool_type),
                    });
                }
                _ => {}
            }
        }
    };
    collect(items(body, "tools"));
    for item in items(body, "input") {
        if s(item, "type") == "additional_tools" {
            collect(items(item, "tools"));
        }
    }
    keys
}

// ---------------------------------------------------------------- input history

/// Go: normalizeXAIInputCustomToolCalls. xAI has no custom tools, so replayed custom calls and
/// outputs become function calls and outputs; unusable ones are dropped.
pub fn normalize_input_custom_tool_calls(body: &mut Value) {
    let Some(input) = at(body, "input").and_then(Value::as_array) else { return };
    let mut changed = false;
    let mut out: Vec<Value> = Vec::with_capacity(input.len());
    for item in input {
        let normalized = match s(item, "type").as_str() {
            "custom_tool_call" => {
                let call_id = ts(item, "call_id");
                let name = ts(item, "name");
                if call_id.is_empty() || name.is_empty() {
                    changed = true;
                    continue;
                }
                let mut n = json!({"type": "function_call"});
                cpa_json::set(&mut n, "call_id", call_id);
                cpa_json::set(&mut n, "name", name);
                cpa_json::set(&mut n, "arguments", custom_tool_call_arguments(&item.g("input")));
                n
            }
            "custom_tool_call_output" => {
                let call_id = ts(item, "call_id");
                if call_id.is_empty() {
                    changed = true;
                    continue;
                }
                let mut n = json!({"type": "function_call_output"});
                cpa_json::set(&mut n, "call_id", call_id);
                cpa_json::set(&mut n, "output", custom_tool_call_output(&item.g("output")));
                n
            }
            _ => {
                out.push(item.clone());
                continue;
            }
        };
        out.push(normalized);
        changed = true;
    }
    if changed {
        cpa_json::set(body, "input", Value::Array(out));
    }
}

fn custom_tool_call_arguments(input: &cpa_json::Res<'_>) -> String {
    if !input.exists() {
        return "{}".into();
    }
    if input.kind() == Kind::String {
        let text = input.str();
        let trimmed = text.trim();
        if cpa_json::valid(trimmed.as_bytes()) {
            let parsed = cpa_json::parse_str(trimmed);
            if parsed.is_object() {
                return compact(&parsed);
            }
        }
        return format!(r#"{{"input":{}}}"#, cpa_core::util::go_json_string(&text));
    }
    if input.is_object() {
        return input.raw();
    }
    let raw = input.raw();
    if !raw.is_empty() {
        return format!(r#"{{"input":{raw}}}"#);
    }
    "{}".into()
}

fn custom_tool_call_output(output: &cpa_json::Res<'_>) -> String {
    if !output.exists() {
        return String::new();
    }
    if output.kind() == Kind::String { output.str() } else { output.raw() }
}

/// Go: normalizeXAIInputNamespaceToolCallsWithFold. Replayed namespaced function calls are
/// folded into dispatcher calls or renamed to their qualified flat name.
pub fn normalize_input_namespace_tool_calls_with_fold(body: &mut Value, should_fold: bool) {
    if !body.is_object() {
        return;
    }
    let snapshot: Vec<Value> = items(body, "input").to_vec();
    for (index, item) in snapshot.iter().enumerate() {
        if s(item, "type") != "function_call" {
            continue;
        }
        let namespace_name = ts(item, "namespace");
        let tool_name = ts(item, "name");
        if namespace_name.is_empty() {
            continue;
        }
        let qualified = qualify_namespace_tool_name(&namespace_name, &tool_name);
        let is_folded = if has_function_tool_named(body, &namespace_name) {
            true
        } else if has_function_tool_named(body, &qualified) {
            false
        } else {
            should_fold
        };
        let name_path = format!("input.{index}.name");
        let namespace_path = format!("input.{index}.namespace");
        if is_folded {
            let raw_args = item.g("arguments").str();
            let encoded = dispatcher_arguments_json(&tool_name, &raw_args);
            cpa_json::set(body, &name_path, namespace_name);
            cpa_json::set(body, &format!("input.{index}.arguments"), encoded);
            cpa_json::delete(body, &namespace_path);
            continue;
        }
        if qualified.is_empty() {
            continue;
        }
        cpa_json::set(body, &name_path, qualified);
        cpa_json::delete(body, &namespace_path);
    }
}

/// `json.Marshal(map{"name": child, "arguments": raw-or-string})`: sorted keys, valid JSON
/// arguments embedded as-is, anything else as a string.
fn dispatcher_arguments_json(tool_name: &str, raw_args: &str) -> String {
    let name = cpa_core::util::go_json_string(tool_name);
    if raw_args.is_empty() {
        return format!(r#"{{"name":{name}}}"#);
    }
    if cpa_json::valid(raw_args.as_bytes()) {
        let parsed = cpa_json::parse_str(raw_args);
        return format!(r#"{{"arguments":{},"name":{name}}}"#, compact(&parsed));
    }
    format!(r#"{{"arguments":{},"name":{name}}}"#, cpa_core::util::go_json_string(raw_args))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Table of Go's TestXAISupportsNativeImageGeneration.
    #[test]
    fn native_image_generation_by_model() {
        for (model, want) in [
            ("", false), ("grok-4.5", false), ("grok-4.3", false), ("grok-4", false),
            ("grok-4.20-0309-reasoning", false), ("grok-4.20-multi-agent-0309", false), ("grok-build-0.1", false),
            ("grok-composer-2.5-fast", false), ("grok-3-mini", false), ("gpt-5.6", false),
            ("grok-4.6", true), ("grok-4.6(high)", true), ("xai/grok-4.6", true), ("grok-4.7", true),
            ("grok-5", true), ("grok-5.0", true),
        ] {
            assert_eq!(supports_native_image_generation(model), want, "{model}");
        }
    }

    #[test]
    fn namespace_names_are_qualified_once() {
        assert_eq!(qualify_namespace_tool_name("ns", "tool"), "ns__tool");
        assert_eq!(qualify_namespace_tool_name("ns__", "tool"), "ns__tool");
        assert_eq!(qualify_namespace_tool_name("ns", "ns__tool"), "ns__tool");
        assert_eq!(qualify_namespace_tool_name("ns", "mcp__x"), "mcp__x");
    }
}

//! Responses tool declarations and their Claude names
//! (Go: claude_openai-responses_tool_names.go and the tool descriptor helpers of
//! claude_openai-responses_request.go).
//!
//! Claude tool names must match `^[a-zA-Z0-9_-]{1,64}$`. [`ClaudeToolNames`] maps qualified
//! Responses tool identities to unique Claude names for one request and back; the request and
//! response translators build it from the same Responses JSON, so both sides agree.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use cpa_core::util::sanitize_claude_function_name;
use cpa_json::{J, Res, Value};
use regex::Regex;
use sha2::{Digest, Sha256};

static CLAUDE_TOOL_NAME_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]{1,64}$").expect("static regex"));

#[derive(Debug, Default, Clone)]
pub(super) struct ClaudeToolNames {
    to_claude: HashMap<String, String>,
    from_claude: HashMap<String, String>,
}

/// One declared tool: a top-level or `additional_tools` entry, or a namespace child.
#[derive(Debug, Clone)]
pub(super) struct ToolDescriptor {
    /// Qualified Responses identity.
    pub name: String,
    pub child_name: String,
    pub namespace: String,
    pub tool_type: String,
    pub tool: Value,
    pub source_priority: u8,
    pub direct: bool,
    pub order: usize,
}

pub(super) type ToolWinners = HashMap<String, ToolDescriptor>;

pub(super) fn build_claude_tool_names(root: &Value) -> ClaudeToolNames {
    build_claude_tool_names_with_winners(root, &responses_tool_winners(root))
}

pub(super) fn build_claude_tool_names_with_winners(root: &Value, winners: &ToolWinners) -> ClaudeToolNames {
    let mut m = ClaudeToolNames::default();
    let mut taken: HashSet<String> = HashSet::new();
    let mut declared: Vec<String> = Vec::new();
    for d in responses_tool_descriptors(root) {
        if winners.get(&d.name).is_none_or(|w| w.order != d.order) {
            continue;
        }
        match d.tool_type.as_str() {
            "function" | "custom" => declared.push(d.name),
            _ => m.assign(&d.name, &d.name, &mut taken),
        }
    }
    m.allocate(&declared, &mut taken);
    m.allocate(&responses_history_tool_identities(root), &mut taken);
    m
}

impl ClaudeToolNames {
    /// The Claude name for a qualified Responses identity.
    pub fn claude_name(&self, identity: &str) -> String {
        match self.to_claude.get(identity) {
            Some(name) => name.clone(),
            None => sanitize_claude_function_name(identity),
        }
    }

    /// The qualified Responses identity for a Claude name; unknown names come back unchanged.
    pub fn identity(&self, claude_name: &str) -> String {
        match self.from_claude.get(claude_name) {
            Some(identity) => identity.clone(),
            None => claude_name.to_string(),
        }
    }

    fn assign(&mut self, identity: &str, name: &str, taken: &mut HashSet<String>) {
        self.to_claude.insert(identity.to_string(), name.to_string());
        self.from_claude.insert(name.to_string(), identity.to_string());
        taken.insert(name.to_string());
    }

    /// Names every identity: valid, untaken names stay as they are; others are sanitized when
    /// that is unique, else sanitized, truncated and suffixed with a hash of the identity.
    fn allocate(&mut self, identities: &[String], taken: &mut HashSet<String>) {
        let mut changed: Vec<&String> = Vec::new();
        let mut seen: HashSet<&String> = HashSet::new();
        for id in identities {
            if self.to_claude.contains_key(id) || id.is_empty() || seen.contains(id) {
                continue;
            }
            seen.insert(id);
            if CLAUDE_TOOL_NAME_PATTERN.is_match(id) && !taken.contains(id) {
                self.assign(id, id, taken);
                continue;
            }
            changed.push(id);
        }
        let mut count: HashMap<String, usize> = HashMap::new();
        for id in &changed {
            *count.entry(sanitize_claude_function_name(id)).or_default() += 1;
        }
        let mut hashed: Vec<&String> = Vec::new();
        for id in changed {
            let base = sanitize_claude_function_name(id);
            if count[&base] == 1 && !taken.contains(&base) {
                self.assign(id, &base, taken);
            } else {
                hashed.push(id);
            }
        }
        hashed.sort();
        for id in hashed {
            let mut base = sanitize_claude_function_name(id);
            base.truncate(53); // ASCII only after sanitizing
            for n in 0usize.. {
                let seed = if n > 0 { format!("{id}\0{n}") } else { id.clone() };
                let sum = hex::encode(Sha256::digest(seed.as_bytes()));
                let name = format!("{base}_{}", &sum[..10]);
                if !taken.contains(&name) {
                    self.assign(id, &name, taken);
                    break;
                }
            }
        }
    }
}

/// Qualified names of `function_call` and `custom_tool_call` items in `input`, in order.
fn responses_history_tool_identities(root: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    let input = root.g("input");
    if input.is_array() {
        for item in input.array() {
            if matches!(item.g("type").str().as_str(), "function_call" | "custom_tool_call") {
                let mut name = item.g("name").str();
                let ns = item.g("namespace").str();
                let ns = ns.trim();
                if !ns.is_empty() {
                    name = qualify_responses_namespace_tool_name(ns, &name);
                }
                if !name.is_empty() {
                    ids.push(name);
                }
            }
        }
    }
    ids
}

/// Joins a namespace and child tool name the way the Responses translators emit them.
pub(super) fn qualify_responses_namespace_tool_name(namespace_name: &str, child_name: &str) -> String {
    let child_name = child_name.trim();
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

pub(super) fn is_unsupported_openai_builtin_tool_type(tool_type: &str) -> bool {
    matches!(tool_type, "image_generation" | "file_search" | "code_interpreter" | "computer_use_preview")
}

pub(super) fn responses_tool_name(tool: &Res<'_>) -> String {
    let name = tool.g("name").str().trim().to_string();
    if !name.is_empty() {
        return name;
    }
    tool.g("function.name").str().trim().to_string()
}

pub(super) fn responses_tool_description(tool: &Res<'_>) -> String {
    let description = tool.g("description").str();
    if !description.is_empty() {
        return description;
    }
    tool.g("function.description").str()
}

/// The tool's parameter schema, or a missing result.
pub(super) fn responses_tool_parameters<'a>(tool: &'a Res<'_>) -> Res<'a> {
    for path in
        ["parameters", "parametersJsonSchema", "input_schema", "function.parameters", "function.parametersJsonSchema"]
    {
        let parameters = tool.g(path);
        if parameters.exists() {
            return parameters;
        }
    }
    Res::NONE
}

/// Tool arrays: top-level `tools` (priority 0), then every `additional_tools` item (priority 1).
fn responses_tool_sources(root: &Value) -> Vec<(Value, u8)> {
    let mut sources = Vec::new();
    let tools = root.g("tools");
    if tools.is_array() {
        sources.push((tools.value(), 0));
    }
    let input = root.g("input");
    if input.is_array() {
        for item in input.array() {
            if item.g("type").str() == "additional_tools" {
                let tools = item.g("tools");
                if tools.is_array() {
                    sources.push((tools.value(), 1));
                }
            }
        }
    }
    sources
}

/// Pushes a descriptor; `child` is `(child name, namespace)` for namespace children (which are
/// not direct declarations). Entries without a name are skipped.
fn append(
    descriptors: &mut Vec<ToolDescriptor>,
    tool: &Value,
    name: String,
    tool_type: &str,
    source_priority: u8,
    child: Option<(&str, &str)>,
) {
    if name.is_empty() {
        return;
    }
    let order = descriptors.len();
    let (child_name, namespace) = child.unwrap_or_default();
    descriptors.push(ToolDescriptor {
        name,
        child_name: child_name.to_string(),
        namespace: namespace.to_string(),
        tool_type: tool_type.to_string(),
        tool: tool.clone(),
        source_priority,
        direct: child.is_none(),
        order,
    });
}

/// Every declared tool in source order; `order` is the position in this list.
pub(super) fn responses_tool_descriptors(root: &Value) -> Vec<ToolDescriptor> {
    let mut descriptors: Vec<ToolDescriptor> = Vec::new();
    for (tools, priority) in responses_tool_sources(root) {
        let Value::Array(items) = &tools else { continue };
        for tool in items {
            let tool_res = Res::of(tool);
            let tool_type = tool_res.g("type").str().trim().to_string();
            match tool_type.as_str() {
                "" | "function" => {
                    append(&mut descriptors, tool, responses_tool_name(&tool_res), "function", priority, None)
                }
                "custom" => append(&mut descriptors, tool, responses_tool_name(&tool_res), "custom", priority, None),
                "namespace" => {
                    let namespace_name = tool_res.g("name").str().trim().to_string();
                    let children = tool_res.g("tools");
                    if !children.is_array() {
                        continue;
                    }
                    for child in children.array() {
                        let child_name = responses_tool_name(&child);
                        if child_name.is_empty() {
                            continue;
                        }
                        let qualified = qualify_responses_namespace_tool_name(&namespace_name, &child_name);
                        let Some(child_value) = child.v() else { continue };
                        match child.g("type").str().trim() {
                            "" | "function" => append(
                                &mut descriptors,
                                child_value,
                                qualified,
                                "function",
                                priority,
                                Some((&child_name, &namespace_name)),
                            ),
                            "custom" => append(
                                &mut descriptors,
                                child_value,
                                qualified,
                                "custom",
                                priority,
                                Some((&child_name, &namespace_name)),
                            ),
                            _ => {}
                        }
                    }
                }
                "web_search" => {
                    let external = tool_res.g("external_web_access");
                    if external.exists() && !external.bool() {
                        continue;
                    }
                    let mut name = tool_res.g("name").str().trim().to_string();
                    if name.is_empty() {
                        name = "web_search".to_string();
                    }
                    append(&mut descriptors, tool, name, "web_search", priority, None);
                }
                other => {
                    if is_unsupported_openai_builtin_tool_type(other) {
                        continue;
                    }
                    append(&mut descriptors, tool, tool_res.g("name").str().trim().to_string(), other, priority, None);
                }
            }
        }
    }
    descriptors
}

/// Top-level tools precede `additional_tools`; direct declarations beat namespace children
/// within the same source class; then declaration order.
fn descriptor_precedes(left: &ToolDescriptor, right: &ToolDescriptor) -> bool {
    if left.source_priority != right.source_priority {
        return left.source_priority < right.source_priority;
    }
    if left.direct != right.direct {
        return left.direct;
    }
    left.order < right.order
}

/// The winning declaration for each qualified name.
pub(super) fn responses_tool_winners(root: &Value) -> ToolWinners {
    let mut winners = ToolWinners::new();
    for descriptor in responses_tool_descriptors(root) {
        match winners.get(&descriptor.name) {
            Some(current) if !descriptor_precedes(&descriptor, current) => {}
            _ => {
                winners.insert(descriptor.name.clone(), descriptor);
            }
        }
    }
    winners
}

/// Alias map from client-facing tool names to included qualified names. Direct names are
/// canonical and win over namespace child aliases regardless of declaration order.
pub(super) fn responses_tool_name_map(root: &Value, accepted: &HashSet<String>) -> BTreeMap<String, String> {
    let mut tool_name_map = BTreeMap::new();
    let descriptors = responses_tool_descriptors(root);
    let winners = responses_tool_winners(root);

    for d in &descriptors {
        if winners.get(&d.name).is_none_or(|w| w.order != d.order) || !d.direct || !accepted.contains(&d.name) {
            continue;
        }
        tool_name_map.insert(d.name.clone(), d.name.clone());
    }
    for d in &descriptors {
        if winners.get(&d.name).is_none_or(|w| w.order != d.order)
            || d.direct
            || d.child_name.is_empty()
            || !accepted.contains(&d.name)
        {
            continue;
        }
        if tool_name_map.contains_key(&d.child_name) {
            continue;
        }
        tool_name_map.insert(d.child_name.clone(), d.name.clone());
    }
    tool_name_map
}

/// Resolves a qualified function call name from the original request to the local tool name and
/// namespace the client declared.
pub(super) fn split_responses_qualified_function_call(
    winners: &ToolWinners,
    names: &ClaudeToolNames,
    qualified_name: &str,
) -> (String, String) {
    let qualified_name = qualified_name.trim();
    if qualified_name.is_empty() {
        return (String::new(), String::new());
    }
    let identity = names.identity(qualified_name);
    let Some(descriptor) = winners.get(&identity) else {
        return (identity, String::new());
    };
    if !descriptor.direct {
        return (descriptor.child_name.clone(), descriptor.namespace.clone());
    }
    (identity, String::new())
}

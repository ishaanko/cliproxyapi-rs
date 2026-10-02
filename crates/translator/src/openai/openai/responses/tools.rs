//! Responses tool declarations and their Chat Completions names
//! (Go: openai_openai-responses_tools.go and responses_tool_index.go).

use std::collections::{HashMap, HashSet};

use cpa_core::applypatch;
use cpa_json::{Res, Value, J};

use super::RawSrc;
use crate::common;

/// Chat Completions function name limit enforced by strict upstreams (e.g. z-ai/glm). Responses
/// namespace tools routinely flatten to longer names.
const CHAT_TOOL_NAME_LIMIT: usize = 64;

/// One Responses tool declaration paired with the Chat Completions function name it produces.
/// Namespace children carry both their declared name and the owning namespace, so reverse
/// translation can restore the split identity.
#[derive(Clone)]
pub(super) struct Declaration {
    pub tool: Value,
    pub chat_name: String,
    pub local_name: String,
    pub namespace: String,
    pub custom: bool,
}

/// The tool declarations of a Responses request in one canonical order: the top-level `tools`
/// field first, then Codex Desktop (Responses Lite) `additional_tools` input items, namespace
/// children in declaration order. Declarations producing no Chat Completions tool are skipped.
///
/// The `chat_name` is namespace-qualified, capped to the Chat Completions limit, and
/// disambiguated when two distinct declarations flatten onto the same name. Request conversion,
/// reverse name resolution and freeform tool classification all go through here, so they cannot
/// disagree about which declaration backs a Chat Completions tool name.
fn walk_declarations(root: &Value) -> Vec<Declaration> {
    let mut declarations: Vec<Declaration> = Vec::new();

    let mut emit = |tool: &Res<'_>, namespace_name: &str| {
        let custom = match tool.g("type").str().trim() {
            "" | "function" => false,
            "custom" => true,
            _ => return,
        };
        let local_name = responses_tool_name(tool);
        if local_name.is_empty() {
            return;
        }
        declarations.push(Declaration {
            tool: tool.value(),
            chat_name: qualify_namespace_tool_name(namespace_name, &local_name),
            local_name,
            namespace: namespace_name.to_string(),
            custom,
        });
    };
    let mut scan = |tools: Res<'_>| {
        if !tools.is_array() {
            return;
        }
        for tool in tools.array() {
            if tool.g("type").str().trim() == "namespace" {
                let children = tool.g("tools");
                if children.is_array() {
                    let namespace_name = tool.g("name").str().trim().to_string();
                    for child in children.array() {
                        emit(&child, &namespace_name);
                    }
                }
                continue;
            }
            emit(&tool, "");
        }
    };

    scan(root.g("tools"));
    let input = root.g("input");
    if input.is_array() {
        for item in input.array() {
            if item.g("type").str() == "additional_tools" {
                scan(item.g("tools"));
            }
        }
    }

    disambiguate_chat_tool_names(&mut declarations);
    declarations
}

/// Rewrites flattened names in place when distinct declarations collapse onto the same capped
/// Chat Completions name. Identity is the pre-cap qualified name: declarations that qualified to
/// the same name before the cap (one tool delivered through both `tools` and `additional_tools`,
/// or a flat tool colliding with a namespace child) are the same upstream tool and keep the shared
/// first-wins name, while distinct names that only collide through truncation get `_1` style
/// suffixes so deduplication downstream never silently drops a real tool.
///
/// Qualified names that fit the cap unchanged are claimed before any truncation alias is assigned.
/// Local names that fit the cap are reserved the same way: a replayed call or tool_choice that
/// omits the namespace carries the local name, so a capped alias occupying that name would win
/// the exact-alias match and attribute those calls to the wrong tool. A local name carried by more
/// than one distinct identity is ambiguous and burned instead of awarded to the first declaration.
fn disambiguate_chat_tool_names(declarations: &mut [Declaration]) {
    let mut claimed: HashMap<String, String> = HashMap::with_capacity(declarations.len());
    let mut claim = |candidate: &str, identity: &str| -> bool {
        match claimed.get(candidate) {
            None => {
                claimed.insert(candidate.to_string(), identity.to_string());
                true
            }
            Some(owner) => owner == identity,
        }
    };
    let mut long_declarations: Vec<usize> = Vec::new();
    let mut identities: Vec<String> = Vec::with_capacity(declarations.len());
    // local name -> the single identity declaring it, or "" once a second distinct identity shows
    // the name is ambiguous.
    let mut local_owners: HashMap<String, String> = HashMap::new();
    let mut ambiguous_local_names: HashSet<String> = HashSet::new();
    for (i, d) in declarations.iter().enumerate() {
        let identity = raw_namespace_qualified_name(&d.namespace, &d.local_name);
        if identity.len() > CHAT_TOOL_NAME_LIMIT {
            long_declarations.push(i);
        } else {
            claim(&identity, &identity);
        }
        let local = &d.local_name;
        if local.is_empty() || *local == identity || local.len() > CHAT_TOOL_NAME_LIMIT {
            identities.push(identity);
            continue;
        }
        match local_owners.get(local) {
            None => {
                local_owners.insert(local.clone(), identity.clone());
            }
            Some(owner) if !owner.is_empty() && *owner != identity => {
                local_owners.insert(local.clone(), String::new());
            }
            Some(_) => {}
        }
        identities.push(identity);
    }
    for (local, owner) in &local_owners {
        // Reserving under any identity keeps the name out of every later truncation alias;
        // ambiguous names additionally never get emitted.
        claim(local, owner);
        if owner.is_empty() {
            ambiguous_local_names.insert(local.clone());
        }
    }
    for i in long_declarations {
        let identity = &identities[i];
        let name = declarations[i].chat_name.clone();
        if !ambiguous_local_names.contains(&name) && claim(&name, identity) {
            continue;
        }
        let mut suffix = 1;
        loop {
            let candidate = cap_chat_tool_name(&format!("{name}_{suffix}"));
            suffix += 1;
            if ambiguous_local_names.contains(&candidate) {
                continue;
            }
            if claim(&candidate, identity) {
                declarations[i].chat_name = candidate;
                break;
            }
        }
    }
}

/// Maps a Responses freeform (`custom`) tool onto a Chat Completions function tool with a single
/// freeform `input` string, mirroring the function shape Codex uses for apply_patch.
fn convert_custom_tool_to_chat(tool: &Value, override_name: &str) -> Option<Value> {
    let tool_res = Res::of(tool);
    let mut name = override_name.trim().to_string();
    if name.is_empty() {
        name = responses_tool_name(&tool_res);
    }
    if name.is_empty() {
        return None;
    }
    let mut chat_tool = cpa_json::parse_str(
        r#"{"type":"function","function":{"name":"","description":"","parameters":{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}}}"#,
    );
    cpa_json::set(&mut chat_tool, "function.name", name);
    let description = responses_tool_description(&tool_res);
    if !description.is_empty() {
        cpa_json::set(&mut chat_tool, "function.description", description);
    }
    if applypatch::is_custom_tool(tool) {
        cpa_json::set(&mut chat_tool, "function.description", applypatch::description(tool));
        cpa_json::set(&mut chat_tool, "function.parameters", cpa_json::parse(&applypatch::parameters()));
    }
    Some(chat_tool)
}

fn convert_function_tool_to_chat(tool: &Value, override_name: &str) -> Option<Value> {
    let tool_res = Res::of(tool);
    let mut name = override_name.trim().to_string();
    if name.is_empty() {
        name = responses_tool_name(&tool_res);
    }
    if name.is_empty() {
        return None;
    }
    let mut chat_tool = cpa_json::parse_str(r#"{"type":"function","function":{"name":"","description":"","parameters":{}}}"#);
    cpa_json::set(&mut chat_tool, "function.name", name);
    let description = responses_tool_description(&tool_res);
    if !description.is_empty() {
        cpa_json::set(&mut chat_tool, "function.description", description);
    }
    let parameters = responses_tool_parameters(&tool_res);
    if parameters.exists() {
        cpa_json::set(&mut chat_tool, "function.parameters", parameters.value());
    }
    Some(chat_tool)
}

fn responses_tool_name(tool: &Res<'_>) -> String {
    let name = tool.g("name").str().trim().to_string();
    if !name.is_empty() {
        return name;
    }
    tool.g("function.name").str().trim().to_string()
}

fn responses_tool_description(tool: &Res<'_>) -> String {
    let description = tool.g("description").str();
    if !description.is_empty() {
        return description;
    }
    tool.g("function.description").str()
}

fn responses_tool_parameters<'a>(tool: &'a Res<'_>) -> Res<'a> {
    for path in ["parameters", "parametersJsonSchema", "input_schema", "function.parameters", "function.parametersJsonSchema"] {
        let parameters = tool.g(path);
        if parameters.exists() {
            return parameters;
        }
    }
    Res::NONE
}

/// Flattens a tool output (plain string or array of content parts) into one text payload for a
/// Chat Completions tool message. `src` locates `output` in the request.
pub(super) fn responses_tool_output_text(output: &Res<'_>, src: &RawSrc<'_>) -> String {
    if output.is_string() {
        return output.str();
    }
    if output.is_array() {
        let mut b = String::new();
        let part_srcs = src.children();
        for (k, part) in output.array().iter().enumerate() {
            if part.is_string() {
                b.push_str(&part.str());
                continue;
            }
            let text = part.g("text");
            if text.exists() {
                b.push_str(&part_srcs.get(k).map_or(RawSrc::none(), |p| p.child("text")).string(&text));
            }
        }
        return b;
    }
    if output.exists() {
        return src.raw(output);
    }
    String::new()
}

/// Extracts the freeform input from the `{"input": "..."}` arguments produced for a converted
/// custom tool; falls back to the raw arguments when the wrapper is absent.
pub(super) fn unwrap_custom_tool_input(arguments: &str) -> String {
    let root = parse_lenient(arguments);
    let v = root.g("input");
    if v.exists() {
        if v.is_string() {
            return v.str();
        }
        // Non-strings come back as the original text of the value.
        let doc = lenient_start(arguments).map_or(arguments, |start| &arguments[start..]);
        return RawSrc::new(doc.as_bytes()).child("input").raw(&v);
    }
    arguments.to_string()
}

/// gjson reads the first object or array in the text, skipping leading garbage and ignoring
/// trailing data; mimicked for text that is not valid JSON as a whole.
fn parse_lenient(text: &str) -> Value {
    let strict = cpa_json::parse(text.as_bytes());
    if !strict.is_null() {
        return strict;
    }
    let Some(start) = lenient_start(text) else { return strict };
    cpa_json::parse(text[start..].as_bytes())
}

/// Offset of the first object or array when `text` is not valid JSON as a whole.
fn lenient_start(text: &str) -> Option<usize> {
    if cpa_json::valid(text.as_bytes()) { None } else { text.find(['{', '[']) }
}

fn qualify_namespace_tool_name(namespace_name: &str, child_name: &str) -> String {
    cap_chat_tool_name(&raw_namespace_qualified_name(namespace_name, child_name))
}

/// [`qualify_namespace_tool_name`] without the length cap, so disambiguation can tell a genuine
/// name apart from a truncation-induced collision.
fn raw_namespace_qualified_name(namespace_name: &str, child_name: &str) -> String {
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

/// Truncates a flattened tool name to the Chat Completions limit keeping the tail (the tool's
/// local name carries the most identifying part). A partial `_`/`-` run left at the start is
/// stripped because some strict upstreams reject names not starting with an alphanumeric. A pure
/// function of the name, so every path deriving a chat function name stays consistent.
fn cap_chat_tool_name(name: &str) -> String {
    if name.len() <= CHAT_TOOL_NAME_LIMIT {
        return name.to_string();
    }
    let truncated = &name.as_bytes()[name.len() - CHAT_TOOL_NAME_LIMIT..];
    let start = truncated.iter().position(|b| !matches!(b, b'_' | b'-')).unwrap_or(truncated.len());
    let trimmed = &truncated[start..];
    String::from_utf8_lossy(if trimmed.is_empty() { truncated } else { trimmed }).into_owned()
}

/// Per-request index of tool declarations; replayed calls and streaming events only consult these
/// maps.
#[derive(Default)]
pub(super) struct ToolIndex {
    declarations: Vec<Declaration>,
    pub by_chat: HashMap<String, Declaration>,
    by_identity: HashMap<(String, String), String>,
    by_raw: HashMap<String, String>,
    /// Empty means multiple distinct emitted tools.
    by_local: HashMap<String, String>,
    pub custom: HashSet<String>,
}

impl ToolIndex {
    pub fn new(root: &Value) -> Self {
        let mut idx = ToolIndex::default();
        for d in walk_declarations(root) {
            idx.declarations.push(d.clone());
            idx.by_identity.entry((d.namespace.clone(), d.local_name.clone())).or_insert_with(|| d.chat_name.clone());
            idx.by_raw
                .entry(raw_namespace_qualified_name(&d.namespace, &d.local_name))
                .or_insert_with(|| d.chat_name.clone());
            if idx.by_chat.contains_key(&d.chat_name) {
                continue;
            }
            if idx.by_local.contains_key(&d.local_name) {
                idx.by_local.insert(d.local_name.clone(), String::new());
            } else {
                idx.by_local.insert(d.local_name.clone(), d.chat_name.clone());
            }
            if d.custom {
                idx.custom.insert(d.chat_name.clone());
            }
            idx.by_chat.insert(d.chat_name.clone(), d);
        }
        idx
    }

    pub fn namespace_name(&self, namespace: &str, name: &str) -> String {
        if let Some(chat_name) = self.by_identity.get(&(namespace.to_string(), name.to_string())) {
            return chat_name.clone();
        }
        self.avoid_alias(&qualify_namespace_tool_name(namespace, name))
    }

    /// Resolves a name from a replayed call or tool_choice that omits the namespace: exact
    /// emitted names win, then uncapped qualified names, then a unique local name, else a capped
    /// alias that avoids every emitted name.
    pub fn canonical_name(&self, name: &str) -> String {
        if self.by_chat.contains_key(name) {
            return name.to_string();
        }
        if let Some(chat_name) = self.by_raw.get(name) {
            return chat_name.clone();
        }
        if let Some(chat_name) = self.by_local.get(name).filter(|n| !n.is_empty()) {
            return chat_name.clone();
        }
        self.avoid_alias(&cap_chat_tool_name(name))
    }

    /// Keeps a fallback name from colliding with any alias the declarations emit.
    fn avoid_alias(&self, candidate: &str) -> String {
        if !self.by_chat.contains_key(candidate) {
            return candidate.to_string();
        }
        let mut suffix = 1;
        loop {
            let variant = cap_chat_tool_name(&format!("{candidate}_{suffix}"));
            if !self.by_chat.contains_key(&variant) {
                return variant;
            }
            suffix += 1;
        }
    }

    /// Writes the declaration's local name and namespace on a tool call item.
    pub fn apply_identity(&self, item: &[u8], qualified_name: &str, item_path: &str) -> Vec<u8> {
        let mut name = qualified_name.trim().to_string();
        let mut namespace = String::new();
        if let Some(d) = self.by_chat.get(&name) {
            name = d.local_name.clone();
            namespace = d.namespace.clone();
        }
        common::set_responses_tool_call_identity(item, &name, &namespace, item_path)
    }

    /// The only custom tool's name and whether it is also the only tool at all; `("", false)`
    /// unless exactly one custom tool exists.
    pub fn single_custom_name(&self) -> (String, bool) {
        match (self.custom.len(), self.custom.iter().next()) {
            (1, Some(name)) => (name.clone(), self.by_chat.len() == 1),
            _ => (String::new(), false),
        }
    }

    /// Whether the winning declaration for `name` is the custom `apply_patch` tool.
    pub fn is_apply_patch(&self, name: &str) -> bool {
        self.by_chat.get(name).is_some_and(|d| d.custom && applypatch::is_custom_tool(&d.tool))
    }

    /// Chat Completions tools for every declaration, first occurrence of each name winning.
    pub fn chat_tools(&self) -> Vec<Value> {
        let mut merged = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for d in &self.declarations {
            if seen.contains(d.chat_name.as_str()) {
                continue;
            }
            let tool = if d.custom {
                convert_custom_tool_to_chat(&d.tool, &d.chat_name)
            } else {
                convert_function_tool_to_chat(&d.tool, &d.chat_name)
            };
            if let Some(tool) = tool {
                merged.push(tool);
                seen.insert(&d.chat_name);
            }
        }
        merged
    }
}

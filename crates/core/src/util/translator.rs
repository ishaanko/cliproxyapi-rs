//! Tool-name mapping and JSON repair helpers used by translators (Go: util/translator.go).
//!
//! Name maps take the original request body as bytes (like Go) and return `HashMap`s; an empty map
//! is Go's nil map (lookups behave identically). Response translators rebuild these maps from the
//! original request to restore the client's tool names.

use std::collections::{HashMap, HashSet};

use cpa_json::{J, Value};
use sha2::{Digest, Sha256};

use super::gemini_schema::escape_gjson_path_key;
use super::sanitize_function_name;

/// Recursively collects the dot paths (gjson escaped) of every object key equal to `field`,
/// descending into arrays by index (Go: Walk). `path` is "" at the root.
pub fn walk(value: &Value, path: &str, field: &str, paths: &mut Vec<String>) {
    let mut visit = |key: String, val: &Value| {
        let safe_key = escape_gjson_path_key(&key);
        let child_path = if path.is_empty() {
            safe_key
        } else {
            format!("{path}.{safe_key}")
        };
        if key == field {
            paths.push(child_path.clone());
        }
        walk(val, &child_path, field, paths);
    };
    match value {
        Value::Object(m) => m.iter().for_each(|(k, v)| visit(k.clone(), v)),
        Value::Array(a) => a
            .iter()
            .enumerate()
            .for_each(|(i, v)| visit(i.to_string(), v)),
        _ => {}
    }
}

/// Moves the value at `old_key_path` to `new_key_path` (set, then delete the old path) in a JSON
/// document. Errors when the old key is missing or the new path is empty. The result is compact JSON.
pub fn rename_key(
    json_str: &str,
    old_key_path: &str,
    new_key_path: &str,
) -> Result<String, String> {
    let mut root = cpa_json::parse_str(json_str);
    let Some(value) = root.g(old_key_path).into_value() else {
        return Err(format!("old key '{old_key_path}' does not exist"));
    };
    if new_key_path.is_empty() {
        return Err(format!(
            "failed to set new key '{new_key_path}': path cannot be empty"
        ));
    }
    cpa_json::set(&mut root, new_key_path, value);
    cpa_json::delete(&mut root, old_key_path);
    Ok(cpa_json::to_string(&root))
}

/// Converts JSON that uses single-quoted strings into RFC 8259 JSON (Go: FixJSON).
///
/// Double-quoted strings are kept as-is; single-quoted strings become double-quoted with inner
/// `"` escaped; `\n \r \t \b \f \/ \" \\ \uXXXX` escapes are preserved, `\'` becomes a literal
/// `'`, unknown escapes keep the backslash. An unterminated single-quoted string is closed.
/// Nothing else is repaired.
pub fn fix_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let (mut in_double, mut in_single, mut escaped) = (false, false, false);
    let runes: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < runes.len() {
        let r = runes[i];
        if in_double {
            out.push(r);
            if escaped {
                escaped = false;
            } else if r == '\\' {
                escaped = true;
            } else if r == '"' {
                in_double = false;
            }
        } else if in_single {
            if escaped {
                escaped = false;
                match r {
                    'n' | 'r' | 't' | 'b' | 'f' | '/' | '"' => {
                        out.push('\\');
                        out.push(r);
                    }
                    '\\' => out.push_str("\\\\"),
                    '\'' => out.push('\''),
                    'u' => {
                        out.push_str("\\u");
                        for _ in 0..4 {
                            match runes.get(i + 1) {
                                Some(peek) if peek.is_ascii_hexdigit() => {
                                    out.push(*peek);
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                    }
                    _ => {
                        out.push('\\');
                        out.push(r);
                    }
                }
            } else if r == '\\' {
                escaped = true;
            } else if r == '\'' {
                out.push('"');
                in_single = false;
            } else if r == '"' {
                out.push_str("\\\"");
            } else {
                out.push(r);
            }
        } else if r == '"' {
            in_double = true;
            out.push(r);
        } else if r == '\'' {
            in_single = true;
            out.push('"');
        } else {
            out.push(r);
        }
        i += 1;
    }
    if in_single {
        out.push('"');
    }
    out
}

/// Lookup key for tool-name matching: trimmed, leading `_` stripped, lowercased.
pub fn canonical_tool_name(name: &str) -> String {
    name.trim().trim_start_matches('_').to_lowercase()
}

fn parse_valid(raw: &[u8]) -> Option<Value> {
    if raw.is_empty() { None } else { cpa_json::parse_valid(raw) }
}

/// canonical-name -> original-name map from a Claude request's `tools[].name` (or
/// `function.name`); first declaration wins. Used to restore exact tool name casing for strict
/// clients such as Claude Code.
pub fn tool_name_map_from_claude_request(raw_json: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(root) = parse_valid(raw_json) else {
        return out;
    };
    let Some(Value::Array(tools)) = root.get("tools") else {
        return out;
    };
    for tool in tools {
        let mut name = tool.g("name").str().trim().to_string();
        if name.is_empty() {
            name = tool.g("function.name").str().trim().to_string();
        }
        if name.is_empty() {
            continue;
        }
        let key = canonical_tool_name(&name);
        if key.is_empty() {
            continue;
        }
        out.entry(key).or_insert(name);
    }
    out
}

/// Original tool name for `name` via a [`tool_name_map_from_claude_request`] map; `name` itself
/// when unmapped.
pub fn map_tool_name(tool_name_map: &HashMap<String, String>, name: &str) -> String {
    if name.is_empty() {
        return name.to_string();
    }
    match tool_name_map.get(&canonical_tool_name(name)) {
        Some(mapped) if !mapped.is_empty() => mapped.clone(),
        _ => name.to_string(),
    }
}

/// original-name -> sanitized-name map from request tools. Exact duplicates share a mapping;
/// distinct names that sanitize to the same value get deterministic hash suffixes so every
/// declaration stays addressable within the 64 byte limit.
pub fn sanitized_function_name_map(raw_json: &[u8]) -> HashMap<String, String> {
    sanitize_unique_names(function_names_from_request(raw_json))
}

/// Request-specific sanitized name when mapped, else plain [`sanitize_function_name`].
pub fn map_sanitized_function_name(name_map: &HashMap<String, String>, name: &str) -> String {
    match name_map.get(name) {
        Some(mapped) if !mapped.is_empty() => mapped.clone(),
        _ => sanitize_function_name(name),
    }
}

/// sanitized-name -> original-name map using the same collision-aware mapping as
/// [`sanitized_function_name_map`] (only entries whose name changed).
pub fn disambiguated_tool_name_map(raw_json: &[u8]) -> HashMap<String, String> {
    sanitized_function_name_map(raw_json)
        .into_iter()
        .filter(|(original, sanitized)| original != sanitized)
        .map(|(original, sanitized)| (sanitized, original))
        .collect()
}

/// Legacy sanitized-name -> original-name map from top-level Claude-style tools (first name wins
/// on collisions). Collision-aware translators should use [`disambiguated_tool_name_map`].
pub fn sanitized_tool_name_map(raw_json: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(root) = parse_valid(raw_json) else {
        return out;
    };
    let Some(Value::Array(tools)) = root.get("tools") else {
        return out;
    };
    for tool in tools {
        let name = tool.g("name").str().trim().to_string();
        if name.is_empty() {
            continue;
        }
        let sanitized = sanitize_function_name(&name);
        if sanitized == name {
            continue;
        }
        match out.get(&sanitized) {
            None => {
                out.insert(sanitized, name);
            }
            Some(existing) => tracing::warn!(
                "sanitized tool name collision: {existing:?} and {name:?} both map to {sanitized:?}, keeping first"
            ),
        }
    }
    out
}

/// Original -> sanitized for a list of names (shared by the Responses tool helpers): names are
/// deduplicated and processed in sorted order; a name whose sanitized base collides gets a hash
/// suffix.
pub(super) fn sanitize_unique_names(names: Vec<String>) -> HashMap<String, String> {
    let mut unique: HashSet<&str> = HashSet::new();
    let mut base_counts: HashMap<String, usize> = HashMap::new();
    for name in &names {
        if name.is_empty() || !unique.insert(name) {
            continue;
        }
        *base_counts.entry(sanitize_function_name(name)).or_insert(0) += 1;
    }
    let mut sorted: Vec<&str> = unique.into_iter().collect();
    sorted.sort_unstable();

    let mut out = HashMap::with_capacity(sorted.len());
    let mut used: HashMap<String, String> = HashMap::with_capacity(sorted.len());
    for name in sorted {
        let base = sanitize_function_name(name);
        let mapped = if base_counts.get(&base).copied().unwrap_or(0) > 1 || used.contains_key(&base)
        {
            disambiguate_sanitized_function_name(&base, name, &used)
        } else {
            base
        };
        out.insert(name.to_string(), mapped.clone());
        used.insert(mapped, name.to_string());
    }
    out
}

/// Tool/function names declared in a request's `tools` (nested `tools`, Gemini
/// `functionDeclarations`, OpenAI `function.name`, or plain `name`), in declaration order.
fn function_names_from_request(raw_json: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let Some(root) = parse_valid(raw_json) else {
        return names;
    };
    let Some(Value::Array(tools)) = root.get("tools") else {
        return names;
    };

    fn collect_declarations(declarations: &Value, names: &mut Vec<String>) {
        if let Value::Array(items) = declarations {
            for declaration in items {
                let name = declaration.g("name").str();
                if !name.is_empty() {
                    names.push(name);
                }
            }
        }
    }
    fn collect_tool(tool: &Value, names: &mut Vec<String>) {
        if let Some(Value::Array(nested)) = tool.g("tools").v() {
            nested.iter().for_each(|t| collect_tool(t, names));
            return;
        }
        let mut has_declarations = false;
        for key in ["functionDeclarations", "function_declarations"] {
            if let Some(declarations @ Value::Array(_)) = tool.g(key).v() {
                collect_declarations(declarations, names);
                has_declarations = true;
            }
        }
        if has_declarations {
            return;
        }
        for path in ["function.name", "name"] {
            let name = tool.g(path).str();
            if !name.is_empty() {
                names.push(name);
                return;
            }
        }
    }
    tools.iter().for_each(|t| collect_tool(t, &mut names));
    names
}

/// `base` + `_` + 12 hex chars of sha256(original \0 attempt), truncated to fit 64 bytes; retried
/// with a higher attempt until unused.
pub(super) fn disambiguate_sanitized_function_name(
    base: &str,
    original: &str,
    used: &HashMap<String, String>,
) -> String {
    for attempt in 0u32.. {
        let digest = Sha256::digest(format!("{original}\0{attempt}").as_bytes());
        let suffix = format!("_{}", hex::encode(&digest[..6]));
        let max_prefix = 64 - suffix.len();
        // `base` is ASCII (sanitized), so byte truncation is on a char boundary.
        let prefix = if base.len() > max_prefix {
            &base[..max_prefix]
        } else {
            base
        };
        let candidate = format!("{prefix}{suffix}");
        if !used.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("attempt counter is unbounded")
}

/// Removes duplicate named declarations from a JSON array while preserving order; unnamed entries
/// are all kept. Non-array input is returned unchanged. The array is re-serialized compactly.
pub fn deduplicate_function_declarations(raw: &[u8]) -> Vec<u8> {
    let Value::Array(items) = cpa_json::parse(raw) else {
        return raw.to_vec();
    };
    let mut seen: HashSet<String> = HashSet::with_capacity(items.len());
    let kept: Vec<Value> = items
        .into_iter()
        .filter(|declaration| {
            let name = declaration.g("name").str();
            name.is_empty() || seen.insert(name)
        })
        .collect();
    cpa_json::to_vec(&Value::Array(kept))
}

/// Original client-facing name for a sanitized one; the sanitized name itself when unmapped.
pub fn restore_sanitized_tool_name(
    tool_name_map: &HashMap<String, String>,
    sanitized_name: &str,
) -> String {
    if sanitized_name.is_empty() {
        return sanitized_name.to_string();
    }
    tool_name_map
        .get(sanitized_name)
        .cloned()
        .unwrap_or_else(|| sanitized_name.to_string())
}

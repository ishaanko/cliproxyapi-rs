//! Claude tool input schema normalization (Go: util/claude_schema.go).

use cpa_json::{Map, Value};

const EMPTY_CLAUDE_TOOL_INPUT_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;

/// Makes a JSON Schema acceptable as a Claude tool input schema: an object without root-level
/// unions. Root `anyOf`/`oneOf`/`allOf` are removed and the properties of object-capable branches
/// are merged into the root `properties` (existing names win; `allOf` branches also union their
/// `required`). Invalid or non-object input yields `{"type":"object","properties":{}}`.
///
/// Like Go (which edits `map[string]json.RawMessage`), only the root object and `properties` get
/// their keys sorted; nested schemas keep their key order. Output is compact JSON.
pub fn normalize_claude_tool_input_schema(schema: &[u8]) -> Vec<u8> {
    let fallback = || EMPTY_CLAUDE_TOOL_INPUT_SCHEMA.as_bytes().to_vec();
    if schema.is_empty() {
        return fallback();
    }
    let Value::Object(mut root) = cpa_json::parse(schema) else {
        return fallback();
    };

    let mut properties = claude_schema_object(root.get("properties"));
    for union_name in ["anyOf", "oneOf", "allOf"] {
        let Some(union_raw) = root.shift_remove(union_name) else {
            continue;
        };
        let branches: Vec<Value> = match union_raw {
            Value::Array(branches) => branches,
            Value::Null => Vec::new(),
            _ => continue,
        };
        for branch in &branches {
            // A null branch decodes to a nil map in Go and is then a harmless no-op.
            let empty = Map::new();
            let branch_map = match branch {
                Value::Object(m) => m,
                Value::Null => &empty,
                _ => continue,
            };
            if !claude_schema_can_be_object(branch_map) {
                continue;
            }
            for (name, property) in claude_schema_object(branch_map.get("properties")) {
                if !properties.contains_key(&name) {
                    properties.insert(name, property);
                }
            }
            if union_name == "allOf" {
                merge_claude_schema_required(&mut root, branch_map.get("required"));
            }
        }
    }

    root.insert("type".into(), Value::String("object".into()));
    properties.sort_keys();
    root.insert("properties".into(), Value::Object(properties));
    root.sort_keys();
    // Nested values stay in their original order (Go embeds them as raw messages).
    match serde_json::to_string(&Value::Object(root)) {
        Ok(out) => out.into_bytes(),
        Err(_) => fallback(),
    }
}

/// The object at `raw`, or an empty map when absent, null or not an object.
fn claude_schema_object(raw: Option<&Value>) -> Map<String, Value> {
    match raw {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    }
}

/// Whether a branch can describe an object: no `type`, `type: "object"` or a type list containing
/// `object`. Any other `type` (including null or lists with non-strings) cannot.
fn claude_schema_can_be_object(schema: &Map<String, Value>) -> bool {
    match schema.get("type") {
        None => true,
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(types)) => {
            let mut has_object = false;
            for t in types {
                match t {
                    Value::String(s) => has_object |= s == "object",
                    Value::Null => {}
                    _ => return false,
                }
            }
            has_object
        }
        Some(_) => false,
    }
}

/// Unions `branch_required` into `root["required"]` (existing entries first, deduplicated). Does
/// nothing when the branch has no (decodable) `required`.
fn merge_claude_schema_required(root: &mut Map<String, Value>, branch_required: Option<&Value>) {
    let string_list = |v: &Value| -> Option<Vec<String>> {
        match v {
            Value::Null => Some(Vec::new()),
            Value::Array(items) => items
                .iter()
                .map(|item| match item {
                    Value::String(s) => Some(s.clone()),
                    Value::Null => Some(String::new()),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    };

    let mut required: Vec<String> = root
        .get("required")
        .and_then(string_list)
        .unwrap_or_default();
    let Some(branch_names) = branch_required.and_then(string_list) else {
        return;
    };

    let mut seen: std::collections::HashSet<String> = required.iter().cloned().collect();
    for name in branch_names {
        if seen.contains(&name) {
            continue;
        }
        seen.insert(name.clone());
        required.push(name);
    }
    if required.is_empty() {
        return;
    }
    root.insert(
        "required".into(),
        Value::Array(required.into_iter().map(Value::String).collect()),
    );
}

/// Whether a regex pattern contains a construct strict upstream JSON Schema validators reject
/// when compiling `pattern`: Unicode property escapes (`\p{..}`, `\P{..}`; Python's `re` fails with
/// "bad escape \p") or the octal NUL escape `\0` (validators reject it, `\x00` is accepted).
/// Anthropic's API accepts both verbatim, so they only fail after translation or forwarding.
/// Dropping the attribute is safe since the client validates its own input locally.
pub fn has_unsupported_unicode_property_escape(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let Some(&next) = bytes.get(i + 1) else { break };
        if (next == b'p' || next == b'P') && bytes.get(i + 2) == Some(&b'{') {
            return true;
        }
        if next == b'0' {
            return true;
        }
        i += 2; // skip the escaped character (including an escaped backslash)
    }
    false
}

//! Dotted-path helpers over `serde_yaml_ng::Value` (ports of `yamlPath`, `setYAMLPath`,
//! `deleteYAMLPath` in `config_v8.go`). Keys are plain strings; a path never indexes sequences.

use serde_yaml_ng::{Mapping, Value};

use crate::error::{ConfigError, Result};

/// Parses a YAML document with anchors/aliases and `<<` merge keys expanded. `None` means the
/// document is empty (blank or comments only).
pub(crate) fn parse_yaml(text: &str) -> Result<Option<Value>> {
    let has_content = text.lines().any(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#')
    });
    if !has_content {
        return Ok(None);
    }
    // Like yaml.Unmarshal, only the first document counts.
    let Some(mut value) =
        crate::rawparse::parse_first_document(text).map_err(ConfigError::Invalid)?
    else {
        return Ok(None);
    };
    value.apply_merge()?;
    Ok(Some(value))
}

/// Looks up a dotted key path. Non-mapping intermediates yield `None`.
pub(crate) fn yaml_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for key in path.split('.') {
        cur = cur.as_mapping()?.get(key)?;
    }
    Some(cur)
}

/// Like [`yaml_path`] but treats a legacy `api-keys` mapping (the v8 upstream groups) as absent.
pub(crate) fn legacy_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let node = yaml_path(root, path)?;
    (path != "api-keys" || !node.is_mapping()).then_some(node)
}

/// Returns the value for `key`, turning `node` into a mapping and inserting the key if needed.
fn get_or_create<'a>(node: &'a mut Value, key: &str) -> &'a mut Value {
    if !node.is_mapping() {
        *node = Value::Mapping(Mapping::new());
    }
    match node {
        Value::Mapping(map) => map
            .entry(Value::String(key.to_string()))
            .or_insert(Value::Null),
        other => other,
    }
}

/// Sets a dotted path, creating (or overwriting non-mapping) intermediate mappings.
pub(crate) fn set_yaml_path(root: &mut Value, path: &str, value: Value) {
    let mut cur = root;
    for key in path.split('.') {
        cur = get_or_create(cur, key);
    }
    *cur = value;
}

/// Deletes a dotted path and prunes ancestors that become empty. Returns whether it existed.
pub(crate) fn delete_yaml_path(root: &mut Value, path: &str) -> bool {
    let (head, rest) = match path.split_once('.') {
        Some((head, rest)) => (head, Some(rest)),
        None => (path, None),
    };
    let Value::Mapping(map) = root else {
        return false;
    };
    if let Some(rest) = rest {
        let Some(child) = map.get_mut(head) else {
            return false;
        };
        if !delete_yaml_path(child, rest) {
            return false;
        }
        let keep = match child {
            Value::Mapping(m) => !m.is_empty(),
            Value::Sequence(s) => !s.is_empty(),
            _ => false,
        };
        if keep {
            return true;
        }
    } else if !map.contains_key(head) {
        return false;
    }
    map.shift_remove(head);
    true
}

pub(crate) fn str_key(key: &str) -> Value {
    Value::String(key.to_string())
}

/// An empty block mapping.
pub(crate) fn empty_map() -> Value {
    Value::Mapping(Mapping::new())
}

/// Drops null values from a legacy-layout document before it is decoded. yaml.v3 leaves scalars and structs untouched when it sees `null`, and a null map/slice/pointer
/// is the same as absent for this schema, so nulls are dropped before decoding. Free-form
/// subtrees (plugin options, payload params and match values) keep theirs.
fn strip_nulls_at(value: &mut Value, path: &mut Vec<String>) {
    match value {
        Value::Mapping(map) => map.retain(|key, child| {
            path.push(key.as_str().unwrap_or_default().to_string());
            let keep = if is_free_form(path) {
                true
            } else if child.is_null() {
                false
            } else {
                strip_nulls_at(child, path);
                true
            };
            path.pop();
            keep
        }),
        Value::Sequence(items) => {
            // yaml.v3 skips null list elements.
            items.retain(|item| !item.is_null());
            items.iter_mut().for_each(|item| strip_nulls_at(item, path));
        }
        _ => {}
    }
}

/// Whether the value at `path` is part of a subtree where null values are data.
fn is_free_form(path: &[String]) -> bool {
    match path {
        // Field presence (even as null) is meaningful for the Home-owned lifecycle settings.
        [first, ..] if first == "credential-concurrency" => true,
        // Presence of the private-IP spellings matters even when null (decoded leniently).
        [first, second, ..] if first == "codex" && second == "live-media-relay" => true,
        // Plugin instances are opaque; `plugins.configs` itself is a normal (nullable) map.
        [first, second, _id, ..] if first == "plugins" && second == "configs" => true,
        [first, rest @ ..] if first == "payload" => rest
            .iter()
            .any(|seg| matches!(seg.as_str(), "params" | "match" | "not-match")),
        _ => false,
    }
}

pub(crate) fn strip_nulls(value: &mut Value) {
    strip_nulls_at(value, &mut Vec::new());
}

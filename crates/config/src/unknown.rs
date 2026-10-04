//! yaml.v3's strict-decode report for unknown fields (`Decoder.KnownFields(true)`), as management
//! writes return it:
//!
//! ```text
//! yaml: unmarshal errors:
//!   line 7: field nope not found in type config.RoutingConfig
//! ```
//!
//! The line is that of the key in the re-marshalled document (`yaml.Marshal` of the flattened
//! tree), and the type is the Go struct holding the key ([`GO_TYPES`]).

use std::collections::{HashMap, HashSet};

use serde_yaml_ng::Value;

use crate::comments::{CPath, Comments, Seg};
use crate::emit::{Styles, probe_lines};
use crate::gotypes::GO_TYPES;

const ROOT_TYPE: &str = "config.legacyConfig";

/// Where a walk down [`GO_TYPES`] stands: at a struct, or inside list/map wrappers of one.
#[derive(Clone, Copy)]
enum At {
    Struct(&'static str),
    Shape(&'static str),
}

fn field_shape(ty: &str, key: &str) -> Option<&'static str> {
    let (_, fields) = GO_TYPES.iter().find(|(name, _)| *name == ty)?;
    fields.iter().find(|(k, _)| *k == key).map(|(_, shape)| *shape)
}

/// The Go struct that holds the unknown key at the end of `path` (the root struct when the walk
/// leaves the known tree).
fn parent_type(path: &[Seg]) -> &'static str {
    let mut at = At::Struct(ROOT_TYPE);
    for seg in &path[..path.len().saturating_sub(1)] {
        loop {
            match (at, seg) {
                (At::Shape(s), seg) => {
                    if let Some(rest) = s.strip_prefix("[]") {
                        if !matches!(seg, Seg::Index(_)) {
                            return ROOT_TYPE;
                        }
                        at = At::Shape(rest);
                    } else if let Some(rest) = s.strip_prefix("{}") {
                        if !matches!(seg, Seg::Key(_)) {
                            return ROOT_TYPE;
                        }
                        at = At::Shape(rest);
                    } else {
                        at = At::Struct(s);
                        continue;
                    }
                }
                (At::Struct(ty), Seg::Key(key)) => match field_shape(ty, key) {
                    Some(shape) => at = At::Shape(shape),
                    None => return ROOT_TYPE,
                },
                (At::Struct(_), Seg::Index(_)) => return ROOT_TYPE,
            }
            break;
        }
    }
    match at {
        At::Struct(ty) => ty,
        At::Shape(s) => s.trim_start_matches("[]").trim_start_matches("{}"),
    }
}

/// Every key and list item of `tree` with its path and value.
fn nodes<'a>(tree: &'a Value, path: &mut CPath, out: &mut Vec<(CPath, &'a Value)>) {
    match tree {
        Value::Mapping(map) => {
            for (k, v) in map {
                path.push(Seg::Key(k.as_str().unwrap_or_default().to_string()));
                out.push((path.clone(), v));
                nodes(v, path, out);
                path.pop();
            }
        }
        Value::Sequence(items) => {
            for (i, v) in items.iter().enumerate() {
                path.push(Seg::Index(i));
                out.push((path.clone(), v));
                nodes(v, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

fn key_name(path: &CPath) -> Option<&str> {
    match path.last() {
        Some(Seg::Key(k)) => Some(k.as_str()),
        _ => None,
    }
}

/// What the Go tree keeps on its nodes while `flattenV8` moves them around: the flow style of
/// collections and the head comments of keys. Nodes of the marshalled `flat` tree are matched to
/// those of the source document by key name and value.
fn moved_marks(text: &str, root: &Value, flat: &Value) -> (Styles, HashMap<CPath, usize>) {
    let source = Styles::from_text(text);
    let heads = Comments::extract(text).head_counts();
    let mut original = Vec::new();
    nodes(root, &mut Vec::new(), &mut original);
    let at_same_path: HashMap<&CPath, &CPath> = original.iter().map(|(path, _)| (path, path)).collect();
    let mut by_node: HashMap<(Option<&str>, &Value), Vec<&CPath>> = HashMap::new();
    for (path, value) in &original {
        by_node.entry((key_name(path), value)).or_default().push(path);
    }
    let mut moved = Vec::new();
    nodes(flat, &mut Vec::new(), &mut moved);
    let mut flow = HashSet::new();
    let mut head = HashMap::new();
    if source.is_flow(&Vec::new()) {
        flow.insert(Vec::new());
    }
    for (path, value) in moved {
        // A container that kept its path but lost children to `flattenV8` no longer equals its
        // source, so the same path counts as a match too.
        let from = match by_node.get(&(key_name(&path), value)) {
            Some(candidates) => candidates.iter().find(|c| ***c == path).or(candidates.first()).copied(),
            None => at_same_path.get(&path).copied(),
        };
        let Some(from) = from else { continue };
        if source.is_flow(from) {
            flow.insert(path.clone());
        }
        if let Some(n) = heads.get(from) {
            head.insert(path, *n);
        }
    }
    (Styles::with_flow(flow), head)
}

/// Translates a path of the null-stripped tree to the marshalled tree: null list items are skipped
/// by the strict decode, so its indexes count the non-null items only.
fn marshalled_path(flat: &Value, path: &[Seg]) -> CPath {
    let mut out = Vec::with_capacity(path.len());
    let mut cur = Some(flat);
    for seg in path {
        match (cur, seg) {
            (Some(Value::Sequence(items)), Seg::Index(want)) => {
                let at = items.iter().enumerate().filter(|(_, i)| !i.is_null()).nth(*want).map(|(i, _)| i);
                out.push(Seg::Index(at.unwrap_or(*want)));
                cur = at.and_then(|i| items.get(i));
            }
            (Some(Value::Mapping(map)), Seg::Key(key)) => {
                out.push(seg.clone());
                cur = map.get(key.as_str());
            }
            _ => {
                out.push(seg.clone());
                cur = None;
            }
        }
    }
    out
}

/// The yaml.v3 error text for the `ignored` unknown-field paths (of the null-stripped tree) of the
/// marshalled document `flat`, which was flattened from `root`, parsed from `text`.
pub(crate) fn unknown_fields_message(text: &str, root: &Value, flat: &Value, ignored: &[Vec<Seg>]) -> String {
    let (styles, head) = moved_marks(text, root, flat);
    let lines = probe_lines(flat, 4, &styles, head);
    let mut out = String::from("yaml: unmarshal errors:");
    for path in ignored {
        let Some(Seg::Key(field)) = path.last() else { continue };
        let mut at = marshalled_path(flat, path);
        // A key inside a flow collection shares the line of the collection.
        let line = loop {
            if let Some(line) = lines.get(&at) {
                break *line;
            }
            if at.pop().is_none() {
                break 1;
            }
        };
        let ty = parent_type(path);
        out.push_str(&format!("\n  line {line}: field {field} not found in type {ty}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::validate_v8_config;

    /// Expected texts come from the reference `ValidateV8Config`.
    fn message(doc: &str) -> String {
        validate_v8_config(doc.as_bytes()).unwrap_err().to_string()
    }

    #[test]
    fn nested_fields_report_line_and_go_type() {
        let doc = "config-version: 8\nrouting:\n  strategy: round-robin\n  nope: 1\nquota-exceeded:\n  nope: 2\nclient:\n  x: 1\n";
        assert_eq!(
            message(doc),
            "yaml: unmarshal errors:\n  line 3: field nope not found in type config.RoutingConfig\n  line 5: field nope not found in type config.QuotaExceeded\n  line 7: field x not found in type config.ClientConfig"
        );
    }

    #[test]
    fn moved_nodes_keep_flow_style_and_head_comments() {
        let doc = "# top comment\nconfig-version: 8\n\n# routing section\nrouting:\n  # strategy comment\n  strategy: round-robin\n  nope: 1\nquota-exceeded:\n  nope: 2\n";
        assert_eq!(
            message(doc),
            "yaml: unmarshal errors:\n  line 5: field nope not found in type config.RoutingConfig\n  line 7: field nope not found in type config.QuotaExceeded"
        );
        let doc = "config-version: 8\napi-keys:\n  claude:\n    - name: a\n      base-url: http://x\n      keys:\n        - api-key: k1\n          zzz: 1\n      models:\n        - {name: m, alias: a, qq: 1}\n        - name: m2\n          alias: b\n          thinking: {levels: [low], pp: 2}\n";
        assert_eq!(
            message(doc),
            "yaml: unmarshal errors:\n  line 4: field qq not found in type config.ClaudeModel\n  line 7: field pp not found in type registry.ThinkingSupport\n  line 9: field zzz not found in type config.ClaudeKey"
        );
        let json = r#"{"config-version": 8, "routing": {"strategy": "round-robin", "nope": 1}, "client": {"zz": 1}}"#;
        assert_eq!(
            message(json),
            "yaml: unmarshal errors:\n  line 1: field nope not found in type config.RoutingConfig\n  line 1: field zz not found in type config.ClientConfig"
        );
    }
}

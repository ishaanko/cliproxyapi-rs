//! Historical v8 aliases at the management API boundary (port of `config_v8_api.go`).
//!
//! Reads and writes of `/config/<path>` accept the historical spellings of the shared upstream
//! and client settings, while the stored document always uses the canonical layout.

use serde_yaml_ng::Value;

use crate::comments::Comments;
use crate::error::Result;
use crate::layout::{SHARED_STRUCT_PATHS, aliases, render_yaml};
use crate::yamlpath::{delete_yaml_path, empty_map, set_yaml_path, yaml_path};

/// The comments of a document text, kept aside while its tree is edited and re-attached by
/// [`marshal_document_with_comments`].
pub struct DocComments(Comments);

impl DocComments {
    /// Collects the comments (and scalar quoting) of `text`.
    pub fn extract(text: &str) -> Self {
        let mut comments = Comments::extract(text);
        comments.indent = 4;
        Self(comments)
    }
}

impl DocComments {
    pub(crate) fn comments(&self) -> &Comments {
        &self.0
    }

    /// Records that `value`, at the key path `path`, was parsed from a JSON request body, which
    /// yaml.v3 keeps in flow style with double-quoted keys and strings.
    pub fn mark_json_subtree(&mut self, path: &[String], value: &Value) {
        self.0.styles.mark_json_subtree(&key_path(path), value);
    }

    /// Records that the mapping key at `path` came from a JSON request body (written quoted).
    pub fn mark_json_key(&mut self, path: &[String]) {
        self.0.styles.mark_json_key(key_path(path));
    }
}

fn key_path(path: &[String]) -> Vec<crate::comments::Seg> {
    path.iter()
        .map(|k| crate::comments::Seg::Key(k.clone()))
        .collect()
}

/// Renders `root` with the 4-space indent of `yaml.Marshal`, attaching `comments` by key path.
pub fn marshal_document_with_comments(root: &Value, comments: &DocComments) -> Result<String> {
    render_yaml(root, &comments.0)
}

/// Moves canonical fields into the requested historical subtree `path` (dotted). Moving, rather
/// than copying, keeps PUT and DELETE semantics when the subtree holds both shared settings and
/// OAuth-only fields.
pub fn project_v8_config_aliases(root: &mut Value, path: &str) {
    if path.is_empty() {
        return;
    }
    let paths = aliases().chain(SHARED_STRUCT_PATHS.iter());
    for (old, current) in paths {
        if path != *old
            && !old.starts_with(&format!("{path}."))
            && !path.starts_with(&format!("{old}."))
        {
            continue;
        }
        let Some(value) = yaml_path(root, current) else {
            continue;
        };
        // Struct aliases only represent empty containers; their populated fields have
        // individual mappings, sometimes with different nesting.
        if matches!(value, Value::Mapping(m) if !m.is_empty()) {
            continue;
        }
        let value = value.clone();
        set_yaml_path(root, old, value);
        delete_yaml_path(root, current);
    }
}

/// Rewrites historical alias paths of a document (or request body) to the canonical ones. A
/// canonical value already present wins; a null historical container resets its fields.
pub fn normalize_v8_config_aliases(root: &mut Value) {
    normalize_aliases(root, None);
}

/// [`normalize_v8_config_aliases`] that also moves the comments of every relocated field, so a
/// document edited through its historical spellings keeps them at the canonical paths.
pub fn normalize_v8_config_aliases_with_comments(root: &mut Value, comments: &mut DocComments) {
    normalize_aliases(root, Some(&mut comments.0));
}

fn normalize_aliases(root: &mut Value, mut comments: Option<&mut Comments>) {
    // A null historical container resets its fields. Represent the reset as null leaves so
    // merging a root PATCH cannot turn it into an empty-map no-op.
    for (container, _) in SHARED_STRUCT_PATHS {
        if !yaml_path(root, container).is_some_and(Value::is_null) {
            continue;
        }
        for (old, current) in aliases() {
            if old.starts_with(&format!("{container}.")) && yaml_path(root, current).is_none() {
                set_yaml_path(root, current, Value::Null);
            }
        }
        delete_yaml_path(root, container);
    }
    for (old, current) in aliases() {
        let Some(value) = yaml_path(root, old).cloned() else {
            continue;
        };
        let moved = yaml_path(root, current).is_none();
        if let Some(comments) = comments.as_deref_mut() {
            if moved {
                comments.move_field(root, old, current);
            } else {
                comments.remove_prefix(&crate::comments::dotted(old));
            }
        }
        if moved {
            set_yaml_path(root, current, value);
        }
        delete_yaml_path(root, old);
    }
    for (old, current) in SHARED_STRUCT_PATHS {
        let Some(value) = yaml_path(root, old) else {
            continue;
        };
        let empty = value.is_null() || matches!(value, Value::Mapping(m) if m.is_empty());
        if !empty {
            continue;
        }
        let moved = yaml_path(root, current).is_none();
        if let Some(comments) = comments.as_deref_mut() {
            if moved {
                comments.move_field(root, old, current);
            } else {
                comments.remove_prefix(&crate::comments::dotted(old));
            }
        }
        if moved {
            set_yaml_path(root, current, empty_map());
        }
        delete_yaml_path(root, old);
    }
}

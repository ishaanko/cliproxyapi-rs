//! Saving the config back to YAML (port of `config_yaml.go`).
//!
//! [`save_config_preserve_comments`] merges the serialised [`Config`] into the document already on
//! disk: existing keys keep their order and position and are updated in place (even to zero
//! values), new keys are only added when they differ from the known defaults, and comments follow
//! their keys (see [`crate::comments`]).

use std::io::Write as _;
use std::path::Path;

use serde_yaml_ng::{Mapping, Value};

use crate::comments::{CPath, Comments, Seg, dotted};
use crate::error::{ConfigError, Result};
use crate::layout::{
    KEY_FAMILIES, family_comments_to_legacy, flatten_v8_with_comments, group_legacy_keys,
    move_family_comments, normalize_config_layout, render_yaml, v8_paths,
};
use crate::load::{decode_config, parse_config_bytes};
use crate::types::*;
use crate::yamlpath::{
    delete_yaml_path, empty_map, legacy_path, parse_yaml, set_yaml_path, str_key, yaml_path,
};

/// Writes `data` with mode 0600 when creating the file (existing files keep their permissions).
pub(crate) fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(data)
}

fn read_text(path: &Path) -> Result<String> {
    let data =
        std::fs::read(path).map_err(|e| ConfigError::io(format!("read {}", path.display()), e))?;
    String::from_utf8(data).map_err(|_| ConfigError::invalid("config is not valid UTF-8"))
}

/// Writes `cfg` back to `path`, preserving comments and key order of the existing file. With
/// `migrate_v8` the result is also migrated to the v8 layout (see [`normalize_config_layout`]) and
/// `cfg.oauth_only_fields` is synchronised with the migrated document.
pub fn save_config_preserve_comments(
    path: impl AsRef<Path>,
    cfg: &mut Config,
    migrate_v8: bool,
) -> Result<()> {
    let path = path.as_ref();
    let data = read_text(path)?;
    let Some(layout) = parse_yaml(&data)? else {
        return Err(ConfigError::invalid("invalid yaml document structure"));
    };
    if !layout.is_mapping() {
        return Err(ConfigError::invalid("expected root mapping node"));
    }
    // `root` is the legacy-named view of the file; `layout` remembers which fields were v8.
    let mut comments = Comments::extract(&data);
    let mut root = flatten_v8_with_comments(&layout, Some(&mut comments))?;
    let generated = serde_yaml_ng::to_value(&*cfg)?;
    if !generated.is_mapping() {
        return Err(ConfigError::invalid("expected generated root mapping node"));
    }

    // Keep obsolete roots until v8 migration can preserve them as comments.
    if !migrate_v8 {
        for key in [
            "auth",
            "ampcode",
            "amp-upstream-url",
            "amp-upstream-api-key",
            "amp-restrict-management-to-localhost",
            "amp-model-mappings",
            "generative-language-api-key",
        ] {
            remove_map_key(&mut root, key);
        }
    }
    remove_legacy_openai_compat_api_keys(&mut root);

    for key in [
        "oauth-excluded-models",
        "oauth-model-alias",
        "oauth-request-scoped-errors",
        "oauth-settings",
    ] {
        prune_mapping_to_generated_keys(&mut root, &generated, key);
    }
    replace_plugin_configs_subtree(&mut root, &generated);

    // Comments live at v8 paths but the merge works on the legacy view (and re-orders lists), so
    // they are re-keyed to legacy paths for the merge and moved back afterwards.
    let stashed = comments_to_legacy(&mut comments, &layout);
    // Merge generated into the original in place, preserving order and comments of existing nodes.
    merge_mapping_preserve(&mut root, &generated, &mut Vec::new(), &mut comments);
    restore_v8_layout(
        &mut root,
        &layout,
        &data,
        &generated,
        &mut comments,
        stashed,
    )?;

    let mut out = render_yaml(&root, &comments)?;
    let mut migrated = None;
    if migrate_v8 {
        let (bytes, _) = normalize_config_layout(out.as_bytes(), true)?;
        out = String::from_utf8(bytes)
            .map_err(|_| ConfigError::invalid("migrated config is not valid UTF-8"))?;
        let parsed = parse_yaml(&out)?;
        migrated = Some(
            decode_config(&parsed.unwrap_or_else(empty_map))
                .map_err(|e| ConfigError::invalid(format!("decode migrated config: {e}")))?,
        );
    }
    write_private(path, out.as_bytes())
        .map_err(|e| ConfigError::io(format!("write {}", path.display()), e))?;
    if let Some(migrated) = migrated {
        // Publish the OAuth scope only after the write succeeds.
        cfg.oauth_only_fields = migrated.oauth_only_fields;
    }
    Ok(())
}

/// Sets a nested scalar such as `["management", "secret-key"]`, preserving comments and positions.
/// Anchors and merge keys are expanded first so a shared anchor is never mutated.
pub fn save_config_update_nested_scalar(
    path: impl AsRef<Path>,
    keys: &[&str],
    value: &str,
) -> Result<()> {
    let path = path.as_ref();
    let data = read_text(path)?;
    let Some(mut root) = parse_yaml(&data)? else {
        return Err(ConfigError::invalid("invalid yaml document structure"));
    };
    if !root.is_mapping() {
        return Err(ConfigError::invalid("config must be a mapping"));
    }
    let comments = Comments::extract(&data);
    set_yaml_path(&mut root, &keys.join("."), Value::String(value.to_string()));
    let out = render_yaml(&root, &comments)?;
    std::fs::write(path, out).map_err(|e| ConfigError::io(format!("write {}", path.display()), e))
}

// ---------------------------------------------------------------------------------------------
// merge
// ---------------------------------------------------------------------------------------------

/// The mapping keys of a document path (sequence indexes are not part of default/prune rules).
fn key_path(cpath: &CPath) -> Vec<String> {
    cpath
        .iter()
        .filter_map(|seg| match seg {
            Seg::Key(k) => Some(k.clone()),
            Seg::Index(_) => None,
        })
        .collect()
}

fn as_strs(keys: &[String]) -> Vec<&str> {
    keys.iter().map(String::as_str).collect()
}

fn remove_map_key(root: &mut Value, key: &str) {
    if let Value::Mapping(map) = root {
        map.shift_remove(key);
    }
}

fn remove_legacy_openai_compat_api_keys(root: &mut Value) {
    let Some(Value::Sequence(items)) = root
        .as_mapping_mut()
        .and_then(|m| m.get_mut("openai-compatibility"))
    else {
        return;
    };
    for item in items {
        remove_map_key(item, "api-keys");
    }
}

/// Removes keys of `dst[key]` that are absent from the generated mapping (so deleted channels
/// disappear), and replaces non-mapping values.
fn prune_mapping_to_generated_keys(dst_root: &mut Value, src_root: &Value, key: &str) {
    let (Some(dst), Some(src)) = (dst_root.as_mapping_mut(), src_root.as_mapping()) else {
        return;
    };
    if !dst.contains_key(key) {
        return;
    }
    let Some(src_val) = src.get(key) else {
        // Keep explicit OAuth maps when the last channel is removed: their presence must survive
        // saves and override legacy fields when restored to the v8 layout.
        dst.insert(str_key(key), empty_map());
        return;
    };
    match (dst.get_mut(key), src_val) {
        (Some(dst_val @ Value::Mapping(_)), Value::Mapping(src_map)) => {
            prune_missing_map_keys(dst_val, src_map)
        }
        (Some(dst_val), _) => *dst_val = src_val.clone(),
        (None, _) => {}
    }
}

fn prune_missing_map_keys(dst: &mut Value, src: &Mapping) {
    if let Value::Mapping(dst) = dst {
        dst.retain(|key, _| {
            key.as_str()
                .is_some_and(|k| !k.trim().is_empty() && src.contains_key(k.trim()))
        });
    }
}

/// Plugin option trees are opaque: replace `plugins.configs` wholesale by the generated one.
fn replace_plugin_configs_subtree(dst_root: &mut Value, src_root: &Value) {
    let Some(dst) = dst_root.as_mapping_mut() else {
        return;
    };
    let src_configs = yaml_path(src_root, "plugins.configs")
        .and_then(Value::as_mapping)
        .filter(|m| !m.is_empty());
    let Some(src_configs) = src_configs else {
        if let Some(Value::Mapping(plugins)) = dst.get_mut("plugins") {
            plugins.shift_remove("configs");
        }
        return;
    };
    let copied = Value::Mapping(src_configs.clone());
    match dst.get_mut("plugins") {
        None => {
            let mut plugins = Mapping::new();
            plugins.insert(str_key("configs"), copied);
            dst.insert(str_key("plugins"), Value::Mapping(plugins));
        }
        Some(Value::Mapping(plugins)) => {
            plugins.insert(str_key("configs"), copied);
        }
        Some(_) => {}
    }
}

fn is_plugin_configs_subtree(path: &[&str]) -> bool {
    path.len() >= 2 && path[0] == "plugins" && path[1] == "configs"
}

/// Merges `src` keys into `dst`: existing keys are updated in place, new keys are added only when
/// their value is non-zero and not a known default.
fn merge_mapping_preserve(
    dst: &mut Value,
    src: &Value,
    cpath: &mut CPath,
    comments: &mut Comments,
) {
    let (Value::Mapping(dst_map), Value::Mapping(src_map)) = (&mut *dst, src) else {
        // Kinds differ: replace dst by src semantics.
        *dst = src.clone();
        return;
    };
    for (key, src_val) in src_map {
        let Some(key_str) = key.as_str() else {
            continue;
        };
        cpath.push(Seg::Key(key_str.to_string()));
        let keys = key_path(cpath);
        let key_refs = as_strs(&keys);
        // plugins.configs is replaced wholesale by `replace_plugin_configs_subtree`.
        if key_refs == ["plugins", "configs"] {
            cpath.pop();
            continue;
        }
        match dst_map.get_mut(key_str) {
            // Always update existing values, even to zero values.
            Some(dst_val) => merge_node_preserve(dst_val, src_val, cpath, comments),
            None => {
                let mut candidate = src_val.clone();
                prune_known_defaults_in_new_node(&key_refs, &mut candidate);
                if !is_known_default_value(&key_refs, &candidate) {
                    dst_map.insert(key.clone(), candidate);
                }
            }
        }
        cpath.pop();
    }
}

/// Merges `src` into `dst` for scalars, mappings and sequences, updating sequences by index after
/// matching items by identity.
fn merge_node_preserve(dst: &mut Value, src: &Value, cpath: &mut CPath, comments: &mut Comments) {
    match src {
        Value::Mapping(_) => {
            if !dst.is_mapping() {
                *dst = src.clone();
            }
            merge_mapping_preserve(dst, src, cpath, comments);
            if should_prune_nested_mapping_keys(&as_strs(&key_path(cpath)))
                && let Value::Mapping(src_map) = src
            {
                prune_missing_map_keys(dst, src_map);
            }
        }
        Value::Sequence(src_items) => {
            // Preserve an explicit null when the new list is empty.
            if dst.is_null() && src_items.is_empty() {
                return;
            }
            if !dst.is_sequence() {
                *dst = Value::Sequence(Vec::new());
            }
            let Value::Sequence(dst_items) = dst else {
                return;
            };
            if let Some(new_to_old) = reorder_sequence_for_merge(dst_items, src_items) {
                comments.permute_sequence(cpath, &new_to_old);
            }
            for (index, src_item) in src_items.iter().enumerate() {
                match dst_items.get_mut(index) {
                    Some(dst_item) => {
                        cpath.push(Seg::Index(index));
                        merge_node_preserve(dst_item, src_item, cpath, comments);
                        cpath.pop();
                        if let (Value::Mapping(_), Value::Mapping(src_map)) = (&*dst_item, src_item)
                        {
                            prune_missing_map_keys(dst_item, src_map);
                        }
                    }
                    None => dst_items.push(src_item.clone()),
                }
            }
            dst_items.truncate(src_items.len());
        }
        _ => *dst = src.clone(),
    }
}

/// Reorders `dst` to follow `src`, matching items by identity (name/alias/api-key/...) and then by
/// structural equality. Unmatched positions get a copy of the new item. Returns, for each new
/// position, the old index it came from.
fn reorder_sequence_for_merge(dst: &mut Vec<Value>, src: &[Value]) -> Option<Vec<Option<usize>>> {
    if dst.is_empty() || src.is_empty() {
        return None;
    }
    let mut used = vec![false; dst.len()];
    let new_to_old: Vec<Option<usize>> = src
        .iter()
        .map(|target| {
            let idx = match_sequence_element(dst, &used, target)?;
            used[idx] = true;
            Some(idx)
        })
        .collect();
    let mut original: Vec<Option<Value>> = std::mem::take(dst).into_iter().map(Some).collect();
    *dst = new_to_old
        .iter()
        .zip(src)
        .map(|(old, new)| {
            old.and_then(|i| original[i].take())
                .unwrap_or_else(|| new.clone())
        })
        .collect();
    Some(new_to_old)
}

fn match_sequence_element(original: &[Value], used: &[bool], target: &Value) -> Option<usize> {
    let candidates = || original.iter().enumerate().filter(|(i, _)| !used[*i]);
    match target {
        Value::Mapping(_) => {
            if let Some(id) = sequence_element_identity(target)
                && let Some((i, _)) = candidates().find(|(_, o)| {
                    o.is_mapping() && sequence_element_identity(o).as_deref() == Some(id.as_str())
                })
            {
                return Some(i);
            }
        }
        _ => {
            if let Some(text) = scalar_text(target)
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                && let Some((i, _)) =
                    candidates().find(|(_, o)| scalar_text(o).is_some_and(|t| t.trim() == text))
            {
                return Some(i);
            }
        }
    }
    // Structural equality for nodes lacking explicit identifiers.
    candidates()
        .find(|(_, o)| nodes_structurally_equal(o, target))
        .map(|(i, _)| i)
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        Value::Tagged(_) => crate::rawparse::raw_text(value).map(str::to_string),
        _ => None,
    }
}

fn sequence_element_identity(node: &Value) -> Option<String> {
    let Value::Mapping(map) = node else {
        return None;
    };
    const IDENTITY_KEYS: [&str; 9] = [
        "id", "name", "alias", "api-key", "api_key", "apikey", "key", "provider", "model",
    ];
    for key in IDENTITY_KEYS {
        let found = map.iter().find_map(|(k, v)| {
            let (k, v) = (k.as_str()?, scalar_text(v)?);
            (k.trim().eq_ignore_ascii_case(key) && !v.trim().is_empty())
                .then(|| v.trim().to_string())
        });
        if let Some(v) = found {
            return Some(format!("{key}={v}"));
        }
    }
    map.iter().find_map(|(k, v)| {
        let (k, v) = (k.as_str()?, scalar_text(v)?);
        (!v.trim().is_empty()).then(|| format!("{}={}", k.trim().to_lowercase(), v.trim()))
    })
}

fn nodes_structurally_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Mapping(a), Value::Mapping(b)) => {
            a.len() == b.len()
                && a.iter().zip(b.iter()).all(|((ka, va), (kb, vb))| {
                    nodes_structurally_equal(ka, kb) && nodes_structurally_equal(va, vb)
                })
        }
        (Value::Sequence(a), Value::Sequence(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| nodes_structurally_equal(x, y))
        }
        (a, b) => match (scalar_text(a), scalar_text(b)) {
            (Some(x), Some(y)) => x.trim() == y.trim(),
            _ => false,
        },
    }
}

/// Credential-nested mappings (cloak, headers) drop keys that disappeared, so deleted entries do
/// not linger; other mappings are left alone.
fn should_prune_nested_mapping_keys(path: &[&str]) -> bool {
    let [.., parent, last] = path else {
        return false;
    };
    match *parent {
        "claude-api-key" => matches!(*last, "cloak" | "headers"),
        "codex-api-key"
        | "gemini-api-key"
        | "interactions-api-key"
        | "xai-api-key"
        | "meta-api-key"
        | "vertex-api-key"
        | "openai-compatibility" => *last == "headers",
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// defaults pruning
// ---------------------------------------------------------------------------------------------

fn is_int(value: &Value) -> bool {
    matches!(value, Value::Number(n) if n.is_i64() || n.is_u64())
}

/// Whether `node` at `path` is zero or a known default that must not be written as a new key.
fn is_known_default_value(path: &[&str], node: &Value) -> bool {
    if is_plugin_configs_subtree(path) {
        return false;
    }
    if path == ["plugins"]
        && let Some(Value::Mapping(configs)) = node.as_mapping().and_then(|m| m.get("configs"))
        && !configs.is_empty()
    {
        return false;
    }
    let Some(last) = path.last() else {
        return false;
    };
    // Credential weights and retry overrides are pointer-backed: zero is explicit.
    if matches!(*last, "weight" | "request-retry") && is_int(node) {
        return false;
    }
    // Pointer-backed booleans: an explicit false is meaningful.
    if matches!(*last, "cache-user-id" | "disable-cooling") && node.is_bool() {
        return false;
    }
    if is_zero_value_node(node) {
        return true;
    }
    match (path.join(".").as_str(), node) {
        ("pprof.addr", Value::String(s)) => s == DEFAULT_PPROF_ADDR,
        ("remote-management.panel-github-repository", Value::String(s)) => {
            s == DEFAULT_PANEL_GITHUB_REPOSITORY
        }
        ("plugins.dir", Value::String(s)) => s == DEFAULT_PLUGINS_DIR,
        ("routing.strategy", Value::String(s)) => s == "round-robin",
        ("error-logs-max-files", n) if is_int(n) => n.as_i64() == Some(10),
        _ => false,
    }
}

/// Removes default-valued descendants from a node about to be added to the destination tree.
fn prune_known_defaults_in_new_node(path: &[&str], node: &mut Value) {
    if is_plugin_configs_subtree(path) {
        return;
    }
    match node {
        Value::Mapping(map) => {
            map.retain(|key, child| {
                let Some(key) = key.as_str() else { return true };
                let mut child_path = path.to_vec();
                child_path.push(key);
                if is_known_default_value(&child_path, child) {
                    return false;
                }
                prune_known_defaults_in_new_node(&child_path, child);
                !matches!(child, Value::Mapping(m) if m.is_empty())
                    && !matches!(child, Value::Sequence(s) if s.is_empty())
            });
        }
        Value::Sequence(items) => items
            .iter_mut()
            .for_each(|child| prune_known_defaults_in_new_node(path, child)),
        _ => {}
    }
}

/// Zero/default scalars, and collections made only of them.
fn is_zero_value_node(node: &Value) -> bool {
    match node {
        Value::Null => true,
        Value::Bool(b) => !*b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Sequence(items) => items.iter().all(is_zero_value_node),
        Value::Mapping(map) => map.values().all(is_zero_value_node),
        Value::Tagged(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// v8 layout restore
// ---------------------------------------------------------------------------------------------

/// A v8 API-key family's comments, set aside while the merge works on the flattened entries.
struct FamilyStash {
    legacy: &'static str,
    family: &'static str,
    comments: Comments,
}

/// Re-keys comments from v8 paths to the legacy paths the merge operates on: leaf fields move to
/// their legacy name, and each flattened legacy entry takes the comments of the group/key it came
/// from (the originals are kept in the returned stash).
fn comments_to_legacy(comments: &mut Comments, layout: &Value) -> Vec<FamilyStash> {
    for (old, current) in v8_paths() {
        if yaml_path(layout, current).is_some() {
            comments.move_prefix(&dotted(current), &dotted(old));
        }
    }
    let mut stashed = Vec::new();
    for (legacy, family) in KEY_FAMILIES {
        let Some(groups) = yaml_path(layout, &format!("api-keys.{family}")) else {
            continue;
        };
        let stash = comments.take_prefix(&dotted(&format!("api-keys.{family}")));
        family_comments_to_legacy(comments, &stash, legacy, family, groups);
        stashed.push(FamilyStash {
            legacy,
            family,
            comments: stash,
        });
    }
    stashed
}

/// After merging into the legacy-named view, moves each field back to the v8 path it had in the
/// original document so saves do not reintroduce legacy spellings.
fn restore_v8_layout(
    root: &mut Value,
    layout: &Value,
    original: &str,
    generated: &Value,
    comments: &mut Comments,
    stashed: Vec<FamilyStash>,
) -> Result<()> {
    let upstreams_is_mapping = yaml_path(layout, "api-keys").is_some_and(Value::is_mapping);
    for (old, current) in v8_paths() {
        let client_key_collision = *old == "api-keys" && upstreams_is_mapping;
        if yaml_path(layout, current).is_none() && !client_key_collision {
            continue;
        }
        let Some(value) = legacy_path(root, old).cloned() else {
            continue;
        };
        delete_yaml_path(root, old);
        set_yaml_path(root, current, value);
        comments.move_prefix(&dotted(old), &dotted(current));
    }
    let mut baseline: Option<Value> = None;
    for (old, family) in KEY_FAMILIES {
        let path = format!("api-keys.{family}");
        let Some(groups) = yaml_path(layout, &path).cloned() else {
            continue;
        };
        if baseline.is_none() {
            baseline = Some(serde_yaml_ng::to_value(parse_config_bytes(
                original.as_bytes(),
            )?)?);
        }
        let before = baseline.as_ref().and_then(|b| yaml_path(b, old));
        let after = yaml_path(generated, old);
        let kept_as_written = before == after;
        let groups = if kept_as_written {
            groups
        } else {
            let keys = yaml_path(root, old)
                .cloned()
                .unwrap_or_else(|| Value::Sequence(Vec::new()));
            // Rebuilt groups: entries re-key onto the new groups by position.
            move_family_comments(comments, old, &path, &keys, family);
            group_legacy_keys(&keys, family)
        };
        if kept_as_written {
            // The groups are unchanged, so their own comments are still right.
            comments.remove_prefix(&dotted(old));
            if let Some(stash) = stashed
                .iter()
                .find(|s| s.legacy == *old && s.family == *family)
            {
                comments.merge(stash.comments.clone());
            }
        }
        delete_yaml_path(root, old);
        set_yaml_path(root, &path, groups);
    }
    Ok(())
}

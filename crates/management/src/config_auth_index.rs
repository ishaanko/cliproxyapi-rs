//! `auth_index` handling of the v8 `api-keys` config tree (Go: `config_auth_index.go`).
//!
//! Reads of `/config` inject the live credential index into every key (and keyless
//! openai-compatibility group) as `auth_index`; the field is transient, so it is stripped from
//! request bodies and from the document before anything is persisted.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use cpa_auth::Auth;
use cpa_config::{Config, format_sorted_headers, normalize_headers};
use cpa_runtime::service::synth::{SynthesisContext, synthesize_config_auths};
use serde_yaml_ng::Value;

use crate::state::ManagementState;

const AUTH_INDEX_KEYS: [&str; 2] = ["auth_index", "auth-index"];

fn str_key(key: &str) -> Value {
    Value::String(key.to_string())
}

/// Child of a mapping node by key (Go: `configV8Node(node, []string{key})`).
fn child<'a>(node: &'a Value, key: &str) -> Option<&'a Value> {
    node.as_mapping()?.get(key)
}

/// The scalar text of a node, trimmed (Go: `strings.TrimSpace(node.Value)`).
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        _ => None,
    }
}

/// Go: `yamlMapScalar`.
fn map_scalar(node: &Value, key: &str) -> String {
    child(node, key).and_then(scalar_text).unwrap_or_default()
}

/// Go: `yamlMapScalarPresent`: a null value counts as absent.
fn map_scalar_present(node: &Value, key: &str) -> Option<String> {
    match child(node, key)? {
        Value::Null => None,
        other => scalar_text(other),
    }
}

/// Go: `yamlMapHeadersPresent`: null or an undecodable value counts as absent.
fn map_headers_present(node: &Value, key: &str) -> Option<BTreeMap<String, String>> {
    let Value::Mapping(map) = child(node, key)? else {
        return None;
    };
    let mut out = BTreeMap::new();
    for (k, v) in map {
        let key = match k {
            Value::String(s) => s.clone(),
            other => scalar_text(other)?,
        };
        let text = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => String::new(),
            _ => return None,
        };
        out.insert(key, text);
    }
    Some(out)
}

/// Go: `setMapScalar`. An existing key keeps its position.
fn set_map_scalar(node: &mut Value, key: &str, value: &str) {
    if let Value::Mapping(map) = node {
        map.insert(str_key(key), Value::String(value.to_string()));
    }
}

fn delete_map_key(node: &mut Value, key: &str) {
    if let Value::Mapping(map) = node {
        map.shift_remove(key);
    }
}

fn strip_from_group(group: &mut Value) {
    if !group.is_mapping() {
        return;
    }
    for key in AUTH_INDEX_KEYS {
        delete_map_key(group, key);
    }
    if let Some(Value::Sequence(keys)) = group.as_mapping_mut().and_then(|m| m.get_mut("keys")) {
        for key_node in keys {
            for key in AUTH_INDEX_KEYS {
                delete_map_key(key_node, key);
            }
        }
    }
}

fn strip_from_groups(groups: &mut Value) {
    match groups {
        Value::Sequence(items) => items.iter_mut().for_each(strip_from_group),
        Value::Mapping(_) => strip_from_group(groups),
        _ => {}
    }
}

fn strip_from_providers_map(api_keys: &mut Value) {
    if let Value::Mapping(map) = api_keys {
        for (_, groups) in map.iter_mut() {
            strip_from_groups(groups);
        }
    }
}

/// Removes every transient `auth_index` / `auth-index` from the document's `api-keys` tree.
pub(crate) fn strip_from_root(root: &mut Value) {
    if let Some(api_keys) = root.as_mapping_mut().and_then(|m| m.get_mut("api-keys")) {
        strip_from_providers_map(api_keys);
    }
}

/// Removes the transient field from a request body addressed at `parts`.
pub(crate) fn strip_from_update(parts: &[String], update: &mut Value) {
    let Some(first) = parts.first() else {
        strip_from_root(update);
        return;
    };
    if first != "api-keys" {
        return;
    }
    if parts.len() == 1 {
        strip_from_providers_map(update);
    } else {
        strip_from_groups(update);
    }
}

fn normalize_model_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed.contains('/') {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// Go: `formatCredentialDedupKey`.
fn dedup_key(
    key: &str,
    base: &str,
    proxy_url: &str,
    prefix: &str,
    headers: &BTreeMap<String, String>,
) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}",
        key.trim(),
        base.trim(),
        proxy_url.trim(),
        normalize_model_prefix(prefix),
        format_sorted_headers(&normalize_headers(headers))
    )
}

fn resolve_inherited_scalar(key_node: &Value, group: &Value, field: &str) -> String {
    map_scalar_present(key_node, field)
        .or_else(|| map_scalar_present(group, field))
        .unwrap_or_default()
}

fn resolve_inherited_headers(
    key_node: &Value,
    group: &Value,
    field: &str,
) -> BTreeMap<String, String> {
    map_headers_present(key_node, field)
        .or_else(|| map_headers_present(group, field))
        .unwrap_or_default()
}

/// The credential index of every live credential (Go: `liveAuthIndexFromManager`).
fn live_index_by_id(st: &ManagementState) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for mut auth in st.registry.list() {
        let id = auth.id.trim().to_string();
        if id.is_empty() {
            continue;
        }
        let mut idx = auth.index.trim().to_string();
        if idx.is_empty() {
            idx = auth.ensure_index();
        }
        if !idx.is_empty() {
            out.insert(id, idx);
        }
    }
    out
}

/// Mapping view of the key entries of one group (`keys: [..]`), when it is a list.
fn keys_of(group: &Value) -> Option<&Vec<Value>> {
    match child(group, "keys")? {
        Value::Sequence(keys) => Some(keys),
        _ => None,
    }
}

fn keys_of_mut(group: &mut Value) -> Option<&mut Vec<Value>> {
    match group.as_mapping_mut()?.get_mut("keys")? {
        Value::Sequence(keys) => Some(keys),
        _ => None,
    }
}

/// Live credential lookup shared by the per-provider walks below.
struct Resolver {
    live: HashMap<String, String>,
}

impl Resolver {
    /// The live index of the credential, else the index computed from the synthesized one.
    fn index(&self, auth: &mut Auth) -> String {
        if let Some(idx) = self.live.get(&auth.id)
            && !idx.trim().is_empty()
        {
            return idx.trim().to_string();
        }
        auth.ensure_index()
    }
}

fn config_index_of(auth: &Auth) -> Option<usize> {
    auth.attributes.get("config_index")?.parse().ok()
}

/// Injects `auth_index` into the `api-keys` tree of a JSON read of the v8 document. `data` is the
/// normalized document the tree came from; credentials are synthesized from it exactly as at
/// runtime, so duplicate and keyless entries resolve to the credential that serves them.
pub(crate) fn inject_api_key_auth_indexes(st: &ManagementState, root: &mut Value, data: &str) {
    let Some(Value::Mapping(api_keys)) = root.as_mapping_mut().and_then(|m| m.get_mut("api-keys"))
    else {
        return;
    };
    let cfg: Arc<Config> = match cpa_config::parse_config_bytes(data.as_bytes()) {
        Ok(cfg) => Arc::new(cfg),
        Err(_) => st.cfg(),
    };
    let ctx = SynthesisContext {
        config: &cfg,
        auth_dir: &cfg.auth_dir,
        now: chrono::Utc::now(),
    };
    let Ok(mut auths) = synthesize_config_auths(&ctx) else {
        return;
    };
    let resolver = Resolver {
        live: live_index_by_id(st),
    };

    let mut by_provider: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, auth) in auths.iter().enumerate() {
        by_provider.entry(auth.provider.clone()).or_default().push(i);
    }
    let provider_auths = |name: &str| by_provider.get(name).cloned().unwrap_or_default();

    // Gemini-style families dedupe equal credentials: map the dedup key to the credential index.
    let mut provider_auth_map: HashMap<&str, HashMap<String, String>> = HashMap::new();
    for name in ["gemini", "interactions", "vertex"] {
        provider_auth_map.insert(name, HashMap::new());
    }
    for (provider, label, len) in [
        ("gemini", "gemini", cfg.gemini_key.len()),
        ("gemini-interactions", "interactions", cfg.interactions_key.len()),
    ] {
        for i in provider_auths(provider) {
            let Some(idx) = config_index_of(&auths[i]).filter(|idx| *idx < len) else {
                continue;
            };
            let entry = if label == "gemini" {
                &cfg.gemini_key[idx]
            } else {
                &cfg.interactions_key[idx]
            };
            let key = dedup_key(
                &entry.api_key,
                &entry.base_url,
                &entry.proxy_url,
                &entry.prefix,
                &entry.headers,
            );
            let value = resolver.index(&mut auths[i]);
            provider_auth_map
                .entry(label)
                .or_default()
                .insert(key, value);
        }
    }
    for i in provider_auths("vertex") {
        let Some(idx) =
            config_index_of(&auths[i]).filter(|idx| *idx < cfg.vertex_compat_api_key.len())
        else {
            continue;
        };
        let entry = &cfg.vertex_compat_api_key[idx];
        let key = format!("{}|{}", entry.api_key.trim(), entry.base_url.trim());
        let value = resolver.index(&mut auths[i]);
        provider_auth_map.entry("vertex").or_default().insert(key, value);
    }

    // Per-position families: `config_index` -> credential.
    let mut by_config_index: HashMap<&str, HashMap<String, usize>> = HashMap::new();
    for name in ["codex", "claude", "xai", "meta"] {
        let map = by_config_index.entry(name).or_default();
        for i in provider_auths(name) {
            if let Some(idx) = auths[i].attributes.get("config_index") {
                map.insert(idx.clone(), i);
            }
        }
    }
    let index_for = |auths: &mut [Auth], family: &str, target: usize| -> Option<String> {
        let i = *by_config_index.get(family)?.get(&target.to_string())?;
        let idx = resolver.index(&mut auths[i]);
        (!idx.is_empty()).then_some(idx)
    };

    for (name_node, groups) in api_keys.iter_mut() {
        let provider = name_node.as_str().unwrap_or_default().trim().to_string();
        let Value::Sequence(groups) = groups else {
            continue;
        };
        match provider.as_str() {
            "openai-compatibility" => {
                inject_openai_compat(groups, &mut auths, &resolver);
            }
            "claude" => {
                let mut raw_idx = 0usize;
                for group in groups.iter_mut() {
                    if !group.is_mapping() {
                        continue;
                    }
                    let group_base = map_scalar(group, "base-url");
                    let Some(keys) = keys_of_mut(group) else {
                        continue;
                    };
                    for key_node in keys {
                        if !key_node.is_mapping() {
                            continue;
                        }
                        let api_key = map_scalar(key_node, "api-key");
                        let target = raw_idx;
                        raw_idx += 1;
                        if api_key.is_empty() && group_base.is_empty() {
                            continue;
                        }
                        if let Some(idx) = index_for(&mut auths, "claude", target) {
                            set_map_scalar(key_node, "auth_index", &idx);
                        }
                    }
                }
            }
            "codex" | "xai" => {
                let family = provider.as_str();
                let mut valid_idx = 0usize;
                for group in groups.iter_mut() {
                    if !group.is_mapping() {
                        continue;
                    }
                    if map_scalar(group, "base-url").is_empty() {
                        continue;
                    }
                    let Some(keys) = keys_of_mut(group) else {
                        continue;
                    };
                    for key_node in keys {
                        if !key_node.is_mapping() {
                            continue;
                        }
                        let target = valid_idx;
                        valid_idx += 1;
                        if let Some(idx) = index_for(&mut auths, family, target) {
                            set_map_scalar(key_node, "auth_index", &idx);
                        }
                    }
                }
            }
            "meta" => {
                let mut valid_idx = 0usize;
                for group in groups.iter_mut() {
                    if !group.is_mapping() {
                        continue;
                    }
                    let Some(keys) = keys_of_mut(group) else {
                        continue;
                    };
                    for key_node in keys {
                        if !key_node.is_mapping() {
                            continue;
                        }
                        let api_key = map_scalar(key_node, "api-key");
                        if api_key.is_empty() || api_key.starts_with("dca:") {
                            continue;
                        }
                        let target = valid_idx;
                        valid_idx += 1;
                        if let Some(idx) = index_for(&mut auths, "meta", target) {
                            set_map_scalar(key_node, "auth_index", &idx);
                        }
                    }
                }
            }
            other => {
                for group in groups.iter_mut() {
                    if !group.is_mapping() {
                        continue;
                    }
                    let group_base = map_scalar(group, "base-url");
                    let group_ro = group.clone();
                    let Some(keys) = keys_of_mut(group) else {
                        continue;
                    };
                    for key_node in keys {
                        if !key_node.is_mapping() {
                            continue;
                        }
                        let api_key = map_scalar(key_node, "api-key");
                        if other == "vertex" {
                            if let Some(idx) = provider_auth_map
                                .get("vertex")
                                .and_then(|m| m.get(&format!("{api_key}|{group_base}")))
                            {
                                set_map_scalar(key_node, "auth_index", idx);
                            }
                            continue;
                        }
                        let proxy_url = resolve_inherited_scalar(key_node, &group_ro, "proxy-url");
                        let prefix = resolve_inherited_scalar(key_node, &group_ro, "prefix");
                        let headers = resolve_inherited_headers(key_node, &group_ro, "headers");
                        let key = dedup_key(&api_key, &group_base, &proxy_url, &prefix, &headers);
                        if let Some(idx) = provider_auth_map
                            .get(other)
                            .and_then(|m| m.get(&key))
                        {
                            set_map_scalar(key_node, "auth_index", idx);
                        }
                    }
                }
            }
        }
    }
}

/// OpenAI-compatible groups: the n-th group with a base URL maps to `config_index` n.
fn inject_openai_compat(groups: &mut [Value], auths: &mut [Auth], resolver: &Resolver) {
    let mut valid_group = 0usize;
    for group in groups.iter_mut() {
        if !group.is_mapping() {
            continue;
        }
        if map_scalar(group, "base-url").is_empty() {
            continue;
        }
        let target = valid_group.to_string();
        valid_group += 1;
        let is_compat = |a: &Auth| {
            a.attributes.get("config_index") == Some(&target)
                && (a.provider == "openai-compatibility"
                    || a.provider.starts_with("openai-compatible-"))
        };
        let keyless = keys_of(group).is_none_or(Vec::is_empty);
        if keyless {
            let found = auths
                .iter_mut()
                .find(|a| is_compat(a) && a.attributes.get("api_key").is_none_or(String::is_empty));
            if let Some(auth) = found {
                let idx = resolver.index(auth);
                if !idx.is_empty() {
                    set_map_scalar(group, "auth_index", &idx);
                }
            }
            continue;
        }
        let group_auths: Vec<usize> = auths
            .iter()
            .enumerate()
            .filter(|(_, a)| is_compat(a))
            .map(|(i, _)| i)
            .collect();
        let Some(keys) = keys_of_mut(group) else {
            continue;
        };
        for (k_idx, key_node) in keys.iter_mut().enumerate() {
            if !key_node.is_mapping() {
                continue;
            }
            if let Some(&i) = group_auths.get(k_idx) {
                let idx = resolver.index(&mut auths[i]);
                if !idx.is_empty() {
                    set_map_scalar(key_node, "auth_index", &idx);
                }
            }
        }
    }
}

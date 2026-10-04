//! `/config`, `/config.yaml` and `/config/{*path}` (Go: `config_v8.go`).
//!
//! The persisted YAML file is the source of truth: every request reads it, normalizes it to the
//! v8 layout in memory, and edits the YAML tree by mapping-key path. Writes go through the
//! config crate (`parse_config_bytes`, `validate_v8_config`, `save_config_preserve_comments`) and
//! the new file is published with an in-place rewrite so single-file container mounts keep working.

use std::io::Write as _;
use std::path::Path;

use axum::body::Body;
use axum::http::{HeaderValue, Method, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::{Value as Json, json};
use serde_yaml_ng::{Mapping, Value};

use crate::config_auth_index;
use crate::http::{ApiError, ApiResult, json_response, no_store};
use crate::state::ManagementState;

/// v8 path of the Codex live-media TURN servers (secrets are redacted on JSON reads).
const ICE_SERVERS_PATH: [&str; 5] = [
    "oauth",
    "providers",
    "codex",
    "live-media-relay",
    "ice-servers",
];

/// Fields owned by Home that clients may not change.
const READ_ONLY_FIELDS: [&str; 3] = [
    "credentials/concurrency/lifecycle-config-revision",
    "credentials/concurrency/observation-barrier-revision",
    "plugins/auth-revision",
];

/// One `/config*` request, already split into its pieces.
pub(crate) struct ConfigRequest {
    pub method: Method,
    /// `/config.yaml` rather than the JSON tree.
    pub yaml: bool,
    /// Key path below the document root (empty for the whole document).
    pub parts: Vec<String>,
    pub body: Bytes,
}

/// `Trim(path, "/")` split on `/`.
pub(crate) fn split_path(path: &str) -> Vec<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        Vec::new()
    } else {
        trimmed.split('/').map(str::to_string).collect()
    }
}

/// Serves one config request. The work runs in its own task holding an owned lock guard, so a
/// client disconnect cannot cancel a write (or its reload) half way or release the lock early.
pub(crate) async fn handle(st: &ManagementState, req: ConfigRequest) -> Response {
    let st = st.clone();
    let task = tokio::spawn(async move {
        let _guard = st.shared.config_lock.clone().lock_owned().await;
        let path = st.config_path.clone();
        let task_st = st.clone();
        let result = tokio::task::spawn_blocking(move || run(&task_st, &path, &req)).await;
        match result {
            Ok(Ok(Outcome::Read(resp))) => resp,
            Ok(Ok(Outcome::Saved)) => {
                st.reload_config().await;
                json_response(200, &json!({"status": "ok", "config-version": 8}))
            }
            Ok(Err(e)) => axum::response::IntoResponse::into_response(e),
            Err(_) => {
                axum::response::IntoResponse::into_response(ApiError::new(500, "write_failed"))
            }
        }
    });
    task.await.unwrap_or_else(|e| {
        tracing::error!("config task failed: {e}");
        axum::response::IntoResponse::into_response(ApiError::new(500, "write_failed"))
    })
}

enum Outcome {
    Read(Response),
    Saved,
}

fn run(st: &ManagementState, config_path: &Path, req: &ConfigRequest) -> ApiResult<Outcome> {
    let raw = std::fs::read(config_path).map_err(|_| ApiError::new(500, "read_failed"))?;
    let (normalized, _) = cpa_config::normalize_config_layout(&raw, true)
        .map_err(|e| ApiError::with_message(500, "invalid_config", e.to_string()))?;
    let text = String::from_utf8(normalized).map_err(|_| ApiError::new(500, "invalid_config"))?;
    let mut root = parse_yaml(&text)
        .ok()
        .flatten()
        .ok_or_else(|| ApiError::new(500, "invalid_config"))?;

    let dotted = req.parts.join(".");
    if req.method == Method::GET {
        cpa_config::project_v8_config_aliases(&mut root, &dotted);
        return read(st, &mut root, &text, req).map(Outcome::Read);
    }

    let before = root.clone();
    // A replacement of the whole document brings its own comments and quoting; every other edit
    // carries the comments of the previous document over to the same key paths.
    let replaces_document = req.parts.is_empty() && req.method == Method::PUT;
    let mut comments = cpa_config::DocComments::extract(if replaces_document {
        std::str::from_utf8(&req.body).unwrap_or_default()
    } else {
        &text
    });
    cpa_config::project_v8_config_aliases(&mut root, &dotted);
    let parts = &req.parts;
    if req.method == Method::DELETE {
        if parts.is_empty() {
            return Err(ApiError::new(400, "cannot_delete_config"));
        }
        if !delete_path(&mut root, parts) {
            return Err(ApiError::new(404, "not_found"));
        }
    } else {
        let mut update = parse_update(&req.body, req.yaml)?;
        config_auth_index::strip_from_update(parts, &mut update);
        if parts.is_empty() && !update.is_mapping() {
            return Err(ApiError::new(400, "config_must_be_object"));
        }
        // A PUT of the whole document is normalized below, together with its comments.
        if parts.is_empty() && !replaces_document {
            cpa_config::normalize_v8_config_aliases(&mut update);
        }
        // Paths identify YAML keys, never array indexes. Lists are replaced whole.
        let dst = navigate(&mut root, parts)?;
        // yaml.v3 keeps the style of nodes parsed from a JSON body (flow, quoted strings).
        let json_body = !req.yaml;
        if req.method == Method::PATCH {
            let mut path = parts.clone();
            merge_patch(dst, update, &mut path, json_body.then_some(&mut comments));
        } else {
            if json_body {
                comments.mark_json_subtree(parts, &update);
            }
            *dst = update;
        }
    }
    cpa_config::normalize_v8_config_aliases_with_comments(&mut root, &mut comments);
    if !req.yaml && req.method != Method::DELETE {
        preserve_turn_secrets(&mut root, &before);
    }
    for field in READ_ONLY_FIELDS {
        let p = split_path(field);
        if node(&before, &p) != node(&root, &p) {
            return Err(ApiError::from_body(
                400,
                json!({"error": "read_only_field", "field": field}),
            ));
        }
    }

    config_auth_index::strip_from_root(&mut root);
    let data = cpa_config::marshal_document_with_comments(&root, &comments)
        .map_err(|e| ApiError::with_message(400, "invalid_config", e.to_string()))?;
    cpa_config::parse_config_bytes(data.as_bytes())
        .map_err(|e| ApiError::with_message(422, "invalid_config", e.to_string()))?;
    cpa_config::validate_v8_config(data.as_bytes())
        .map_err(|e| ApiError::with_message(400, "invalid_config", e.to_string()))?;
    let (data, _) = cpa_config::normalize_config_layout_keeping_styles(data.as_bytes(), true, &comments)
        .map_err(|e| ApiError::with_message(400, "invalid_config", e.to_string()))?;
    // Save the validated canonical tree directly: projecting runtime defaults back onto it loses
    // explicit nulls, empty maps and opaque plugin settings.
    write_config(config_path, &data, Some(&comments)).map_err(|e| {
        ApiError::with_message(500, "write_failed", e.to_string())
    })?;
    Ok(Outcome::Saved)
}

fn read(
    st: &ManagementState,
    root: &mut Value,
    text: &str,
    req: &ConfigRequest,
) -> ApiResult<Response> {
    if !req.yaml {
        redact_turn_secrets(root);
        config_auth_index::inject_api_key_auth_indexes(st, root, text);
    }
    let Some(value) = node(root, &req.parts) else {
        return Err(ApiError::new(404, "not_found"));
    };
    if req.yaml {
        let mut resp = Response::new(Body::from(text.to_string()));
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/yaml; charset=utf-8"),
        );
        return Ok(no_store(resp));
    }
    // Go marshals decoded maps with sorted keys.
    let json: Json =
        serde_json::to_value(value).map_err(|_| ApiError::new(500, "decode_failed"))?;
    Ok(no_store(json_response(
        200,
        &cpa_auth::util::sort_json(&json),
    )))
}

/// Go: `WriteConfig`. A v8 document is re-normalized to the latest layout, then written in place
/// (same inode, `O_TRUNC`, fsync) with comment indentation normalized.
pub(crate) fn write_config(
    path: &Path,
    data: &[u8],
    marks: Option<&cpa_config::DocComments>,
) -> std::io::Result<()> {
    let data = cpa_config::normalize_for_write(data, marks)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let data = String::from_utf8_lossy(&data);
    let data = cpa_config::normalize_comment_indentation(&data);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all(data.as_bytes())?;
    f.sync_all()
}

/// Parses a YAML (or JSON) document with anchors expanded. `None` for blank or comment-only text.
fn parse_yaml(text: &str) -> Result<Option<Value>, serde_yaml_ng::Error> {
    if !text.lines().any(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#')
    }) {
        return Ok(None);
    }
    let mut value: Value = serde_yaml_ng::from_str(text)?;
    value.apply_merge()?;
    Ok(Some(value))
}

/// Request body as a YAML value. JSON requests must be valid JSON first.
fn parse_update(body: &[u8], yaml: bool) -> ApiResult<Value> {
    if !yaml && serde_json::from_slice::<serde::de::IgnoredAny>(body).is_err() {
        return Err(ApiError::new(400, "invalid_json"));
    }
    let text = std::str::from_utf8(body).map_err(|_| ApiError::new(400, "invalid_body"))?;
    match parse_yaml(text) {
        Ok(Some(v)) => Ok(v),
        _ => Err(ApiError::new(400, "invalid_body")),
    }
}

/// The node at a key path (mapping keys only).
fn node<'a>(root: &'a Value, parts: &[String]) -> Option<&'a Value> {
    parts
        .iter()
        .try_fold(root, |cur, part| cur.as_mapping()?.get(part.as_str()))
}

/// The node at a key path, creating missing mappings. Fails when an intermediate is not a mapping.
fn navigate<'a>(root: &'a mut Value, parts: &[String]) -> ApiResult<&'a mut Value> {
    let mut dst = root;
    for part in parts {
        let Value::Mapping(map) = dst else {
            return Err(ApiError::new(400, "invalid_path"));
        };
        if part.is_empty() {
            return Err(ApiError::new(400, "invalid_path"));
        }
        dst = map
            .entry(Value::String(part.clone()))
            .or_insert_with(|| Value::Mapping(Mapping::new()));
    }
    Ok(dst)
}

/// Removes the key and prunes only ancestors that became empty mappings; other explicit empty
/// maps can carry inheritance or plugin semantics and stay.
fn delete_path(root: &mut Value, parts: &[String]) -> bool {
    let (Value::Mapping(map), Some(first)) = (root, parts.first()) else {
        return false;
    };
    let Some(child) = map.get_mut(first.as_str()) else {
        return false;
    };
    if parts.len() > 1 {
        if !delete_path(child, &parts[1..]) {
            return false;
        }
        if child.as_mapping().is_some_and(|m| !m.is_empty()) {
            return true;
        }
    }
    map.shift_remove(first.as_str());
    true
}

/// Unlike JSON merge-patch, null is retained: optional key overrides use it to inherit the group
/// value. DELETE is the explicit field-removal operation.
fn merge_patch(
    dst: &mut Value,
    src: Value,
    path: &mut Vec<String>,
    mut marks: Option<&mut cpa_config::DocComments>,
) {
    match (dst, src) {
        (Value::Mapping(d), Value::Mapping(s)) => {
            for (key, value) in s {
                path.push(key.as_str().unwrap_or_default().to_string());
                match d.get_mut(&key) {
                    Some(old) => merge_patch(old, value, path, marks.as_deref_mut()),
                    None => {
                        if let Some(marks) = marks.as_deref_mut() {
                            marks.mark_json_key(path);
                            marks.mark_json_subtree(path, &value);
                        }
                        d.insert(key, value);
                    }
                }
                path.pop();
            }
        }
        (dst, src) => {
            if let Some(marks) = marks {
                marks.mark_json_subtree(path, &src);
            }
            *dst = src;
        }
    }
}

fn ice_servers_mut(root: &mut Value) -> Option<&mut Vec<Value>> {
    let mut cur = root;
    for part in ICE_SERVERS_PATH {
        cur = cur.as_mapping_mut()?.get_mut(part)?;
    }
    cur.as_sequence_mut()
}

fn redact_turn_secrets(root: &mut Value) {
    if let Some(servers) = ice_servers_mut(root) {
        for server in servers {
            if let Some(m) = server.as_mapping_mut() {
                m.shift_remove("username");
                m.shift_remove("credential");
            }
        }
    }
}

/// JSON reads redact TURN secrets, so a client writing that JSON back omits them. Restore omitted
/// credentials by matching entries on their `urls` (each previous entry used once), so a changed
/// URL cannot inherit credentials of a different server. Explicit empty strings and null clear a
/// secret; YAML writes keep full replacement semantics.
fn preserve_turn_secrets(root: &mut Value, before: &Value) {
    let prev_path: Vec<String> = ICE_SERVERS_PATH.iter().map(|s| (*s).to_string()).collect();
    let Some(Value::Sequence(previous)) = node(before, &prev_path) else {
        return;
    };
    let Some(next) = ice_servers_mut(root) else {
        return;
    };
    let mut matched = vec![false; previous.len()];
    for server in next {
        let Some(urls) = server.as_mapping().and_then(|m| m.get("urls")).cloned() else {
            continue;
        };
        let Some(i) = previous.iter().enumerate().position(|(i, old)| {
            !matched[i] && old.as_mapping().and_then(|m| m.get("urls")) == Some(&urls)
        }) else {
            continue;
        };
        matched[i] = true;
        let (Some(map), Some(old)) = (server.as_mapping_mut(), previous[i].as_mapping()) else {
            continue;
        };
        for name in ["username", "credential"] {
            if !map.contains_key(name)
                && let Some(secret) = old.get(name)
            {
                map.insert(Value::String(name.into()), secret.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn y(s: &str) -> Value {
        serde_yaml_ng::from_str(s).unwrap()
    }

    fn p(s: &str) -> Vec<String> {
        split_path(s)
    }

    #[test]
    fn delete_prunes_only_emptied_ancestors() {
        let mut v = y("a:\n  b:\n    c: 1\n  keep: {}\nz: 1\n");
        assert!(delete_path(&mut v, &p("a/b/c")));
        assert_eq!(v, y("a:\n  keep: {}\nz: 1\n"));
        assert!(!delete_path(&mut v, &p("a/missing")));
        assert!(delete_path(&mut v, &p("a/keep")));
        assert_eq!(v, y("z: 1\n"));
    }

    #[test]
    fn patch_merges_maps_and_keeps_null_but_replaces_lists() {
        let mut v = y("a: {x: 1, y: [1, 2]}\n");
        merge_patch(&mut v, y("a: {y: [3], z: null}\n"), &mut Vec::new(), None);
        assert_eq!(v, y("a: {x: 1, y: [3], z: null}\n"));
    }

    #[test]
    fn navigate_creates_mappings_and_rejects_scalars() {
        let mut v = y("a: 1\n");
        assert!(navigate(&mut v, &p("b/c")).is_ok());
        assert_eq!(v, y("a: 1\nb: {c: {}}\n"));
        assert!(navigate(&mut v, &p("a/x")).is_err());
    }

    #[test]
    fn turn_secrets_are_redacted_and_restored_by_urls() {
        let doc = "oauth:\n  providers:\n    codex:\n      live-media-relay:\n        ice-servers:\n        - urls: [turn:a]\n          username: u\n          credential: c\n        - urls: [turn:b]\n          username: u2\n";
        let before = y(doc);
        let mut read = before.clone();
        redact_turn_secrets(&mut read);
        assert!(read != before);
        // The client writes the redacted JSON back with the second server's URL changed.
        let mut write = read;
        let servers = ice_servers_mut(&mut write).unwrap();
        servers[1]
            .as_mapping_mut()
            .unwrap()
            .insert("urls".into(), y("[turn:c]"));
        preserve_turn_secrets(&mut write, &before);
        let servers = ice_servers_mut(&mut write).unwrap();
        assert_eq!(
            servers[0].as_mapping().unwrap().get("credential"),
            Some(&Value::String("c".into()))
        );
        assert!(servers[1].as_mapping().unwrap().get("username").is_none());
    }
}

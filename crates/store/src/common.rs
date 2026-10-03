//! Helpers shared by the Postgres, object and repository stores (the unexported helpers of
//! internal/store plus the common body of each store's `Save`).

use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use cpa_auth::credmeta::Metadata;
use cpa_auth::store::StoreError;
use cpa_auth::util::{clean_path, marshal_compact};
use cpa_auth::Auth;
use serde_json::Value;

pub(crate) fn backend_err(msg: impl Into<String>) -> StoreError {
    StoreError::Backend(msg.into())
}

/// Go `os.MkdirAll(dir, 0o700)`.
pub(crate) fn mkdir_all_private(dir: &Path) -> std::io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// Go `os.WriteFile(path, data, 0o600)` (mode applies to newly created files only).
pub(crate) fn write_file_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(data)
}

/// `misc.CopyConfigTemplate`: copies `src` to `dst` (dir 0700, file 0600, truncating) and syncs.
pub fn copy_config_template(src: &Path, dst: &Path) -> std::io::Result<()> {
    let data = fs::read(src)?;
    if let Some(dir) = dst.parent().filter(|d| !d.as_os_str().is_empty()) {
        mkdir_all_private(dir)?;
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut out = opts.open(dst)?;
    out.write_all(&data)?;
    out.sync_all()
}

/// `normalizeLineEndings`: CRLF and lone CR become LF.
pub(crate) fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Byte flavor of [`normalize_line_endings`] (`normalizeLineEndingsBytes`).
pub(crate) fn normalize_line_endings_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\r' => {
                out.push(b'\n');
                if data.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    out
}

/// `valueAsString`: the string value, or "" for anything else.
pub(crate) fn value_as_string(v: Option<&Value>) -> &str {
    v.and_then(Value::as_str).unwrap_or("")
}

/// `labelFor`: label, else email, else project_id (each trimmed).
pub(crate) fn label_for(metadata: &Metadata) -> String {
    for key in ["label", "email", "project_id"] {
        let v = value_as_string(metadata.get(key)).trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    String::new()
}

/// `normalizeAuthID`: `filepath.ToSlash(filepath.Clean(id))`.
pub(crate) fn normalize_auth_id(id: &str) -> String {
    clean_path(Path::new(id)).to_string_lossy().replace('\\', "/")
}

/// Semantic JSON equality (`jsonEqual`): key order and number formatting insensitive.
pub(crate) fn json_equal(a: &[u8], b: &[u8]) -> bool {
    match (
        serde_json::from_slice::<Value>(a),
        serde_json::from_slice::<Value>(b),
    ) {
        (Ok(a), Ok(b)) => deep_equal_json(&a, &b),
        _ => false,
    }
}

fn deep_equal_json(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| deep_equal_json(v, w)))
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| deep_equal_json(p, q))
        }
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (x, y) => x == y,
    }
}

/// `filepath.Rel(base, path)` for already-cleaned absolute paths; `None` when they share no root.
pub(crate) fn rel_path(base: &Path, path: &Path) -> Option<PathBuf> {
    let base = clean_path(base);
    let path = clean_path(path);
    let b: Vec<Component> = base.components().collect();
    let p: Vec<Component> = path.components().collect();
    let common = b.iter().zip(&p).take_while(|(x, y)| x == y).count();
    if common == 0 && (b.first() != p.first()) {
        return None;
    }
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &p[common..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    Some(out)
}

/// `filepath.WalkDir` over `*.json` files in lexical order; directory read errors abort the walk.
pub(crate) fn walk_json_files(dir: &Path, visit: &mut dyn FnMut(&Path)) -> std::io::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_json_files(&path, visit)?;
        } else if entry.file_name().to_string_lossy().to_lowercase().ends_with(".json") {
            visit(&path);
        }
    }
    Ok(())
}

/// What the shared `Save` body did to the file.
pub(crate) enum Written {
    /// Metadata-only credential whose content already matched the file on disk.
    Unchanged,
    Written,
}

/// The `switch` shared by every remote store's `Save`: write the credential file through its
/// token storage, or from bare metadata (temp file + rename, skipped when semantically equal).
/// `prefix` is the store's error prefix (`postgres store`, `object store`).
pub(crate) fn write_auth_file(prefix: &str, auth: &mut Auth, path: &Path) -> Result<Written, StoreError> {
    if let Some(storage) = auth.storage.clone() {
        auth.metadata.insert("disabled".into(), Value::Bool(auth.disabled));
        storage.save_to_file(path, &auth.metadata)?;
        return Ok(Written::Written);
    }
    if auth.metadata.is_empty() {
        return Err(backend_err(format!("{prefix}: nothing to persist for {}", auth.id)));
    }
    auth.metadata.insert("disabled".into(), Value::Bool(auth.disabled));
    let raw = marshal_compact(&Value::Object(auth.metadata.clone()))
        .map_err(|e| backend_err(format!("{prefix}: marshal metadata: {e}")))?;
    match fs::read(path) {
        Ok(existing) if json_equal(&existing, raw.as_bytes()) => return Ok(Written::Unchanged),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(backend_err(format!("{prefix}: read existing metadata: {e}"))),
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    write_file_private(&tmp, raw.as_bytes())
        .map_err(|e| backend_err(format!("{prefix}: write temp auth file: {e}")))?;
    fs::rename(&tmp, path).map_err(|e| backend_err(format!("{prefix}: rename auth file: {e}")))?;
    Ok(Written::Written)
}

/// Runtime updates must not recreate a disabled credential whose file was removed on purpose.
pub(crate) fn skip_disabled_recreate(auth: &Auth, creation_intent: bool, path: &Path) -> bool {
    auth.disabled && !creation_intent && !path.exists()
}

/// Sets `path` / `source_backend` / `file_name` after a successful save.
pub(crate) fn stamp_saved(auth: &mut Auth, path: &Path, backend: &str) {
    auth.attributes.insert(
        cpa_auth::types::ATTRIBUTE_PATH.into(),
        path.to_string_lossy().into_owned(),
    );
    auth.attributes
        .insert(cpa_auth::types::ATTRIBUTE_SOURCE_BACKEND.into(), backend.into());
    if auth.file_name.trim().is_empty() {
        auth.file_name = auth.id.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings() {
        assert_eq!(normalize_line_endings("a\r\nb\rc\n"), "a\nb\nc\n");
        assert_eq!(normalize_line_endings_bytes(b"a\r\nb\rc\n\r\r\n"), b"a\nb\nc\n\n\n");
    }

    #[test]
    fn rel_paths() {
        assert_eq!(rel_path(Path::new("/a/b"), Path::new("/a/b/c/d.json")), Some("c/d.json".into()));
        assert_eq!(rel_path(Path::new("/a/b"), Path::new("/a/x")), Some("../x".into()));
        assert_eq!(rel_path(Path::new("/a/b"), Path::new("/a/b")), Some(".".into()));
    }

    #[test]
    fn json_equality_ignores_order() {
        assert!(json_equal(br#"{"a":1,"b":[1,2]}"#, br#"{"b":[1,2],"a":1.0}"#));
        assert!(!json_equal(br#"{"a":1}"#, br#"{"a":2}"#));
        assert!(!json_equal(b"nope", b"{}"));
    }

    #[test]
    fn auth_id_normalization() {
        assert_eq!(normalize_auth_id("a//b/../c.json"), "a/c.json");
    }
}

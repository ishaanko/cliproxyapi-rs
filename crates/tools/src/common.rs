//! Pieces shared by the model fetch tools: config and auth-dir resolution, credential listing,
//! Go-compatible JSON output.

use std::path::{Path, PathBuf};

use cpa_auth::store::{FileTokenStore, Store};
use cpa_auth::types::Auth;
use cpa_config::Config;
use serde::Serialize;
use serde_json::Value;

/// Logging like the Go tools' `init`: base logger at info level.
pub fn init_logging() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).try_init();
}

/// Resolved environment of a fetch tool.
pub struct Env {
    pub cfg: Config,
    pub wd: PathBuf,
    pub auths_dir: String,
    pub auths_dir_overridden: bool,
    pub output_path: PathBuf,
}

/// Go's `filepath.Join(a, b)` for the tools' simple cases.
fn join(a: &Path, b: &str) -> PathBuf {
    PathBuf::from(cpa_core::util::filepath_clean(&format!("{}/{b}", a.to_string_lossy())))
}

/// The shared prologue of every fetch tool: working directory, config (default
/// `<wd>/config.yaml`), auth directory (flag override or config `auth-dir`, `~` resolved) and the
/// absolute output path. Errors are the Go `error: ...` messages.
pub fn setup(
    auths_dir_flag: &str,
    auths_dir_overridden: bool,
    config_path: &str,
    output_path: &str,
) -> Result<Env, String> {
    let wd = std::env::current_dir().map_err(|e| format!("error: cannot get working directory: {e}"))?;

    let config_path = if config_path.trim().is_empty() {
        join(&wd, "config.yaml").to_string_lossy().into_owned()
    } else {
        config_path.to_string()
    };
    let cfg = cpa_config::load_config_optional(&config_path, false)
        .map_err(|e| format!("error: failed to load config file {config_path}: {e}"))?;

    let mut auths_dir = if !auths_dir_overridden {
        cfg.auth_dir.clone()
    } else {
        let trimmed = auths_dir_flag.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('~') && !Path::new(auths_dir_flag).is_absolute() {
            join(&wd, auths_dir_flag).to_string_lossy().into_owned()
        } else {
            auths_dir_flag.to_string()
        }
    };
    auths_dir = cpa_core::util::resolve_auth_dir(&auths_dir)
        .map_err(|e| format!("error: failed to resolve auth directory: {e}"))?;

    let output_path = if Path::new(output_path).is_absolute() {
        PathBuf::from(output_path)
    } else {
        join(&wd, output_path)
    };
    Ok(Env { cfg, wd, auths_dir, auths_dir_overridden, output_path })
}

/// Lists the credentials of `auths_dir` (Go: `FileTokenStore.List`).
pub fn list_auths(auths_dir: &str) -> Result<Vec<Auth>, String> {
    FileTokenStore::with_dir(auths_dir)
        .list()
        .map_err(|e| format!("error: failed to list auth files: {e}"))
}

/// `metaStringValue` of the codex tool: a trimmed string value, else empty.
pub fn meta_string_trimmed(meta: &serde_json::Map<String, Value>, key: &str) -> String {
    meta.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// `metaStringValue` of the antigravity tool: the string value as is, else empty.
pub fn meta_string(meta: &serde_json::Map<String, Value>, key: &str) -> String {
    meta.get(key).and_then(Value::as_str).map(str::to_string).unwrap_or_default()
}

/// `json.Marshal` / `json.MarshalIndent("", "  ")` of a struct: Go escapes `<`, `>`, `&`,
/// U+2028 and U+2029 inside strings.
pub fn go_marshal<T: Serialize>(value: &T, pretty: bool) -> Result<Vec<u8>, String> {
    let text = if pretty { serde_json::to_string_pretty(value) } else { serde_json::to_string(value) }
        .map_err(|e| e.to_string())?;
    // serde_json only emits these characters inside string literals, so a global replace is safe.
    let escaped = text
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    Ok(escaped.into_bytes())
}

/// `json.Indent(raw, "", "  ")` plus a trailing newline: re-indents without touching string
/// contents or number formatting. `raw` must be valid JSON.
pub fn pretty_json(raw: &[u8]) -> Result<Vec<u8>, String> {
    serde_json::from_slice::<serde::de::IgnoredAny>(raw).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(raw.len() * 2);
    let mut depth = 0usize;
    let mut need_indent = false;
    let mut in_string = false;
    let mut escaped = false;
    let newline = |out: &mut Vec<u8>, depth: usize| {
        out.push(b'\n');
        out.extend(std::iter::repeat_n(b' ', depth * 2));
    };
    for &c in raw {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        if matches!(c, b' ' | b'\t' | b'\r' | b'\n') {
            continue;
        }
        if need_indent && c != b']' && c != b'}' {
            need_indent = false;
            depth += 1;
            newline(&mut out, depth);
        }
        match c {
            b'"' => {
                in_string = true;
                out.push(c);
            }
            b'{' | b'[' => {
                out.push(c);
                need_indent = true;
            }
            b',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            b':' => out.extend_from_slice(b": "),
            b'}' | b']' => {
                if need_indent {
                    // Empty object or array stays on one line.
                    need_indent = false;
                } else {
                    depth = depth.saturating_sub(1);
                    newline(&mut out, depth);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push(b'\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_json_matches_go_indent() {
        let raw = br#"{"a":[1,2,{"b":"x\"y,<z>"}],"c":{},"d":[],"e":1.50}"#;
        let got = String::from_utf8(pretty_json(raw).unwrap()).unwrap();
        let want = "{\n  \"a\": [\n    1,\n    2,\n    {\n      \"b\": \"x\\\"y,<z>\"\n    }\n  ],\n  \"c\": {},\n  \"d\": [],\n  \"e\": 1.50\n}\n";
        assert_eq!(got, want);
        assert!(pretty_json(b"{\"a\":").is_err());
    }

    #[test]
    fn go_marshal_escapes_html_like_go() {
        #[derive(Serialize)]
        struct T {
            name: &'static str,
        }
        let out = go_marshal(&T { name: "a<b>&c" }, false).unwrap();
        let want = ["{\"name\":\"a", "\\", "u003cb", "\\", "u003e", "\\", "u0026c\"}"].concat();
        assert_eq!(String::from_utf8(out).unwrap(), want);
    }
}

//! Small helpers shared across the crate: Go-compatible JSON encoding, time formatting,
//! path cleaning and secret-safe random generation.

use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, SecondsFormat, Utc};
use serde_json::{Map, Value};

/// Go's zero `time.Time` (0001-01-01T00:00:00Z). Used where Go code stores "unset" as a zero time.
pub fn zero_time() -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(1, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc())
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// True for `None` and Go's zero time.
pub fn is_unset(t: &Option<DateTime<Utc>>) -> bool {
    match t {
        None => true,
        Some(t) => *t == zero_time(),
    }
}

/// `time.Now().Format(time.RFC3339)`: local offset, `Z` when the local zone is UTC.
pub fn now_rfc3339_local() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `t.Format(time.RFC3339)` with the local offset.
pub fn format_rfc3339_local(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `t.UTC().Format(time.RFC3339)`.
pub fn format_rfc3339_utc(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `time.Now().UTC().Format(time.RFC3339)`.
pub fn now_rfc3339_utc() -> String {
    format_rfc3339_utc(Utc::now())
}

/// `now + seconds` formatted like Go's `time.Now().Add(...).Format(time.RFC3339)` (local offset).
pub fn expiry_local(expires_in_secs: i64) -> String {
    format_rfc3339_local(Utc::now() + chrono::Duration::seconds(expires_in_secs))
}

/// Recursively sorts object keys. Go marshals `map[string]any` with sorted keys at every level, and
/// every credential file written by the Go app goes through such a map, so we do the same.
pub fn sort_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::with_capacity(map.len());
            for k in keys {
                if let Some(v) = map.get(k) {
                    out.insert(k.clone(), sort_json(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_json).collect()),
        other => other.clone(),
    }
}

/// Go's `encoding/json` escapes `<`, `>`, `&`, U+2028 and U+2029 inside strings. None of these can
/// occur outside a JSON string literal, so a plain replace on the serialized text is safe.
fn go_html_escape(s: String) -> String {
    if !s.contains(['<', '>', '&', '\u{2028}', '\u{2029}']) {
        return s;
    }
    s.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// `json.Marshal(map)`: compact, sorted keys, HTML-escaped, no trailing newline.
pub fn marshal_compact(value: &Value) -> serde_json::Result<String> {
    Ok(go_html_escape(serde_json::to_string(&sort_json(value))?))
}

/// `json.NewEncoder(w).Encode(map)`: compact with trailing newline.
pub fn encode_compact(value: &Value) -> serde_json::Result<String> {
    let mut s = marshal_compact(value)?;
    s.push('\n');
    Ok(s)
}

/// `enc.SetIndent("", "  "); enc.Encode(map)`: two-space indent with trailing newline.
pub fn encode_pretty(value: &Value) -> serde_json::Result<String> {
    let mut s = go_html_escape(serde_json::to_string_pretty(&sort_json(value))?);
    s.push('\n');
    Ok(s)
}

/// Lexical `filepath.Clean`.
pub fn clean_path(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(comp),
            },
            other => out.push(other),
        }
    }
    if out.is_empty() {
        return PathBuf::from(".");
    }
    out.into_iter().collect()
}

/// `filepath.Clean(filepath.Abs(p))`; falls back to the cleaned input when the cwd is unavailable.
pub fn abs_clean(p: &str) -> String {
    let path = Path::new(p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    clean_path(&abs).to_string_lossy().into_owned()
}

/// Go `url.QueryEscape`: unreserved `-_.~` and alphanumerics pass through, space is `+`.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Go `url.Values.Encode`: `k=v` pairs joined by `&`, sorted by key (stable for equal keys).
pub fn encode_query(pairs: &[(&str, &str)]) -> String {
    let mut sorted: Vec<&(&str, &str)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    sorted
        .iter()
        .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Trimmed string value of a JSON value, `""` for non-strings (Go `v.(string)` + `TrimSpace`).
pub fn trimmed_str(v: Option<&Value>) -> String {
    v.and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// Lowercase hex of `n` random bytes.
pub fn random_hex(n: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// First `n` hex chars of sha256(input).
pub fn sha256_hex_prefix(input: &str, n: usize) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    let full = hex::encode(digest);
    full[..n.min(full.len())].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clean_path_matches_go() {
        assert_eq!(
            clean_path(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(clean_path(Path::new("/../x")), PathBuf::from("/x"));
        assert_eq!(clean_path(Path::new("a/../..")), PathBuf::from(".."));
    }

    #[test]
    fn query_encoding_matches_go_values_encode() {
        assert_eq!(
            encode_query(&[
                ("scope", "a:b c"),
                ("code", "true"),
                ("redirect_uri", "http://localhost:1/cb?x=~*")
            ]),
            "code=true&redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcb%3Fx%3D~%2A&scope=a%3Ab+c"
        );
    }

    #[test]
    fn go_style_json_encoding() {
        let v = json!({"b": 1, "a": {"z": "<x&y>", "c": []}});
        assert_eq!(
            marshal_compact(&v).unwrap(),
            "{\"a\":{\"c\":[],\"z\":\"\\u003cx\\u0026y\\u003e\"},\"b\":1}"
        );
        let pretty = encode_pretty(&json!({"k": ["v"]})).unwrap();
        assert_eq!(pretty, "{\n  \"k\": [\n    \"v\"\n  ]\n}\n");
    }
}

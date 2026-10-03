//! HTML-escaping of text and JSON documents returned to browser-facing management clients
//! (Go: internal/htmlsanitize).

use serde_json::Value;

/// `html.EscapeString`: escapes `< > & ' "`.
pub fn string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes each string in `values` while preserving order.
pub fn strings<S: AsRef<str>>(values: &[S]) -> Vec<String> {
    values.iter().map(|v| string(v.as_ref())).collect()
}

/// Escapes all string values in a JSON document. Returns the re-encoded body (object keys sorted,
/// numbers untouched, no HTML escaping) and `true`, or the input and `false` when `body` is not a
/// single JSON value.
pub fn json_body(body: &[u8]) -> (Vec<u8>, bool) {
    let trimmed = body.trim_ascii();
    if trimmed.is_empty() {
        return (body.to_vec(), false);
    }
    // Go's decoder maps each invalid UTF-8 byte inside strings to U+FFFD.
    let text = go_lossy(trimmed);
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return (body.to_vec(), false);
    };
    let mut out = String::with_capacity(text.len());
    encode(&json_value(value), &mut out);
    (out.into_bytes(), true)
}

/// Escapes JSON bodies when the content type or body shape indicates JSON.
pub fn json_body_if_likely(body: &[u8], content_type: &str) -> (Vec<u8>, bool) {
    if is_json_content_type(content_type) || looks_like_json(body) {
        return json_body(body);
    }
    (body.to_vec(), false)
}

/// Recursively escapes string values (not object keys) in JSON data.
pub fn json_value(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(string(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(json_value).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, json_value(v))).collect()),
        other => other,
    }
}

/// True for `application/json` and `+json` media types (case-insensitive, parameters ignored).
pub fn is_json_content_type(content_type: &str) -> bool {
    let trimmed = content_type.trim();
    let media = match parse_media_type(trimmed) {
        Some(m) => m,
        None => trimmed.to_lowercase(),
    };
    media == "application/json" || media.ends_with("+json")
}

/// True when `body` starts with an object or array marker.
pub fn looks_like_json(body: &[u8]) -> bool {
    matches!(body.trim_ascii().first(), Some(b'{' | b'['))
}

/// Lossy UTF-8 decode that emits one U+FFFD per invalid byte, like Go's string conversion.
fn go_lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for _ in chunk.invalid() {
            out.push('\u{FFFD}');
        }
    }
    out
}

/// `json.Encoder` with `SetEscapeHTML(false)` over `map[string]any` data: sorted object keys.
fn encode(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => encode_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_string(key, out);
                out.push(':');
                encode(item, out);
            }
            out.push('}');
        }
    }
}

fn encode_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn is_tspecial(b: u8) -> bool {
    b"()<>@,;:\\\"/[]?=".contains(&b)
}

fn is_token_char(b: u8) -> bool {
    b > b' ' && b < 0x7f && !is_tspecial(b)
}

fn consume_token(v: &str) -> (&str, &str) {
    let end = v.bytes().position(|b| !is_token_char(b)).unwrap_or(v.len());
    v.split_at(end)
}

fn consume_value(v: &str) -> (String, &str) {
    if v.is_empty() {
        return (String::new(), v);
    }
    if !v.starts_with('"') {
        let (tok, rest) = consume_token(v);
        return (tok.to_string(), rest);
    }
    // Quoted string; a backslash only escapes a tspecial byte (Go keeps it literal otherwise).
    let bytes = v.as_bytes();
    let mut buf: Vec<u8> = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        let r = bytes[i];
        if r == b'"' {
            return (String::from_utf8_lossy(&buf).into_owned(), &v[i + 1..]);
        }
        if r == b'\\' && i + 1 < bytes.len() && is_tspecial(bytes[i + 1]) {
            buf.push(bytes[i + 1]);
            i += 2;
            continue;
        }
        if r == b'\r' || r == b'\n' {
            return (String::new(), v);
        }
        buf.push(r);
        i += 1;
    }
    (String::new(), v)
}

/// One `; key=value` parameter: `(key, value, rest)`, or `None` when malformed.
fn consume_media_param(v: &str) -> Option<(String, String, &str)> {
    let rest = v.trim_start();
    let rest = rest.strip_prefix(';')?.trim_start();
    let (param, rest) = consume_token(rest);
    if param.is_empty() {
        return None;
    }
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let (value, rest2) = consume_value(rest);
    if value.is_empty() && rest2.len() == rest.len() {
        return None;
    }
    Some((param.to_lowercase(), value, rest2))
}

/// `mime.ParseMediaType` reduced to what the caller needs: the lowercased media type, or `None`
/// where Go returns an error (bad type token, malformed or duplicate parameter).
fn parse_media_type(v: &str) -> Option<String> {
    let (media, params) = match v.split_once(';') {
        Some((m, _)) => (m, &v[m.len()..]),
        None => (v, ""),
    };
    let media = media.trim().to_lowercase();
    let (typ, rest) = consume_token(&media);
    if typ.is_empty() {
        return None;
    }
    if !rest.is_empty() {
        let sub = rest.strip_prefix('/')?;
        let (subtype, rest) = consume_token(sub);
        if subtype.is_empty() || !rest.is_empty() {
            return None;
        }
    }
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut rest = params;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        match consume_media_param(rest) {
            Some((key, value, next)) => {
                // Duplicate names are tolerated only when the values are equal.
                match seen.iter().find(|(k, _)| *k == key) {
                    Some((_, existing)) if *existing != value => return None,
                    Some(_) => {}
                    None => seen.push((key, value)),
                }
                rest = next;
            }
            None => {
                if rest.trim() == ";" {
                    break;
                }
                return None;
            }
        }
    }
    Some(media)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_body_escapes_string_values() {
        let (got, ok) = json_body(
            br#"{"title":"<script>alert(1)</script>","items":["safe & sound",{"description":"<b>mode</b>"}],"count":1}"#,
        );
        assert!(ok);
        let body: Value = serde_json::from_slice(&got).unwrap();
        assert_eq!(body["title"], string("<script>alert(1)</script>"));
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], string("safe & sound"));
        assert_eq!(items[1]["description"], string("<b>mode</b>"));
        assert_eq!(body["count"], 1);
    }

    #[test]
    fn json_body_if_likely_skips_non_json_html() {
        let body = b"<!doctype html><title>plugin</title>";
        let (got, ok) = json_body_if_likely(body, "text/html; charset=utf-8");
        assert!(!ok);
        assert_eq!(got, body);
    }

    #[test]
    fn json_body_matches_go_encoder_quirks() {
        // Sorted keys, numbers kept verbatim, no HTML escaping, U+2028 escaped.
        let (got, ok) = json_body("{\"b\":1.50,\"a\":\"\u{2028}\\u0001'\"}".as_bytes());
        assert!(ok);
        assert_eq!(String::from_utf8(got).unwrap(), "{\"a\":\"\\u2028\\u0001&#39;\",\"b\":1.50}");
        assert!(!json_body(b"{} {}").1);
        assert!(!json_body(b"  ").1);
    }

    #[test]
    fn content_type_detection() {
        for ct in ["application/json", " Application/JSON; charset=utf-8", "application/problem+json"] {
            assert!(is_json_content_type(ct), "{ct}");
        }
        // Malformed parameters make Go fall back to the whole string.
        assert!(!is_json_content_type("application/json; charset"));
        assert!(!is_json_content_type("text/html"));
    }
}

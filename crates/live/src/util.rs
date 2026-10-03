//! Go-compatible helpers shared by the live handlers: media types, `json.RawMessage` maps, model
//! extraction and URL helpers.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use serde_json::value::RawValue;

use crate::go_json;

/// Default Codex live model.
pub const DEFAULT_LIVE_MODEL: &str = "gpt-live-1-codex";

/// `callIDPattern`: `^[A-Za-z0-9_-]{1,128}$`.
static CALL_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{1,128}$").expect("static regex"));

pub fn is_call_id(s: &str) -> bool {
    CALL_ID.is_match(s)
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_token_char)
}

/// `mime.ParseMediaType`: lowercased media type plus parameters (keys lowercased). `None` on any
/// syntax error (callers only test `err == nil`).
pub fn parse_media_type(value: &str) -> Option<(String, Vec<(String, String)>)> {
    let (base, rest) = match value.split_once(';') {
        Some((b, r)) => (b, Some(r)),
        None => (value, None),
    };
    let media = base.trim().to_ascii_lowercase();
    let valid = match media.split_once('/') {
        Some((a, b)) => is_token(a) && is_token(b),
        None => is_token(&media),
    };
    if !valid {
        return None;
    }
    let mut params: Vec<(String, String)> = Vec::new();
    let mut rest = rest.map(str::to_string).unwrap_or_default();
    while !rest.trim().is_empty() {
        let trimmed = rest.trim_start().to_string();
        let eq = trimmed.find('=')?;
        let key = trimmed[..eq].trim().to_ascii_lowercase();
        if !is_token(&key) || params.iter().any(|(k, _)| *k == key) {
            return None;
        }
        let after = trimmed[eq + 1..].trim_start();
        let (val, remainder) = if let Some(quoted) = after.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = quoted.char_indices();
            let mut end = None;
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        let (_, n) = chars.next()?;
                        out.push(n);
                    }
                    '"' => {
                        end = Some(i + 1);
                        break;
                    }
                    c => out.push(c),
                }
            }
            (out, quoted[end?..].to_string())
        } else {
            let end = after.find(';').unwrap_or(after.len());
            let v = after[..end].trim();
            if !is_token(v) {
                return None;
            }
            (v.to_string(), after[end..].to_string())
        };
        params.push((key, val));
        let remainder = remainder.trim_start();
        rest = match remainder.strip_prefix(';') {
            Some(r) => r.to_string(),
            None if remainder.is_empty() => String::new(),
            None => return None,
        };
    }
    Some((media, params))
}

/// Media type of a `Content-Type` value, or `None` when it does not parse.
pub fn media_type(content_type: &str) -> Option<String> {
    parse_media_type(content_type).map(|(m, _)| m)
}

/// `json.Compact` with HTML escaping, as `json.Marshal` applies to a `json.RawMessage`.
pub fn compact_escaped(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in raw.chars() {
        if in_string {
            match c {
                _ if escaped => {
                    escaped = false;
                    out.push(c);
                }
                '\\' => {
                    escaped = true;
                    out.push(c);
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                '<' => out.push_str("\\u003c"),
                '>' => out.push_str("\\u003e"),
                '&' => out.push_str("\\u0026"),
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                c => out.push(c),
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !matches!(c, ' ' | '\t' | '\n' | '\r') {
            out.push(c);
        }
    }
    out
}

/// A decoded `map[string]json.RawMessage`: keys sorted, raw values kept verbatim.
pub type RawMap = BTreeMap<String, Box<RawValue>>;

/// Name Go uses for a JSON value kind in `UnmarshalTypeError`.
fn go_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `json.Unmarshal(body, &map[string]json.RawMessage)` (Go 1.25 names the value type `jsontext.Value`): syntax errors and non-object roots carry
/// Go's messages. A JSON `null` decodes to an empty map.
pub fn unmarshal_raw_map(body: &[u8]) -> Result<RawMap, String> {
    go_json::check_valid(body)?;
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Null) => Ok(RawMap::new()),
        Ok(Value::Object(_)) => serde_json::from_slice::<RawMap>(body).map_err(|e| e.to_string()),
        Ok(other) => Err(format!(
            "json: cannot unmarshal {} into Go value of type map[string]jsontext.Value",
            go_kind(&other)
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// `json.Marshal(map[string]json.RawMessage)`.
pub fn marshal_raw_map(map: &RawMap) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in map.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&cpa_core::util::go_json_string(key));
        out.push(':');
        out.push_str(&compact_escaped(value.get()));
    }
    out.push('}');
    out
}

/// Boxes already-valid JSON text as a raw value.
pub fn raw_value(json: &str) -> Box<RawValue> {
    RawValue::from_string(compact_escaped(json)).unwrap_or_else(|_| RawValue::from_string("null".into()).expect("null literal"))
}

/// `json.Marshal(string)`.
pub fn json_string(s: &str) -> String {
    cpa_core::util::go_json_string(s)
}

/// Decodes a struct field that is a string (or null) from a case-insensitive object key match,
/// the way `encoding/json` fills `struct{Model string}`. `Err` for a type mismatch.
fn string_field(obj: &serde_json::Map<String, Value>, name: &str) -> Result<String, ()> {
    let mut out = String::new();
    for (key, value) in obj {
        if !key.eq_ignore_ascii_case(name) {
            continue;
        }
        match value {
            Value::String(s) => out = s.clone(),
            Value::Null => {}
            _ => return Err(()),
        }
    }
    Ok(out)
}

/// Last case-insensitive match of an object member (what a struct field receives).
fn member<'a>(obj: &'a serde_json::Map<String, Value>, name: &str) -> Option<&'a Value> {
    obj.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v).next_back()
}

/// `modelFromJSON`: trimmed `session.model`, else trimmed `model`; empty on any decode error.
pub fn model_from_json(body: &[u8]) -> String {
    let Ok(Value::Object(root)) = serde_json::from_slice::<Value>(body) else {
        return String::new();
    };
    let Ok(model) = string_field(&root, "model") else {
        return String::new();
    };
    let mut session_model = String::new();
    for (key, value) in &root {
        if !key.eq_ignore_ascii_case("session") {
            continue;
        }
        match value {
            Value::Object(session) => match string_field(session, "model") {
                Ok(m) => session_model = m,
                Err(()) => return String::new(),
            },
            Value::Null => {}
            _ => return String::new(),
        }
    }
    if !session_model.trim().is_empty() {
        return session_model.trim().to_string();
    }
    model.trim().to_string()
}

/// `member` re-exported for decoders that need the raw value of one field.
pub fn json_member<'a>(obj: &'a serde_json::Map<String, Value>, name: &str) -> Option<&'a Value> {
    member(obj, name)
}

/// `codexRealtimeModel`: realtime aliases map to the live model.
pub fn codex_realtime_model(model: &str) -> String {
    let trimmed = model.trim();
    let lower = trimmed.to_lowercase();
    if lower.is_empty() || lower == "gpt-realtime" || lower.starts_with("gpt-realtime-") || lower.contains("realtime-preview") {
        return DEFAULT_LIVE_MODEL.to_string();
    }
    trimmed.to_string()
}

/// `websocketHTTPURL`: `ws`/`wss` become `http`/`https`.
pub fn websocket_http_url(raw: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return raw.to_string();
    };
    let scheme = match parsed.scheme().to_ascii_lowercase().as_str() {
        "ws" => "http",
        "wss" => "https",
        _ => return raw.to_string(),
    };
    if parsed.set_scheme(scheme).is_err() {
        return raw.to_string();
    }
    parsed.to_string()
}

/// `callIDFromLocation`: call id from a bare id, a `call_id` query value, or a `.../live/<id>` or
/// `.../calls/<id>` path.
pub fn call_id_from_location(location: &str) -> String {
    let location = location.trim();
    if is_call_id(location) {
        return location.to_string();
    }
    // Relative locations are common; resolve against a dummy base like `url.Parse` accepts them.
    let parsed = match url::Url::parse(location) {
        Ok(u) => u,
        Err(_) => match url::Url::parse("http://x.invalid").and_then(|b| b.join(location)) {
            Ok(u) => u,
            Err(_) => return String::new(),
        },
    };
    if let Some((_, v)) = parsed.query_pairs().find(|(k, _)| k == "call_id") {
        let v = v.trim().to_string();
        if is_call_id(&v) {
            return v;
        }
    }
    let path = parsed.path().trim_matches('/').to_string();
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 2 {
        return String::new();
    }
    let call_id = parts[parts.len() - 1];
    let previous = parts[parts.len() - 2];
    if !is_call_id(call_id) || (previous != "live" && previous != "calls") {
        return String::new();
    }
    call_id.to_string()
}

/// `bearerToken`: the token of a `Bearer` Authorization header (scheme case-insensitive).
pub fn bearer_token(headers: &http::HeaderMap) -> String {
    let value = headers.get(http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("").trim();
    let prefix = "Bearer ";
    if value.len() < prefix.len() || !value.is_char_boundary(prefix.len()) || !value[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return String::new();
    }
    value[prefix.len()..].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types() {
        assert_eq!(parse_media_type("Application/JSON; charset=utf-8").map(|m| m.0).as_deref(), Some("application/json"));
        let (m, p) = parse_media_type("multipart/form-data; boundary=\"a b\"").unwrap();
        assert_eq!((m.as_str(), p[0].1.as_str()), ("multipart/form-data", "a b"));
        assert!(parse_media_type("").is_none());
        assert!(parse_media_type("text/plain; x").is_none());
    }

    #[test]
    fn model_extraction() {
        assert_eq!(model_from_json(br#"{"model":" a ","session":{"model":"b"}}"#), "b");
        assert_eq!(model_from_json(br#"{"model":" a "}"#), "a");
        assert_eq!(model_from_json(br#"{"model":3}"#), "");
        assert_eq!(model_from_json(b"v=0"), "");
        assert_eq!(model_from_json(br#"{"Model":"x"}"#), "x");
    }

    #[test]
    fn raw_map_roundtrip() {
        let map = unmarshal_raw_map(br#"{"b": [1, 2], "a": "<x>"}"#).unwrap();
        assert_eq!(marshal_raw_map(&map), "{\"a\":\"\\u003cx\\u003e\",\"b\":[1,2]}");
        assert_eq!(
            unmarshal_raw_map(b"[1]").unwrap_err(),
            "json: cannot unmarshal array into Go value of type map[string]jsontext.Value"
        );
    }

    #[test]
    fn locations() {
        assert_eq!(call_id_from_location("/v1/live/call-123"), "call-123");
        assert_eq!(call_id_from_location("rtc_abc"), "rtc_abc");
        assert_eq!(call_id_from_location("/v1/realtime/calls/x?y=1"), "x");
        assert_eq!(call_id_from_location("/other/x"), "");
        assert_eq!(call_id_from_location("/a?call_id=zz"), "zz");
    }
}

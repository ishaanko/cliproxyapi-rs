//! Reads a few top-level fields of a JSON request body without building the whole tree.
//!
//! Handlers and the metadata builder only need `model`, `stream`, `service_tier`, a thinking
//! setting or two. Parsing a 2 MB conversation into a `Value` for that is the single most
//! expensive step the server itself does, so the body is validated with the strict allocation-free
//! scan (`cpa_json::valid`) and only the wanted top-level members are parsed.

use serde_json::{Map, Value};

/// What to extract for a top-level key.
#[derive(Clone, Copy)]
pub enum Want {
    /// The parsed value.
    Value,
    /// Only that the key exists (stored as JSON `null`); for large members.
    Exists,
}

/// An object holding only the requested top-level members of `body`, in body order (a repeated
/// key keeps the last value, like a full parse). `None` when `body` is not valid JSON. A valid
/// body that is not an object yields `Value::Null`, on which every lookup misses.
pub fn mini_root(body: &[u8], keys: &[(&str, Want)]) -> Option<Value> {
    if body.is_empty() || !cpa_json::valid(body) {
        return None;
    }
    // A valid body always scans; a scanner bug must not change behavior, so fall back to a parse.
    Some(scan(body, keys).unwrap_or_else(|| cpa_json::parse(body)))
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// End (exclusive) of the string starting at `start` (a `"`).
fn string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while let Some(&c) = b.get(i) {
        match c {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// End (exclusive) of the JSON value starting at `start`.
fn value_end(b: &[u8], start: usize) -> Option<usize> {
    match *b.get(start)? {
        b'"' => string_end(b, start),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = start;
            while let Some(&c) = b.get(i) {
                match c {
                    b'"' => {
                        i = string_end(b, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            let len = b[start..]
                .iter()
                .position(|c| matches!(c, b',' | b'}' | b']') || c.is_ascii_whitespace())
                .unwrap_or(b.len() - start);
            Some(start + len)
        }
    }
}

fn scan(b: &[u8], keys: &[(&str, Want)]) -> Option<Value> {
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return Some(Value::Null);
    }
    i = skip_ws(b, i + 1);
    let mut out = Map::new();
    while *b.get(i)? != b'}' {
        let key_end = string_end(b, i)?;
        let raw_key = &b[i + 1..key_end - 1];
        // Keys without escapes (all of them in practice) compare as plain bytes.
        let wanted = if raw_key.contains(&b'\\') {
            let key: String = serde_json::from_slice(&b[i..key_end]).ok()?;
            keys.iter().find(|(k, _)| *k == key).map(|(k, w)| (*k, *w))
        } else {
            keys.iter().find(|(k, _)| k.as_bytes() == raw_key).map(|(k, w)| (*k, *w))
        };
        i = skip_ws(b, key_end);
        if *b.get(i)? != b':' {
            return None;
        }
        i = skip_ws(b, i + 1);
        let end = value_end(b, i)?;
        if let Some((key, want)) = wanted {
            let value = match want {
                Want::Value => cpa_json::parse(&b[i..end]),
                Want::Exists => Value::Null,
            };
            out.insert(key.to_string(), value);
        }
        i = skip_ws(b, end);
        match *b.get(i)? {
            b',' => i = skip_ws(b, i + 1),
            b'}' => break,
            _ => return None,
        }
    }
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_json::J;

    const KEYS: &[(&str, Want)] = &[("model", Want::Value), ("stream", Want::Value), ("messages", Want::Exists), ("thinking", Want::Value)];

    #[test]
    fn matches_a_full_parse_for_the_wanted_keys() {
        let bodies: &[&[u8]] = &[
            br#"{"model":"m","stream":true,"messages":[{"role":"user","content":"x}"}],"other":{"a":[1,2,{"model":"inner"}]}}"#,
            br#" { "stream" : false , "model" : "a\"b" , "thinking":{"type":"enabled","budget_tokens":5} } "#,
            br#"{"model":"esc","model":"dup"}"#,
            br#"{"n":12.50,"model":"m","big":123456789012345678901234567890}"#,
            b"[1,2,3]",
            b"{}",
        ];
        for body in bodies {
            let mini = mini_root(body, KEYS).expect("valid");
            let full = cpa_json::parse(body);
            for path in ["model", "stream", "thinking.type", "thinking.budget_tokens"] {
                assert_eq!(mini.g(path).v(), full.g(path).v(), "{path} in {}", String::from_utf8_lossy(body));
            }
            assert_eq!(mini.g("messages").exists(), full.g("messages").exists());
        }
    }

    #[test]
    fn invalid_json_is_none() {
        assert!(mini_root(br#"{"model":"m""#, KEYS).is_none());
        assert!(mini_root(b"", KEYS).is_none());
    }
}

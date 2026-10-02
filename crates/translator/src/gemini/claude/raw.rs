//! Raw JSON text lookup for the places where Go copies a value's original text (`gjson.Result.Raw`)
//! into a string field, so whitespace and escapes of the upstream document survive. Parsed
//! `Value`s only keep a compact re-serialization.
//!
//! Shared by the gemini/claude, gemini/openai/chat_completions and gemini/interactions
//! converters (a candidate to move into `cpa_json`).

use cpa_json::{Res, Value};

use crate::common::contains_json_ref;

/// Sets a Gemini functionResponse `path` (`...response` or `...response.result`) from JSON source
/// text the way Go's `SetGeminiFunctionResponseRaw` does, but keeping the exact text when the value
/// contains a `$ref` (which is stored as an opaque string). Blank or invalid text sets `""`.
pub(crate) fn set_function_response_from_text(part: &mut Value, path: &str, text: &str) {
    let text = text.trim();
    if text.is_empty() || !cpa_json::valid(text.as_bytes()) {
        cpa_json::set(part, path, "");
        return;
    }
    let parsed = cpa_json::parse_str(text);
    if contains_json_ref(&Res::of(&parsed)) {
        let target = if path.ends_with("response") { format!("{path}.result") } else { path.to_string() };
        cpa_json::set(part, &target, text);
    } else {
        cpa_json::set(part, path, parsed);
    }
}

/// A borrowed JSON document that can return the exact source text of nested values.
pub(crate) struct RawDoc<'a>(&'a [u8]);

impl<'a> RawDoc<'a> {
    pub(crate) fn new(doc: &'a [u8]) -> Self {
        Self(doc)
    }

    /// Source text of the value at `path` (plain dot separated object keys and array indexes,
    /// first match wins like gjson). `None` when absent or when the document is not scannable.
    pub(crate) fn at(&self, path: &str) -> Option<&'a str> {
        let b = self.0;
        let mut start = skip_ws(b, 0);
        let mut end = value_end(b, start)?;
        for key in path.split('.') {
            (start, end) = child(b, start, key)?;
        }
        std::str::from_utf8(&b[start..end]).ok()
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
        i += 1;
    }
    i
}

/// End (exclusive) of the string literal starting at `start` (a `"`).
fn string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// End (exclusive) of the value starting at `start`.
fn value_end(b: &[u8], start: usize) -> Option<usize> {
    match *b.get(start)? {
        b'"' => string_end(b, start),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = start;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        i = string_end(b, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
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
            let mut i = start;
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']') && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            (i > start).then_some(i)
        }
    }
}

/// Span of the member `key` (object) or element `key` (array index) of the container at `start`.
fn child(b: &[u8], start: usize, key: &str) -> Option<(usize, usize)> {
    match *b.get(start)? {
        b'{' => {
            let mut i = start + 1;
            loop {
                i = skip_ws(b, i);
                if *b.get(i)? != b'"' {
                    return None;
                }
                let key_end = string_end(b, i)?;
                let matches_key = match serde_json::from_slice::<String>(&b[i..key_end]) {
                    Ok(k) => k == key,
                    Err(_) => false,
                };
                i = skip_ws(b, key_end);
                if *b.get(i)? != b':' {
                    return None;
                }
                let value_start = skip_ws(b, i + 1);
                let value_stop = value_end(b, value_start)?;
                if matches_key {
                    return Some((value_start, value_stop));
                }
                i = skip_ws(b, value_stop);
                if *b.get(i)? != b',' {
                    return None;
                }
                i += 1;
            }
        }
        b'[' => {
            let wanted: usize = key.parse().ok()?;
            let mut i = start + 1;
            let mut index = 0;
            loop {
                i = skip_ws(b, i);
                let value_start = i;
                let value_stop = value_end(b, value_start)?;
                if index == wanted {
                    return Some((value_start, value_stop));
                }
                index += 1;
                i = skip_ws(b, value_stop);
                if *b.get(i)? != b',' {
                    return None;
                }
                i += 1;
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::RawDoc;

    #[test]
    fn extracts_nested_raw_text() {
        let doc = br#" {"a": [ {"x": 1}, {"b" : { "c" :  "d\"}" , "e":[1, 2]} } ], "k": 3.50 }"#;
        let raw = RawDoc::new(doc);
        assert_eq!(raw.at("a.1.b"), Some(r#"{ "c" :  "d\"}" , "e":[1, 2]}"#));
        assert_eq!(raw.at("a.1.b.e"), Some("[1, 2]"));
        assert_eq!(raw.at("k"), Some("3.50"));
        assert_eq!(raw.at("a.2"), None);
        assert_eq!(raw.at("zz"), None);
    }
}

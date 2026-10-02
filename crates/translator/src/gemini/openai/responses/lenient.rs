//! gjson-style leniency for the response and request converters.
//!
//! The Go code reads upstream payloads with gjson, which accepts malformed documents (mismatched
//! closers, truncation, lone surrogate escapes), exposes raw value text (`Result.Raw`, with the
//! sender's whitespace and duplicate keys) and is sometimes combined with an explicit validity
//! check (`gjson.ValidBytes`). `serde_json::Value` loses all of that, so this module provides:
//! - [`parse_gjson`] / [`gjson_valid`]: strict parsing with lone surrogates tolerated, then a
//!   tolerant fallback parser;
//! - [`raw_path`] / [`array_elements`]: raw substring extraction by path.

use std::borrow::Cow;

use cpa_json::{Map, Value};

/// Replaces unpaired `\uD800-\uDFFF` escapes inside JSON text with `�` (what gjson's string
/// unescaping yields). `None` when nothing needed replacing.
fn sanitize_lone_surrogates(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let hex4 = |at: usize| -> Option<u32> {
        let h = s.get(at..at + 4)?;
        u32::from_str_radix(h, 16).ok()
    };
    let mut out = String::new();
    let mut last = 0usize;
    let mut changed = false;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        if b.get(i + 1) == Some(&b'u') {
            if let Some(code) = hex4(i + 2) {
                if (0xD800..=0xDBFF).contains(&code) {
                    let paired = b.get(i + 6) == Some(&b'\\')
                        && b.get(i + 7) == Some(&b'u')
                        && hex4(i + 8).is_some_and(|low| (0xDC00..=0xDFFF).contains(&low));
                    if paired {
                        i += 12;
                        continue;
                    }
                } else if !(0xDC00..=0xDFFF).contains(&code) {
                    i += 6;
                    continue;
                }
                out.push_str(&s[last..i]);
                out.push_str("\\ufffd");
                last = i + 6;
                changed = true;
                i += 6;
                continue;
            }
        }
        // Any other escape: skip the escaped character.
        i += 2;
    }
    if !changed {
        return None;
    }
    out.push_str(&s[last..]);
    Some(out)
}

/// gjson `ValidBytes`: well-formed JSON, where lone surrogate escapes are accepted.
pub(super) fn gjson_valid(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    match sanitize_lone_surrogates(&text) {
        Some(fixed) => cpa_json::valid(fixed.as_bytes()),
        None => cpa_json::valid(raw),
    }
}

/// gjson `ParseBytes`: a document `Value` even for malformed input; `None` where gjson would
/// report a non-existent result (input that does not start like JSON).
pub(super) fn parse_gjson(raw: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(raw);
    let fixed = sanitize_lone_surrogates(&text);
    let bytes: &[u8] = fixed.as_deref().map(str::as_bytes).unwrap_or(raw);
    if cpa_json::valid(bytes) {
        return Some(cpa_json::parse(bytes));
    }
    let mut p = Tolerant { b: text.as_bytes(), i: 0 };
    p.skip_ws();
    match p.b.get(p.i) {
        Some(b'{') | Some(b'[') => p.value(),
        _ => None,
    }
}

/// Tolerant recursive-descent parser: either closer ends any container, EOF ends everything,
/// duplicate keys keep the first value.
struct Tolerant<'a> {
    b: &'a [u8],
    i: usize,
}

impl Tolerant<'_> {
    fn skip_ws(&mut self) {
        while self.b.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Option<Value> {
        self.skip_ws();
        match *self.b.get(self.i)? {
            b'{' => {
                self.i += 1;
                let mut map = Map::new();
                loop {
                    self.skip_ws();
                    match self.b.get(self.i) {
                        None => break,
                        Some(b'}') | Some(b']') => {
                            self.i += 1;
                            break;
                        }
                        Some(b',') => self.i += 1,
                        Some(b'"') => {
                            let key = self.string();
                            self.skip_ws();
                            if self.b.get(self.i) == Some(&b':') {
                                self.i += 1;
                            }
                            let val = self.value().unwrap_or(Value::Null);
                            map.entry(key).or_insert(val);
                        }
                        Some(_) => self.i += 1,
                    }
                }
                Some(Value::Object(map))
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_ws();
                    match self.b.get(self.i) {
                        None => break,
                        Some(b'}') | Some(b']') => {
                            self.i += 1;
                            break;
                        }
                        Some(b',') => self.i += 1,
                        Some(_) => match self.value() {
                            Some(v) => items.push(v),
                            None => self.i += 1,
                        },
                    }
                }
                Some(Value::Array(items))
            }
            b'"' => Some(Value::String(self.string())),
            _ => {
                let start = self.i;
                while self.b.get(self.i).is_some_and(|c| !matches!(c, b',' | b'}' | b']') && !c.is_ascii_whitespace()) {
                    self.i += 1;
                }
                let lit = std::str::from_utf8(&self.b[start..self.i]).ok()?;
                match serde_json::from_str::<Value>(lit) {
                    Ok(v) => Some(v),
                    Err(_) => None,
                }
            }
        }
    }

    /// A string literal starting at the opening quote; an unterminated one runs to EOF.
    fn string(&mut self) -> String {
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        while let Some(&c) = self.b.get(self.i) {
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(&e) = self.b.get(self.i) else { break };
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let ch = self.unicode_escape();
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn hex4(&self, at: usize) -> Option<u32> {
        let h = std::str::from_utf8(self.b.get(at..at + 4)?).ok()?;
        u32::from_str_radix(h, 16).ok()
    }

    /// Decodes the digits after `\u` (cursor on the first digit); lone surrogates give U+FFFD.
    fn unicode_escape(&mut self) -> char {
        let Some(code) = self.hex4(self.i) else { return '\u{FFFD}' };
        self.i += 4;
        if (0xD800..=0xDBFF).contains(&code) {
            if self.b.get(self.i) == Some(&b'\\') && self.b.get(self.i + 1) == Some(&b'u') {
                if let Some(low) = self.hex4(self.i + 2).filter(|l| (0xDC00..=0xDFFF).contains(l)) {
                    self.i += 6;
                    return char::from_u32(0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00)).unwrap_or('\u{FFFD}');
                }
            }
            return '\u{FFFD}';
        }
        char::from_u32(code).unwrap_or('\u{FFFD}')
    }
}

// ---------------------------------------------------------------- raw scanning

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
        i += 1;
    }
    i
}

fn skip_string(b: &[u8], mut i: usize) -> usize {
    i += 1;
    while let Some(&c) = b.get(i) {
        match c {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// End (exclusive) of the value starting at `i`. Containers are matched by depth only, like
/// gjson, so mismatched closers do not matter.
fn skip_value(b: &[u8], i: usize) -> usize {
    match b.get(i) {
        None => i,
        Some(b'"') => skip_string(b, i),
        Some(b'{') | Some(b'[') => {
            let mut depth = 0usize;
            let mut j = i;
            while let Some(&c) = b.get(j) {
                match c {
                    b'"' => {
                        j = skip_string(b, j);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return j + 1;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            b.len()
        }
        Some(_) => {
            let mut j = i;
            while b.get(j).is_some_and(|c| !matches!(c, b',' | b'}' | b']') && !c.is_ascii_whitespace()) {
                j += 1;
            }
            j
        }
    }
}

fn key_matches(raw_key: &str, key: &str) -> bool {
    // raw_key includes the quotes.
    match serde_json::from_str::<String>(raw_key) {
        Ok(k) => k == key,
        Err(_) => raw_key.trim_matches('"') == key,
    }
}

/// The raw text of member `key` of the object `raw` (first match).
fn object_member<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    let b = raw.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(b, i);
        match b.get(i)? {
            b'}' | b']' => return None,
            b',' => i += 1,
            b'"' => {
                let key_end = skip_string(b, i);
                let this_key = &raw[i..key_end];
                i = skip_ws(b, key_end);
                if b.get(i) == Some(&b':') {
                    i += 1;
                }
                i = skip_ws(b, i);
                let end = skip_value(b, i);
                if key_matches(this_key, key) {
                    return Some(&raw[i..end]);
                }
                i = end;
            }
            _ => i += 1,
        }
    }
}

/// The raw text of the elements of the array `raw`.
pub(super) fn array_elements(raw: &str) -> Vec<&str> {
    let b = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'[') {
        return out;
    }
    i += 1;
    loop {
        i = skip_ws(b, i);
        match b.get(i) {
            None | Some(b'}') | Some(b']') => return out,
            Some(b',') => i += 1,
            Some(_) => {
                let end = skip_value(b, i);
                out.push(&raw[i..end]);
                i = end.max(i + 1);
            }
        }
    }
}

/// The raw text at `path` (object keys and array indexes) inside `raw`.
pub(super) fn raw_path<'a>(raw: &'a str, path: &[&str]) -> Option<&'a str> {
    let mut cur = raw.trim_start();
    for seg in path {
        cur = if cur.starts_with('{') {
            object_member(cur, seg)?
        } else if cur.starts_with('[') {
            let idx: usize = seg.parse().ok()?;
            *array_elements(cur).get(idx)?
        } else {
            return None;
        };
    }
    Some(cur)
}

/// `Cow` text of a payload.
pub(super) fn payload_text(raw: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(raw)
}

/// Original text of values keyed by their compact serialization.
pub(super) type RawTexts = std::collections::HashMap<String, String>;

/// The original text of a value given its compact form (the compact form when unknown).
pub(super) fn restore_raw(raws: &RawTexts, compact: String) -> String {
    raws.get(&compact).cloned().unwrap_or(compact)
}

/// Collects the original text of every object/array `output` of the request's `input` items and
/// of the elements of array outputs, so `$ref` results can be stringified as sent (Go copies
/// `Result.Raw`).
pub(super) fn collect_output_raws(raw: &str) -> RawTexts {
    let mut raws = RawTexts::new();
    let Some(input) = raw_path(raw, &["input"]) else { return raws };
    let mut remember = |text: &str| {
        if let Ok(v) = serde_json::from_str::<Value>(text) {
            raws.entry(v.to_string()).or_insert_with(|| text.trim().to_string());
        }
    };
    for item in array_elements(input) {
        let Some(output) = object_member(item, "output") else { continue };
        if output.starts_with('{') || output.starts_with('[') {
            remember(output);
            for element in array_elements(output) {
                remember(element);
            }
        }
    }
    raws
}

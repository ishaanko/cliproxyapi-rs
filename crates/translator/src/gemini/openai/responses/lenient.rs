//! gjson-style leniency for the response and request converters.
//!
//! The Go code reads payloads with gjson, which accepts malformed documents (mismatched
//! closers, truncation) and is sometimes combined with an explicit validity check
//! (`gjson.ValidBytes`). [`parse_gjson`] gives a `Value` for such input where a strict parse
//! fails. Original-text lookups use `cpa_json::raw_at`.

use cpa_json::{Map, Value};

/// gjson `ValidBytes`.
pub(super) fn gjson_valid(raw: &[u8]) -> bool {
    cpa_json::valid(raw)
}

/// gjson `ParseBytes`: a document `Value` even for malformed input; `None` where gjson would
/// report a non-existent result (input that does not start like JSON).
pub(super) fn parse_gjson(raw: &[u8]) -> Option<Value> {
    if cpa_json::valid(raw) {
        return Some(cpa_json::parse(raw));
    }
    let text = String::from_utf8_lossy(raw);
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

/// Original text of values keyed by their compact serialization.
pub(super) type RawTexts = std::collections::HashMap<String, String>;

/// The original text of a value given its compact form (the compact form when unknown).
pub(super) fn restore_raw(raws: &RawTexts, compact: String) -> String {
    raws.get(&compact).cloned().unwrap_or(compact)
}

/// Collects the original text of every object/array `output` of the request's `input` items and
/// of the elements of array outputs, so `$ref` results can be stringified as sent (Go copies
/// `Result.Raw`). `root` is the parsed `src`.
pub(super) fn collect_output_raws(src: &[u8], root: &Value) -> RawTexts {
    let mut raws = RawTexts::new();
    let Some(items) = root.get("input").and_then(Value::as_array) else { return raws };
    let mut remember = |value: &Value, path: String| {
        if let Some(text) = cpa_json::raw_at(src, &path) {
            raws.entry(value.to_string()).or_insert_with(|| text.trim().to_string());
        }
    };
    for (i, item) in items.iter().enumerate() {
        let Some(output) = item.get("output") else { continue };
        match output {
            Value::Object(_) => remember(output, format!("input.{i}.output")),
            Value::Array(elements) => {
                remember(output, format!("input.{i}.output"));
                for (j, element) in elements.iter().enumerate() {
                    remember(element, format!("input.{i}.output.{j}"));
                }
            }
            _ => {}
        }
    }
    raws
}

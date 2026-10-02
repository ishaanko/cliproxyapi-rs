//! gjson/sjson-compatible helpers over `serde_json::Value`.
//!
//! The Go implementation manipulates JSON almost exclusively through tidwall's gjson
//! (reads) and sjson (writes). This crate reproduces their observable semantics so that
//! ported code can be translated nearly line by line:
//!
//! ```ignore
//! let v = cpa_json::parse(body);
//! let model = v.g("model").str();            // gjson.GetBytes(body, "model").String()
//! let n = v.g("messages.#").int();           // gjson count
//! let mut out = cpa_json::parse_str(r#"{"role":"user"}"#);
//! cpa_json::set(&mut out, "content.-1", "hi"); // sjson append
//! ```
//!
//! Supported path syntax: `a.b`, numeric indexes, `\.` escapes, `*`/`?` key wildcards,
//! `#` (array length), `#.rest` (map over elements), `#(query)` / `#(query)#`
//! (first/all matches; ops `== = != < <= > >= % !%`, or bare key for existence), `@this`,
//! and `|` (treated like `.`). Writes support numeric indexes and `-1` (append).
//!
//! Value numbers keep their raw text (serde_json `arbitrary_precision`) and objects keep
//! key order (`preserve_order`); deletes use order-preserving removal, matching sjson.

use std::borrow::Cow;

pub use serde_json::{json, Map, Number, Value};

/// gjson's `Type`. Missing values report `Null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null,
    False,
    Number,
    String,
    True,
    Json,
}

/// A gjson-like lookup result. Missing paths are represented by `None`.
#[derive(Debug, Clone)]
pub struct Res<'a>(pub Option<Cow<'a, Value>>);

impl<'a> Res<'a> {
    pub const NONE: Res<'static> = Res(None);

    pub fn of(v: &'a Value) -> Self {
        Res(Some(Cow::Borrowed(v)))
    }

    pub fn owned(v: Value) -> Res<'static> {
        Res(Some(Cow::Owned(v)))
    }

    /// Borrow the underlying value, if present.
    pub fn v(&self) -> Option<&Value> {
        self.0.as_deref()
    }

    pub fn exists(&self) -> bool {
        self.0.is_some()
    }

    pub fn kind(&self) -> Kind {
        match self.v() {
            None | Some(Value::Null) => Kind::Null,
            Some(Value::Bool(false)) => Kind::False,
            Some(Value::Bool(true)) => Kind::True,
            Some(Value::Number(_)) => Kind::Number,
            Some(Value::String(_)) => Kind::String,
            Some(_) => Kind::Json,
        }
    }

    pub fn is_array(&self) -> bool {
        matches!(self.v(), Some(Value::Array(_)))
    }
    pub fn is_object(&self) -> bool {
        matches!(self.v(), Some(Value::Object(_)))
    }
    pub fn is_bool(&self) -> bool {
        matches!(self.v(), Some(Value::Bool(_)))
    }
    pub fn is_string(&self) -> bool {
        matches!(self.v(), Some(Value::String(_)))
    }
    pub fn is_number(&self) -> bool {
        matches!(self.v(), Some(Value::Number(_)))
    }
    /// True when the value exists and is JSON `null`.
    pub fn is_null(&self) -> bool {
        matches!(self.v(), Some(Value::Null))
    }

    /// Borrowed string content when the value is a JSON string.
    pub fn as_str(&self) -> Option<&str> {
        match self.v() {
            Some(Value::String(s)) => Some(s),
            _ => None,
        }
    }

    /// gjson `String()`: strings verbatim, numbers as text, bools as "true"/"false",
    /// null/missing as "", objects/arrays as compact JSON.
    pub fn str(&self) -> String {
        match self.v() {
            None | Some(Value::Null) => String::new(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => number_string(n),
            Some(v) => v.to_string(),
        }
    }

    /// gjson `Int()`: safe-float fast path, then wrapping integer parse of the raw text,
    /// then Go's float->int conversion (out of range -> i64::MIN on amd64).
    pub fn int(&self) -> i64 {
        match self.v() {
            Some(Value::Bool(true)) => 1,
            Some(Value::String(s)) => go_parse_int(s).unwrap_or(0),
            Some(Value::Number(n)) => {
                let raw = n.to_string();
                let f = raw.parse::<f64>().unwrap_or(0.0);
                if f.abs() <= MAX_SAFE {
                    return f as i64;
                }
                go_parse_int(&raw).unwrap_or_else(|| go_f64_to_i64(f))
            }
            _ => 0,
        }
    }

    /// gjson `Uint()`.
    pub fn uint(&self) -> u64 {
        match self.v() {
            Some(Value::Bool(true)) => 1,
            Some(Value::String(s)) => go_parse_uint(s).unwrap_or(0),
            Some(Value::Number(n)) => {
                let raw = n.to_string();
                let f = raw.parse::<f64>().unwrap_or(0.0);
                if f.abs() <= MAX_SAFE && f >= 0.0 {
                    return f as u64;
                }
                go_parse_uint(&raw).unwrap_or_else(|| go_f64_to_u64(f))
            }
            _ => 0,
        }
    }

    /// gjson `Float()`.
    pub fn float(&self) -> f64 {
        match self.v() {
            Some(Value::Bool(true)) => 1.0,
            Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
            Some(Value::Number(n)) => n.to_string().parse::<f64>().unwrap_or(0.0),
            _ => 0.0,
        }
    }

    /// gjson `Bool()`: strings use Go's `strconv.ParseBool` on the lowercased text.
    pub fn bool(&self) -> bool {
        match self.v() {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => matches!(s.to_lowercase().as_str(), "1" | "t" | "true"),
            Some(Value::Number(n)) => n.to_string().parse::<f64>().map(|f| f != 0.0).unwrap_or(false),
            _ => false,
        }
    }

    /// Compact JSON text of the value, or "" when missing (gjson `Raw`).
    pub fn raw(&self) -> String {
        match self.v() {
            None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    /// Owned copy of the value; `Value::Null` when missing (gjson `Value()` analogue).
    pub fn value(&self) -> Value {
        self.v().cloned().unwrap_or(Value::Null)
    }

    /// Consume into an owned value, `None` when missing.
    pub fn into_value(self) -> Option<Value> {
        self.0.map(Cow::into_owned)
    }

    /// gjson `Array()`: elements of an array, `[]` for null/missing, `[self]` otherwise.
    pub fn array(&self) -> Vec<Res<'_>> {
        match self.v() {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(a)) => a.iter().map(Res::of).collect(),
            Some(v) => vec![Res::of(v)],
        }
    }

    /// Object entries in order (gjson `ForEach` over an object). Empty for non-objects.
    pub fn entries(&self) -> Vec<(&str, Res<'_>)> {
        match self.v() {
            Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.as_str(), Res::of(v))).collect(),
            _ => vec![],
        }
    }

    /// gjson `ForEach`: objects yield (key, value), arrays yield (index, value), scalars yield
    /// (missing, self) once. Return `false` from the callback to stop.
    pub fn for_each(&self, mut f: impl FnMut(Res<'_>, Res<'_>) -> bool) {
        match self.v() {
            None => {}
            Some(Value::Object(m)) => {
                for (k, v) in m {
                    if !f(Res::owned(Value::String(k.clone())), Res::of(v)) {
                        break;
                    }
                }
            }
            Some(Value::Array(a)) => {
                for (i, v) in a.iter().enumerate() {
                    if !f(Res::owned(Value::from(i)), Res::of(v)) {
                        break;
                    }
                }
            }
            Some(v) => {
                f(Res::NONE, Res::of(v));
            }
        }
    }

    /// Nested lookup relative to this result.
    pub fn get(&self, path: &str) -> Res<'_> {
        match self.v() {
            None => Res::NONE,
            Some(v) => get(v, path),
        }
    }

    /// Alias of [`Res::get`] for symmetry with [`J::g`].
    pub fn g(&self, path: &str) -> Res<'_> {
        self.get(path)
    }
}

/// Largest integer exactly representable in f64 (gjson `safeInt` bound).
const MAX_SAFE: f64 = 9007199254740991.0;

/// gjson `parseInt`: optional '-', digits only, wrapping on overflow.
fn go_parse_int(s: &str) -> Option<i64> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = digits.bytes().fold(0i64, |n, b| n.wrapping_mul(10).wrapping_add(i64::from(b - b'0')));
    Some(if neg { n.wrapping_neg() } else { n })
}

/// gjson `parseUint`: digits only, wrapping on overflow.
fn go_parse_uint(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.bytes().fold(0u64, |n, b| n.wrapping_mul(10).wrapping_add(u64::from(b - b'0'))))
}

/// Go `int64(f)` on amd64: truncation, out-of-range/NaN -> i64::MIN.
fn go_f64_to_i64(f: f64) -> i64 {
    if f.is_nan() || f >= 9.223372036854775807e18 || f < -9.223372036854775808e18 { i64::MIN } else { f as i64 }
}

/// Go `uint64(f)` on amd64 for values gjson can reach: out-of-range/negative -> 1<<63.
fn go_f64_to_u64(f: f64) -> u64 {
    if f.is_nan() || f < 0.0 || f >= 1.8446744073709552e19 { 1 << 63 } else { f as u64 }
}

/// gjson number `String()`: integer literals verbatim, everything else via shortest float.
fn number_string(n: &Number) -> String {
    let raw = n.to_string();
    let digits = raw.strip_prefix('-').unwrap_or(&raw);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        return raw;
    }
    match raw.parse::<f64>() {
        Ok(f) => format_float(f),
        Err(_) => raw,
    }
}

/// Go `strconv.FormatFloat(f, 'f', -1, 64)`.
pub fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf".into() } else { "-Inf".into() };
    }
    let s = format!("{f}");
    if s == "-0" { "-0".into() } else { s }
}

/// A JSON number from an f64, formatted like sjson (`1.0` -> `1`). Non-finite -> null.
pub fn num_f64(f: f64) -> Value {
    if !f.is_finite() {
        return Value::Null;
    }
    serde_json::from_str::<Number>(&format_float(f)).map(Value::Number).unwrap_or(Value::Null)
}

/// Extension trait giving `Value` gjson-style reads.
pub trait J {
    fn g(&self, path: &str) -> Res<'_>;
}

impl J for Value {
    fn g(&self, path: &str) -> Res<'_> {
        get(self, path)
    }
}

/// Parse bytes; invalid JSON yields `Value::Null` (gjson is lenient, so callers must not
/// rely on errors).
pub fn parse(bytes: &[u8]) -> Value {
    if nesting_depth(bytes) > MAX_DEPTH {
        return Value::Null;
    }
    match parse_strict(bytes) {
        Some(v) => v,
        // gjson decodes unpaired surrogate escapes as U+FFFD; serde_json rejects them.
        None => match replace_lone_surrogates(bytes) {
            Some(fixed) => parse_strict(&fixed).unwrap_or_else(|| parse_tolerant(bytes)),
            None => parse_tolerant(bytes),
        },
    }
}

/// gjson-style leniency for malformed documents (mismatched closers, truncation, duplicate
/// keys keep the first value). Only reached when strict parsing fails; gjson reads such input
/// rather than rejecting it, and Go translators rely on that. Non-container input -> Null.
fn parse_tolerant(bytes: &[u8]) -> Value {
    let text = String::from_utf8_lossy(bytes);
    let mut p = tolerant::Tolerant { b: text.as_bytes(), i: 0 };
    p.skip_ws();
    match p.b.get(p.i) {
        Some(b'{') | Some(b'[') => p.value().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

mod tolerant {
    use super::{Map, Value};

/// Tolerant recursive-descent parser: either closer ends any container, EOF ends everything,
/// duplicate keys keep the first value.
pub(super) struct Tolerant<'a> {
    pub(super) b: &'a [u8],
    pub(super) i: usize,
}

impl Tolerant<'_> {
    pub(super) fn skip_ws(&mut self) {
        while self.b.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    pub(super) fn value(&mut self) -> Option<Value> {
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

}

fn parse_strict(bytes: &[u8]) -> Option<Value> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    de.disable_recursion_limit();
    let v = serde::Deserialize::deserialize(serde_stacker::Deserializer::new(&mut de));
    match v {
        Ok(v) if de.end().is_ok() => Some(v),
        _ => None,
    }
}

/// Rewrite `\uD800`-`\uDFFF` escapes that are not part of a valid surrogate pair to
/// `\ufffd`. Returns `None` when nothing changed.
fn replace_lone_surrogates(bytes: &[u8]) -> Option<Vec<u8>> {
    fn hex4(b: &[u8]) -> Option<u16> {
        std::str::from_utf8(b.get(..4)?).ok().and_then(|h| u16::from_str_radix(h, 16).ok())
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'u' {
                if let Some(cp) = hex4(&bytes[i + 2..]) {
                    let paired = (0xD800..0xDC00).contains(&cp)
                        && bytes.get(i + 6) == Some(&b'\\')
                        && bytes.get(i + 7) == Some(&b'u')
                        && hex4(&bytes[(i + 8).min(bytes.len())..]).is_some_and(|lo| (0xDC00..0xE000).contains(&lo));
                    if paired {
                        out.extend_from_slice(&bytes[i..i + 12]);
                        i += 12;
                        continue;
                    }
                    if (0xD800..0xE000).contains(&cp) {
                        out.extend_from_slice(b"\\ufffd");
                        changed = true;
                        i += 6;
                        continue;
                    }
                }
            }
            out.extend_from_slice(&bytes[i..i + 2]);
            i += 2;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    changed.then_some(out)
}

/// Original JSON text of the value at a plain dotted key/index `path` (first match for
/// duplicate keys), scanning `src` as sent. Use where Go copies `gjson.Result.Raw` into
/// output text and whitespace/duplicate keys must survive; [`Res::raw`] re-serializes.
pub fn raw_at<'a>(src: &'a [u8], path: &str) -> Option<&'a str> {
    let mut start = raw::skip_ws(src, 0);
    for seg in set_keys(path) {
        start = raw::child_start(src, start, &seg)?;
    }
    let end = raw::value_end(src, start)?;
    std::str::from_utf8(&src[start..end]).ok()
}

mod raw {
    pub(super) fn skip_ws(json: &[u8], mut i: usize) -> usize {
        while json.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
            i += 1;
        }
        i
    }

    fn string_end(json: &[u8], start: usize) -> Option<usize> {
        let mut i = start + 1;
        while let Some(&b) = json.get(i) {
            match b {
                b'\\' => i += 2,
                b'"' => return Some(i + 1),
                _ => i += 1,
            }
        }
        None
    }

    pub(super) fn value_end(json: &[u8], start: usize) -> Option<usize> {
        match *json.get(start)? {
            b'"' => string_end(json, start),
            b'{' | b'[' => {
                let mut depth = 0usize;
                let mut i = start;
                while let Some(&b) = json.get(i) {
                    match b {
                        b'"' => {
                            i = string_end(json, i)?;
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
                let len = json[start..]
                    .iter()
                    .position(|b| matches!(b, b',' | b'}' | b']') || b.is_ascii_whitespace())
                    .unwrap_or(json.len() - start);
                Some(start + len)
            }
        }
    }

    /// Start of member `seg` of the object (or element `seg` of the array) at `start`.
    pub(super) fn child_start(json: &[u8], start: usize, seg: &str) -> Option<usize> {
        let is_object = match *json.get(start)? {
            b'{' => true,
            b'[' => false,
            _ => return None,
        };
        let index: Option<usize> = if is_object { None } else { Some(seg.parse().ok()?) };
        let mut i = skip_ws(json, start + 1);
        let mut n = 0usize;
        loop {
            if matches!(*json.get(i)?, b'}' | b']') {
                return None;
            }
            let matches = if is_object {
                let key_end = string_end(json, i)?;
                let key = &json[i..key_end];
                let hit = serde_json::from_slice::<String>(key).is_ok_and(|k| k == seg);
                i = skip_ws(json, key_end);
                if *json.get(i)? != b':' {
                    return None;
                }
                i = skip_ws(json, i + 1);
                hit
            } else {
                index == Some(n)
            };
            if matches {
                return Some(i);
            }
            i = skip_ws(json, value_end(json, i)?);
            match *json.get(i)? {
                b',' => i = skip_ws(json, i + 1),
                _ => return None,
            }
            n += 1;
        }
    }
}

pub fn parse_str(s: &str) -> Value {
    parse(s.as_bytes())
}

/// Maximum accepted nesting. Go has no practical limit, but serde_json's default (128) drops
/// real-world deep tool schemas; values deeper than this are treated as invalid to keep
/// recursive drop/serialize/eval within thread stacks.
pub const MAX_DEPTH: usize = 1000;

/// Maximum bracket nesting outside strings (cheap pre-scan; does not validate).
fn nesting_depth(bytes: &[u8]) -> usize {
    let (mut depth, mut max, mut in_str, mut esc) = (0usize, 0usize, false, false);
    for &b in bytes {
        if in_str {
            match (esc, b) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => {
                depth += 1;
                max = max.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// gjson `ValidBytes`.
pub fn valid(bytes: &[u8]) -> bool {
    if nesting_depth(bytes) > MAX_DEPTH {
        return false;
    }
    let mut de = serde_json::Deserializer::from_slice(bytes);
    de.disable_recursion_limit();
    let ok = serde::Deserialize::deserialize(serde_stacker::Deserializer::new(&mut de))
        .map(|_: serde::de::IgnoredAny| ())
        .is_ok();
    ok && de.end().is_ok()
}

/// Compact serialization.
pub fn to_vec(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

pub fn to_string(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

// ---------------------------------------------------------------- path parsing

#[derive(Debug, Clone)]
enum Comp {
    Key(String, bool), // (key, has_wildcard)
    Hash,
    Query(String, bool), // (query text, all matches)
    This,
    Unsupported,
}

/// Split a path on unescaped `.`/`|`, ignoring separators inside `#(...)` and quotes.
fn split_path(path: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut in_quote = false;
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            cur.push('\\');
            if let Some(n) = chars.next() {
                cur.push(n);
            }
            continue;
        }
        // Parentheses and quotes only matter inside a `#(...)` query component.
        let in_query = cur.starts_with("#(");
        if in_query && in_quote {
            if c == '"' {
                in_quote = false;
            }
            cur.push(c);
            continue;
        }
        match c {
            '"' if in_query && depth > 0 => {
                in_quote = true;
                cur.push(c);
            }
            '(' if in_query || (cur == "#") => {
                depth += 1;
                cur.push(c);
            }
            ')' if in_query => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            '.' | '|' if depth == 0 => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn has_unescaped_wild(s: &str) -> bool {
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '*' | '?' => return true,
            _ => {}
        }
    }
    false
}

fn parse_comps(path: &str) -> Vec<Comp> {
    split_path(path)
        .into_iter()
        .map(|p| {
            if p == "#" {
                Comp::Hash
            } else if let Some(rest) = p.strip_prefix("#(") {
                if let Some(q) = rest.strip_suffix(")#") {
                    Comp::Query(q.to_string(), true)
                } else if let Some(q) = rest.strip_suffix(')') {
                    Comp::Query(q.to_string(), false)
                } else {
                    Comp::Unsupported
                }
            } else if p == "@this" {
                Comp::This
            } else if p.strip_prefix('@').is_some_and(is_gjson_modifier) {
                Comp::Unsupported
            } else {
                let wild = has_unescaped_wild(&p);
                Comp::Key(if wild { p } else { unescape(&p) }, wild)
            }
        })
        .collect()
}

/// gjson's built-in modifier names (`@name` or `@name:args`). Other `@...` components are
/// plain keys, e.g. JSON-LD `@type`.
fn is_gjson_modifier(rest: &str) -> bool {
    let name = rest.split(':').next().unwrap_or(rest);
    matches!(
        name,
        "pretty" | "ugly" | "reverse" | "this" | "valid" | "flatten" | "join" | "keys" | "values" | "tostr" | "fromstr" | "group" | "dig"
    )
}

/// Glob match supporting `*` and `?`, with `\` escaping in the pattern.
fn wild_match(pattern: &str, s: &str) -> bool {
    fn rec(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => (0..=s.len()).any(|i| rec(&p[1..], &s[i..])),
            Some('?') => !s.is_empty() && rec(&p[1..], &s[1..]),
            Some('\\') if p.len() > 1 => s.first() == Some(&p[1]) && rec(&p[2..], &s[1..]),
            Some(c) => s.first() == Some(c) && rec(&p[1..], &s[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = s.chars().collect();
    rec(&p, &s)
}

// ---------------------------------------------------------------- get

/// gjson `Get`.
pub fn get<'a>(v: &'a Value, path: &str) -> Res<'a> {
    if path.is_empty() {
        return Res::NONE;
    }
    Res(eval(v, &parse_comps(path)))
}

fn eval<'a>(v: &'a Value, comps: &[Comp]) -> Option<Cow<'a, Value>> {
    let Some((head, rest)) = comps.split_first() else {
        return Some(Cow::Borrowed(v));
    };
    match head {
        Comp::Key(k, wild) => {
            let child = match v {
                Value::Object(m) => {
                    if *wild {
                        m.iter().find(|(key, _)| wild_match(k, key)).map(|(_, c)| c)
                    } else {
                        m.get(k)
                    }
                }
                Value::Array(a) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
                _ => None,
            }?;
            eval(child, rest)
        }
        Comp::Hash => {
            let Value::Array(a) = v else { return None };
            if rest.is_empty() {
                return Some(Cow::Owned(Value::from(a.len())));
            }
            let out: Vec<Value> = a.iter().filter_map(|e| eval(e, rest).map(Cow::into_owned)).collect();
            Some(Cow::Owned(Value::Array(out)))
        }
        Comp::Query(q, all) => {
            let Value::Array(a) = v else { return None };
            let query = Query::parse(q);
            if *all {
                let out: Vec<Value> = a
                    .iter()
                    .filter(|e| query.matches(e))
                    .filter_map(|e| eval(e, rest).map(Cow::into_owned))
                    .collect();
                Some(Cow::Owned(Value::Array(out)))
            } else {
                let hit = a.iter().find(|e| query.matches(e))?;
                eval(hit, rest)
            }
        }
        Comp::This => eval(v, rest),
        Comp::Unsupported => None,
    }
}

struct Query {
    key: String,
    op: Option<&'static str>,
    value: String,
}

impl Query {
    fn parse(q: &str) -> Self {
        const OPS: [&str; 9] = ["==", "!=", "<=", ">=", "!%", "<", ">", "=", "%"];
        let mut in_quote = false;
        let mut chars = q.char_indices();
        while let Some((i, c)) = chars.next() {
            if c == '\\' {
                chars.next();
                continue;
            }
            if c == '"' {
                in_quote = !in_quote;
            }
            if !in_quote {
                if let Some(op) = OPS.into_iter().find(|op| q[i..].starts_with(op)) {
                    let raw = q[i + op.len()..].trim();
                    let value = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
                        serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw[1..raw.len() - 1].to_string())
                    } else {
                        raw.to_string()
                    };
                    return Query { key: q[..i].trim().to_string(), op: Some(op), value };
                }
            }
        }
        Query { key: q.trim().to_string(), op: None, value: String::new() }
    }

    fn matches(&self, elem: &Value) -> bool {
        let target = if self.key.is_empty() { Res::of(elem) } else { get(elem, &self.key) };
        let Some(op) = self.op else {
            return target.exists();
        };
        let Some(tv) = target.v() else { return false };
        let op = if op == "=" { "==" } else { op };
        match tv {
            Value::String(s) => {
                let r = &self.value;
                match op {
                    "==" => s == r,
                    "!=" => s != r,
                    "<" => s < r,
                    "<=" => s <= r,
                    ">" => s > r,
                    ">=" => s >= r,
                    "%" => wild_match(r, s),
                    "!%" => !wild_match(r, s),
                    _ => false,
                }
            }
            Value::Number(n) => {
                let a = n.to_string().parse::<f64>().unwrap_or(0.0);
                let Ok(b) = self.value.parse::<f64>() else { return false };
                match op {
                    "==" => a == b,
                    "!=" => a != b,
                    "<" => a < b,
                    "<=" => a <= b,
                    ">" => a > b,
                    ">=" => a >= b,
                    _ => false,
                }
            }
            Value::Bool(b) => {
                let r = self.value == "true";
                match op {
                    "==" => *b == r,
                    "!=" => *b != r,
                    _ => false,
                }
            }
            _ => false,
        }
    }
}

// ---------------------------------------------------------------- set / delete

fn set_keys(path: &str) -> Vec<String> {
    split_path(path).iter().map(|p| unescape(p)).collect()
}

fn is_index(k: &str) -> bool {
    k == "-1" || (!k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()))
}

/// sjson `Set`: creates intermediate containers (arrays for numeric keys, objects
/// otherwise), pads arrays with nulls, appends on `-1`, keeps existing key positions.
/// Returns `false` and leaves `v` unchanged where sjson errors (a non-numeric key into an
/// existing array); most Go callers ignore that error, so the result can be ignored too.
pub fn set(v: &mut Value, path: &str, val: impl Into<Value>) -> bool {
    let keys = set_keys(path);
    if !settable(v, &keys) {
        return false;
    }
    set_at(v, &keys, val.into());
    true
}

/// Walk the existing part of the path; fails on a non-numeric key into an array.
fn settable(v: &Value, keys: &[String]) -> bool {
    let Some((k, rest)) = keys.split_first() else { return true };
    match v {
        Value::Object(m) => m.get(k).is_none_or(|c| settable(c, rest)),
        Value::Array(a) => {
            if !is_index(k) {
                return false;
            }
            k == "-1" || k.parse::<usize>().ok().and_then(|i| a.get(i)).is_none_or(|c| settable(c, rest))
        }
        _ => true,
    }
}

/// sjson `SetRaw`: parse `raw` as JSON and set it. Invalid raw JSON is stored as a string
/// and reported as an error (sjson would emit invalid JSON).
pub fn set_raw(v: &mut Value, path: &str, raw: &str) -> Result<(), serde_json::Error> {
    match serde_json::from_str::<Value>(raw) {
        Ok(parsed) => {
            set(v, path, parsed);
            Ok(())
        }
        Err(e) => {
            set(v, path, Value::String(raw.to_string()));
            Err(e)
        }
    }
}

fn set_at(v: &mut Value, keys: &[String], val: Value) {
    let Some((k, rest)) = keys.split_first() else {
        *v = val;
        return;
    };
    let as_index = is_index(k);
    match v {
        Value::Object(_) => {}
        Value::Array(_) if as_index => {}
        _ => {
            *v = if as_index { Value::Array(vec![]) } else { Value::Object(Map::new()) };
        }
    }
    match v {
        Value::Object(m) => {
            let child = m.entry(k.clone()).or_insert(Value::Null);
            set_at(child, rest, val);
        }
        Value::Array(a) => {
            let idx = if k == "-1" {
                a.push(Value::Null);
                a.len() - 1
            } else {
                let i: usize = k.parse().unwrap_or(0);
                while a.len() <= i {
                    a.push(Value::Null);
                }
                i
            };
            set_at(&mut a[idx], rest, val);
        }
        _ => unreachable!(),
    }
}

/// sjson `Delete`: order-preserving removal; missing paths are a no-op.
pub fn delete(v: &mut Value, path: &str) {
    let keys = set_keys(path);
    del_at(v, &keys);
}

fn del_at(v: &mut Value, keys: &[String]) {
    let Some((k, rest)) = keys.split_first() else { return };
    if rest.is_empty() {
        match v {
            Value::Object(m) => {
                m.shift_remove(k);
            }
            Value::Array(a) => {
                if k == "-1" {
                    a.pop();
                } else if let Ok(i) = k.parse::<usize>() {
                    if i < a.len() {
                        a.remove(i);
                    }
                }
            }
            _ => {}
        }
        return;
    }
    let child = match v {
        Value::Object(m) => m.get_mut(k),
        Value::Array(a) => {
            if k == "-1" {
                a.last_mut()
            } else {
                k.parse::<usize>().ok().and_then(|i| a.get_mut(i))
            }
        }
        _ => None,
    };
    if let Some(c) = child {
        del_at(c, rest);
    }
}

/// Mutable access to an existing value at a plain dotted path (no queries).
pub fn get_mut<'a>(v: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut cur = v;
    for k in set_keys(path) {
        cur = match cur {
            Value::Object(m) => m.get_mut(&k)?,
            Value::Array(a) => a.get_mut(k.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Escape a literal key for use inside a path (sjson/gjson `\.` etc.).
pub fn escape_key(k: &str) -> String {
    let mut out = String::with_capacity(k.len());
    for c in k.chars() {
        if matches!(c, '.' | '*' | '?' | '|' | '#' | '@' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests;

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

    /// gjson `Int()`.
    pub fn int(&self) -> i64 {
        match self.v() {
            Some(Value::Bool(true)) => 1,
            Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
            Some(Value::Number(n)) => {
                let raw = n.to_string();
                if let Ok(i) = raw.parse::<i64>() {
                    return i;
                }
                let f = raw.parse::<f64>().unwrap_or(0.0);
                f as i64
            }
            _ => 0,
        }
    }

    /// gjson `Uint()`.
    pub fn uint(&self) -> u64 {
        match self.v() {
            Some(Value::Bool(true)) => 1,
            Some(Value::String(s)) => s.parse::<u64>().unwrap_or(0),
            Some(Value::Number(n)) => {
                let raw = n.to_string();
                if let Ok(i) = raw.parse::<u64>() {
                    return i;
                }
                let f = raw.parse::<f64>().unwrap_or(0.0);
                if f < 0.0 { 0 } else { f as u64 }
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
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

pub fn parse_str(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
}

/// gjson `ValidBytes`.
pub fn valid(bytes: &[u8]) -> bool {
    serde_json::from_slice::<&serde_json::value::RawValue>(bytes).is_ok()
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
        if in_quote {
            if c == '"' {
                in_quote = false;
            }
            cur.push(c);
            continue;
        }
        match c {
            '"' if depth > 0 => {
                in_quote = true;
                cur.push(c);
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
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
            } else if p.starts_with('@') {
                Comp::Unsupported
            } else {
                let wild = has_unescaped_wild(&p);
                Comp::Key(if wild { p } else { unescape(&p) }, wild)
            }
        })
        .collect()
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
        let bytes = q.as_bytes();
        let mut in_quote = false;
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_quote = !in_quote;
            }
            if !in_quote {
                for op in OPS {
                    if q[i..].starts_with(op) {
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
            i += 1;
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
pub fn set(v: &mut Value, path: &str, val: impl Into<Value>) {
    let keys = set_keys(path);
    set_at(v, &keys, val.into());
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

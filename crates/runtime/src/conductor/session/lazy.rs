//! Lazily parsed request body for session extraction.
//!
//! Session identity probes look up a few dozen well-known keys, almost always finding none, and
//! `derive_id` needs only the leading messages. Building a full `Value` tree for that (several
//! times per request) dominated the proxy's CPU, so [`Doc`] indexes the top-level object with one
//! raw scan and parses a member only when a lookup reaches it. Anything that is not a well-formed
//! top-level object (arrays, leading text, trailing text, duplicate keys, absurd nesting) falls
//! back to `cpa_json::parse`, so lookups agree with a full parse.

use std::borrow::Cow;
use std::cell::OnceCell;

use cpa_json::{J, MAX_DEPTH, Res, Value};

/// A parsed-on-demand JSON document.
pub struct Doc<'a> {
    kind: Kind<'a>,
}

enum Kind<'a> {
    /// Top-level object indexed by a raw scan; members parse on first use.
    Lazy(Box<Lazy<'a>>),
    Owned(Value),
}

struct Lazy<'a> {
    members: Vec<Member<'a>>,
    /// Whole-document parse for the rare lookup that cannot be answered per member.
    full: OnceCell<Value>,
    bytes: &'a [u8],
}

struct Member<'a> {
    key: Cow<'a, str>,
    raw: &'a [u8],
    parsed: OnceCell<Value>,
}

/// What a top-level member holds, judged from its first byte without parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Missing,
    String,
    Array,
    Object,
    Other,
}

impl<'a> Doc<'a> {
    /// Indexes `bytes`; empty input is a null document.
    pub fn new(bytes: &'a [u8]) -> Doc<'a> {
        Doc::lazy(bytes).unwrap_or_else(|| Doc::owned(cpa_json::parse(bytes)))
    }

    /// Like [`Doc::new`] but `None` unless the top-level object could be indexed (nothing is
    /// parsed otherwise), for callers that only want the cheap path.
    pub fn lazy(bytes: &'a [u8]) -> Option<Doc<'a>> {
        scan_members(bytes).map(|members| Doc {
            kind: Kind::Lazy(Box::new(Lazy { members, full: OnceCell::new(), bytes })),
        })
    }

    pub fn owned(v: Value) -> Doc<'static> {
        Doc { kind: Kind::Owned(v) }
    }

    /// True unless the document is JSON `null` or unparsable (Go: `root.Exists()` on a parsed root).
    pub fn exists(&self) -> bool {
        match &self.kind {
            Kind::Lazy(_) => true,
            Kind::Owned(v) => !v.is_null(),
        }
    }

    pub fn is_lazy(&self) -> bool {
        matches!(self.kind, Kind::Lazy(_))
    }

    /// gjson lookup; only the member the path starts at is parsed.
    pub fn g(&self, path: &str) -> Res<'_> {
        match &self.kind {
            Kind::Owned(v) => v.g(path),
            Kind::Lazy(l) => {
                if path.is_empty() {
                    return Res::NONE;
                }
                let Some((first, rest)) = split_plain_head(path) else {
                    return l.full().g(path);
                };
                let Some(m) = l.member(first) else {
                    return Res::NONE;
                };
                let v = m.value();
                match rest {
                    None => Res::of(v),
                    Some(r) => v.g(r),
                }
            }
        }
    }

    /// Whether the top-level member `key` exists (even when null).
    pub fn has(&self, key: &str) -> bool {
        match &self.kind {
            Kind::Lazy(l) => l.member(key).is_some(),
            Kind::Owned(v) => matches!(v, Value::Object(m) if m.contains_key(key)),
        }
    }

    /// The object stored under `key`, as its own lazy document. `None` when absent. Non-object
    /// members yield a document whose lookups all miss, like a lookup into a scalar.
    pub fn sub(&self, key: &str) -> Option<Doc<'a>> {
        match &self.kind {
            Kind::Lazy(l) => l.member(key).map(|m| Doc::new(m.raw)),
            Kind::Owned(v) => match v {
                Value::Object(m) => m.get(key).map(|c| Doc::owned(c.clone())),
                _ => None,
            },
        }
    }

    /// Parsed top-level member `key`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match &self.kind {
            Kind::Lazy(l) => l.member(key).map(Member::value),
            Kind::Owned(v) => match v {
                Value::Object(m) => m.get(key),
                _ => None,
            },
        }
    }

    /// First present member among `keys` (Go: first non-nil map lookup).
    pub fn first(&self, keys: &[&str]) -> Option<&Value> {
        keys.iter().find_map(|k| self.get(k))
    }

    pub fn member_kind(&self, key: &str) -> MemberKind {
        match &self.kind {
            Kind::Lazy(l) => match l.member(key) {
                None => MemberKind::Missing,
                Some(m) => match m.raw.first() {
                    Some(b'"') => MemberKind::String,
                    Some(b'[') => MemberKind::Array,
                    Some(b'{') => MemberKind::Object,
                    _ => MemberKind::Other,
                },
            },
            Kind::Owned(_) => match self.get(key) {
                None => MemberKind::Missing,
                Some(Value::String(_)) => MemberKind::String,
                Some(Value::Array(_)) => MemberKind::Array,
                Some(Value::Object(_)) => MemberKind::Object,
                Some(_) => MemberKind::Other,
            },
        }
    }

    /// Elements of the array stored under `key`, each parsed only when reached. `None` when the
    /// member is missing or not an array.
    pub fn elements(&self, key: &str) -> Option<Elements<'_>> {
        match &self.kind {
            Kind::Lazy(l) => {
                let m = l.member(key)?;
                if m.raw.first() != Some(&b'[') {
                    return None;
                }
                Some(Elements::Raw { bytes: m.raw, pos: 1 })
            }
            Kind::Owned(_) => match self.get(key) {
                Some(Value::Array(a)) => Some(Elements::Parsed(a.iter())),
                _ => None,
            },
        }
    }
}

impl J for Doc<'_> {
    fn g(&self, path: &str) -> Res<'_> {
        Doc::g(self, path)
    }
}

impl<'a> Lazy<'a> {
    fn member(&self, key: &str) -> Option<&Member<'a>> {
        self.members.iter().find(|m| m.key == key)
    }

    fn full(&self) -> &Value {
        self.full.get_or_init(|| cpa_json::parse(self.bytes))
    }
}

impl Member<'_> {
    fn value(&self) -> &Value {
        self.parsed.get_or_init(|| cpa_json::parse(self.raw))
    }
}

/// Array elements in order (see [`Doc::elements`]).
pub enum Elements<'a> {
    Parsed(std::slice::Iter<'a, Value>),
    Raw { bytes: &'a [u8], pos: usize },
}

impl<'a> Iterator for Elements<'a> {
    type Item = Cow<'a, Value>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Elements::Parsed(it) => it.next().map(Cow::Borrowed),
            Elements::Raw { bytes, pos } => {
                let b = *bytes;
                let mut i = skip_ws(b, *pos);
                if b.get(i) == Some(&b',') {
                    i = skip_ws(b, i + 1);
                }
                if matches!(b.get(i), None | Some(b']')) {
                    *pos = b.len();
                    return None;
                }
                let Some(end) = value_end(b, i, 3) else {
                    *pos = b.len();
                    return None;
                };
                *pos = end;
                Some(Cow::Owned(cpa_json::parse(&b[i..end])))
            }
        }
    }
}

/// Bytes that make a path head more than a literal key for gjson: escapes, wildcards, queries,
/// modifiers, pipes.
const SPECIAL: [bool; 256] = {
    let mut t = [false; 256];
    let specials = *b"\\*?#@|()\"";
    let mut i = 0;
    while i < specials.len() {
        t[specials[i] as usize] = true;
        i += 1;
    }
    t
};

/// Splits `path` at its first `.`; `None` when the head is not a plain literal key (the caller
/// then evaluates the path against the whole document).
fn split_plain_head(path: &str) -> Option<(&str, Option<&str>)> {
    for (i, &b) in path.as_bytes().iter().enumerate() {
        if b == b'.' {
            return Some((&path[..i], Some(&path[i + 1..])));
        }
        if SPECIAL[b as usize] {
            return None;
        }
    }
    Some((path, None))
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// Index after the string whose opening quote is at `start`.
fn string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    loop {
        i += memchr::memchr2(b'"', b'\\', b.get(i..)?)?;
        if b[i] == b'"' {
            return Some(i + 1);
        }
        i += 2;
    }
}

/// Index after the value starting at `start`; `depth` is the nesting level a container starting
/// there would have (the top-level object is level 1). `None` for malformed input or nesting
/// deeper than `cpa_json` accepts.
fn value_end(b: &[u8], start: usize, depth: usize) -> Option<usize> {
    match *b.get(start)? {
        b'"' => string_end(b, start),
        b'{' | b'[' => {
            let mut level = depth - 1;
            let mut i = start;
            while let Some(&c) = b.get(i) {
                match c {
                    b'"' => {
                        i = string_end(b, i)?;
                        continue;
                    }
                    b'{' | b'[' => {
                        level += 1;
                        if level > MAX_DEPTH {
                            return None;
                        }
                    }
                    b'}' | b']' => {
                        level = level.checked_sub(1)?;
                        if level < depth {
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
            (len > 0).then_some(start + len)
        }
    }
}

/// Members of a top-level object, `None` unless `b` is exactly one well-formed object with unique
/// keys (surrounding whitespace allowed).
fn scan_members(b: &[u8]) -> Option<Vec<Member<'_>>> {
    let mut i = skip_ws(b, 0);
    if *b.get(i)? != b'{' {
        return None;
    }
    i += 1;
    let mut members: Vec<Member<'_>> = Vec::new();
    loop {
        i = skip_ws(b, i);
        match *b.get(i)? {
            b'}' => {
                i += 1;
                break;
            }
            b'"' => {}
            _ => return None,
        }
        let key_end = string_end(b, i)?;
        let key_raw = &b[i + 1..key_end - 1];
        let key = if memchr::memchr(b'\\', key_raw).is_some() {
            Cow::Owned(serde_json::from_slice::<String>(&b[i..key_end]).ok()?)
        } else {
            Cow::Borrowed(std::str::from_utf8(key_raw).ok()?)
        };
        i = skip_ws(b, key_end);
        if *b.get(i)? != b':' {
            return None;
        }
        i = skip_ws(b, i + 1);
        let end = value_end(b, i, 2)?;
        if members.iter().any(|m| m.key == key) {
            return None;
        }
        members.push(Member { key, raw: &b[i..end], parsed: OnceCell::new() });
        i = skip_ws(b, end);
        match *b.get(i)? {
            b',' => i += 1,
            b'}' => {
                i += 1;
                break;
            }
            _ => return None,
        }
    }
    (skip_ws(b, i) == b.len()).then_some(members)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCS: &[&str] = &[
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"metadata":{"user_id":"u","a":{"b":[1,2]}},"stream":false}"#,
        "  {\n\"request\" : {\"contents\":[1]}, \"k\\u0065y\": \"v\", \"n\": 12.50, \"t\": true, \"z\": null }  ",
        r#"{"a":"x}\"{","b":{"c":"]"},"d":[]}"#,
        r#"{"a":1,"a":2}"#,
        r#"{"a":1} trailing"#,
        r#"[{"a":1}]"#,
        r#"prefix {"a":1}"#,
        r#"{"a":"unterminated"#,
        r#"{}"#,
        r#"null"#,
        "",
    ];
    const PATHS: &[&str] = &[
        "model", "messages", "messages.0.role", "messages.#", "metadata.user_id", "metadata.a.b.1", "request.contents", "request",
        "key", "n", "t", "z", "a", "b.c", "d", "missing", "metadata.missing", "k\\u0065y", "mess*", "@this", "#", "",
    ];

    #[test]
    fn lazy_lookups_match_a_full_parse() {
        for doc in DOCS {
            let lazy = Doc::new(doc.as_bytes());
            let full = cpa_json::parse(doc.as_bytes());
            for path in PATHS {
                assert_eq!(lazy.g(path).value(), full.g(path).value(), "doc {doc:?} path {path:?}");
                assert_eq!(lazy.g(path).exists(), full.g(path).exists(), "doc {doc:?} path {path:?}");
            }
        }
    }

    #[test]
    fn only_wellformed_objects_stay_lazy() {
        assert!(Doc::new(DOCS[0].as_bytes()).is_lazy());
        assert!(Doc::new(DOCS[1].as_bytes()).is_lazy());
        assert!(Doc::new(DOCS[2].as_bytes()).is_lazy());
        assert!(Doc::new(DOCS[8].as_bytes()).is_lazy());
        for doc in DOCS[3..8].iter().chain(&DOCS[9..]) {
            assert!(!Doc::new(doc.as_bytes()).is_lazy(), "{doc:?}");
        }
    }

    #[test]
    fn elements_parse_one_at_a_time() {
        let doc = Doc::new(br#"{"messages":[{"a":1}, "s", [2], 3, null],"s":"x"}"#);
        let got: Vec<Value> = doc.elements("messages").map(|e| e.map(Cow::into_owned).collect()).unwrap_or_default();
        assert_eq!(got.len(), 5);
        assert_eq!(got[1], Value::String("s".into()));
        assert!(doc.elements("s").is_none() && doc.elements("none").is_none());
        assert_eq!(doc.member_kind("s"), MemberKind::String);
        assert_eq!(doc.member_kind("messages"), MemberKind::Array);
        assert_eq!(doc.member_kind("none"), MemberKind::Missing);
    }

    #[test]
    fn deep_nesting_falls_back_to_the_full_parse_rules() {
        let deep = format!("{{\"a\":{}{}}}", "[".repeat(MAX_DEPTH + 5), "]".repeat(MAX_DEPTH + 5));
        let doc = Doc::new(deep.as_bytes());
        assert!(!doc.is_lazy() && !doc.exists());
    }
}

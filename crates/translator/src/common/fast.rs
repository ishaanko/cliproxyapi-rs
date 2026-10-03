//! Building blocks for the allocation-light "fast paths" of the hot translators.
//!
//! A fast path deserializes one event with a borrowed serde struct and writes the output with
//! hand-built templates. It only accepts the canonical shapes it understands: any surprise
//! (null where a value is read, wrong type, duplicate key, invalid UTF-8, malformed JSON)
//! makes serde fail, and the caller then runs the general `cpa_json::Value` translation, which
//! stays the reference for every odd input.

use std::borrow::Cow;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, Visitor};

/// A field that is either absent or present with a non-null value of type `T`. JSON `null`
/// fails deserialization (gjson distinguishes absent from null, so fast paths bail out on it).
#[derive(Debug, Default)]
pub enum Field<T> {
    #[default]
    Absent,
    Present(T),
}

impl<T> Field<T> {
    pub fn as_ref(&self) -> Option<&T> {
        match self {
            Field::Absent => None,
            Field::Present(v) => Some(v),
        }
    }

    pub fn exists(&self) -> bool {
        matches!(self, Field::Present(_))
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::deserialize(d).map(Field::Present)
    }
}

/// A JSON string borrowed from the input when it has no escapes, owned otherwise.
#[derive(Debug)]
pub struct Str<'a>(pub Cow<'a, str>);

impl std::ops::Deref for Str<'_> {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Str<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Str<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_borrowed_str<E: de::Error>(self, s: &'de str) -> Result<Str<'de>, E> {
                Ok(Str(Cow::Borrowed(s)))
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Str<'de>, E> {
                Ok(Str(Cow::Owned(s.to_owned())))
            }
            fn visit_string<E: de::Error>(self, s: String) -> Result<Str<'de>, E> {
                Ok(Str(Cow::Owned(s)))
            }
        }
        d.deserialize_str(V)
    }
}

/// Appends `s` as a JSON string literal, escaped exactly like serde_json's compact writer
/// (`"` `\` and control characters only; everything else, including `/` and non-ASCII, verbatim).
pub fn push_json_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            0..=0x1f => {
                out.extend_from_slice(&bytes[start..i]);
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.extend_from_slice(b"\\u00");
                out.push(HEX[usize::from(b >> 4)]);
                out.push(HEX[usize::from(b & 0xf)]);
                start = i + 1;
                continue;
            }
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        out.extend_from_slice(esc);
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

/// True when a JSON string literal (with its quotes) is already in serde_json's canonical
/// compact form, so re-serializing its decoded value would reproduce the same bytes: only the
/// escapes `\" \\ \b \f \n \r \t` and `\u00xx` (lowercase hex) for the remaining control
/// characters. The literal must come from a successfully parsed document (valid UTF-8, no raw
/// control characters).
pub fn is_canonical_literal(lit: &str) -> bool {
    let inner = lit.as_bytes();
    let inner = match inner {
        [b'"', mid @ .., b'"'] => mid,
        _ => return false,
    };
    let mut at = 0;
    while let Some(p) = memchr::memchr(b'\\', &inner[at..]) {
        let i = at + p;
        match inner.get(i + 1) {
            Some(b'"' | b'\\' | b'n' | b'r' | b't' | b'b' | b'f') => at = i + 2,
            Some(b'u') => {
                // Only `\u00` + two lowercase hex digits naming a control character that has no
                // short escape.
                let Some([b'0', b'0', h, l]) = inner.get(i + 2..i + 6).map(|s| [s[0], s[1], s[2], s[3]]) else {
                    return false;
                };
                let hex = |c: u8| match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    _ => None,
                };
                let (Some(h), Some(l)) = (hex(h), hex(l)) else { return false };
                let code = h * 16 + l;
                if code >= 0x20 || matches!(code, 8 | 9 | 10 | 12 | 13) {
                    return false;
                }
                at = i + 6;
            }
            _ => return false,
        }
    }
    true
}

/// Decoded content of a JSON string literal (with quotes); borrowed when it has no escapes.
pub fn decode_literal(lit: &str) -> Option<Cow<'_, str>> {
    let inner = lit.strip_prefix('"')?.strip_suffix('"')?;
    if memchr::memchr(b'\\', inner.as_bytes()).is_none() {
        return Some(Cow::Borrowed(inner));
    }
    serde_json::from_str::<String>(lit).ok().map(Cow::Owned)
}

/// Appends the JSON string literal `lit` as serde_json would re-serialize its value: verbatim when
/// it is already canonical (the common case, a memcpy), else decoded and re-escaped.
pub fn push_literal(out: &mut Vec<u8>, lit: &str) -> Option<()> {
    if is_canonical_literal(lit) {
        out.extend_from_slice(lit.as_bytes());
    } else {
        push_json_str(out, &decode_literal(lit)?);
    }
    Some(())
}

/// True when `raw` (a `RawValue` text) is a JSON string literal.
pub fn is_string_literal(raw: &str) -> bool {
    raw.as_bytes().first() == Some(&b'"')
}

/// Appends a decimal integer.
pub fn push_int(out: &mut Vec<u8>, n: i64) {
    out.extend_from_slice(itoa::Buffer::new().format(n).as_bytes());
}

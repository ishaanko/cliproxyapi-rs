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

/// Appends a decimal integer.
pub fn push_int(out: &mut Vec<u8>, n: i64) {
    out.extend_from_slice(itoa::Buffer::new().format(n).as_bytes());
}

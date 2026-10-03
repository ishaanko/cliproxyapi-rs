//! Hand-written strict JSON reader and compact writer for [`Value`].
//!
//! `serde_json` reaches `Value` through generic visitors; for the small documents that dominate
//! proxy traffic (SSE events, chat requests) that indirection costs more than the parsing. This
//! module builds the same `Value` directly from bytes and writes it back with a word-at-a-time
//! string scanner. Acceptance and output are byte-identical to `serde_json` (checked by a
//! differential test): anything this reader is unsure about reports [`Fail`] and the caller
//! falls back to the serde path.

use serde_json::{Map, Number, Value};

/// Why [`parse`] did not produce a value.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fail {
    /// Not valid JSON (or invalid UTF-8 / a lone surrogate escape).
    Syntax,
    /// Nesting beyond [`MAX_NEST`]; the serde path grows the stack for these.
    Deep,
    /// A shape serde_json treats specially (`$serde_json::private::*` keys).
    Special,
}

/// Containers nested deeper than this are left to the stack-growing serde path.
const MAX_NEST: usize = 128;

/// Strict RFC 8259 parse of the whole input (leading/trailing whitespace allowed).
pub(crate) fn parse(bytes: &[u8]) -> Result<Value, Fail> {
    let mut p = Parser { b: bytes, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != bytes.len() {
        return Err(Fail::Syntax);
    }
    Ok(v)
}

/// Validity check with the same acceptance as [`parse`], without building a value.
pub(crate) fn validate(bytes: &[u8]) -> Result<(), Fail> {
    let mut p = Parser { b: bytes, i: 0 };
    p.ws();
    p.skip_value(0)?;
    p.ws();
    if p.i != bytes.len() {
        return Err(Fail::Syntax);
    }
    Ok(())
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;

#[inline]
fn has_zero(x: u64) -> u64 {
    x.wrapping_sub(LO) & !x & HI
}

/// Index of the first byte at or after `i` that ends a plain run of string content: `"`, `\`
/// or a control character (below 0x20). `b.len()` when none.
#[inline]
fn find_special(b: &[u8], mut i: usize) -> usize {
    while let Some(c) = b.get(i..).and_then(<[u8]>::first_chunk::<8>) {
        let w = u64::from_le_bytes(*c);
        let below_space = w.wrapping_sub(LO * 0x20) & !w & HI;
        let hit = below_space | has_zero(w ^ (LO * 0x22)) | has_zero(w ^ (LO * 0x5c));
        if hit != 0 {
            // Carries only corrupt bytes above the first real hit, so the lowest set bit is exact.
            return i + (hit.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while let Some(&c) = b.get(i) {
        if c < 0x20 || c == b'"' || c == b'\\' {
            break;
        }
        i += 1;
    }
    i
}

/// Offset of the first control character in `s`, checked 64 bytes at a time with a branch-free
/// reduction the compiler vectorizes.
fn first_control(s: &[u8]) -> Option<usize> {
    for (n, chunk) in s.chunks(64).enumerate() {
        if chunk.iter().fold(false, |any, &c| any | (c < 0x20)) {
            return chunk.iter().position(|&c| c < 0x20).map(|p| n * 64 + p);
        }
    }
    None
}

/// [`find_special`] tuned for long strings: a few word-sized probes catch short ones, then
/// memchr (SIMD) finds the next quote or backslash and a vector pass checks the span for
/// control characters.
fn find_special_long(b: &[u8], mut i: usize) -> usize {
    let probe_end = (i + 24).min(b.len());
    while i + 8 <= probe_end {
        let w = u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or([0; 8]));
        let below_space = w.wrapping_sub(LO * 0x20) & !w & HI;
        let hit = below_space | has_zero(w ^ (LO * 0x22)) | has_zero(w ^ (LO * 0x5c));
        if hit != 0 {
            return i + (hit.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    let Some(rest) = b.get(i..) else { return b.len() };
    let stop = memchr::memchr2(b'"', b'\\', rest).map_or(b.len(), |r| i + r);
    first_control(&b[i..stop]).map_or(stop, |r| i + r)
}

impl Parser<'_> {
    #[inline]
    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\n' | b'\t' | b'\r')) {
            self.i += 1;
        }
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), Fail> {
        if self.b.get(self.i..self.i + word.len()) == Some(word) {
            self.i += word.len();
            Ok(())
        } else {
            Err(Fail::Syntax)
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, Fail> {
        match *self.b.get(self.i).ok_or(Fail::Syntax)? {
            b'"' => self.string().map(Value::String),
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'-' | b'0'..=b'9' => self.number(),
            b't' => self.literal(b"true").map(|()| Value::Bool(true)),
            b'f' => self.literal(b"false").map(|()| Value::Bool(false)),
            b'n' => self.literal(b"null").map(|()| Value::Null),
            _ => Err(Fail::Syntax),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, Fail> {
        if depth >= MAX_NEST {
            return Err(Fail::Deep);
        }
        self.i += 1;
        self.ws();
        let mut items = Vec::new();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(Fail::Syntax),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, Fail> {
        if depth >= MAX_NEST {
            return Err(Fail::Deep);
        }
        self.i += 1;
        self.ws();
        let mut map = Map::new();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return Err(Fail::Syntax);
            }
            let key = self.string()?;
            if key.starts_with("$serde_json::private::") {
                return Err(Fail::Special);
            }
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err(Fail::Syntax);
            }
            self.i += 1;
            self.ws();
            let v = self.value(depth + 1)?;
            // Duplicate keys: the last value wins and keeps the first position (IndexMap).
            map.insert(key, v);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(map));
                }
                _ => return Err(Fail::Syntax),
            }
        }
    }

    /// Scans a number token per the JSON grammar and returns its end offset.
    fn scan_number(&mut self) -> Result<usize, Fail> {
        let b = self.b;
        let mut i = self.i;
        if b.get(i) == Some(&b'-') {
            i += 1;
        }
        match b.get(i) {
            Some(b'0') => {
                i += 1;
                if matches!(b.get(i), Some(b'0'..=b'9')) {
                    return Err(Fail::Syntax);
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(b.get(i), Some(b'0'..=b'9')) {
                    i += 1;
                }
            }
            _ => return Err(Fail::Syntax),
        }
        if b.get(i) == Some(&b'.') {
            i += 1;
            let digits = i;
            while matches!(b.get(i), Some(b'0'..=b'9')) {
                i += 1;
            }
            if i == digits {
                return Err(Fail::Syntax);
            }
        }
        if matches!(b.get(i), Some(b'e' | b'E')) {
            i += 1;
            if matches!(b.get(i), Some(b'+' | b'-')) {
                i += 1;
            }
            let digits = i;
            while matches!(b.get(i), Some(b'0'..=b'9')) {
                i += 1;
            }
            if i == digits {
                return Err(Fail::Syntax);
            }
        }
        Ok(i)
    }

    fn number(&mut self) -> Result<Value, Fail> {
        let end = self.scan_number()?;
        // The token is ASCII by construction. serde_json (arbitrary_precision) keeps the text
        // but spells exponents as `e` plus an explicit sign, and reads `-0` as the integer 0.
        let token = utf8(&self.b[self.i..end])?;
        self.i = end;
        let text = match token.find(['e', 'E']) {
            Some(at) => {
                let (mantissa, exp) = (&token[..at], &token[at + 1..]);
                let mut s = String::with_capacity(token.len() + 1);
                s.push_str(mantissa);
                s.push('e');
                if !exp.starts_with(['+', '-']) {
                    s.push('+');
                }
                s.push_str(exp);
                s
            }
            None if token == "-0" => "0".to_owned(),
            None => token.to_owned(),
        };
        Ok(Value::Number(Number::from_string_unchecked(text)))
    }

    /// Scans a string body (cursor on the opening quote): index of the closing quote and
    /// whether any escape occurs. Control characters and EOF are errors.
    fn scan_string(&self) -> Result<(usize, bool), Fail> {
        let mut i = self.i + 1;
        let mut escaped = false;
        loop {
            i = find_special_long(self.b, i);
            match self.b.get(i) {
                Some(b'"') => return Ok((i, escaped)),
                Some(b'\\') => {
                    escaped = true;
                    i += 2;
                }
                _ => return Err(Fail::Syntax),
            }
        }
    }

    /// Parses a string starting at the opening quote.
    fn string(&mut self) -> Result<String, Fail> {
        let (end, escaped) = self.scan_string()?;
        // Escape sequences are ASCII, so validating the raw span validates every segment.
        let raw = utf8(&self.b[self.i + 1..end])?;
        self.i = end + 1;
        if !escaped {
            return Ok(raw.to_owned());
        }
        let mut out = String::with_capacity(raw.len());
        unescape(raw, Some(&mut out))?;
        Ok(out)
    }

    /// Like [`Parser::string`] without building the text (UTF-8 and escapes still checked).
    fn skip_string(&mut self) -> Result<(), Fail> {
        let (end, escaped) = self.scan_string()?;
        let raw = utf8(&self.b[self.i + 1..end])?;
        self.i = end + 1;
        if escaped {
            unescape(raw, None)?;
        }
        Ok(())
    }

    fn skip_value(&mut self, depth: usize) -> Result<(), Fail> {
        match *self.b.get(self.i).ok_or(Fail::Syntax)? {
            b'"' => self.skip_string(),
            b'{' | b'[' => {
                if depth >= MAX_NEST {
                    return Err(Fail::Deep);
                }
                let (close, is_obj) = if self.b[self.i] == b'{' { (b'}', true) } else { (b']', false) };
                self.i += 1;
                self.ws();
                if self.b.get(self.i) == Some(&close) {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    self.ws();
                    if is_obj {
                        if self.b.get(self.i) != Some(&b'"') {
                            return Err(Fail::Syntax);
                        }
                        self.skip_string()?;
                        self.ws();
                        if self.b.get(self.i) != Some(&b':') {
                            return Err(Fail::Syntax);
                        }
                        self.i += 1;
                        self.ws();
                    }
                    self.skip_value(depth + 1)?;
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(&c) if c == close => {
                            self.i += 1;
                            return Ok(());
                        }
                        _ => return Err(Fail::Syntax),
                    }
                }
            }
            b'-' | b'0'..=b'9' => {
                self.i = self.scan_number()?;
                Ok(())
            }
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            _ => Err(Fail::Syntax),
        }
    }
}

/// UTF-8 check with an ASCII shortcut: ASCII is checked at word speed and needs no decoding,
/// which is most keys and identifiers; other text goes to the SIMD validator.
#[inline]
fn utf8(b: &[u8]) -> Result<&str, Fail> {
    if b.is_ascii() {
        // SAFETY: every byte is below 0x80, and ASCII bytes are valid UTF-8 on their own.
        return Ok(unsafe { std::str::from_utf8_unchecked(b) });
    }
    simdutf8::basic::from_utf8(b).map_err(|_| Fail::Syntax)
}

fn hex4(b: &[u8], at: usize) -> Result<u32, Fail> {
    let digits = b.get(at..at + 4).ok_or(Fail::Syntax)?;
    let mut n = 0u32;
    for &d in digits {
        let v = match d {
            b'0'..=b'9' => d - b'0',
            b'a'..=b'f' => d - b'a' + 10,
            b'A'..=b'F' => d - b'A' + 10,
            _ => return Err(Fail::Syntax),
        };
        n = n << 4 | u32::from(v);
    }
    Ok(n)
}

/// Decodes the `\uXXXX` escape whose hex digits start at `at` (surrogate pairs included);
/// returns the char and the index after the escape.
fn unicode_escape(b: &[u8], at: usize) -> Result<(char, usize), Fail> {
    let hi = hex4(b, at)?;
    let (code, next) = match hi {
        0xD800..=0xDBFF => {
            if b.get(at + 4..at + 6) != Some(b"\\u") {
                return Err(Fail::Syntax);
            }
            let lo = hex4(b, at + 6)?;
            if !(0xDC00..=0xDFFF).contains(&lo) {
                return Err(Fail::Syntax);
            }
            (0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00), at + 10)
        }
        0xDC00..=0xDFFF => return Err(Fail::Syntax),
        other => (other, at + 4),
    };
    Ok((char::from_u32(code).ok_or(Fail::Syntax)?, next))
}

/// Resolves the escapes of a string body (already UTF-8 validated), appending the result to
/// `out` when given; with `None` it only checks them.
fn unescape(raw: &str, mut out: Option<&mut String>) -> Result<(), Fail> {
    let b = raw.as_bytes();
    let mut seg = 0;
    while let Some(rel) = memchr::memchr(b'\\', &b[seg..]) {
        let at = seg + rel;
        if let Some(o) = out.as_deref_mut() {
            o.push_str(&raw[seg..at]);
        }
        let mut next = at + 2;
        let ch = match *b.get(at + 1).ok_or(Fail::Syntax)? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let (c, n) = unicode_escape(b, at + 2)?;
                next = n;
                c
            }
            _ => return Err(Fail::Syntax),
        };
        if let Some(o) = out.as_deref_mut() {
            o.push(ch);
        }
        seg = next;
    }
    if let Some(o) = out {
        o.push_str(&raw[seg..]);
    }
    Ok(())
}

// ---------------------------------------------------------------- writer

/// Appends `s` as a JSON string literal with serde_json's escaping (`"`, `\`, control
/// characters; everything else, including `/`, DEL and non-ASCII, verbatim).
pub(crate) fn write_str(out: &mut Vec<u8>, s: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let b = s.as_bytes();
    out.reserve(b.len() + 2);
    out.push(b'"');
    let mut start = 0;
    let mut i = 0;
    loop {
        i = find_special(b, i);
        if i >= b.len() {
            break;
        }
        out.extend_from_slice(&b[start..i]);
        match b[i] {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c => out.extend_from_slice(&[b'\\', b'u', b'0', b'0', HEX[usize::from(c >> 4)], HEX[usize::from(c & 15)]]),
        }
        i += 1;
        start = i;
    }
    out.extend_from_slice(&b[start..]);
    out.push(b'"');
}

/// Rough output size, so the writer reserves once instead of doubling through a large body.
fn size_hint(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) => 5,
        Value::Number(n) => n.as_str().len(),
        Value::String(s) => s.len() + 2,
        Value::Array(a) => 2 + a.iter().map(|e| size_hint(e) + 1).sum::<usize>(),
        Value::Object(m) => 2 + m.iter().map(|(k, e)| k.len() + 4 + size_hint(e)).sum::<usize>(),
    }
}

/// Compact serialization identical to `serde_json::to_vec`.
pub(crate) fn to_vec(v: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(size_hint(v));
    write_value(&mut out, v);
    out
}

pub(crate) fn write_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => out.extend_from_slice(n.as_str().as_bytes()),
        Value::String(s) => write_str(out, s),
        Value::Array(a) => {
            out.push(b'[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(out, e);
            }
            out.push(b']');
        }
        Value::Object(m) => {
            out.push(b'{');
            for (i, (k, e)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_str(out, k);
                out.push(b':');
                write_value(out, e);
            }
            out.push(b'}');
        }
    }
}

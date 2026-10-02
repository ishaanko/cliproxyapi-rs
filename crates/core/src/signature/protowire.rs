//! Minimal port of `google.golang.org/protobuf/encoding/protowire` (consume side only).
//!
//! Signature validation walks protobuf wire data by hand, exactly like Go: tags, varints,
//! length-delimited fields and group skipping, with the same failure classes.

use std::fmt;

/// Go: `protowire.DefaultRecursionLimit`.
const DEFAULT_RECURSION_LIMIT: i32 = 10000;

/// Wire type of a field (Go: `protowire.Type`). Values 6 and 7 are kept as `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireType {
    Varint,
    Fixed64,
    Bytes,
    StartGroup,
    EndGroup,
    Fixed32,
    Other(u8),
}

impl WireType {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Varint,
            1 => Self::Fixed64,
            2 => Self::Bytes,
            3 => Self::StartGroup,
            4 => Self::EndGroup,
            5 => Self::Fixed32,
            other => Self::Other(other),
        }
    }
}

/// Parse failures (Go: `protowire.ParseError` codes). `Display` matches Go's error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    Truncated,
    FieldNumber,
    Overflow,
    Reserved,
    EndGroup,
    RecursionDepth,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "unexpected EOF",
            Self::FieldNumber => "proto: invalid field number",
            Self::Overflow => "proto: variable length integer overflow",
            Self::Reserved => "proto: cannot parse reserved wire type",
            Self::EndGroup => "proto: mismatching end group marker",
            Self::RecursionDepth => "proto: parse error",
        })
    }
}

impl std::error::Error for ParseError {}

type Parsed<T> = Result<T, ParseError>;

/// Parses a varint, returning the value and its encoded length (Go: `ConsumeVarint`).
pub fn consume_varint(b: &[u8]) -> Parsed<(u64, usize)> {
    let mut v: u64 = 0;
    for (i, &byte) in b.iter().take(10).enumerate() {
        if i == 9 {
            // The tenth byte may only carry the top bit of the value.
            return if byte < 2 {
                Ok((v | (u64::from(byte) << 63), 10))
            } else {
                Err(ParseError::Overflow)
            };
        }
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            return Ok((v, i + 1));
        }
    }
    Err(ParseError::Truncated)
}

/// Parses a tag, returning the field number, wire type and encoded length (Go: `ConsumeTag`).
pub fn consume_tag(b: &[u8]) -> Parsed<(u32, WireType, usize)> {
    let (v, n) = consume_varint(b)?;
    let num = v >> 3;
    if num < 1 || num > i32::MAX as u64 {
        return Err(ParseError::FieldNumber);
    }
    Ok((num as u32, WireType::from_bits((v & 7) as u8), n))
}

/// Parses a length-prefixed value, returning the payload and total consumed length.
pub fn consume_bytes(b: &[u8]) -> Parsed<(&[u8], usize)> {
    let (len, n) = consume_varint(b)?;
    let rest = &b[n..];
    if len > rest.len() as u64 {
        return Err(ParseError::Truncated);
    }
    let len = len as usize;
    Ok((&rest[..len], n + len))
}

pub fn consume_fixed32(b: &[u8]) -> Parsed<(u32, usize)> {
    let bytes: [u8; 4] = b
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or(ParseError::Truncated)?;
    Ok((u32::from_le_bytes(bytes), 4))
}

pub fn consume_fixed64(b: &[u8]) -> Parsed<(u64, usize)> {
    let bytes: [u8; 8] = b
        .get(..8)
        .and_then(|s| s.try_into().ok())
        .ok_or(ParseError::Truncated)?;
    Ok((u64::from_le_bytes(bytes), 8))
}

/// Length of a field value of the given wire type; groups are skipped to their matching end
/// marker (Go: `ConsumeFieldValue`).
pub fn consume_field_value(num: u32, typ: WireType, b: &[u8]) -> Parsed<usize> {
    consume_field_value_d(num, typ, b, DEFAULT_RECURSION_LIMIT)
}

fn consume_field_value_d(num: u32, typ: WireType, b: &[u8], depth: i32) -> Parsed<usize> {
    match typ {
        WireType::Varint => consume_varint(b).map(|(_, n)| n),
        WireType::Fixed32 => consume_fixed32(b).map(|(_, n)| n),
        WireType::Fixed64 => consume_fixed64(b).map(|(_, n)| n),
        WireType::Bytes => consume_bytes(b).map(|(_, n)| n),
        WireType::StartGroup => {
            if depth < 0 {
                return Err(ParseError::RecursionDepth);
            }
            let total = b.len();
            let mut rest = b;
            loop {
                let (num2, typ2, n) = consume_tag(rest)?;
                rest = &rest[n..];
                if typ2 == WireType::EndGroup {
                    if num != num2 {
                        return Err(ParseError::EndGroup);
                    }
                    return Ok(total - rest.len());
                }
                let n = consume_field_value_d(num2, typ2, rest, depth - 1)?;
                rest = &rest[n..];
            }
        }
        WireType::EndGroup => Err(ParseError::EndGroup),
        WireType::Other(_) => Err(ParseError::Reserved),
    }
}

/// Append side of the wire format, used to build signature fixtures in tests.
#[cfg(test)]
pub(crate) mod append {
    use super::WireType;

    pub fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v & 0x7f) as u8 | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    pub fn tag(out: &mut Vec<u8>, num: u32, typ: WireType) {
        let bits = match typ {
            WireType::Varint => 0,
            WireType::Fixed64 => 1,
            WireType::Bytes => 2,
            WireType::StartGroup => 3,
            WireType::EndGroup => 4,
            WireType::Fixed32 => 5,
            WireType::Other(b) => b,
        };
        varint(out, (u64::from(num) << 3) | u64::from(bits));
    }
}

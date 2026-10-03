//! Minimal protobuf wire format helpers (the subset of Go's `protowire` the Devin protocol uses).
//!
//! Encoders append to a `Vec<u8>`; decoders take a slice and return `None` on malformed input
//! (truncation, varint overflow, field number 0, unmatched groups), like protowire's negative
//! consume results.

pub const VARINT: u8 = 0;
pub const FIXED64: u8 = 1;
pub const BYTES: u8 = 2;
pub const START_GROUP: u8 = 3;
pub const END_GROUP: u8 = 4;
pub const FIXED32: u8 = 5;

const MAX_FIELD_NUMBER: u64 = i32::MAX as u64;
const MAX_GROUP_DEPTH: usize = 10_000;

pub fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

pub fn put_tag(buf: &mut Vec<u8>, num: u32, typ: u8) {
    put_varint(buf, (u64::from(num) << 3) | u64::from(typ));
}

/// Length-delimited field (string or bytes).
pub fn put_bytes(buf: &mut Vec<u8>, num: u32, value: &[u8]) {
    put_tag(buf, num, BYTES);
    put_varint(buf, value.len() as u64);
    buf.extend_from_slice(value);
}

pub fn put_str(buf: &mut Vec<u8>, num: u32, value: &str) {
    put_bytes(buf, num, value.as_bytes());
}

pub fn put_varint_field(buf: &mut Vec<u8>, num: u32, value: u64) {
    put_tag(buf, num, VARINT);
    put_varint(buf, value);
}

pub fn put_fixed64_field(buf: &mut Vec<u8>, num: u32, value: u64) {
    put_tag(buf, num, FIXED64);
    buf.extend_from_slice(&value.to_le_bytes());
}

/// `(value, bytes consumed)`; at most 10 bytes, the 10th may only carry bit 63.
pub fn get_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &b) in data.iter().take(10).enumerate() {
        if i == 9 && b > 1 {
            return None;
        }
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// `(field number, wire type, bytes consumed)`.
pub fn get_tag(data: &[u8]) -> Option<(u32, u8, usize)> {
    let (v, n) = get_varint(data)?;
    let num = v >> 3;
    if num == 0 || num > MAX_FIELD_NUMBER {
        return None;
    }
    Some((num as u32, (v & 7) as u8, n))
}

/// Length-delimited payload and total bytes consumed (length prefix included).
pub fn get_bytes(data: &[u8]) -> Option<(&[u8], usize)> {
    let (len, n) = get_varint(data)?;
    let end = n.checked_add(usize::try_from(len).ok()?)?;
    if end > data.len() {
        return None;
    }
    Some((&data[n..end], end))
}

pub fn get_fixed64(data: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(..8)?.try_into().ok()?))
}

pub fn get_fixed32(data: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(..4)?.try_into().ok()?))
}

/// Bytes occupied by one field value of wire type `typ` whose tag (number `num`) was already
/// consumed. Groups are skipped through their matching end tag.
pub fn skip_field(num: u32, typ: u8, data: &[u8]) -> Option<usize> {
    match typ {
        VARINT => get_varint(data).map(|(_, n)| n),
        FIXED64 => (data.len() >= 8).then_some(8),
        FIXED32 => (data.len() >= 4).then_some(4),
        BYTES => get_bytes(data).map(|(_, n)| n),
        START_GROUP => skip_group(num, data),
        _ => None,
    }
}

/// Skips a group body through its matching end tag. Nested groups are tracked on an explicit
/// stack, so hostile nesting costs memory but never call stack.
fn skip_group(num: u32, data: &[u8]) -> Option<usize> {
    let mut open = vec![num];
    let mut pos = 0;
    while let Some(&current) = open.last() {
        let (inner_num, inner_typ, n) = get_tag(data.get(pos..)?)?;
        pos += n;
        match inner_typ {
            END_GROUP if inner_num == current => {
                open.pop();
            }
            END_GROUP => return None,
            START_GROUP => {
                if open.len() >= MAX_GROUP_DEPTH {
                    return None;
                }
                open.push(inner_num);
            }
            _ => pos += skip_field(inner_num, inner_typ, data.get(pos..)?)?,
        }
    }
    Some(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip_and_overflow() {
        for v in [0u64, 1, 127, 128, 300, u64::MAX] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            assert_eq!(get_varint(&b), Some((v, b.len())));
        }
        // Eleven continuation bytes and a 10th byte above 1 both overflow.
        assert_eq!(get_varint(&[0xff; 11]), None);
        assert_eq!(
            get_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]),
            None
        );
        assert_eq!(get_varint(&[0x80]), None);
    }

    #[test]
    fn groups_are_skipped_to_the_matching_end_tag() {
        // field 1 start group, a varint field 2 inside, field 1 end group.
        let mut b = Vec::new();
        put_varint_field(&mut b, 2, 7);
        put_tag(&mut b, 1, END_GROUP);
        b.push(0xAA);
        assert_eq!(skip_field(1, START_GROUP, &b), Some(b.len() - 1));
        assert_eq!(skip_field(9, START_GROUP, &b), None);
    }
}

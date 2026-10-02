//! Go-compatible base64 decoding (`encoding/base64` Std/RawStd/URL/RawURL encodings).
//!
//! A port of Go's `decodeQuantum` loop rather than a wrapper over the `base64` crate, because the
//! observable behavior differs: Go ignores `\r` and `\n` anywhere in the input, accepts non-zero
//! trailing bits, and reports `CorruptInputError` offsets that tests and logs compare.

use base64::Engine;

/// Go's `base64.CorruptInputError` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy)]
struct Encoding {
    url_safe: bool,
    padded: bool,
}

const STD: Encoding = Encoding { url_safe: false, padded: true };
const RAW_STD: Encoding = Encoding { url_safe: false, padded: false };
const URL: Encoding = Encoding { url_safe: true, padded: true };
const RAW_URL: Encoding = Encoding { url_safe: true, padded: false };

impl Encoding {
    fn sextet(self, b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' if !self.url_safe => Some(62),
            b'/' if !self.url_safe => Some(63),
            b'-' if self.url_safe => Some(62),
            b'_' if self.url_safe => Some(63),
            _ => None,
        }
    }
}

fn corrupt(offset: usize) -> DecodeError {
    DecodeError(format!("illegal base64 data at input byte {offset}"))
}

fn is_newline(b: u8) -> bool {
    b == b'\n' || b == b'\r'
}

/// Go's `decodeQuantum`: decodes up to four sextets starting at `si`, appending the output bytes.
/// Returns the next input index.
fn decode_quantum(enc: Encoding, dst: &mut Vec<u8>, src: &[u8], mut si: usize) -> Result<usize, DecodeError> {
    let mut dbuf = [0u8; 4];
    let mut dlen = 4;
    let mut trailing_garbage: Option<usize> = None;
    let mut j = 0usize;
    while j < 4 {
        if si == src.len() {
            if j == 0 {
                return Ok(si);
            }
            if j == 1 || enc.padded {
                return Err(corrupt(si - j));
            }
            dlen = j;
            break;
        }
        let input = src[si];
        si += 1;
        if let Some(out) = enc.sextet(input) {
            dbuf[j] = out;
            j += 1;
            continue;
        }
        if is_newline(input) {
            continue;
        }
        if input != b'=' || !enc.padded {
            return Err(corrupt(si - 1));
        }
        // Padding reached.
        match j {
            0 | 1 => return Err(corrupt(si - 1)),
            2 => {
                // "==" is expected; the first "=" is already consumed.
                while si < src.len() && is_newline(src[si]) {
                    si += 1;
                }
                if si == src.len() {
                    return Err(corrupt(src.len()));
                }
                if src[si] != b'=' {
                    return Err(corrupt(si - 1));
                }
                si += 1;
            }
            _ => {}
        }
        while si < src.len() && is_newline(src[si]) {
            si += 1;
        }
        if si < src.len() {
            trailing_garbage = Some(si);
        }
        dlen = j;
        break;
    }

    let val = (u32::from(dbuf[0]) << 18) | (u32::from(dbuf[1]) << 12) | (u32::from(dbuf[2]) << 6) | u32::from(dbuf[3]);
    let bytes = val.to_be_bytes(); // [0, b0, b1, b2]
    match dlen {
        4 => dst.extend_from_slice(&bytes[1..4]),
        3 => dst.extend_from_slice(&bytes[1..3]),
        2 => dst.push(bytes[1]),
        _ => {}
    }
    match trailing_garbage {
        Some(offset) => Err(corrupt(offset)),
        None => Ok(si),
    }
}

fn decode(enc: Encoding, src: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut dst = Vec::with_capacity(src.len() / 4 * 3 + 3);
    let mut si = 0;
    while si < src.len() {
        si = decode_quantum(enc, &mut dst, src, si)?;
    }
    Ok(dst)
}

/// `base64.StdEncoding.DecodeString`.
pub fn std(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode(STD, input.as_ref())
}

/// `base64.RawStdEncoding.DecodeString`.
pub fn raw_std(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode(RAW_STD, input.as_ref())
}

/// `base64.URLEncoding.DecodeString`.
pub fn url(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode(URL, input.as_ref())
}

/// `base64.RawURLEncoding.DecodeString`.
pub fn raw_url(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode(RAW_URL, input.as_ref())
}

/// `base64.RawStdEncoding.EncodeToString`.
pub fn encode_raw_std(input: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(input)
}

/// `base64.StdEncoding.EncodeToString`.
pub fn encode_std(input: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(input)
}

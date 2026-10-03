//! Pluggable remote KV store for the global caches (Go: the `internal/home` KV client the
//! `internal/cache` files switch to when Home mode is on).
//!
//! `cpa-core` cannot depend on the Home client (the config crate depends on core), so the
//! executors crate installs a [`KvBackend`] at startup. The caches are synchronous, hence the
//! backend is too: its implementation bridges to the async Home client. With no backend, or one
//! reporting "not Home mode", the caches use their in-process maps exactly as before.

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Failure of a KV operation (Home mode on, but the store errored or is unavailable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvError {
    message: String,
    cas_unsupported: bool,
}

impl KvError {
    pub fn new(message: impl fmt::Display) -> Self {
        KvError { message: message.to_string(), cas_unsupported: false }
    }

    /// Home predates compare-and-swap (Go: `homekv.ErrCompareAndSwapUnsupported`).
    pub fn compare_and_swap_unsupported(message: impl fmt::Display) -> Self {
        KvError { message: message.to_string(), cas_unsupported: true }
    }

    pub fn is_compare_and_swap_unsupported(&self) -> bool {
        self.cas_unsupported
    }
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for KvError {}

pub type KvResult<T> = Result<T, KvError>;

/// The remote store behind Home mode. A zero `ttl` means no expiry.
pub trait KvBackend: Send + Sync {
    /// `Ok(false)`: not in Home mode (use the in-process cache). `Ok(true)`: usable. `Err`: Home
    /// mode is on but the store is unavailable (Go: `CurrentKVClient`'s error).
    fn status(&self) -> KvResult<bool>;
    fn get(&self, key: &str) -> KvResult<Option<Vec<u8>>>;
    /// Plain `SET` with an optional expiry; `Ok(false)` when Home did not write.
    fn set(&self, key: &str, value: &[u8], ttl: Duration) -> KvResult<bool>;
    /// Atomic replace when the stored value still equals `expected` (`None`: key must be absent).
    fn compare_and_swap(&self, key: &str, expected: Option<&[u8]>, value: &[u8], ttl: Duration) -> KvResult<bool>;
    fn del(&self, key: &str) -> KvResult<()>;
    fn expire(&self, key: &str, ttl: Duration) -> KvResult<()>;
}

static BACKEND: OnceLock<Arc<dyn KvBackend>> = OnceLock::new();

/// Installs the process-wide backend (first call wins).
pub fn install_kv_backend(backend: Arc<dyn KvBackend>) {
    let _ = BACKEND.set(backend);
}

/// Where a cache operation runs.
pub(crate) enum Store {
    Local,
    Home(&'static dyn KvBackend),
}

/// The active store; `Err` when Home mode is on but the KV store is unavailable.
pub(crate) fn store() -> KvResult<Store> {
    let Some(backend) = BACKEND.get() else { return Ok(Store::Local) };
    Ok(if backend.status()? { Store::Home(backend.as_ref()) } else { Store::Local })
}

/// Hex SHA-256 of a key component, so keys never carry raw session or credential text (Go:
/// `homekv.HashKeyPart`).
pub(crate) fn hash_key_part(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value.as_bytes()))
}

/// `<prefix>:<sha256(a)>:<sha256(b)>` with both parts trimmed, the Home key of a replay cache
/// entry (`prefix` such as `cpa:kimi:thinking-replay`).
pub(crate) fn scoped_kv_key(prefix: &str, a: &str, b: &str) -> String {
    format!("{prefix}:{}:{}", hash_key_part(a.trim()), hash_key_part(b.trim()))
}

/// Compact JSON text of `raw` as Go's `json.Marshal` emits a `json.RawMessage` (`json.Compact`
/// with HTML escaping): the bytes are validated and insignificant whitespace dropped, while
/// escapes, key order, duplicate keys, number text and lone surrogate escapes stay byte for byte;
/// only `<`, `>`, `&`, U+2028 and U+2029 inside strings are rewritten to `\u` escapes. Input that
/// is not valid UTF-8 is an error.
pub(crate) fn compact_raw_json(raw: &[u8]) -> KvResult<String> {
    compact_json_bytes(raw).ok_or_else(|| KvError::new("invalid JSON in raw message"))
}

/// Parser state of [`compact_json_bytes`].
#[derive(Clone, Copy, PartialEq)]
enum Expect {
    Value,
    ValueOrEnd,
    KeyOrEnd,
    Key,
    Colon,
    AfterValue,
}

/// Go's `json.Nesting` limit.
const MAX_JSON_DEPTH: usize = 10_000;

fn compact_json_bytes(src: &[u8]) -> Option<String> {
    let mut out: Vec<u8> = Vec::with_capacity(src.len());
    let mut stack: Vec<u8> = Vec::new();
    let mut expect = Expect::Value;
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        if matches!(c, b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
            continue;
        }
        match expect {
            Expect::KeyOrEnd if c == b'}' => {
                stack.pop();
                out.push(c);
                expect = Expect::AfterValue;
                i += 1;
            }
            Expect::ValueOrEnd if c == b']' => {
                stack.pop();
                out.push(c);
                expect = Expect::AfterValue;
                i += 1;
            }
            Expect::Value | Expect::ValueOrEnd => match c {
                b'{' | b'[' => {
                    if stack.len() >= MAX_JSON_DEPTH {
                        return None;
                    }
                    stack.push(c);
                    out.push(c);
                    expect = if c == b'{' { Expect::KeyOrEnd } else { Expect::ValueOrEnd };
                    i += 1;
                }
                b'"' => {
                    i = compact_json_string(src, i, &mut out)?;
                    expect = Expect::AfterValue;
                }
                b'-' | b'0'..=b'9' => {
                    i = compact_json_number(src, i, &mut out)?;
                    expect = Expect::AfterValue;
                }
                b't' | b'f' | b'n' => {
                    let lit: &[u8] = match c {
                        b't' => b"true",
                        b'f' => b"false",
                        _ => b"null",
                    };
                    if !src[i..].starts_with(lit) {
                        return None;
                    }
                    out.extend_from_slice(lit);
                    i += lit.len();
                    expect = Expect::AfterValue;
                }
                _ => return None,
            },
            Expect::KeyOrEnd | Expect::Key => {
                if c != b'"' {
                    return None;
                }
                i = compact_json_string(src, i, &mut out)?;
                expect = Expect::Colon;
            }
            Expect::Colon => {
                if c != b':' {
                    return None;
                }
                out.push(c);
                expect = Expect::Value;
                i += 1;
            }
            Expect::AfterValue => match (stack.last().copied(), c) {
                (Some(b'{'), b',') => {
                    out.push(c);
                    expect = Expect::Key;
                    i += 1;
                }
                (Some(b'['), b',') => {
                    out.push(c);
                    expect = Expect::Value;
                    i += 1;
                }
                (Some(b'{'), b'}') | (Some(b'['), b']') => {
                    stack.pop();
                    out.push(c);
                    i += 1;
                }
                _ => return None,
            },
        }
    }
    if !stack.is_empty() || expect != Expect::AfterValue {
        return None;
    }
    String::from_utf8(out).ok()
}

/// Copies the string starting at `src[start]` (a quote) into `out`, returning the index after its
/// closing quote.
fn compact_json_string(src: &[u8], start: usize, out: &mut Vec<u8>) -> Option<usize> {
    out.push(b'"');
    let mut i = start + 1;
    while i < src.len() {
        let c = src[i];
        match c {
            b'"' => {
                out.push(b'"');
                return Some(i + 1);
            }
            0..=0x1f => return None,
            b'\\' => {
                let esc = *src.get(i + 1)?;
                match esc {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                        out.extend_from_slice(&[b'\\', esc]);
                        i += 2;
                    }
                    b'u' => {
                        let hex = src.get(i + 2..i + 6)?;
                        if !hex.iter().all(u8::is_ascii_hexdigit) {
                            return None;
                        }
                        out.extend_from_slice(&src[i..i + 6]);
                        i += 6;
                    }
                    _ => return None,
                }
            }
            b'<' | b'>' | b'&' => {
                out.extend_from_slice(format!("\\u00{c:02x}").as_bytes());
                i += 1;
            }
            0xE2 if src.get(i + 1) == Some(&0x80) && src.get(i + 2).is_some_and(|b| b & !1 == 0xA8) => {
                out.extend_from_slice(if src[i + 2] == 0xA8 { b"\\u2028" } else { b"\\u2029" });
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    None
}

/// Copies the number starting at `src[start]` into `out`, returning the index after it.
fn compact_json_number(src: &[u8], start: usize, out: &mut Vec<u8>) -> Option<usize> {
    let digits = |mut i: usize| {
        let begin = i;
        while src.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        (i > begin).then_some(i)
    };
    let mut i = start;
    if src[i] == b'-' {
        i += 1;
    }
    match src.get(i)? {
        b'0' => i += 1,
        b'1'..=b'9' => i = digits(i)?,
        _ => return None,
    }
    if src.get(i) == Some(&b'.') {
        i = digits(i + 1)?;
    }
    if matches!(src.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(src.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        i = digits(i)?;
    }
    out.extend_from_slice(&src[start..i]);
    Some(i)
}

/// Home value of a `[][]byte`: a JSON array of base64 strings (how Go's `json.Marshal` stores
/// byte slices), used by the Codex, xAI and Antigravity caches.
pub(crate) fn encode_items(items: &[Vec<u8>]) -> KvResult<Vec<u8>> {
    use base64::Engine as _;
    let encoded: Vec<String> = items.iter().map(|item| base64::engine::general_purpose::STANDARD.encode(item)).collect();
    serde_json::to_vec(&encoded).map_err(KvError::new)
}

/// Inverse of [`encode_items`]; JSON `null` (or a null element) reads as empty.
pub(crate) fn decode_items(raw: &[u8]) -> KvResult<Vec<Vec<u8>>> {
    use base64::Engine as _;
    let encoded: Option<Vec<Option<String>>> = serde_json::from_slice(raw).map_err(KvError::new)?;
    encoded
        .unwrap_or_default()
        .into_iter()
        .map(|item| match item {
            Some(text) => base64::engine::general_purpose::STANDARD.decode(text).map_err(KvError::new),
            None => Ok(Vec::new()),
        })
        .collect()
}

/// Reads `key`, first installing `tombstone` when it is absent (up to four attempts), so a stale
/// writer can never publish over a state that a newer request has already observed as a miss.
/// Values over `max_bytes` are refused. `label` names the cache in error messages.
pub(crate) fn read_or_reserve(
    backend: &dyn KvBackend,
    key: &str,
    ttl: Duration,
    max_bytes: usize,
    label: &str,
    tombstone: impl Fn() -> KvResult<Vec<u8>>,
) -> KvResult<Vec<u8>> {
    for _ in 0..4 {
        if let Some(raw) = backend.get(key)? {
            if raw.len() > max_bytes {
                return Err(KvError::new(format!("{label} value exceeds size limit")));
            }
            return Ok(raw);
        }
        let reservation = tombstone()?;
        if backend.compare_and_swap(key, None, &reservation, ttl)? {
            return Ok(reservation);
        }
    }
    Err(KvError::new(format!("could not reserve absent {label} state")))
}

#[cfg(test)]
mod compact_tests {
    use super::compact_raw_json;

    // Go `json.Compact` + HTML escaping: bytes survive, only whitespace goes.
    #[test]
    fn compaction_keeps_escapes_duplicate_keys_numbers_and_lone_surrogates() {
        let raw = " {\"b\" : 1.50e+2 , \"a\":[ true,null ,\"x\\/\\ud800\\n\"],\"b\":\"<&>\u{2028}\u{2029}\" } ".as_bytes();
        assert_eq!(
            compact_raw_json(raw).unwrap(),
            "{\"b\":1.50e+2,\"a\":[true,null,\"x\\/\\ud800\\n\"],\"b\":\"\\u003c\\u0026\\u003e\\u2028\\u2029\"}"
        );
    }

    #[test]
    fn invalid_json_is_rejected() {
        for bad in ["", "{", "[1,]", "{\"a\"}", "01", "\"a\nb\"", "{} x", "tru", "\"\\q\""] {
            assert!(compact_raw_json(bad.as_bytes()).is_err(), "{bad:?}");
        }
    }
}

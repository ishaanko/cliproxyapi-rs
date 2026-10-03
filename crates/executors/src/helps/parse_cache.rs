//! Per-request memo of parsed JSON bodies for the byte-oriented request pipelines.
//!
//! Request preparation (Claude, and others) is a chain of `&[u8] -> Vec<u8>` stages that each
//! parse the whole body, read or edit a few fields and serialize it again, and the read-only
//! helpers (probe detection, beta header assembly, ...) re-read the same bytes several times. The
//! parse dominates, so this memo keeps the last few `(bytes, Value)` pairs of the current thread:
//! a stage that receives bytes it (or a previous stage) already parsed skips the parse, and
//! [`edit`] stores the value it just edited next to its serialization so the next stage hits.
//!
//! Lookups compare the full bytes, so a hit is exact. Caching is only active between
//! [`scope`] and the drop of its guard (one synchronous request preparation on one thread); the
//! entries are released with the guard, so nothing outlives a request.

use std::cell::RefCell;
use std::sync::Arc;

use cpa_json::Value;

/// Entries kept per thread: the working body plus the original and pre-cloaking copies.
const CAPACITY: usize = 3;

struct Entry {
    bytes: Vec<u8>,
    /// Result of `cpa_json::valid(bytes)` once asked.
    valid: Option<bool>,
    value: Option<Arc<Value>>,
}

#[derive(Default)]
struct Cache {
    depth: usize,
    /// Most recently used first.
    entries: Vec<Entry>,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// Keeps the memo active on this thread until dropped.
#[must_use = "the memo is released when the guard is dropped"]
pub struct Scope(());

/// Enables the memo for the current thread (nestable). Call from synchronous request
/// preparation only: the memo is per thread and must not be held across an `.await`.
pub fn scope() -> Scope {
    CACHE.with(|c| c.borrow_mut().depth += 1);
    Scope(())
}

impl Drop for Scope {
    fn drop(&mut self) {
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.depth = c.depth.saturating_sub(1);
            if c.depth == 0 {
                c.entries.clear();
            }
        });
    }
}

/// Whether a [`scope`] is open on this thread.
pub fn active() -> bool {
    CACHE.with(|c| c.borrow().depth) > 0
}

/// Removes and returns the entry holding exactly `bytes`.
fn take_entry(bytes: &[u8]) -> Option<Entry> {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.depth == 0 {
            return None;
        }
        let pos = c.entries.iter().position(|e| e.bytes == bytes)?;
        Some(c.entries.remove(pos))
    })
}

fn put_entry(entry: Entry) {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.depth == 0 {
            return;
        }
        c.entries.insert(0, entry);
        c.entries.truncate(CAPACITY);
    });
}

/// `cpa_json::parse(bytes)` shared through the memo; the result is read-only.
pub fn parse(bytes: &[u8]) -> Arc<Value> {
    if !active() {
        return Arc::new(cpa_json::parse(bytes));
    }
    let mut entry = take_entry(bytes).unwrap_or_else(|| Entry { bytes: bytes.to_vec(), valid: None, value: None });
    let value = Arc::clone(entry.value.get_or_insert_with(|| Arc::new(cpa_json::parse(bytes))));
    put_entry(entry);
    value
}

/// `cpa_json::valid(bytes)`, computed once per distinct body.
pub fn valid(bytes: &[u8]) -> bool {
    if !active() {
        return cpa_json::valid(bytes);
    }
    let mut entry = take_entry(bytes).unwrap_or_else(|| Entry { bytes: bytes.to_vec(), valid: None, value: None });
    let ok = *entry.valid.get_or_insert_with(|| cpa_json::valid(bytes));
    put_entry(entry);
    ok
}

/// Parses `bytes` (from the memo when possible), applies `f`, and returns the re-serialized body
/// when `f` reports a change, else a copy of `bytes`. The edited value is memoized under the
/// returned bytes so the next stage does not parse them again.
pub fn edit(bytes: &[u8], f: impl FnOnce(&mut Value) -> bool) -> Vec<u8> {
    let active = active();
    let mut entry = take_entry(bytes).unwrap_or_else(|| Entry { bytes: Vec::new(), valid: None, value: None });
    // Take the value out of the Arc when this was its last owner, else copy it.
    let mut value = match entry.value.take() {
        Some(shared) => Arc::try_unwrap(shared).unwrap_or_else(|shared| (*shared).clone()),
        None => cpa_json::parse(bytes),
    };
    if f(&mut value) {
        let out = cpa_json::to_vec(&value);
        if active {
            put_entry(Entry { bytes: out.clone(), valid: Some(true), value: Some(Arc::new(value)) });
        }
        out
    } else {
        if active {
            if entry.bytes.is_empty() {
                entry.bytes = bytes.to_vec();
            }
            entry.value = Some(Arc::new(value));
            put_entry(entry);
        }
        bytes.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_chain_without_reparsing_and_stay_exact() {
        let _scope = scope();
        let body = br#"{"a":1,"b":{"c":[1,2]}}"#.to_vec();
        let first = edit(&body, |v| cpa_json::set(v, "a", 2));
        assert_eq!(first, br#"{"a":2,"b":{"c":[1,2]}}"#);
        // The edited value is memoized under its serialization: parse() must agree with a real parse.
        assert_eq!(*parse(&first), cpa_json::parse(&first));
        let unchanged = edit(&first, |_| false);
        assert_eq!(unchanged, first);
        // Different bytes never hit.
        assert_eq!(*parse(br#"{"a":3}"#), cpa_json::parse(br#"{"a":3}"#));
    }

    #[test]
    fn nothing_is_kept_outside_a_scope() {
        {
            let _scope = scope();
            parse(b"{\"a\":1}");
        }
        CACHE.with(|c| assert!(c.borrow().entries.is_empty()));
        parse(b"{\"a\":1}");
        CACHE.with(|c| assert!(c.borrow().entries.is_empty()));
    }
}

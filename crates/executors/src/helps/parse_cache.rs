//! Per-request memo of parsed JSON bodies for the byte-oriented request pipelines.
//!
//! Request preparation (Claude, and others) is a chain of `&[u8] -> Vec<u8>` stages that each
//! parse the whole body, read or edit a few fields and serialize it again, and the read-only
//! helpers (probe detection, beta header assembly, ...) re-read the same bytes several times. The
//! parse dominates, so this memo keeps the last `(bytes, Value)` pair of the current thread:
//! a stage that receives bytes it (or a previous stage) already parsed skips the parse, and
//! [`edit`] stores the value it just edited next to its serialization so the next stage hits.
//!
//! Lookups compare the full bytes, so a hit is exact. Caching is only active between
//! [`scope`] and the drop of its guard (one synchronous request preparation on one thread); the
//! entries are released with the guard, so nothing outlives a request.
//!
//! Only the newest pair is kept: every stage replaces the body it reads, and measurements showed
//! older pairs (the original and pre-cloaking copies) are not looked up again, while each costs a
//! body copy plus its tree for the whole preparation (~25% of the live heap of a 2 MB request).
//! A [`scope`] also stops `cpa_json`'s own memo from storing trees for the same bodies.
//!
//! Memory is bounded: an entry costs its bytes plus the estimated heap of its tree
//! ([`cpa_json::tree_cost`]), a thread keeps at most [`MAX_THREAD_BYTES`] and all threads together
//! [`GLOBAL_BUDGET`]; past either, the oldest entries are evicted or the new one is not kept (its
//! callers then parse as before, with `cpa_json`'s own memo still catching large repeats). Bodies
//! below [`TRACK_LEN`] are not costed (three of them are far below any budget).
//! Large bodies stay cached here rather than deferring to that memo, which hands out deep clones:
//! twenty reads of a 440 KB body took 4 ms here, 20 ms through the memo and 28 ms uncached.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cpa_json::Value;

/// Entries kept per thread: the working body.
const CAPACITY: usize = 1;

/// Heap one thread's entries may hold, and the same across all threads (so many large requests in
/// flight cannot multiply it; `cpa_json`'s memo has the same shape of limits).
const MAX_THREAD_BYTES: usize = 32 * 1024 * 1024;
const GLOBAL_BUDGET: usize = 64 * 1024 * 1024;
/// Bodies shorter than this are not costed (a tree walk would outweigh what it protects).
const TRACK_LEN: usize = 8 * 1024;
static IN_USE: AtomicUsize = AtomicUsize::new(0);

struct Entry {
    bytes: Vec<u8>,
    /// Result of `cpa_json::valid(bytes)` once asked.
    valid: Option<bool>,
    value: Option<Arc<Value>>,
    /// Estimated heap of `value`, 0 when unknown or `bytes` is below [`TRACK_LEN`].
    tree: usize,
    /// Bytes charged to [`IN_USE`] while the entry sits in the cache; refunded on drop.
    charge: Charge,
}

impl Entry {
    fn empty() -> Entry {
        Entry { bytes: Vec::new(), valid: None, value: None, tree: 0, charge: Charge(0) }
    }

    fn new(bytes: &[u8]) -> Entry {
        Entry { bytes: bytes.to_vec(), ..Entry::empty() }
    }

    /// Cost of the entry as kept in the cache.
    fn cost(&self) -> usize {
        self.bytes.len() + self.tree
    }

    /// Attaches a freshly parsed or edited tree and costs it.
    fn set_value(&mut self, value: Arc<Value>) {
        self.tree = if self.bytes.len() >= TRACK_LEN { cpa_json::tree_cost(&value) } else { 0 };
        self.value = Some(value);
    }
}

/// A claim on [`IN_USE`], returned when dropped.
struct Charge(usize);

impl Drop for Charge {
    fn drop(&mut self) {
        if self.0 > 0 {
            IN_USE.fetch_sub(self.0, Ordering::Relaxed);
        }
    }
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

/// Keeps the memo active on this thread until dropped. `!Send`, so holding it across an `.await`
/// (where the task may resume on another thread) fails to compile.
#[must_use = "the memo is released when the guard is dropped"]
pub struct Scope(PhantomData<*const ()>, cpa_json::NoTrees);

/// Enables the memo for the current thread (nestable). Call from synchronous request
/// preparation only: the memo is per thread and must not be held across an `.await`.
pub fn scope() -> Scope {
    CACHE.with(|c| c.borrow_mut().depth += 1);
    Scope(PhantomData, cpa_json::suspend_trees())
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
        let mut entry = c.entries.remove(pos);
        // Dropping the old claim refunds it; `put_entry` charges again.
        entry.charge = Charge(0);
        Some(entry)
    })
}

/// Stores `entry` as most recently used, evicting the oldest entries to stay within the budgets;
/// an entry that cannot fit is dropped.
fn put_entry(mut entry: Entry) {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let cost = entry.cost();
        if c.depth == 0 || cost > MAX_THREAD_BYTES {
            return;
        }
        c.entries.truncate(CAPACITY - 1);
        while c.entries.iter().map(|e| e.cost()).sum::<usize>() + cost > MAX_THREAD_BYTES {
            c.entries.pop();
        }
        if IN_USE.fetch_add(cost, Ordering::Relaxed) + cost > GLOBAL_BUDGET {
            IN_USE.fetch_sub(cost, Ordering::Relaxed);
            return;
        }
        entry.charge = Charge(cost);
        c.entries.insert(0, entry);
    });
}

/// `cpa_json::parse(bytes)` shared through the memo; the result is read-only.
pub fn parse(bytes: &[u8]) -> Arc<Value> {
    if !active() {
        return Arc::new(cpa_json::parse(bytes));
    }
    let mut entry = take_entry(bytes).unwrap_or_else(|| Entry::new(bytes));
    let value = match &entry.value {
        Some(v) => Arc::clone(v),
        None => {
            let v = Arc::new(cpa_json::parse(bytes));
            entry.set_value(Arc::clone(&v));
            v
        }
    };
    put_entry(entry);
    value
}

/// `cpa_json::valid(bytes)`, computed once per distinct body.
pub fn valid(bytes: &[u8]) -> bool {
    if !active() {
        return cpa_json::valid(bytes);
    }
    let mut entry = take_entry(bytes).unwrap_or_else(|| Entry::new(bytes));
    let ok = *entry.valid.get_or_insert_with(|| cpa_json::valid(bytes));
    put_entry(entry);
    ok
}

/// Parses `bytes` (from the memo when possible), applies `f`, and returns the re-serialized body
/// when `f` reports a change, else a copy of `bytes`. The edited value is memoized under the
/// returned bytes so the next stage does not parse them again.
///
/// Contract: `f` must return `true` whenever it mutated the value. On `false` the value is assumed
/// untouched and is kept under the original `bytes`, so an unreported mutation would poison later
/// lookups of those bytes with a tree that no longer matches them.
pub fn edit(bytes: &[u8], f: impl FnOnce(&mut Value) -> bool) -> Vec<u8> {
    let active = active();
    let mut entry = take_entry(bytes).unwrap_or_else(Entry::empty);
    // Take the value out of the Arc when this was its last owner, else copy it.
    let mut value = match entry.value.take() {
        Some(shared) => Arc::try_unwrap(shared).unwrap_or_else(|shared| (*shared).clone()),
        None => cpa_json::parse(bytes),
    };
    if f(&mut value) {
        let out = cpa_json::to_vec(&value);
        if active {
            let mut fresh = Entry { bytes: out.clone(), valid: Some(true), ..Entry::empty() };
            fresh.set_value(Arc::new(value));
            put_entry(fresh);
        }
        out
    } else {
        if active {
            if entry.bytes.is_empty() {
                entry.bytes = bytes.to_vec();
            }
            // The tree is unchanged, so a known cost stays valid.
            if entry.tree == 0 {
                entry.set_value(Arc::new(value));
            } else {
                entry.value = Some(Arc::new(value));
            }
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

    #[test]
    fn big_entries_are_costed_once() {
        let body = format!(r#"{{"a":"{}","b":[1,2,3]}}"#, "x".repeat(20_000)).into_bytes();
        {
            let _scope = scope();
            parse(&body);
            let cost = CACHE.with(|c| c.borrow().entries.iter().map(Entry::cost).sum::<usize>());
            assert!(cost > body.len());
            assert!(IN_USE.load(Ordering::Relaxed) >= cost);
            // A hit does not double-charge: the cost stays that of one entry.
            parse(&body);
            assert_eq!(CACHE.with(|c| c.borrow().entries.iter().map(|e| e.charge.0).sum::<usize>()), cost);
        }
        CACHE.with(|c| assert!(c.borrow().entries.is_empty()));
    }
}

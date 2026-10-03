//! Per-request memo of parses of large documents.
//!
//! Request handling parses the same multi-megabyte body many times (each helper reads a field or
//! two through `parse(body).g(..)`). Inside a [`scope`] the second sighting of a large document
//! stores its parsed tree, and every later `parse`/`valid` of identical bytes costs a hash of the
//! input plus a deep clone instead of a full parse. Outside a scope nothing is cached, and the
//! memo is dropped when the scope ends, so nothing is shared between requests.
//!
//! Identity is the length plus a 64-bit hash with a per-process random seed, so a body cannot be
//! crafted to collide with another one.

use std::cell::RefCell;
use std::future::Future;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

/// Documents shorter than this are parsed directly (a parse is cheaper than hashing + cloning).
pub(crate) const MIN_LEN: usize = 32 * 1024;
/// Trees kept per scope, and the byte budget (source length) they may cover.
const MAX_TREES: usize = 2;
const MAX_TREE_BYTES: usize = 16 * 1024 * 1024;
/// Hashes remembered to detect a second sighting.
const MAX_SEEN: usize = 16;
/// Source bytes covered by stored trees across all live scopes; past it nothing new is stored,
/// so many large requests in flight cannot multiply memory use.
const GLOBAL_BUDGET: usize = 64 * 1024 * 1024;
static IN_USE: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
pub(crate) struct Memo {
    seen: Vec<(u64, usize)>,
    trees: Vec<Tree>,
    valid: Vec<(u64, usize, bool)>,
}

impl Drop for Memo {
    fn drop(&mut self) {
        let held: usize = self.trees.iter().map(|t| t.len).sum();
        if held > 0 {
            IN_USE.fetch_sub(held, Ordering::Relaxed);
        }
    }
}

struct Tree {
    hash: u64,
    len: usize,
    value: Value,
}

tokio::task_local! {
    static MEMO: RefCell<Memo>;
}

/// Runs `fut` with a fresh memo; every `parse`/`valid` made by it (on this task) may reuse
/// earlier work on identical large inputs.
pub async fn scope<F: Future>(fut: F) -> F::Output {
    MEMO.scope(RefCell::new(Memo::default()), fut).await
}

/// Synchronous variant of [`scope`], for blocking sections and tests.
pub fn scope_sync<R>(f: impl FnOnce() -> R) -> R {
    MEMO.sync_scope(RefCell::new(Memo::default()), f)
}

fn in_scope() -> bool {
    MEMO.try_with(|_| ()).is_ok()
}

fn digest(bytes: &[u8]) -> u64 {
    static STATE: std::sync::LazyLock<foldhash::fast::RandomState> = std::sync::LazyLock::new(foldhash::fast::RandomState::default);
    STATE.hash_one(bytes)
}

/// Claims `len` bytes of the global budget.
fn reserve(len: usize) -> bool {
    if IN_USE.fetch_add(len, Ordering::Relaxed) + len <= GLOBAL_BUDGET {
        return true;
    }
    IN_USE.fetch_sub(len, Ordering::Relaxed);
    false
}

/// `parse` through the memo; `miss` does the real parse.
pub(crate) fn parse(bytes: &[u8], miss: impl FnOnce(&[u8]) -> Value) -> Value {
    if bytes.len() < MIN_LEN || !in_scope() {
        return miss(bytes);
    }
    let hash = digest(bytes);
    let len = bytes.len();
    let hit = MEMO.try_with(|m| m.borrow().trees.iter().find(|t| t.hash == hash && t.len == len).map(|t| t.value.clone()));
    if let Ok(Some(v)) = hit {
        return v;
    }
    let value = miss(bytes);
    let _ = MEMO.try_with(|m| {
        let mut m = m.borrow_mut();
        if m.seen.contains(&(hash, len)) {
            let budget_used: usize = m.trees.iter().map(|t| t.len).sum();
            if m.trees.len() < MAX_TREES && budget_used + len <= MAX_TREE_BYTES && reserve(len) {
                m.trees.push(Tree { hash, len, value: value.clone() });
            }
        } else {
            if m.seen.len() == MAX_SEEN {
                m.seen.remove(0);
            }
            m.seen.push((hash, len));
        }
    });
    value
}

/// `valid` through the memo; `miss` does the real check.
pub(crate) fn valid(bytes: &[u8], miss: impl FnOnce(&[u8]) -> bool) -> bool {
    if bytes.len() < MIN_LEN || !in_scope() {
        return miss(bytes);
    }
    let hash = digest(bytes);
    let len = bytes.len();
    let hit = MEMO.try_with(|m| m.borrow().valid.iter().find(|(h, l, _)| *h == hash && *l == len).map(|(_, _, ok)| *ok));
    if let Ok(Some(ok)) = hit {
        return ok;
    }
    let ok = miss(bytes);
    let _ = MEMO.try_with(|m| {
        let mut m = m.borrow_mut();
        if m.valid.len() == MAX_SEEN {
            m.valid.remove(0);
        }
        m.valid.push((hash, len, ok));
    });
    ok
}

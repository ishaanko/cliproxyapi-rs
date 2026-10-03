//! Per-request memo of parses of large documents.
//!
//! Request handling parses the same multi-megabyte body many times (each helper reads a field or
//! two through `parse(body).g(..)`), and each pipeline stage re-parses what the previous one
//! serialized. Inside a [`scope`], a large document seen a second time keeps its parsed tree, so
//! later `parse`/`valid` calls on identical bytes cost a hash of the input plus a deep clone
//! instead of a full parse. Output of `to_vec` counts as already seen (and as valid), since the
//! next stage nearly always parses it. Outside a scope nothing is cached, and the memo is dropped
//! when the scope ends, so nothing is shared between requests.
//!
//! Identity is the length plus a 64-bit hash with a per-process random seed, so a body cannot be
//! crafted to collide with another one.

use std::cell::RefCell;
use std::future::Future;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

/// Documents shorter than this are handled directly (a parse is cheaper than hashing + cloning).
pub(crate) const MIN_LEN: usize = 32 * 1024;
/// Trees kept per scope (least recently used goes first), and the source bytes they may cover.
const MAX_TREES: usize = 3;
const MAX_TREE_BYTES: usize = 16 * 1024 * 1024;
/// Hashes remembered to detect a second sighting, and validity verdicts.
const MAX_SEEN: usize = 16;
/// Source bytes covered by stored trees across all live scopes; past it nothing new is stored,
/// so many large requests in flight cannot multiply memory use.
const GLOBAL_BUDGET: usize = 64 * 1024 * 1024;
static IN_USE: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
pub(crate) struct Memo {
    seen: Vec<(u64, usize)>,
    /// Most recently used last.
    trees: Vec<Tree>,
    valid: Vec<(u64, usize, bool)>,
}

struct Tree {
    hash: u64,
    len: usize,
    value: Value,
}

impl Drop for Memo {
    fn drop(&mut self) {
        let held: usize = self.trees.iter().map(|t| t.len).sum();
        if held > 0 {
            IN_USE.fetch_sub(held, Ordering::Relaxed);
        }
    }
}

impl Memo {
    fn note_seen(&mut self, key: (u64, usize)) {
        if !self.seen.contains(&key) {
            if self.seen.len() == MAX_SEEN {
                self.seen.remove(0);
            }
            self.seen.push(key);
        }
    }

    fn note_valid(&mut self, hash: u64, len: usize, ok: bool) {
        if !self.valid.iter().any(|(h, l, _)| *h == hash && *l == len) {
            if self.valid.len() == MAX_SEEN {
                self.valid.remove(0);
            }
            self.valid.push((hash, len, ok));
        }
    }

    fn drop_tree(&mut self, at: usize) {
        let t = self.trees.remove(at);
        IN_USE.fetch_sub(t.len, Ordering::Relaxed);
    }

    /// Stores a tree, evicting the least recently used ones to stay within the budgets.
    fn store_tree(&mut self, hash: u64, len: usize, value: Value) {
        if len > MAX_TREE_BYTES {
            return;
        }
        while !self.trees.is_empty()
            && (self.trees.len() >= MAX_TREES || self.trees.iter().map(|t| t.len).sum::<usize>() + len > MAX_TREE_BYTES)
        {
            self.drop_tree(0);
        }
        if IN_USE.fetch_add(len, Ordering::Relaxed) + len > GLOBAL_BUDGET {
            IN_USE.fetch_sub(len, Ordering::Relaxed);
            return;
        }
        self.trees.push(Tree { hash, len, value });
    }
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

/// `parse` through the memo; `miss` does the real parse.
pub(crate) fn parse(bytes: &[u8], miss: impl FnOnce(&[u8]) -> Value) -> Value {
    if bytes.len() < MIN_LEN || !in_scope() {
        return miss(bytes);
    }
    let hash = digest(bytes);
    let len = bytes.len();
    let hit = MEMO.try_with(|m| {
        let mut m = m.borrow_mut();
        let at = m.trees.iter().position(|t| t.hash == hash && t.len == len)?;
        let tree = m.trees.remove(at);
        let value = tree.value.clone();
        m.trees.push(tree);
        Some(value)
    });
    if let Ok(Some(v)) = hit {
        return v;
    }
    let value = miss(bytes);
    let _ = MEMO.try_with(|m| {
        let mut m = m.borrow_mut();
        if m.seen.contains(&(hash, len)) {
            m.store_tree(hash, len, value.clone());
        } else {
            m.note_seen((hash, len));
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
        m.note_valid(hash, len, ok);
        // Validated documents are nearly always parsed next.
        m.note_seen((hash, len));
    });
    ok
}

/// Records freshly serialized output: valid by construction, and likely parsed by the next stage.
pub(crate) fn note_serialized(bytes: &[u8]) {
    if bytes.len() < MIN_LEN || !in_scope() {
        return;
    }
    let hash = digest(bytes);
    let len = bytes.len();
    let _ = MEMO.try_with(|m| {
        let mut m = m.borrow_mut();
        m.note_valid(hash, len, true);
        m.note_seen((hash, len));
    });
}

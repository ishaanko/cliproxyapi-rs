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
//! Identity is the length plus a 128-bit digest (two foldhash instances with independent
//! per-process random seeds). foldhash is not cryptographic and makes no HashDoS promise; the
//! digest is only as safe as its seeds staying unknown to whoever sends the bodies, and 128 bits
//! keep accidental collisions out of reach.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::marker::PhantomData;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

/// Documents shorter than this are handled directly (a parse is cheaper than hashing + cloning).
pub(crate) const MIN_LEN: usize = 32 * 1024;
/// Trees kept per scope (least recently used goes first), and the estimated heap bytes they may
/// hold (see [`tree_cost`]; numeric arrays cost 20-30x their source text).
const MAX_TREES: usize = 3;
const MAX_TREE_BYTES: usize = 32 * 1024 * 1024;
/// Digests remembered to detect a second sighting, and validity verdicts.
const MAX_SEEN: usize = 16;
/// Estimated tree bytes across all live scopes; past it nothing new is stored, so many large
/// requests in flight cannot multiply memory use (real use stays within about twice this).
const GLOBAL_BUDGET: usize = 64 * 1024 * 1024;
static IN_USE: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
pub(crate) struct Memo {
    seen: Vec<Key>,
    /// Most recently used last.
    trees: Vec<Tree>,
    valid: Vec<(Key, bool)>,
}

/// Identity of a document: 128-bit digest and length.
type Key = (u128, usize);

struct Tree {
    key: Key,
    /// Estimated heap bytes of `value`, as charged to the budgets.
    cost: usize,
    value: Value,
}

impl Drop for Memo {
    fn drop(&mut self) {
        let held: usize = self.trees.iter().map(|t| t.cost).sum();
        if held > 0 {
            IN_USE.fetch_sub(held, Ordering::Relaxed);
        }
    }
}

impl Memo {
    fn note_seen(&mut self, key: Key) {
        if !self.seen.contains(&key) {
            if self.seen.len() == MAX_SEEN {
                self.seen.remove(0);
            }
            self.seen.push(key);
        }
    }

    fn note_valid(&mut self, key: Key, ok: bool) {
        if !self.valid.iter().any(|(k, _)| *k == key) {
            if self.valid.len() == MAX_SEEN {
                self.valid.remove(0);
            }
            self.valid.push((key, ok));
        }
    }

    fn drop_tree(&mut self, at: usize) {
        let t = self.trees.remove(at);
        IN_USE.fetch_sub(t.cost, Ordering::Relaxed);
    }

    /// Stores a tree, evicting the least recently used ones to stay within the budgets.
    fn store_tree(&mut self, key: Key, value: Value) {
        let cost = tree_cost(&value);
        if cost > MAX_TREE_BYTES {
            return;
        }
        while !self.trees.is_empty()
            && (self.trees.len() >= max_trees() || self.trees.iter().map(|t| t.cost).sum::<usize>() + cost > MAX_TREE_BYTES)
        {
            self.drop_tree(0);
        }
        if IN_USE.fetch_add(cost, Ordering::Relaxed) + cost > GLOBAL_BUDGET {
            IN_USE.fetch_sub(cost, Ordering::Relaxed);
            return;
        }
        self.trees.push(Tree { key, cost, value });
    }
}

/// Bytes a malloc chunk of `n` requested bytes occupies (8-byte header, 16-byte granule, 32 minimum).
fn chunk(n: usize) -> usize {
    if n == 0 { 0 } else { ((n + 8 + 15) & !15).max(32) }
}

/// Estimated heap bytes of a parsed tree (also used by callers that keep trees alive and need to
/// bound them): node slots plus string/number text and object entries,
/// rounded up to allocator chunks.
pub fn tree_cost(v: &Value) -> usize {
    const NODE: usize = std::mem::size_of::<Value>();
    // Key, value and the hash/index words an object keeps per entry.
    const ENTRY: usize = std::mem::size_of::<String>() + NODE + 24;
    match v {
        Value::Null | Value::Bool(_) => 0,
        Value::Number(n) => chunk(n.as_str().len()),
        Value::String(s) => chunk(s.capacity()),
        Value::Array(a) => chunk(a.capacity() * NODE) + a.iter().map(tree_cost).sum::<usize>(),
        Value::Object(m) => chunk(m.len() * ENTRY) + m.iter().map(|(k, e)| chunk(k.capacity()) + tree_cost(e)).sum::<usize>(),
    }
}

tokio::task_local! {
    static MEMO: RefCell<Memo>;
}

thread_local! {
    /// Nesting depth of [`suspend_trees`] guards on this thread.
    static NO_TREES: Cell<usize> = const { Cell::new(0) };
}

/// Trees kept per scope while a [`NoTrees`] guard is alive on the thread.
const SUSPENDED_TREES: usize = 1;

/// Trees the memo of the current thread may hold now.
fn max_trees() -> usize {
    if NO_TREES.with(Cell::get) == 0 { MAX_TREES } else { SUSPENDED_TREES }
}

/// While alive, the memo keeps at most one tree on this thread (digests and validity verdicts are
/// unaffected). For synchronous sections that keep their own parse cache: more trees would only
/// add copies to the resident set. `!Send`, so it cannot be held across an `.await`.
#[must_use = "the full tree budget returns when the guard is dropped"]
pub struct NoTrees(PhantomData<*const ()>);

pub fn suspend_trees() -> NoTrees {
    NO_TREES.with(|d| d.set(d.get() + 1));
    NoTrees(PhantomData)
}

impl Drop for NoTrees {
    fn drop(&mut self) {
        NO_TREES.with(|d| d.set(d.get().saturating_sub(1)));
    }
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

fn digest(bytes: &[u8]) -> Key {
    use std::sync::LazyLock;
    static HI: LazyLock<foldhash::fast::RandomState> = LazyLock::new(foldhash::fast::RandomState::default);
    static LO: LazyLock<foldhash::fast::RandomState> = LazyLock::new(foldhash::fast::RandomState::default);
    (u128::from(HI.hash_one(bytes)) << 64 | u128::from(LO.hash_one(bytes)), bytes.len())
}

/// `parse` through the memo; `miss` does the real parse.
pub(crate) fn parse(bytes: &[u8], miss: impl FnOnce(&[u8]) -> Value) -> Value {
    if bytes.len() < MIN_LEN || !in_scope() {
        return miss(bytes);
    }
    let key = digest(bytes);
    let hit = MEMO.try_with(|m| {
        let mut m = m.borrow_mut();
        let at = m.trees.iter().position(|t| t.key == key)?;
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
        if m.seen.contains(&key) {
            m.store_tree(key, value.clone());
        } else {
            m.note_seen(key);
        }
    });
    value
}

/// `valid` through the memo; `miss` does the real check.
pub(crate) fn valid(bytes: &[u8], miss: impl FnOnce(&[u8]) -> bool) -> bool {
    if bytes.len() < MIN_LEN || !in_scope() {
        return miss(bytes);
    }
    let key = digest(bytes);
    let hit = MEMO.try_with(|m| m.borrow().valid.iter().find(|(k, _)| *k == key).map(|(_, ok)| *ok));
    if let Ok(Some(ok)) = hit {
        return ok;
    }
    let ok = miss(bytes);
    let _ = MEMO.try_with(|m| {
        let mut m = m.borrow_mut();
        m.note_valid(key, ok);
        // Validated documents are nearly always parsed next.
        m.note_seen(key);
    });
    ok
}

/// Records freshly serialized output: likely parsed by the next stage, and valid unless
/// `is_valid` (consulted only for large output inside a scope) says `valid` would reject it.
pub(crate) fn note_serialized(bytes: &[u8], is_valid: impl FnOnce() -> bool) {
    if bytes.len() < MIN_LEN || !in_scope() || !is_valid() {
        return;
    }
    let key = digest(bytes);
    let _ = MEMO.try_with(|m| {
        let mut m = m.borrow_mut();
        m.note_valid(key, true);
        m.note_seen(key);
    });
}

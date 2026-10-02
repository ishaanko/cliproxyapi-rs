//! Generic bounded LRU (Go: cache/bounded_lru.go).

use std::collections::HashMap;
use std::hash::Hash;

use parking_lot::Mutex;

type EvictFn<K, V> = Box<dyn Fn(K, V) + Send + Sync>;

struct Node<K, V> {
    key: K,
    value: V,
    prev: Option<usize>,
    next: Option<usize>,
}

/// Slab-backed doubly linked list ordered most recently used first, plus the key index.
struct Inner<K, V> {
    index: HashMap<K, usize>,
    nodes: Vec<Option<Node<K, V>>>,
    free: Vec<usize>,
    head: Option<usize>,
    tail: Option<usize>,
}

impl<K: Eq + Hash + Clone, V> Inner<K, V> {
    fn node(&mut self, slot: usize) -> &mut Node<K, V> {
        self.nodes[slot].as_mut().expect("live LRU slot")
    }

    fn unlink(&mut self, slot: usize) {
        let (prev, next) = {
            let n = self.node(slot);
            (n.prev.take(), n.next.take())
        };
        match prev {
            Some(p) => self.node(p).next = next,
            None => self.head = next,
        }
        match next {
            Some(n) => self.node(n).prev = prev,
            None => self.tail = prev,
        }
    }

    fn push_front(&mut self, slot: usize) {
        let old_head = self.head;
        {
            let n = self.node(slot);
            n.prev = None;
            n.next = old_head;
        }
        if let Some(h) = old_head {
            self.node(h).prev = Some(slot);
        }
        self.head = Some(slot);
        if self.tail.is_none() {
            self.tail = Some(slot);
        }
    }

    fn touch(&mut self, slot: usize) {
        if self.head != Some(slot) {
            self.unlink(slot);
            self.push_front(slot);
        }
    }

    fn remove(&mut self, slot: usize) -> (K, V) {
        self.unlink(slot);
        self.free.push(slot);
        let node = self.nodes[slot].take().expect("live LRU slot");
        self.index.remove(&node.key);
        (node.key, node.value)
    }
}

/// Stores at most `capacity` values and evicts the least recently used one when a new key crosses
/// the bound. The optional eviction callback runs after the cache lock is released.
pub struct BoundedLru<K, V> {
    inner: Mutex<Inner<K, V>>,
    capacity: usize,
    on_evict: Option<EvictFn<K, V>>,
}

impl<K: Eq + Hash + Clone, V: Clone> BoundedLru<K, V> {
    /// `capacity` below 1 is raised to 1.
    pub fn new(capacity: usize, on_evict: Option<EvictFn<K, V>>) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Mutex::new(Inner {
                index: HashMap::with_capacity(capacity),
                nodes: Vec::new(),
                free: Vec::new(),
                head: None,
                tail: None,
            }),
            capacity,
            on_evict,
        }
    }

    /// Returns the cached value or creates and stores one while holding the cache lock. `create`
    /// must not call back into this cache.
    pub fn get_or_add(&self, key: K, create: impl FnOnce() -> V) -> V {
        let mut inner = self.inner.lock();
        if let Some(&slot) = inner.index.get(&key) {
            inner.touch(slot);
            return inner.node(slot).value.clone();
        }

        let value = create();
        let node = Node {
            key: key.clone(),
            value: value.clone(),
            prev: None,
            next: None,
        };
        let slot = match inner.free.pop() {
            Some(slot) => {
                inner.nodes[slot] = Some(node);
                slot
            }
            None => {
                inner.nodes.push(Some(node));
                inner.nodes.len() - 1
            }
        };
        inner.index.insert(key, slot);
        inner.push_front(slot);

        let evicted = if inner.index.len() > self.capacity {
            inner.tail.map(|tail| inner.remove(tail))
        } else {
            None
        };
        drop(inner);

        if let (Some((k, v)), Some(on_evict)) = (evicted, &self.on_evict) {
            on_evict(k, v);
        }
        value
    }

    /// The cached value, marking it most recently used.
    pub fn get(&self, key: &K) -> Option<V> {
        let mut inner = self.inner.lock();
        let slot = *inner.index.get(key)?;
        inner.touch(slot);
        Some(inner.node(slot).value.clone())
    }

    /// Removes the key (running the eviction callback); reports whether it was present.
    pub fn delete(&self, key: &K) -> bool {
        let mut inner = self.inner.lock();
        let Some(&slot) = inner.index.get(key) else {
            return false;
        };
        let (k, v) = inner.remove(slot);
        drop(inner);
        if let Some(on_evict) = &self.on_evict {
            on_evict(k, v);
        }
        true
    }

    pub fn len(&self) -> usize {
        self.inner.lock().index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

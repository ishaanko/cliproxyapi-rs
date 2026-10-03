//! Insertion-ordered map backing `serde_json::Map` (replaces `IndexMap`).
//!
//! JSON objects in proxy traffic are tiny (a handful of keys), so entries live in one `Vec` and
//! lookups scan it; no hash is computed and there is no second allocation. Past
//! [`INDEX_THRESHOLD`] entries a hash index of positions (`HashTable<u32>`) keeps lookups O(1).
//! Keys are client-chosen, so the index uses std's randomly keyed SipHash, like `IndexMap` did;
//! foldhash was tried and disclaims HashDoS resistance, so a sender could aim keys at one bucket.
//!
//! The API mirrors the subset of `indexmap::IndexMap` that `serde_json::Map` uses, including its
//! order semantics: `insert` of an existing key keeps the key's position, `shift_remove`
//! preserves order, `swap_remove` moves the last entry into the hole.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::borrow::Borrow;
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::FusedIterator;
use core::mem;
use core::slice;
use hashbrown::HashTable;
use std::collections::hash_map::RandomState;
use std::sync::LazyLock;

/// Maps with more entries than this get a hash index.
const INDEX_THRESHOLD: usize = 16;

static HASHER: LazyLock<RandomState> = LazyLock::new(RandomState::new);

fn hash_of<Q: Hash + ?Sized>(key: &Q) -> u64 {
    HASHER.hash_one(key)
}

pub struct FlatMap<K, V> {
    entries: Vec<(K, V)>,
    /// Positions into `entries`, present only above [`INDEX_THRESHOLD`].
    index: Option<Box<HashTable<u32>>>,
}

impl<K, V> Default for FlatMap<K, V> {
    fn default() -> Self {
        FlatMap { entries: Vec::new(), index: None }
    }
}

impl<K: Clone, V: Clone> Clone for FlatMap<K, V> {
    fn clone(&self) -> Self {
        FlatMap { entries: self.entries.clone(), index: self.index.clone() }
    }
}

impl<K: Eq + Hash, V> FlatMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let mut map = FlatMap { entries: Vec::with_capacity(capacity), index: None };
        if capacity > INDEX_THRESHOLD {
            map.index = Some(Box::new(HashTable::with_capacity(capacity)));
        }
        map
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.index = None;
    }

    fn find<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        match &self.index {
            Some(ix) => ix
                .find(hash_of(key), |&i| self.entries[i as usize].0.borrow() == key)
                .map(|&i| i as usize),
            None => self.entries.iter().position(|(k, _)| k.borrow() == key),
        }
    }

    /// Rebuilds the index from scratch (or drops it for small maps) after a bulk change.
    fn reindex(&mut self) {
        if self.entries.len() <= INDEX_THRESHOLD {
            self.index = None;
            return;
        }
        let mut table = HashTable::with_capacity(self.entries.len());
        for (i, (k, _)) in self.entries.iter().enumerate() {
            table.insert_unique(hash_of(k), i as u32, |&j| hash_of(&self.entries[j as usize].0));
        }
        self.index = Some(Box::new(table));
    }

    /// Appends a key known to be absent; returns its position.
    fn push_new(&mut self, key: K, value: V) -> usize {
        let i = self.entries.len();
        match &mut self.index {
            Some(ix) => {
                let h = hash_of(&key);
                self.entries.push((key, value));
                let entries = &self.entries;
                ix.insert_unique(h, i as u32, |&j| hash_of(&entries[j as usize].0));
            }
            None => {
                self.entries.push((key, value));
                if self.entries.len() > INDEX_THRESHOLD {
                    self.reindex();
                }
            }
        }
        i
    }

    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        self.find(key).map(|i| &self.entries[i].1)
    }

    pub fn get_key_value<Q>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        self.find(key).map(|i| {
            let (k, v) = &self.entries[i];
            (k, v)
        })
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        self.find(key).is_some()
    }

    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        let i = self.find(key)?;
        Some(&mut self.entries[i].1)
    }

    /// Panics when the key is absent (like `IndexMap`'s `Index`).
    pub fn index<Q>(&self, key: &Q) -> &V
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        self.get(key).expect("no entry found for key")
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        match self.find(&key) {
            Some(i) => Some(mem::replace(&mut self.entries[i].1, value)),
            None => {
                self.push_new(key, value);
                None
            }
        }
    }

    /// Inserts at `index`, moving the key there when it already exists (its value is replaced).
    /// Panics like `IndexMap::shift_insert`: `index` must be below `len` for an existing key and
    /// at most `len` for a new one.
    #[track_caller]
    pub fn shift_insert(&mut self, index: usize, key: K, value: V) -> Option<V> {
        let len = self.entries.len();
        let exists = self.find(&key).is_some();
        assert!(if exists { index < len } else { index <= len }, "index out of bounds: the len is {len} but the index is {index}");
        let old = self.shift_remove(&key);
        self.entries.insert(index, (key, value));
        self.reindex();
        old
    }

    /// Removes the entry at `i`, moving the last entry into its place.
    fn swap_remove_at(&mut self, i: usize) -> (K, V) {
        let last = self.entries.len() - 1;
        if let Some(ix) = &mut self.index {
            if let Ok(e) = ix.find_entry(hash_of(&self.entries[i].0), |&j| j as usize == i) {
                e.remove();
            }
            if i != last {
                if let Some(slot) = ix.find_mut(hash_of(&self.entries[last].0), |&j| j as usize == last) {
                    *slot = i as u32;
                }
            }
        }
        let out = self.entries.swap_remove(i);
        if self.entries.len() <= INDEX_THRESHOLD {
            self.index = None;
        }
        out
    }

    /// Removes the entry at `i`, shifting later entries down.
    fn shift_remove_at(&mut self, i: usize) -> (K, V) {
        if let Some(ix) = &mut self.index {
            if let Ok(e) = ix.find_entry(hash_of(&self.entries[i].0), |&j| j as usize == i) {
                e.remove();
            }
            for j in ix.iter_mut() {
                if *j as usize > i {
                    *j -= 1;
                }
            }
        }
        let out = self.entries.remove(i);
        if self.entries.len() <= INDEX_THRESHOLD {
            self.index = None;
        }
        out
    }

    pub fn swap_remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        self.swap_remove_entry(key).map(|(_, v)| v)
    }

    pub fn swap_remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        let i = self.find(key)?;
        Some(self.swap_remove_at(i))
    }

    pub fn shift_remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        self.shift_remove_entry(key).map(|(_, v)| v)
    }

    pub fn shift_remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: ?Sized + Eq + Hash,
    {
        let i = self.find(key)?;
        Some(self.shift_remove_at(i))
    }

    pub fn entry(&mut self, key: K) -> Entry<'_, K, V> {
        match self.find(&key) {
            Some(idx) => Entry::Occupied(OccupiedEntry { map: self, idx }),
            None => Entry::Vacant(VacantEntry { map: self, key }),
        }
    }

    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter { iter: self.entries.iter() }
    }

    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut { iter: self.entries.iter_mut() }
    }

    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys { iter: self.entries.iter() }
    }

    pub fn values(&self) -> Values<'_, K, V> {
        Values { iter: self.entries.iter() }
    }

    pub fn values_mut(&mut self) -> ValuesMut<'_, K, V> {
        ValuesMut { iter: self.entries.iter_mut() }
    }

    pub fn into_values(self) -> IntoValues<K, V> {
        IntoValues { iter: self.entries.into_iter() }
    }

    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&K, &mut V) -> bool,
    {
        self.entries.retain_mut(|(k, v)| keep(k, v));
        self.reindex();
    }

    pub fn sort_unstable_keys(&mut self)
    where
        K: Ord,
    {
        self.entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        self.reindex();
    }
}

impl<K: Eq + Hash, V> Extend<(K, V)> for FlatMap<K, V> {
    fn extend<T: IntoIterator<Item = (K, V)>>(&mut self, iter: T) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<K: Eq + Hash, V> FromIterator<(K, V)> for FlatMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let iter = iter.into_iter();
        let mut map = FlatMap::with_capacity(iter.size_hint().0);
        map.extend(iter);
        map
    }
}

/// Order-insensitive, like `IndexMap`.
impl<K: Eq + Hash, V: PartialEq> PartialEq for FlatMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k).is_some_and(|o| v == o))
    }
}

impl<K: Eq + Hash, V: Eq> Eq for FlatMap<K, V> {}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for FlatMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.entries.iter().map(|(k, v)| (k, v))).finish()
    }
}

impl<K, V> IntoIterator for FlatMap<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    fn into_iter(self) -> IntoIter<K, V> {
        IntoIter { iter: self.entries.into_iter() }
    }
}

impl<'a, K, V> IntoIterator for &'a FlatMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    fn into_iter(self) -> Iter<'a, K, V> {
        Iter { iter: self.entries.iter() }
    }
}

// ---------------------------------------------------------------- entry API

pub enum Entry<'a, K, V> {
    Occupied(OccupiedEntry<'a, K, V>),
    Vacant(VacantEntry<'a, K, V>),
}

pub struct VacantEntry<'a, K, V> {
    map: &'a mut FlatMap<K, V>,
    key: K,
}

impl<'a, K: Eq + Hash, V> VacantEntry<'a, K, V> {
    pub fn key(&self) -> &K {
        &self.key
    }

    pub fn insert(self, value: V) -> &'a mut V {
        let i = self.map.push_new(self.key, value);
        &mut self.map.entries[i].1
    }
}

pub struct OccupiedEntry<'a, K, V> {
    map: &'a mut FlatMap<K, V>,
    idx: usize,
}

impl<'a, K: Eq + Hash, V> OccupiedEntry<'a, K, V> {
    pub fn key(&self) -> &K {
        &self.map.entries[self.idx].0
    }

    pub fn get(&self) -> &V {
        &self.map.entries[self.idx].1
    }

    pub fn get_mut(&mut self) -> &mut V {
        &mut self.map.entries[self.idx].1
    }

    pub fn into_mut(self) -> &'a mut V {
        &mut self.map.entries[self.idx].1
    }

    pub fn insert(&mut self, value: V) -> V {
        mem::replace(&mut self.map.entries[self.idx].1, value)
    }

    pub fn swap_remove(self) -> V {
        self.map.swap_remove_at(self.idx).1
    }

    pub fn shift_remove(self) -> V {
        self.map.shift_remove_at(self.idx).1
    }

    pub fn swap_remove_entry(self) -> (K, V) {
        self.map.swap_remove_at(self.idx)
    }

    pub fn shift_remove_entry(self) -> (K, V) {
        self.map.shift_remove_at(self.idx)
    }
}

// ---------------------------------------------------------------- iterators

macro_rules! slice_iter {
    ($name:ident<$($lt:lifetime,)? $k:ident, $v:ident>, $inner:ty, $item:ty, |$e:pat_param| $conv:expr) => {
        pub struct $name<$($lt,)? $k, $v> {
            iter: $inner,
        }

        impl<$($lt,)? $k, $v> Iterator for $name<$($lt,)? $k, $v> {
            type Item = $item;
            #[inline]
            fn next(&mut self) -> Option<Self::Item> {
                self.iter.next().map(|$e| $conv)
            }
            #[inline]
            fn size_hint(&self) -> (usize, Option<usize>) {
                self.iter.size_hint()
            }
        }

        impl<$($lt,)? $k, $v> DoubleEndedIterator for $name<$($lt,)? $k, $v> {
            #[inline]
            fn next_back(&mut self) -> Option<Self::Item> {
                self.iter.next_back().map(|$e| $conv)
            }
        }

        impl<$($lt,)? $k, $v> ExactSizeIterator for $name<$($lt,)? $k, $v> {
            #[inline]
            fn len(&self) -> usize {
                self.iter.len()
            }
        }

        impl<$($lt,)? $k, $v> FusedIterator for $name<$($lt,)? $k, $v> {}
    };
}

slice_iter!(Iter<'a, K, V>, slice::Iter<'a, (K, V)>, (&'a K, &'a V), |(k, v)| (k, v));
slice_iter!(IterMut<'a, K, V>, slice::IterMut<'a, (K, V)>, (&'a K, &'a mut V), |(k, v)| (&*k, v));
slice_iter!(IntoIter<K, V>, alloc::vec::IntoIter<(K, V)>, (K, V), |kv| kv);
slice_iter!(Keys<'a, K, V>, slice::Iter<'a, (K, V)>, &'a K, |(k, _)| k);
slice_iter!(Values<'a, K, V>, slice::Iter<'a, (K, V)>, &'a V, |(_, v)| v);
slice_iter!(ValuesMut<'a, K, V>, slice::IterMut<'a, (K, V)>, &'a mut V, |(_, v)| v);
slice_iter!(IntoValues<K, V>, alloc::vec::IntoIter<(K, V)>, V, |(_, v)| v);

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Iter { iter: self.iter.clone() }
    }
}

impl<K, V> Clone for Keys<'_, K, V> {
    fn clone(&self) -> Self {
        Keys { iter: self.iter.clone() }
    }
}

impl<K, V> Clone for Values<'_, K, V> {
    fn clone(&self) -> Self {
        Values { iter: self.iter.clone() }
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for Iter<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone().map(|(k, v)| (k, v))).finish()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for IterMut<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IterMut")
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for IntoIter<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IntoIter")
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for Keys<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone().map(|(k, _)| k)).finish()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for Values<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone().map(|(_, v)| v)).finish()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for ValuesMut<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ValuesMut")
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for IntoValues<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IntoValues")
    }
}

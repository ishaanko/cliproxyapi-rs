//! Randomized comparison of the vendored flat `Map` (hash index above 16 entries)
//! against `indexmap::IndexMap`: same contents, same iteration order after every operation.

use cpa_json::{Map, Value};
use indexmap::IndexMap;

fn snapshot_map(m: &Map<String, Value>) -> Vec<(String, Value)> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn snapshot_model(m: &IndexMap<String, Value>) -> Vec<(String, Value)> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

#[test]
fn flat_map_matches_indexmap() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..300 {
        // Alternate between lazily indexed maps and ones preallocated past the index threshold.
        let (mut map, mut model) = if round % 2 == 0 {
            (Map::new(), IndexMap::new())
        } else {
            let cap = 17 + (round % 40) as usize;
            (Map::with_capacity(cap), IndexMap::<String, Value>::with_capacity(cap))
        };
        let universe = 8 + (round % 50) as u64;
        for step in 0..600u64 {
            let key = format!("key{}", next() % universe);
            let val = Value::from(step);
            match next() % 12 {
                0..=2 => assert_eq!(map.insert(key.clone(), val.clone()), model.insert(key, val)),
                3 => assert_eq!(map.shift_remove(&key), model.shift_remove(&key)),
                4 => assert_eq!(map.swap_remove(&key), model.swap_remove(&key)),
                5 => {
                    let a = map.entry(key.clone()).or_insert(val.clone()).clone();
                    let b = model.entry(key).or_insert(val).clone();
                    assert_eq!(a, b);
                }
                6 => assert_eq!(map.get(&key), model.get(&key)),
                7 => {
                    if let Some(v) = map.get_mut(&key) {
                        *v = Value::from(1u64);
                    }
                    if let Some(v) = model.get_mut(&key) {
                        *v = Value::from(1u64);
                    }
                }
                9 => {
                    // Existing keys move within 0..len, new keys insert within 0..=len.
                    let bound = if model.contains_key(&key) { model.len() } else { model.len() + 1 };
                    let index = (next() % bound as u64) as usize;
                    assert_eq!(map.shift_insert(index, key.clone(), val.clone()), model.shift_insert(index, key, val));
                }
                10 => {
                    if next() % 8 == 0 {
                        map.sort_keys();
                        model.sort_unstable_keys();
                    } else {
                        // A clone is independent of the original and keeps its order and index.
                        let copy = map.clone();
                        map.insert("clone-probe".into(), Value::Null);
                        map.shift_remove("clone-probe");
                        assert_eq!(snapshot_map(&copy), snapshot_map(&map));
                        map = copy;
                    }
                }
                _ => {
                    if next() % 40 == 0 {
                        let cut = next() % 3;
                        map.retain(|k, _| k.len() as u64 % 3 != cut);
                        model.retain(|k, _| k.len() as u64 % 3 != cut);
                    } else {
                        assert_eq!(map.contains_key(&key), model.contains_key(&key));
                    }
                }
            }
            assert_eq!(map.len(), model.len());
            assert_eq!(snapshot_map(&map), snapshot_model(&model), "round {round} step {step}");
            // Every key must still be reachable (index consistency after removals).
            for (k, v) in &model {
                assert_eq!(map.get(k), Some(v), "round {round} step {step} key {k}");
            }
        }
    }
}

/// Out-of-range `shift_insert` panics like `IndexMap`: `len` is allowed for a new key only.
#[test]
fn shift_insert_out_of_range_panics_like_indexmap() {
    let attempt = |index: usize, key: &str| {
        std::panic::catch_unwind(|| {
            let mut map = Map::new();
            map.insert("a".into(), Value::Null);
            map.shift_insert(index, key.into(), Value::Null);
        })
        .is_err()
    };
    let model_attempt = |index: usize, key: &str| {
        std::panic::catch_unwind(|| {
            let mut map = IndexMap::new();
            map.insert("a".to_string(), Value::Null);
            map.shift_insert(index, key.to_string(), Value::Null);
        })
        .is_err()
    };
    for index in 0..4 {
        for key in ["a", "b"] {
            assert_eq!(attempt(index, key), model_attempt(index, key), "index {index} key {key}");
        }
    }
}

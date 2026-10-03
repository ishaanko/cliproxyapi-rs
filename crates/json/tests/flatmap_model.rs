//! Randomized comparison of the vendored flat `Map` (hash index above 16 entries)
//! against `indexmap::IndexMap`: same contents, same iteration order after every operation.



use indexmap::IndexMap;
use cpa_json::{Map, Value};

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
        let mut map = Map::new();
        let mut model: IndexMap<String, Value> = IndexMap::new();
        let universe = 8 + (round % 50) as u64;
        for step in 0..600u64 {
            let key = format!("key{}", next() % universe);
            let val = Value::from(step);
            match next() % 9 {
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

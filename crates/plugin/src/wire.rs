//! serde adapters that reproduce Go `encoding/json` shapes: nil slices and maps encode as
//! `null`, `[]byte` as base64, `time.Time` as RFC 3339 with the zero time for unset.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Values Go would encode as `null` when nil/empty.
pub trait EmptyLike {
    fn is_empty_like(&self) -> bool;
}
impl<T> EmptyLike for Vec<T> {
    fn is_empty_like(&self) -> bool {
        self.is_empty()
    }
}
impl<K, V> EmptyLike for std::collections::BTreeMap<K, V> {
    fn is_empty_like(&self) -> bool {
        self.is_empty()
    }
}
impl EmptyLike for serde_json::Map<String, serde_json::Value> {
    fn is_empty_like(&self) -> bool {
        self.is_empty()
    }
}

/// `#[serde(default, with = "nul")]`: empty serializes as `null`, `null` deserializes as empty.
pub mod nul {
    use super::*;
    pub fn serialize<T: Serialize + EmptyLike, S: Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
        if v.is_empty_like() { s.serialize_none() } else { v.serialize(s) }
    }
    pub fn deserialize<'de, T: Deserialize<'de> + Default, D: Deserializer<'de>>(d: D) -> Result<T, D::Error> {
        Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
    }
}

/// `[]byte`: base64 string, `null` when empty.
pub mod b64 {
    use super::*;
    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        if v.is_empty() { s.serialize_none() } else { s.serialize_str(&STANDARD.encode(v)) }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        match Option::<String>::deserialize(d)? {
            None => Ok(Vec::new()),
            Some(s) => STANDARD.decode(s.as_bytes()).map_err(serde::de::Error::custom),
        }
    }
}

/// `[][]byte` history chunks: array of base64 strings, `null` when empty.
pub mod b64_list {
    use super::*;
    pub fn serialize<S: Serializer>(v: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
        if v.is_empty() {
            return s.serialize_none();
        }
        let items: Vec<String> = v.iter().map(|b| STANDARD.encode(b)).collect();
        items.serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<u8>>, D::Error> {
        let items = Option::<Vec<String>>::deserialize(d)?.unwrap_or_default();
        items.into_iter().map(|s| STANDARD.decode(s.as_bytes()).map_err(serde::de::Error::custom)).collect()
    }
}

/// Raw JSON value kept verbatim (Go `json.RawMessage`).
pub type Raw = Option<Box<serde_json::value::RawValue>>;

/// `time.Time`: unset is `0001-01-01T00:00:00Z`.
pub mod gotime {
    pub use cpa_auth::types::go_time::{deserialize, serialize};
}

pub fn is_false(v: &bool) -> bool {
    !*v
}
pub fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
pub fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}
pub fn is_empty_str(v: &str) -> bool {
    v.is_empty()
}

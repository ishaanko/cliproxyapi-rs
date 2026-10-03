//! Case-insensitive struct decoding. Go's `encoding/json` matches object keys to struct fields
//! ignoring case, and plugins (hand-written C or Rust ones in particular) rely on it, so the host
//! decodes everything a plugin sends through [`from_value`] / [`from_slice`]: object keys are
//! rewritten to the declared field spelling when they differ only in case.

use serde::de::value::{MapDeserializer, SeqDeserializer};
use serde::de::{self, DeserializeOwned, IntoDeserializer, Visitor};
use serde::forward_to_deserialize_any;
use serde::Deserializer as _;
use serde_json::{Map, Value};

/// Decodes `value` into `T` with Go's key matching.
pub fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, serde_json::Error> {
    T::deserialize(Fold(value))
}

/// Parses `raw` and decodes it into `T` with Go's key matching.
pub fn from_slice<T: DeserializeOwned>(raw: &[u8]) -> Result<T, serde_json::Error> {
    from_value(serde_json::from_slice::<Value>(raw)?)
}

/// A JSON value that deserializes structs with case-folded keys.
struct Fold(Value);

impl<'de> IntoDeserializer<'de, serde_json::Error> for Fold {
    type Deserializer = Fold;
    fn into_deserializer(self) -> Fold {
        self
    }
}

/// Renames the keys of `map` that match one of `fields` ignoring case; later duplicates win.
fn fold_keys(map: Map<String, Value>, fields: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in map {
        let target = if fields.contains(&key.as_str()) {
            key
        } else {
            let lower = key.to_lowercase();
            fields.iter().find(|f| f.to_lowercase() == lower).map(|f| (*f).to_string()).unwrap_or(key)
        };
        out.insert(target, value);
    }
    out
}

impl Fold {
    fn visit<'de, V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, serde_json::Error> {
        match self.0 {
            Value::Object(map) => visitor.visit_map(MapDeserializer::new(map.into_iter().map(|(k, v)| (k, Fold(v))))),
            Value::Array(items) => visitor.visit_seq(SeqDeserializer::new(items.into_iter().map(Fold))),
            other => other.deserialize_any(visitor),
        }
    }
}

macro_rules! delegate_scalars {
    ($($method:ident),*) => {
        $(fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            match self.0 {
                Value::Object(_) | Value::Array(_) => self.visit(visitor),
                other => other.$method(visitor),
            }
        })*
    };
}

impl<'de> de::Deserializer<'de> for Fold {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.visit(visitor)
    }

    delegate_scalars!(
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string
    );

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, name: &'static str, visitor: V) -> Result<V::Value, Self::Error> {
        // `Box<RawValue>` asks for the raw JSON text of the value.
        if name == "$serde_json::private::RawValue" {
            let text = serde_json::to_string(&self.0)?;
            return visitor.visit_map(MapDeserializer::new(std::iter::once((name, text))));
        }
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Object(map) => Fold(Value::Object(fold_keys(map, fields))).visit(visitor),
            other => other.deserialize_struct(name, fields, visitor),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_enum(name, variants, visitor)
    }

    forward_to_deserialize_any! {
        bytes byte_buf unit unit_struct seq tuple tuple_struct map identifier ignored_any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(default, rename_all = "PascalCase")]
    struct Inner {
        file_name: String,
        #[serde(rename = "URL")]
        url: String,
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(default, rename_all = "PascalCase")]
    struct Outer {
        resources: Vec<Inner>,
        #[serde(rename = "GitHubRepository")]
        repo: Option<String>,
        count: i64,
    }

    #[test]
    fn keys_match_ignoring_case_at_every_level() {
        let v: Outer = from_slice(br#"{"resources":[{"fileName":"a","url":"u"}],"githubrepository":"r","COUNT":3}"#).unwrap();
        assert_eq!(v, Outer { resources: vec![Inner { file_name: "a".into(), url: "u".into() }], repo: Some("r".into()), count: 3 });
    }

    #[test]
    fn raw_values_keep_their_json() {
        #[derive(Deserialize)]
        struct Holder {
            json: Option<Box<serde_json::value::RawValue>>,
        }
        let h: Holder = from_slice(br#"{"JSON":{"a":[1,2]}}"#).unwrap();
        assert_eq!(h.json.unwrap().get(), r#"{"a":[1,2]}"#);
        let h: Holder = from_slice(br#"{"json":null}"#).unwrap();
        assert!(h.json.is_none());
    }

    #[test]
    fn exact_keys_and_nulls_still_decode() {
        let v: Outer = from_slice(br#"{"GitHubRepository":null,"Count":2}"#).unwrap();
        assert_eq!(v, Outer { resources: vec![], repo: None, count: 2 });
    }
}

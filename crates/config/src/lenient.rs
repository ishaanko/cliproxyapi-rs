//! A `Value` deserializer with yaml.v3's scalar coercions.
//!
//! The Go loader decodes with yaml.v3, which is more forgiving than serde's strict typing when
//! the target type is known:
//! - a typed bool accepts the YAML 1.1 spellings `y/yes/on` and `n/no/off` (any of the three
//!   casings yaml.v3 lists);
//! - an integer field accepts a float and truncates it (`port: 8317.0`);
//! - a string field accepts any scalar and keeps its text (`api-keys: [123456]`, `prefix: 7`).
//!
//! serde drives the target type through `deserialize_bool` / `deserialize_i64` /
//! `deserialize_string`, so the coercions live in those methods; everything else behaves like
//! the plain value deserializer.

use serde::de::{self, DeserializeOwned, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_yaml_ng::{Error, Value};

/// Decodes `value` into `T` with yaml.v3-style scalar coercions.
pub(crate) fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, Error> {
    T::deserialize(Lenient(value))
}

/// Deserializer over an owned [`Value`] (see the module docs).
pub(crate) struct Lenient(pub Value);

/// yaml.v3's YAML 1.1 bool spellings, accepted only for typed bool fields.
fn yaml11_bool(s: &str) -> Option<bool> {
    match s {
        "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Some(true),
        "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Some(false),
        _ => None,
    }
}

impl<'de> Deserializer<'de> for Lenient {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            Value::Bool(b) => visitor.visit_bool(b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    visitor.visit_i64(i)
                } else if let Some(u) = n.as_u64() {
                    visitor.visit_u64(u)
                } else {
                    visitor.visit_f64(n.as_f64().unwrap_or_default())
                }
            }
            Value::String(s) => visitor.visit_string(s),
            Value::Sequence(items) => visitor.visit_seq(SeqDe(items.into_iter())),
            Value::Mapping(map) => visitor.visit_map(MapDe { iter: map.into_iter(), value: None }),
            Value::Tagged(tagged) => Lenient(tagged.value).deserialize_any(visitor),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::String(s) => match yaml11_bool(&s) {
                Some(b) => visitor.visit_bool(b),
                None => visitor.visit_string(s),
            },
            other => Lenient(other).deserialize_any(visitor),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_string(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Number(n) => visitor.visit_string(n.to_string()),
            Value::Bool(b) => visitor.visit_string(b.to_string()),
            other => Lenient(other).deserialize_any(visitor),
        }
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_int(visitor)
    }
    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_int(visitor)
    }
    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_int(visitor)
    }
    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_int(visitor)
    }
    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_int(visitor)
    }
    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_int(visitor)
    }
    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_int(visitor)
    }
    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_int(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            other => visitor.visit_some(Lenient(other)),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    // A null where a collection or struct is expected leaves it empty/defaulted, as in yaml.v3.
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Null => Lenient(Value::Mapping(serde_yaml_ng::Mapping::new())).deserialize_any(visitor),
            other => Lenient(other).deserialize_any(visitor),
        }
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Null => Lenient(Value::Sequence(Vec::new())).deserialize_any(visitor),
            other => Lenient(other).deserialize_any(visitor),
        }
    }

    serde::forward_to_deserialize_any! {
        i128 u128 f32 f64 char bytes byte_buf unit unit_struct tuple tuple_struct enum identifier ignored_any
    }
}

impl Lenient {
    /// Integer fields take integral numbers; a float is truncated toward zero when it fits.
    fn deserialize_int<'de, V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Number(n) if n.as_i64().is_none() && n.as_u64().is_none() => {
                let f = n.as_f64().unwrap_or(f64::NAN);
                if f.is_finite() && f.abs() < 9.2e18 {
                    visitor.visit_i64(f as i64)
                } else {
                    Err(<Error as de::Error>::custom(format!("number {f} does not fit an integer")))
                }
            }
            other => Lenient(other).deserialize_any(visitor),
        }
    }
}

struct SeqDe(std::vec::IntoIter<Value>);

impl<'de> SeqAccess<'de> for SeqDe {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, Error> {
        self.0.next().map(|v| seed.deserialize(Lenient(v))).transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

struct MapDe {
    iter: serde_yaml_ng::mapping::IntoIter,
    value: Option<Value>,
}

impl<'de> MapAccess<'de> for MapDe {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, Error> {
        match self.iter.next() {
            Some((key, value)) => {
                self.value = Some(value);
                seed.deserialize(Lenient(key)).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let value = self.value.take().ok_or_else(|| <Error as de::Error>::custom("map value requested before its key"))?;
        seed.deserialize(Lenient(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

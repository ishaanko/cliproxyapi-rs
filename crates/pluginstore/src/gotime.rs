//! JSON time encoding that matches Go's `time.Time` (RFC 3339 with trimmed fractional
//! seconds, zero value `0001-01-01T00:00:00Z`). `None` stands for the Go zero time.

use chrono::{DateTime, Datelike, SecondsFormat, Timelike, Utc};
use serde::{Deserialize, Deserializer, Serializer};

const ZERO: &str = "0001-01-01T00:00:00Z";

/// Formats like Go's `time.Time.MarshalJSON` (RFC3339Nano).
pub fn format_rfc3339_nano(time: &DateTime<Utc>) -> String {
    let base = time.to_rfc3339_opts(SecondsFormat::Nanos, true);
    match base.strip_suffix('Z') {
        Some(body) if body.contains('.') => {
            let trimmed = body.trim_end_matches('0').trim_end_matches('.');
            format!("{trimmed}Z")
        }
        _ => base,
    }
}

/// Go `Time.IsZero` on an optional timestamp.
pub fn is_zero(time: &Option<DateTime<Utc>>) -> bool {
    match time {
        None => true,
        Some(t) => {
            t.year() == 1 && t.ordinal() == 1 && t.num_seconds_from_midnight() == 0 && t.nanosecond() == 0
        }
    }
}

pub fn serialize<S: Serializer>(
    value: &Option<DateTime<Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(time) => serializer.serialize_str(&format_rfc3339_nano(time)),
        None => serializer.serialize_str(ZERO),
    }
}

pub fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error> {
    let Some(raw) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let parsed = DateTime::parse_from_rfc3339(&raw).map_err(serde::de::Error::custom)?;
    let utc = parsed.with_timezone(&Utc);
    Ok(if is_zero(&Some(utc)) { None } else { Some(utc) })
}

/// Same encoding for timestamps that are always set.
pub mod required {
    use super::*;

    pub fn serialize<S: Serializer>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_rfc3339_nano(value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<DateTime<Utc>, D::Error> {
        let raw = String::deserialize(deserializer)?;
        DateTime::parse_from_rfc3339(&raw)
            .map(|t| t.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
}

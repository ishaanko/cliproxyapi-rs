//! Go `time.Duration` text handling (`time.ParseDuration` / `Duration.String`).
//!
//! The config schema stores some durations as Go-style strings ("3s", "250ms", "1m30s") and the
//! reference implementation both parses and re-emits them, so the exact syntax matters.

use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

/// A signed duration in nanoseconds, matching Go's `time.Duration` (int64).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct GoDuration(pub i64);

impl GoDuration {
    pub const MICROSECOND: i64 = 1_000;
    pub const MILLISECOND: i64 = 1_000_000;
    pub const SECOND: i64 = 1_000_000_000;

    pub const fn from_millis(ms: i64) -> Self {
        Self(ms * Self::MILLISECOND)
    }

    pub const fn from_secs(s: i64) -> Self {
        Self(s * Self::SECOND)
    }

    /// Converts to a std duration; negative values clamp to zero.
    pub fn to_std(self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.0.max(0) as u64)
    }

    /// Parses a Go duration string such as "300ms", "-1.5h" or "2h45m".
    pub fn parse(input: &str) -> Result<Self, DurationParseError> {
        parse_duration(input).map(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("time: invalid duration {0:?}")]
pub struct DurationParseError(pub String);

fn unit_nanos(unit: &str) -> Option<u64> {
    Some(match unit {
        "ns" => 1,
        "us" | "\u{b5}s" | "\u{3bc}s" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60 * 1_000_000_000,
        "h" => 3600 * 1_000_000_000,
        _ => return None,
    })
}

/// Port of Go's `time.ParseDuration`.
fn parse_duration(orig: &str) -> Result<i64, DurationParseError> {
    let err = || DurationParseError(orig.to_string());
    let mut s = orig;
    let mut neg = false;
    if let Some(rest) = s.strip_prefix('-') {
        neg = true;
        s = rest;
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest;
    }
    if s == "0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(err());
    }
    const LIMIT: u64 = 1u64 << 63;
    let mut total: u64 = 0;
    while !s.is_empty() {
        // Integer part.
        let int_len = s.bytes().take_while(u8::is_ascii_digit).count();
        let (int_str, rest) = s.split_at(int_len);
        s = rest;
        let pre = int_len > 0;
        let mut int_val: u64 = 0;
        for b in int_str.bytes() {
            int_val = int_val
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(b - b'0')))
                .ok_or_else(err)?;
            if int_val > LIMIT {
                return Err(err());
            }
        }
        // Optional fraction (digits beyond u64 precision are ignored, as in Go).
        let mut frac_val: u64 = 0;
        let mut scale: f64 = 1.0;
        let mut post = false;
        if let Some(rest) = s.strip_prefix('.') {
            let frac_len = rest.bytes().take_while(u8::is_ascii_digit).count();
            let (frac_str, rest) = rest.split_at(frac_len);
            s = rest;
            post = frac_len > 0;
            let mut overflow = false;
            for b in frac_str.bytes() {
                if overflow {
                    continue;
                }
                match frac_val
                    .checked_mul(10)
                    .and_then(|v| v.checked_add(u64::from(b - b'0')))
                {
                    Some(v) if v <= LIMIT => {
                        frac_val = v;
                        scale *= 10.0;
                    }
                    _ => overflow = true,
                }
            }
        }
        if !pre && !post {
            return Err(err());
        }
        // Unit.
        let unit_len = s
            .char_indices()
            .find(|(_, c)| *c == '.' || c.is_ascii_digit())
            .map_or(s.len(), |(i, _)| i);
        if unit_len == 0 {
            return Err(err());
        }
        let (unit, rest) = s.split_at(unit_len);
        s = rest;
        let unit = unit_nanos(unit).ok_or_else(err)?;
        if int_val > LIMIT / unit {
            return Err(err());
        }
        let mut v = int_val * unit;
        if frac_val > 0 {
            v += (frac_val as f64 * (unit as f64 / scale)) as u64;
            if v > LIMIT {
                return Err(err());
            }
        }
        total = total.checked_add(v).ok_or_else(err)?;
        if total > LIMIT {
            return Err(err());
        }
    }
    if neg {
        Ok((-(total as i128)) as i64)
    } else if total > i64::MAX as u64 {
        Err(err())
    } else {
        Ok(total as i64)
    }
}

/// Port of Go's `Duration.String`: "0s", "1.5s", "2m0s", "1h30m0s", "250ms".
impl fmt::Display for GoDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.0;
        if d == 0 {
            return f.write_str("0s");
        }
        let mut u = d.unsigned_abs();
        let mut out;
        if u < GoDuration::SECOND as u64 {
            // Sub-second values use ns, us or ms with a fractional part.
            let (unit, prec) = if u < GoDuration::MICROSECOND as u64 {
                ("ns", 0)
            } else if u < GoDuration::MILLISECOND as u64 {
                ("\u{b5}s", 3)
            } else {
                ("ms", 6)
            };
            let frac = take_frac(&mut u, prec);
            out = format!("{u}{frac}{unit}");
        } else {
            let frac = take_frac(&mut u, 9);
            // `u` now holds whole seconds.
            out = format!("{}{frac}s", u % 60);
            u /= 60;
            if u > 0 {
                out = format!("{}m{out}", u % 60);
                u /= 60;
                if u > 0 {
                    out = format!("{u}h{out}");
                }
            }
        }
        if d < 0 {
            out.insert(0, '-');
        }
        f.write_str(&out)
    }
}

/// Splits `v` into `v / 10^prec` (left in `v`) and the fractional digits (returned as ".ddd" with
/// trailing zeros removed, or empty when zero).
fn take_frac(v: &mut u64, prec: u32) -> String {
    let div = 10u64.pow(prec);
    let frac = *v % div;
    *v /= div;
    if frac == 0 {
        return String::new();
    }
    let digits = format!("{:0width$}", frac, width = prec as usize);
    format!(".{}", digits.trim_end_matches('0'))
}

impl Serialize for GoDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Only strings are accepted, as with yaml.v3 decoding into `time.Duration`: a bare `0` is an
/// integer scalar and is rejected, while `"0"` parses.
impl<'de> Deserialize<'de> for GoDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DurationVisitor;
        impl de::Visitor<'_> for DurationVisitor {
            type Value = GoDuration;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration string such as \"3s\"")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<GoDuration, E> {
                GoDuration::parse(v).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(DurationVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_match_go() {
        for (text, nanos) in [
            ("0", 0),
            ("3s", 3_000_000_000),
            ("250ms", 250_000_000),
            ("1m30s", 90_000_000_000),
            ("1.5h", 5_400_000_000_000),
            ("-2s", -2_000_000_000),
            ("1h2m3s4ms5us6ns", 3_723_004_005_006),
        ] {
            assert_eq!(GoDuration::parse(text).unwrap().0, nanos, "{text}");
        }
        for bad in ["", "s", "1", "1x", "--1s", "1.s.s"] {
            assert!(GoDuration::parse(bad).is_err(), "{bad}");
        }
        for (nanos, text) in [
            (0, "0s"),
            (3_000_000_000, "3s"),
            (250_000_000, "250ms"),
            (90_000_000_000, "1m30s"),
            (7_200_000_000_000, "2h0m0s"),
            (1_500_000_000, "1.5s"),
            (1_500, "1.5\u{b5}s"),
            (-2_000_000_000, "-2s"),
        ] {
            assert_eq!(GoDuration(nanos).to_string(), text);
        }
    }
}

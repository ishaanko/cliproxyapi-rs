//! Go `encoding/json` compatible serialization helpers.
//!
//! Several Go helpers pin behavior to what `json.Marshal` emits (HTML escaping, sorted map keys,
//! float64 numbers). These writers reproduce it where the output feeds hashes or wire bytes.

use serde_json::Value;

/// How a [`Value`] is rendered, matching a particular Go encoder configuration.
#[derive(Clone, Copy)]
pub struct GoJsonStyle {
    /// `json.Marshal` default escapes `<`, `>`, `&` as `<`, `>`, `&`.
    pub html_escape: bool,
    /// Numbers decoded into `float64` (no `UseNumber`): re-formatted like Go's float encoder.
    pub float_numbers: bool,
}

impl GoJsonStyle {
    /// `json.Marshal(v)` where `v` came from `json.Unmarshal` into `any` (float64 numbers).
    pub const MARSHAL_ANY: GoJsonStyle = GoJsonStyle {
        html_escape: true,
        float_numbers: true,
    };
    /// `json.Marshal` of a value decoded with `UseNumber` (raw number text kept).
    pub const MARSHAL_USE_NUMBER: GoJsonStyle = GoJsonStyle {
        html_escape: true,
        float_numbers: false,
    };
    /// Encoder with `SetEscapeHTML(false)` over a `UseNumber` value.
    pub const NO_HTML_ESCAPE: GoJsonStyle = GoJsonStyle {
        html_escape: false,
        float_numbers: false,
    };
}

/// Compact JSON with object keys sorted bytewise at every level, as Go emits `map[string]any`.
/// Returns `None` when `float_numbers` is set and a number overflows float64 (Go's unmarshal
/// would have failed).
pub fn go_json_sorted(v: &Value, style: GoJsonStyle) -> Option<String> {
    let mut out = String::new();
    write_value(v, style, &mut out)?;
    Some(out)
}

/// `json.Marshal(string)`: quoted, HTML-escaped (`<`, `>`, `&`), U+2028/2029 escaped.
pub fn go_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_string(s, GoJsonStyle::MARSHAL_USE_NUMBER, &mut out);
    out
}

/// Canonical re-encoding of arbitrary JSON text the way Go's `json.Unmarshal` into `any` followed
/// by `json.Marshal` would produce it (sorted keys, float64 numbers, HTML escaping). `None` when
/// the text is not a single valid JSON value.
pub fn go_json_canonicalize(raw: &str) -> Option<String> {
    if !cpa_json::valid(raw.as_bytes()) {
        return None;
    }
    let v: Value = cpa_json::parse_str(raw);
    go_json_sorted(&v, GoJsonStyle::MARSHAL_ANY)
}

fn write_value(v: &Value, style: GoJsonStyle, out: &mut String) -> Option<()> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if style.float_numbers {
                out.push_str(&go_format_float(
                    n.to_string()
                        .parse::<f64>()
                        .ok()
                        .filter(|f| f.is_finite())?,
                ));
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => write_string(s, style, out),
        Value::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, style, out)?;
            }
            out.push(']');
        }
        Value::Object(m) => {
            let mut entries: Vec<(&String, &Value)> = m.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (k, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, style, out);
                out.push(':');
                write_value(item, style, out)?;
            }
            out.push('}');
        }
    }
    Some(())
}

fn write_string(s: &str, style: GoJsonStyle, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            '<' | '>' | '&' if style.html_escape => out.push_str(&format!("\\u{:04x}", c as u32)),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Go's JSON float64 encoding: shortest repr, `e` notation outside `[1e-6, 1e21)` with the
/// exponent cleaned (`e-07` -> `e-7`, `e+21`).
fn go_format_float(f: f64) -> String {
    let abs = f.abs();
    if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
        let s = format!("{f:e}");
        return match s.split_once('e') {
            Some((mantissa, exp)) if !exp.starts_with('-') => format!("{mantissa}e+{exp}"),
            _ => s,
        };
    }
    if f == 0.0 && f.is_sign_negative() {
        return "-0".into();
    }
    format!("{f}")
}

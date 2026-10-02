//! yaml.v3 wording for type errors found while decoding a config document.
//!
//! The typed decoder reports serde errors such as `invalid type: string "many", expected i64`.
//! yaml.v3 says `yaml: unmarshal errors:\n  line 6: cannot unmarshal !!str `many` into int`, and
//! management responses echo that text. The decode failure is replayed with
//! `serde_path_to_error` to learn which key failed, and that key is located in the source text to
//! recover the line number.

use serde_yaml_ng::Value;

use crate::lenient::Lenient;
use crate::types::Config;

/// What serde said about a scalar of the wrong type.
struct Mismatch {
    /// yaml.v3 tag of the value found (`!!str`, `!!int`, ...).
    tag: &'static str,
    /// The value, for scalars.
    scalar: Option<String>,
    /// Go type of the field.
    go_type: &'static str,
}

fn parse_mismatch(message: &str) -> Option<Mismatch> {
    let rest = message.strip_prefix("invalid type: ")?;
    let (found, expected) = rest.split_once(", expected ")?;
    let (tag, scalar) = if let Some(s) = found.strip_prefix("string ") {
        ("!!str", Some(s.trim_matches('"').to_string()))
    } else if let Some(i) = found.strip_prefix("integer `") {
        ("!!int", Some(i.trim_end_matches('`').to_string()))
    } else if let Some(f) = found.strip_prefix("floating point `") {
        ("!!float", Some(f.trim_end_matches('`').to_string()))
    } else if let Some(b) = found.strip_prefix("boolean `") {
        ("!!bool", Some(b.trim_end_matches('`').to_string()))
    } else if found.starts_with("map") {
        ("!!map", None)
    } else if found.starts_with("sequence") {
        ("!!seq", None)
    } else {
        return None;
    };
    let go_type = match expected.split_whitespace().next()? {
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize" => "int",
        "f32" | "f64" => "float64",
        "bool" => "bool",
        "a" if expected.starts_with("a string") => "string",
        "a" if expected.starts_with("a sequence") => "[]interface {}",
        "a" if expected.starts_with("a map") => "map[string]interface {}",
        "string" => "string",
        _ => return None,
    };
    Some(Mismatch {
        tag,
        scalar,
        go_type,
    })
}

/// 1-based line of the `key:` entry holding `scalar` (or just the first `key:` entry).
fn find_line(text: &str, key: &str, scalar: Option<&str>) -> Option<usize> {
    let mut first = None;
    for (i, line) in text.lines().enumerate() {
        let body = line.trim_start().trim_start_matches("- ").trim_start();
        let Some(value) = body
            .strip_prefix(key)
            .and_then(|r| r.strip_prefix(':'))
            .map(|r| r.trim())
        else {
            continue;
        };
        first.get_or_insert(i + 1);
        let value = value
            .split(" #")
            .next()
            .unwrap_or(value)
            .trim()
            .trim_matches(['"', '\'']);
        if scalar.is_none_or(|s| s == value) {
            return Some(i + 1);
        }
    }
    first
}

/// The yaml.v3 message for a failed decode of `flat` (the legacy-layout view of the document
/// parsed from `text`), or `None` when the failure is not a plain type mismatch.
pub(crate) fn unmarshal_message(flat: &Value, text: &str) -> Option<String> {
    let err = serde_path_to_error::deserialize::<_, Config>(Lenient(flat.clone())).err()?;
    let mismatch = parse_mismatch(&err.inner().to_string())?;
    let path = err.path().to_string();
    let key = path
        .rsplit('.')
        .next()?
        .split('[')
        .next()
        .filter(|k| !k.is_empty())?;
    let line = find_line(text, key, mismatch.scalar.as_deref())?;
    let shown = match &mismatch.scalar {
        Some(v) if v.chars().count() > 10 => {
            let head: String = v.chars().take(7).collect();
            format!(" `{head}...`")
        }
        Some(v) => format!(" `{v}`"),
        None => String::new(),
    };
    Some(format!(
        "yaml: unmarshal errors:\n  line {line}: cannot unmarshal {}{shown} into {}",
        mismatch.tag, mismatch.go_type
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_mismatch_reads_like_yaml_v3() {
        let text = "port: 1\nrequest-retry: many\n";
        let flat: Value = serde_yaml_ng::from_str(text).unwrap();
        assert_eq!(
            unmarshal_message(&flat, text).as_deref(),
            Some("yaml: unmarshal errors:\n  line 2: cannot unmarshal !!str `many` into int")
        );
    }
}

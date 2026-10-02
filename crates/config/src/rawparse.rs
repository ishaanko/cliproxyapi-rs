//! YAML text -> `serde_yaml_ng::Value` that keeps the original text of scalars.
//!
//! serde YAML resolves `True`, `1.50`, `12e4` or `0o7` to a bool/number and forgets how they were
//! written. yaml.v3 decodes such a scalar into a *string* field using its source text, which
//! matters for secrets (`api-keys: [0o7]`). This parser resolves plain scalars with yaml.v3's
//! rules (including `0x`/`0o`/`0b`/leading-zero octal and `_` in integers) and, when the resolved
//! value would not print back as the source text, wraps it in a `cpa-raw` tagged value that holds
//! the resolved scalar and the original text. [`resolved`], [`raw_text`] and [`untag_raw`] see
//! through that wrapper; the typed decoder uses the raw text for string fields.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;

use serde_yaml_ng::value::{Tag, TaggedValue};
use serde_yaml_ng::{Mapping, Number, Value};
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};

const RAW_TAG: &str = "cpa-raw";

/// A scalar plus its original text, only built when the two differ.
fn wrap_raw(resolved: Value, raw: &str) -> Value {
    Value::Tagged(Box::new(TaggedValue {
        tag: Tag::new(RAW_TAG),
        value: Value::Sequence(vec![resolved, Value::String(raw.to_string())]),
    }))
}

fn as_raw_pair(value: &Value) -> Option<(&Value, &str)> {
    let Value::Tagged(tagged) = value else {
        return None;
    };
    if tagged.tag != RAW_TAG {
        return None;
    }
    match &tagged.value {
        Value::Sequence(items) => match items.as_slice() {
            [resolved, Value::String(raw)] => Some((resolved, raw)),
            _ => None,
        },
        _ => None,
    }
}

/// The scalar a value stands for, looking through the raw-text wrapper.
pub(crate) fn resolved(value: &Value) -> &Value {
    as_raw_pair(value).map_or(value, |(resolved, _)| resolved)
}

/// The source text of a scalar that was wrapped by the parser.
pub(crate) fn raw_text(value: &Value) -> Option<&str> {
    as_raw_pair(value).map(|(_, raw)| raw)
}

/// Replaces every raw-text wrapper by the scalar it resolved to (needed before serialising).
pub(crate) fn untag_raw(value: &mut Value) {
    if let Some((resolved, _)) = as_raw_pair(value) {
        *value = resolved.clone();
        return;
    }
    match value {
        Value::Mapping(map) => {
            let keys: Vec<Value> = map.keys().cloned().collect();
            for key in keys {
                if let Some(child) = map.get_mut(&key) {
                    untag_raw(child);
                }
            }
        }
        Value::Sequence(items) => items.iter_mut().for_each(untag_raw),
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// yaml.v3 plain scalar resolution
// ---------------------------------------------------------------------------------------------

/// Go `strconv.ParseInt(s, 0, 64)` (sign, `0x`/`0o`/`0b` prefixes, leading-zero octal).
fn parse_go_int(s: &str) -> Option<i64> {
    let (negative, body) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let magnitude = parse_go_uint(body)?;
    if negative {
        0i64.checked_sub_unsigned(magnitude)
    } else {
        i64::try_from(magnitude).ok()
    }
}

/// Go `strconv.ParseUint(s, 0, 64)` for unsigned text.
fn parse_go_uint(s: &str) -> Option<u64> {
    let (radix, digits) = match s.as_bytes() {
        [b'0', b'x' | b'X', ..] => (16, &s[2..]),
        [b'0', b'o' | b'O', ..] => (8, &s[2..]),
        [b'0', b'b' | b'B', ..] => (2, &s[2..]),
        [b'0', _, ..] => (8, &s[1..]),
        _ => (10, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| (b as char).is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

/// yaml.v3's float syntax (`yamlStyleFloat`).
fn is_yaml_float(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    if let Some(exp) = exponent {
        let exp = exp.strip_prefix(['-', '+']).unwrap_or(exp);
        if exp.is_empty() || !exp.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    let all_digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
    match mantissa.split_once('.') {
        // `.5` needs digits after the dot; `5.` and `5.5` need digits before it.
        Some(("", frac)) => !frac.is_empty() && all_digits(frac),
        Some((int, frac)) => !int.is_empty() && all_digits(int) && all_digits(frac),
        None => !mantissa.is_empty() && all_digits(mantissa),
    }
}

pub(crate) fn resolve_plain(raw: &str) -> Value {
    match raw {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "true" | "True" | "TRUE" => return lossy_or(Value::Bool(true), raw),
        "false" | "False" | "FALSE" => return lossy_or(Value::Bool(false), raw),
        ".nan" | ".NaN" | ".NAN" => return lossy_or(Value::Number(Number::from(f64::NAN)), raw),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            return lossy_or(Value::Number(Number::from(f64::INFINITY)), raw);
        }
        "-.inf" | "-.Inf" | "-.INF" => {
            return lossy_or(Value::Number(Number::from(f64::NEG_INFINITY)), raw);
        }
        _ => {}
    }
    if !raw.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '+' | '-' | '.')) {
        return Value::String(raw.to_string());
    }
    let plain = raw.replace('_', "");
    if let Some(i) = parse_go_int(&plain) {
        return lossy_or(Value::Number(Number::from(i)), raw);
    }
    if let Some(u) = parse_go_uint(&plain) {
        return lossy_or(Value::Number(Number::from(u)), raw);
    }
    if is_yaml_float(&plain)
        && let Ok(f) = plain.parse::<f64>()
    {
        return lossy_or(Value::Number(Number::from(f)), raw);
    }
    Value::String(raw.to_string())
}

/// Keeps `value` as is when it prints back as `raw`, otherwise remembers the source text.
fn lossy_or(value: Value, raw: &str) -> Value {
    let canonical = match &value {
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        _ => return value,
    };
    if canonical == raw {
        value
    } else {
        wrap_raw(value, raw)
    }
}

// ---------------------------------------------------------------------------------------------
// Event -> Value builder
// ---------------------------------------------------------------------------------------------

enum Frame {
    Seq {
        anchor: usize,
        items: Vec<Value>,
    },
    Map {
        anchor: usize,
        map: Mapping,
        key: Option<Value>,
    },
}

#[derive(Default)]
struct Builder {
    stack: Vec<Frame>,
    anchors: HashMap<usize, Value>,
    root: Option<Value>,
    error: Option<String>,
}

impl Builder {
    fn push_value(&mut self, value: Value, anchor: usize) {
        if anchor != 0 {
            self.anchors.insert(anchor, value.clone());
        }
        match self.stack.last_mut() {
            None => self.root = Some(value),
            Some(Frame::Seq { items, .. }) => items.push(value),
            Some(Frame::Map { map, key, .. }) => match key.take() {
                None => {
                    if value.is_null() {
                        self.error
                            .get_or_insert_with(|| "empty mapping key".to_string());
                    }
                    // Keys use the resolved scalar (raw text is only kept for values).
                    *key = Some(resolved(&value).clone());
                }
                Some(k) => {
                    if map.contains_key(&k) {
                        self.error
                            .get_or_insert_with(|| "mapping key already defined".to_string());
                    }
                    map.insert(k, value);
                }
            },
        }
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, event: Event, mark: Marker) {
        match event {
            Event::Scalar(text, style, anchor, tag) => {
                let forced_str = tag.as_ref().is_some_and(|t| t.suffix == "str");
                let value = if forced_str || style != TScalarStyle::Plain {
                    Value::String(text)
                } else {
                    resolve_plain(&text)
                };
                self.push_value(value, anchor);
            }
            Event::SequenceStart(anchor, _) => self.stack.push(Frame::Seq {
                anchor,
                items: Vec::new(),
            }),
            Event::MappingStart(anchor, _) => {
                self.stack.push(Frame::Map {
                    anchor,
                    map: Mapping::new(),
                    key: None,
                });
            }
            Event::SequenceEnd => {
                if let Some(Frame::Seq { anchor, items }) = self.stack.pop() {
                    self.push_value(Value::Sequence(items), anchor);
                }
            }
            Event::MappingEnd => {
                if let Some(Frame::Map { anchor, map, .. }) = self.stack.pop() {
                    self.push_value(Value::Mapping(map), anchor);
                }
            }
            Event::Alias(id) => match self.anchors.get(&id).cloned() {
                Some(value) => self.push_value(value, 0),
                None => {
                    let line = mark.line();
                    self.error
                        .get_or_insert_with(|| format!("unknown anchor at line {line}"));
                }
            },
            Event::Nothing
            | Event::StreamStart
            | Event::StreamEnd
            | Event::DocumentStart
            | Event::DocumentEnd => {}
        }
    }
}

/// Rewrites a yaml-rust2 error as yaml.v3 words it: the libyaml problem text, with `line N:` taken
/// from the 0-based problem mark (`N` as is for parser errors, plus one for scanner errors; no
/// prefix while that mark is on the first line). The context mark yaml.v3 prefers is not
/// available, which only matters when a construct spans several lines.
fn go_yaml_error(err: &yaml_rust2::scanner::ScanError) -> String {
    let info = err.info();
    let line0 = err.marker().line().saturating_sub(1);
    let parser_error = info.starts_with("while parsing");
    let problem = match info.split_once(", ") {
        Some((context, rest)) if context.starts_with("while ") => rest,
        _ => info,
    };
    let problem = if problem.starts_with("expected ") {
        format!("did not find {problem}")
    } else {
        problem.to_string()
    };
    match (line0, parser_error) {
        (0, _) => format!("yaml: {problem}"),
        (n, true) => format!("yaml: line {n}: {problem}"),
        (n, false) => format!("yaml: line {}: {problem}", n + 1),
    }
}

/// Parses the first YAML document of `text`. `Ok(None)` means it has no content.
pub(crate) fn parse_first_document(text: &str) -> Result<Option<Value>, String> {
    let mut builder = Builder::default();
    // The parser asserts on some malformed documents; a bad config must not take the process down.
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        Parser::new_from_str(text).load(&mut builder, false)
    }));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(go_yaml_error(&err)),
        Err(_) => return Err("yaml: malformed document".to_string()),
    }
    if let Some(error) = builder.error {
        return Err(format!("yaml: {error}"));
    }
    Ok(builder.root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Value {
        parse_first_document(text).unwrap().unwrap()
    }

    #[test]
    fn plain_scalars_resolve_like_yaml_v3() {
        let v = parse(
            "a: 0x1F\nb: 0o17\nc: 010\nd: 1_000\ne: -2\nf: .5\ng: 1e3\nh: '8317'\ni: yes\nj: ~\nk: 08\n",
        );
        let int = |k: &str| resolved(&v[k]).as_i64();
        assert_eq!(
            (int("a"), int("b"), int("c"), int("d"), int("e")),
            (Some(31), Some(15), Some(8), Some(1000), Some(-2))
        );
        assert_eq!(resolved(&v["f"]).as_f64(), Some(0.5));
        assert_eq!(resolved(&v["g"]).as_f64(), Some(1000.0));
        assert_eq!(v["h"], Value::String("8317".into()));
        assert_eq!(v["i"], Value::String("yes".into()));
        assert!(v["j"].is_null());
        assert_eq!(
            resolved(&v["k"]).as_f64(),
            Some(8.0),
            "invalid octal falls back to float"
        );
    }

    #[test]
    fn lossy_scalars_keep_their_source_text() {
        let v = parse("k: [True, 1.50, 12e4, 0o7, 42, true, 1.5]\n");
        let items = v["k"].as_sequence().unwrap();
        let raws: Vec<Option<&str>> = items.iter().map(raw_text).collect();
        assert_eq!(
            raws,
            [
                Some("True"),
                Some("1.50"),
                Some("12e4"),
                Some("0o7"),
                None,
                None,
                None
            ]
        );
        let mut cleaned = v.clone();
        untag_raw(&mut cleaned);
        assert_eq!(cleaned["k"][0], Value::Bool(true));
        assert_eq!(cleaned["k"][3].as_i64(), Some(7));
    }

    #[test]
    fn structure_errors_and_anchors() {
        assert!(parse_first_document("a: 1\na: 2\n").is_err());
        assert!(parse_first_document("a: *missing\n").is_err());
        let v = parse("base: &b {x: 1}\nuse: *b\n---\nother: 2\n");
        assert_eq!(v["use"]["x"], Value::Number(1.into()));
        assert!(v.get("other").is_none(), "only the first document counts");
    }
}

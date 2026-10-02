//! Recovers the client's original formatting of JSON containers that Go reads with gjson `.Raw`
//! and forwards as strings (tool arguments and results). `Value` re-serializes compactly, so the
//! translators swap those containers for strings holding their original text before reading them.

use cpa_json::Value;

/// Parses a payload like gjson reads it: a payload cut off before its closing brackets still
/// yields the fields that arrived. Hopeless input yields `Null`.
pub(crate) fn parse_lenient(bytes: &[u8]) -> Value {
    let parsed = cpa_json::parse(bytes);
    if !parsed.is_null() {
        return parsed;
    }
    let mut stack: Vec<u8> = Vec::new();
    let (mut in_string, mut escaped) = (false, false);
    for &b in bytes {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut fixed = bytes.trim_ascii_end().to_vec();
    if in_string {
        fixed.push(b'"');
    }
    if fixed.last() == Some(&b',') {
        fixed.pop();
    }
    fixed.extend(stack.iter().rev());
    cpa_json::parse(&fixed)
}

/// A step along a JSON path: object key or array index.
#[derive(Clone, Copy)]
pub(crate) enum Seg<'a> {
    Key(&'a str),
    Index(usize),
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// End offset of the string literal starting at the quote at `i`.
fn skip_string(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// End offset of the JSON value starting at `i` (scalars end at the next delimiter).
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = skip_string(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let delimiter = |c: &u8| matches!(c, b',' | b'}' | b']') || c.is_ascii_whitespace();
            Some(b[i..].iter().position(delimiter).map_or(b.len(), |p| i + p))
        }
    }
}

/// Start offset of the value at `path` inside the JSON text `b` (first match).
fn locate(b: &[u8], path: &[Seg<'_>]) -> Option<usize> {
    let mut at = skip_ws(b, 0);
    for seg in path {
        let mut i = skip_ws(b, at);
        match (seg, b.get(i)?) {
            (Seg::Key(key), b'{') => {
                i = skip_ws(b, i + 1);
                loop {
                    if *b.get(i)? != b'"' {
                        return None;
                    }
                    let key_end = skip_string(b, i)?;
                    let name: String = serde_json::from_slice(&b[i..key_end]).ok()?;
                    i = skip_ws(b, key_end);
                    if *b.get(i)? != b':' {
                        return None;
                    }
                    i = skip_ws(b, i + 1);
                    if name == *key {
                        break;
                    }
                    i = skip_ws(b, skip_value(b, i)?);
                    if *b.get(i)? != b',' {
                        return None;
                    }
                    i = skip_ws(b, i + 1);
                }
            }
            (Seg::Index(n), b'[') => {
                i = skip_ws(b, i + 1);
                for _ in 0..*n {
                    i = skip_ws(b, skip_value(b, i)?);
                    if *b.get(i)? != b',' {
                        return None;
                    }
                    i = skip_ws(b, i + 1);
                }
            }
            _ => return None,
        }
        at = i;
    }
    Some(at)
}

/// The value at `path` of `root`, if present.
fn value_at<'v>(root: &'v mut Value, path: &[Seg<'_>]) -> Option<&'v mut Value> {
    let mut cur = root;
    for seg in path {
        cur = match seg {
            Seg::Key(k) => cur.get_mut(*k)?,
            Seg::Index(i) => cur.get_mut(*i)?,
        };
    }
    Some(cur)
}

/// Replaces the object/array at `path` with a string holding its original text, so later
/// `json_string_value` calls see the client's formatting like gjson's `.Raw` does. `bytes` is the
/// JSON text `root` was parsed from.
pub(crate) fn restore_raw_text(bytes: &[u8], root: &mut Value, path: &[Seg<'_>]) {
    let Some(target) = value_at(root, path) else { return };
    if !target.is_object() && !target.is_array() {
        return;
    }
    let Some(start) = locate(bytes, path) else { return };
    let Some(end) = skip_value(bytes, start) else { return };
    if let Ok(text) = std::str::from_utf8(&bytes[start..end]) {
        *target = Value::String(text.to_string());
    }
}

/// Restores the original text of Interactions function-call `arguments` objects: the event's
/// `step` and each entry of `steps` / `interaction.steps`.
pub(crate) fn restore_step_arguments(bytes: &[u8], root: &mut Value) {
    restore_raw_text(bytes, root, &[Seg::Key("step"), Seg::Key("arguments")]);
    for prefix in [&[Seg::Key("steps")][..], &[Seg::Key("interaction"), Seg::Key("steps")][..]] {
        let count = value_at(root, prefix).and_then(|v| v.as_array().map(Vec::len)).unwrap_or(0);
        for i in 0..count {
            let mut path = prefix.to_vec();
            path.extend([Seg::Index(i), Seg::Key("arguments")]);
            restore_raw_text(bytes, root, &path);
        }
    }
}


/// Restores the original text of `arguments`, `result` and `output` in the entries of an
/// Interactions request `input` (array of steps, or a single step object).
pub(crate) fn restore_input_raw_text(bytes: &[u8], root: &mut Value) {
    let keys = ["arguments", "result", "output"];
    let count = match root.get("input") {
        Some(Value::Array(items)) => Some(items.len()),
        Some(Value::Object(_)) => None,
        _ => return,
    };
    match count {
        Some(count) => {
            for i in 0..count {
                for key in keys {
                    restore_raw_text(bytes, root, &[Seg::Key("input"), Seg::Index(i), Seg::Key(key)]);
                }
            }
        }
        None => {
            for key in keys {
                restore_raw_text(bytes, root, &[Seg::Key("input"), Seg::Key(key)]);
            }
        }
    }
}

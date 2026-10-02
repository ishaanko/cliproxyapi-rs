//! Original-text lookups for the few places where Go reads `gjson.Result.Raw`.
//!
//! `cpa_json` parses into a `Value`, which drops whitespace and collapses duplicate object keys.
//! Two Claude translator behaviors depend on the client's exact text: the structured output
//! instruction embeds the schema as sent, and the apply_patch bridge must see a duplicated
//! `input` key to reject the call. These helpers scan the original bytes for those values.

use cpa_json::Res;

use crate::common;

/// The raw JSON text of the value at a dotted key/index `path` (first match for duplicate keys),
/// or `None` when the path is absent. Scans the bytes as sent, without normalizing them.
pub(crate) fn raw_at<'a>(json: &'a [u8], path: &str) -> Option<&'a str> {
    let mut start = skip_ws(json, 0);
    for seg in path.split('.') {
        start = child_start(json, start, seg)?;
    }
    let end = value_end(json, start)?;
    std::str::from_utf8(&json[start..end]).ok()
}

fn skip_ws(json: &[u8], mut i: usize) -> usize {
    while json.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    i
}

/// End (exclusive) of the string literal starting at `start` (an opening quote).
fn string_end(json: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while let Some(&b) = json.get(i) {
        match b {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// End (exclusive) of the value starting at `start`.
fn value_end(json: &[u8], start: usize) -> Option<usize> {
    match *json.get(start)? {
        b'"' => string_end(json, start),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = start;
            while let Some(&b) = json.get(i) {
                match b {
                    b'"' => {
                        i = string_end(json, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            let len = json[start..]
                .iter()
                .position(|b| matches!(b, b',' | b'}' | b']') || b.is_ascii_whitespace())
                .unwrap_or(json.len() - start);
            Some(start + len)
        }
    }
}

/// Start of the member `seg` of the object (or element `seg` of the array) at `start`.
fn child_start(json: &[u8], start: usize, seg: &str) -> Option<usize> {
    let is_object = match *json.get(start)? {
        b'{' => true,
        b'[' => false,
        _ => return None,
    };
    let index: Option<usize> = if is_object { None } else { Some(seg.parse().ok()?) };
    let mut i = skip_ws(json, start + 1);
    let mut n = 0usize;
    loop {
        match *json.get(i)? {
            b'}' | b']' => return None,
            _ => {}
        }
        let mut matches = false;
        if is_object {
            let key_end = string_end(json, i)?;
            matches = &json[i + 1..key_end - 1] == seg.as_bytes();
            i = skip_ws(json, key_end);
            if *json.get(i)? != b':' {
                return None;
            }
            i = skip_ws(json, i + 1);
        } else if index == Some(n) {
            matches = true;
        }
        if matches {
            return Some(i);
        }
        i = skip_ws(json, value_end(json, i)?);
        match *json.get(i)? {
            b',' => i = skip_ws(json, i + 1),
            _ => return None,
        }
        n += 1;
    }
}

/// The structured output instruction (Go: `common.BuildClaudeStructuredOutputInstruction`) with
/// the schema embedded as the client sent it. `format` is the value at `format_path` of `body`.
pub(crate) fn structured_output_instruction(body: &[u8], format_path: &str, format: &Res<'_>) -> String {
    let instruction = common::build_claude_structured_output_instruction(format);
    if instruction.is_empty() || !format.exists() {
        return instruction;
    }
    // common serializes the schema compactly; swap in the schema text as sent.
    let (schema_path, schema) = if format.g("json_schema.schema").exists() {
        ("json_schema.schema", format.g("json_schema.schema"))
    } else {
        ("schema", format.g("schema"))
    };
    if !schema.exists() {
        return instruction;
    }
    let compact = format!("JSON Schema:\n{}\n", schema.raw());
    let Some(raw) = raw_at(body, &format!("{format_path}.{schema_path}")) else {
        return instruction;
    };
    match instruction.rfind(&compact) {
        Some(at) => format!("{}JSON Schema:\n{raw}\n{}", &instruction[..at], &instruction[at + compact.len()..]),
        None => instruction,
    }
}

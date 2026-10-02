//! Gemini content turn helpers (Go: common/gemini.go).
//!
//! Content turns and parts are JSON byte items (`Vec<u8>`), like Go's `[][]byte`.

use cpa_json::{J, Res, Value};

/// Whether a Gemini part contains hidden model thought.
pub fn is_gemini_thought_part(part: &Res<'_>) -> bool {
    part.g("thought").bool()
}

/// A part's raw JSON bytes.
fn raw(part: &Res<'_>) -> Vec<u8> {
    cpa_json::to_vec(&part.value())
}

fn has_function_response(v: &Value) -> bool {
    v.g("functionResponse").exists() || v.g("function_response").exists()
}

/// Sets `parts` of a turn to the joined raw `parts`.
fn with_parts(content: &Value, parts: &[Vec<u8>]) -> Vec<u8> {
    let mut turn = content.clone();
    cpa_json::set(&mut turn, "parts", cpa_json::parse(&super::join_raw_array(parts)));
    cpa_json::to_vec(&turn)
}

/// Merges consecutive user Content turns. Mid-conversation system messages in Claude requests are
/// downgraded to user reminder turns; when adjacent to other user turns or tool results their
/// parts are merged into a single user turn (text parts first, see [`reorder_gemini_user_parts`]).
/// Consecutive model turns are never merged, to keep part indices (thought signatures, reasoning
/// replay) stable.
pub fn merge_adjacent_gemini_contents(contents: &[Vec<u8>]) -> Vec<Vec<u8>> {
    if contents.len() <= 1 {
        return contents.to_vec();
    }
    let mut merged: Vec<Vec<u8>> = Vec::with_capacity(contents.len());
    for content in contents {
        if content.is_empty() {
            continue;
        }
        let parsed = cpa_json::parse(content);
        let role = parsed.g("role").str();
        let parts_result = parsed.g("parts");
        if !parts_result.is_array() || parts_result.array().is_empty() {
            continue;
        }
        if let Some(last_json) = merged.last() {
            let last = cpa_json::parse(last_json);
            if last.g("role").str() == "user" && role == "user" {
                let mut combined: Vec<Vec<u8>> = last.g("parts").array().iter().map(raw).collect();
                combined.extend(parts_result.array().iter().map(raw));
                let combined = reorder_gemini_user_parts(combined);
                let updated = with_parts(&last, &combined);
                *merged.last_mut().expect("checked non-empty") = updated;
                continue;
            }
        }
        merged.push(content.clone());
    }
    merged
}

/// Whether a content turn contains any functionCall part.
pub fn content_has_gemini_function_call(content: &[u8]) -> bool {
    cpa_json::parse(content)
        .g("parts")
        .array()
        .iter()
        .any(|part| part.g("functionCall").exists() || part.g("function_call").exists())
}

/// Whether a content turn contains any functionResponse part.
pub fn content_has_gemini_function_response(content: &[u8]) -> bool {
    cpa_json::parse(content)
        .g("parts")
        .array()
        .iter()
        .any(|part| part.g("functionResponse").exists() || part.g("function_response").exists())
}

/// Reorders the parts of a Gemini user turn so text parts (prompt text, system reminders) precede
/// functionResponse parts. This avoids upstream validation failures such as Vertex AI's 400
/// "Requests ending with a model turn are not supported" when a functionResponse is followed by
/// text in the same turn. Parts are only reordered when a functionResponse is followed by text.
pub fn reorder_gemini_user_parts(parts: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut has_fr = false;
    let mut has_trailing_text = false;
    for p in &parts {
        let parsed = cpa_json::parse(p);
        if has_function_response(&parsed) {
            has_fr = true;
        } else if has_fr && parsed.g("text").exists() {
            has_trailing_text = true;
            break;
        }
    }
    if !has_fr || !has_trailing_text {
        return parts;
    }

    let (mut prompt_parts, tool_parts): (Vec<_>, Vec<_>) = parts
        .into_iter()
        .partition(|p| cpa_json::parse(p).g("text").exists());
    prompt_parts.extend(tool_parts);
    prompt_parts
}

/// Merges consecutive user Content turns, but leaves turns containing functionResponse unmerged
/// to preserve tool-call/response boundaries.
pub fn merge_adjacent_gemini_user_contents(contents: &[Vec<u8>]) -> Vec<Vec<u8>> {
    if contents.len() <= 1 {
        return contents.to_vec();
    }
    let mut merged: Vec<Vec<u8>> = Vec::with_capacity(contents.len());
    for content in contents {
        if content.is_empty() {
            continue;
        }
        let parsed = cpa_json::parse(content);
        let role = parsed.g("role").str();
        let parts_result = parsed.g("parts");
        if !parts_result.is_array() || parts_result.array().is_empty() {
            continue;
        }
        if let Some(last_json) = merged.last() {
            let last = cpa_json::parse(last_json);
            if last.g("role").str() == "user"
                && role == "user"
                && !content_has_gemini_function_response(last_json)
                && !content_has_gemini_function_response(content)
            {
                let mut combined: Vec<Vec<u8>> = last.g("parts").array().iter().map(raw).collect();
                combined.extend(parts_result.array().iter().map(raw));
                let updated = with_parts(&last, &combined);
                *merged.last_mut().expect("checked non-empty") = updated;
                continue;
            }
        }
        merged.push(content.clone());
    }
    merged
}

/// Separates functionResponse parts from other user parts. Gemini/Antigravity requires a function
/// response turn to immediately follow the model turn containing the corresponding call; text or
/// reminders in the same user turn, or reminder turns in between, make the response look orphaned
/// to the upstream validator.
pub fn split_gemini_function_response_turns(contents: &[Vec<u8>]) -> Vec<Vec<u8>> {
    if contents.is_empty() {
        return contents.to_vec();
    }
    let mut split: Vec<Vec<u8>> = Vec::with_capacity(contents.len());
    for content in contents {
        let parsed = cpa_json::parse(content);
        if parsed.g("role").str() != "user" || !content_has_gemini_function_response(content) {
            split.push(content.clone());
            continue;
        }
        let (response_parts, other_parts): (Vec<_>, Vec<_>) = parsed
            .g("parts")
            .array()
            .iter()
            .map(|part| (has_function_response(&part.value()), raw(part)))
            .partition(|(is_response, _)| *is_response);
        let response_parts: Vec<Vec<u8>> = response_parts.into_iter().map(|(_, p)| p).collect();
        let other_parts: Vec<Vec<u8>> = other_parts.into_iter().map(|(_, p)| p).collect();
        if !response_parts.is_empty() {
            split.push(with_parts(&parsed, &response_parts));
        }
        if !other_parts.is_empty() {
            split.push(with_parts(&parsed, &other_parts));
        }
    }

    // For any consecutive block of user turns right after a model turn containing function calls,
    // turns containing functionResponse precede pure text/reminder turns, so responses
    // immediately follow the model turn even when mid-session reminders sit in between.
    let role_of = |turn: &[u8]| cpa_json::parse(turn).g("role").str();
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(split.len());
    let n = split.len();
    let mut i = 0;
    while i < n {
        if role_of(&split[i]) != "user" {
            out.push(split[i].clone());
            i += 1;
            continue;
        }
        let preceding_model_has_fc = out
            .last()
            .is_some_and(|last| role_of(last) == "model" && content_has_gemini_function_call(last));

        let mut j = i;
        let mut has_fr = false;
        while j < n && role_of(&split[j]) == "user" {
            if content_has_gemini_function_response(&split[j]) {
                has_fr = true;
            }
            j += 1;
        }
        let user_run = &split[i..j];
        if preceding_model_has_fc && has_fr && user_run.len() > 1 {
            let mut combined_fr_parts: Vec<Vec<u8>> = Vec::new();
            let mut other_turns: Vec<&Vec<u8>> = Vec::with_capacity(user_run.len());
            for turn in user_run {
                if content_has_gemini_function_response(turn) {
                    combined_fr_parts.extend(cpa_json::parse(turn).g("parts").array().iter().map(raw));
                } else {
                    other_turns.push(turn);
                }
            }
            if !combined_fr_parts.is_empty() {
                let fr_turn = cpa_json::parse_str(r#"{"role":"user","parts":[]}"#);
                out.push(with_parts(&fr_turn, &combined_fr_parts));
            }
            out.extend(other_turns.into_iter().cloned());
        } else {
            out.extend(user_run.iter().cloned());
        }
        i = j;
    }
    out
}

/// Whether `value` (recursively) contains a string-valued `$ref` property.
pub fn contains_json_ref(value: &Res<'_>) -> bool {
    value.v().is_some_and(value_contains_json_ref)
}

fn value_contains_json_ref(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, child)| {
            (key == "$ref" && child.is_string()) || value_contains_json_ref(child)
        }),
        Value::Array(items) => items.iter().any(value_contains_json_ref),
        _ => false,
    }
}

/// Sets the Gemini functionResponse `result`/`response` field at `path`. A result containing a
/// string-valued `$ref` is stored as opaque JSON text, so Gemini/Vertex AI does not read it as a
/// reference to a media part in `function_response.parts` and reject the request with HTTP 400. For
/// a path ending in `response`, the stringified result goes under its `.result` child, since
/// `response` must be an object. A missing result sets the empty string.
pub fn set_gemini_function_response_result(part: &[u8], path: &str, result: &Res<'_>) -> Vec<u8> {
    let mut root = cpa_json::parse(part);
    if !result.exists() {
        cpa_json::set(&mut root, path, "");
    } else if contains_json_ref(result) {
        let target_path = if path.ends_with("response") {
            format!("{path}.result")
        } else {
            path.to_string()
        };
        cpa_json::set(&mut root, &target_path, result.raw());
    } else {
        cpa_json::set(&mut root, path, result.value());
    }
    cpa_json::to_vec(&root)
}

/// [`set_gemini_function_response_result`] for a raw JSON string (blank or invalid JSON sets the
/// empty string).
pub fn set_gemini_function_response_raw(part: &[u8], path: &str, raw_json: &str) -> Vec<u8> {
    let trimmed = raw_json.trim();
    let parsed = if trimmed.is_empty() {
        None
    } else {
        serde_json::from_str::<Value>(trimmed).ok()
    };
    match parsed {
        Some(value) => set_gemini_function_response_result(part, path, &Res::owned(value)),
        None => set_gemini_function_response_result(part, path, &Res::NONE),
    }
}

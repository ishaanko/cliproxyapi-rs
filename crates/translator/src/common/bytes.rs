//! Raw JSON array, SSE frame and token-count helpers (Go: common/bytes.go).

use cpa_json::Value;

/// `{"totalTokens":N,"promptTokensDetails":[{"modality":"TEXT","tokenCount":N}]}` (Gemini
/// countTokens response).
pub fn gemini_token_count_json(count: i64) -> Vec<u8> {
    format!(
        r#"{{"totalTokens":{count},"promptTokensDetails":[{{"modality":"TEXT","tokenCount":{count}}}]}}"#
    )
    .into_bytes()
}

/// `{"input_tokens":N}` (Claude count_tokens response).
pub fn claude_input_tokens_json(count: i64) -> Vec<u8> {
    format!(r#"{{"input_tokens":{count}}}"#).into_bytes()
}

/// Joins raw JSON items into an array: `[]` when empty, else `[a,b,...]`.
pub fn join_raw_array<T: AsRef<[u8]>>(items: &[T]) -> Vec<u8> {
    let mut out = Vec::with_capacity(items.iter().map(|i| i.as_ref().len() + 1).sum::<usize>() + 2);
    out.push(b'[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(item.as_ref());
    }
    out.push(b']');
    out
}

/// Sets the JSON array at `path` to the raw `items` (no-op for an empty item list). The result is
/// the original bytes when `path` is empty or an item is not valid JSON.
pub fn set_raw_array_items<T: AsRef<[u8]>>(data: &[u8], path: &str, items: &[T]) -> Vec<u8> {
    if items.is_empty() || path.is_empty() {
        return data.to_vec();
    }
    let joined = join_raw_array(items);
    let Ok(array) = serde_json::from_slice::<Value>(&joined) else {
        return data.to_vec();
    };
    let mut root = cpa_json::parse(data);
    if sjson_rejects_path(&root, path) {
        return data.to_vec();
    }
    cpa_json::set(&mut root, path, array);
    cpa_json::to_vec(&root)
}

/// sjson refuses to descend into an existing array with a key that is neither an index nor `-1`
/// ("cannot set array element for non-numeric key") and leaves the document unchanged;
/// `cpa_json::set` would turn the array into an object instead.
fn sjson_rejects_path(root: &Value, path: &str) -> bool {
    let mut current = Some(root);
    for key in path.split('.') {
        let Some(node) = current else {
            return false;
        };
        match node {
            Value::Array(items) => {
                if key == "-1" {
                    return false;
                }
                let Ok(index) = key.parse::<usize>() else {
                    return true;
                };
                current = items.get(index);
            }
            Value::Object(map) => current = map.get(key),
            _ => return false,
        }
    }
    false
}

/// One complete SSE frame: `event: <e>\ndata: <payload>\n\n`. Each frame carries its own
/// blank-line terminator so concatenated frames stay separable downstream.
pub fn sse_event_data(event: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(event.len() + payload.len() + 16);
    append_sse_event_bytes(&mut out, event, payload, 2);
    out
}

/// Appends `event: <e>\ndata: <payload>` followed by `trailing_newlines` newlines (no automatic
/// blank line: callers pass 1 or 2).
pub fn append_sse_event_string(out: &mut Vec<u8>, event: &str, payload: &str, trailing_newlines: usize) {
    append_sse_event_bytes(out, event, payload.as_bytes(), trailing_newlines);
}

/// Byte-payload variant of [`append_sse_event_string`].
pub fn append_sse_event_bytes(out: &mut Vec<u8>, event: &str, payload: &[u8], trailing_newlines: usize) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(payload);
    out.extend(std::iter::repeat_n(b'\n', trailing_newlines));
}

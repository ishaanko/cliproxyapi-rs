//! Responses API usage normalization (Go: helps/responses_usage_helpers.go).

use cpa_json::{J, Kind, Value};

use super::text::trim_space;

/// Makes Responses `usage` objects carry `output_tokens_details.reasoning_tokens` and
/// `input_tokens_details.cached_tokens` (defaulting to 0), which strict clients require.
/// Handles plain JSON payloads, single `data:` SSE lines and multi-line SSE frames
/// (`event: ...\ndata: ...`). `response.compaction` objects and payloads that already carry the
/// fields are returned unchanged (same bytes).
pub fn ensure_responses_usage_details(payload: &[u8]) -> Vec<u8> {
    if payload.is_empty() {
        return payload.to_vec();
    }
    let trimmed = trim_space(payload);
    if trimmed.is_empty() {
        return payload.to_vec();
    }
    // JSON first: a payload starting with '{' is a plain object.
    if trimmed[0] == b'{' {
        return match patch_json(trimmed) {
            Some(updated) => updated,
            None => payload.to_vec(),
        };
    }
    // SSE frames: patch the JSON of every `data:` line.
    if !contains_data_prefix(payload) {
        return payload.to_vec();
    }
    let mut lines: Vec<Vec<u8>> = payload.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
    let mut modified = false;
    for line in &mut lines {
        if !trim_space(line).starts_with(b"data:") {
            continue;
        }
        let prefix_len = if line.starts_with(b"data: ") { 6 } else { 5 };
        let data = trim_space(&line[prefix_len.min(line.len())..]);
        if data.first() != Some(&b'{') {
            continue;
        }
        if let Some(updated) = patch_json(data) {
            let mut next = line[..prefix_len].to_vec();
            next.extend_from_slice(&updated);
            *line = next;
            modified = true;
        }
    }
    if modified { lines.join(&b'\n') } else { payload.to_vec() }
}

fn contains_data_prefix(payload: &[u8]) -> bool {
    payload.windows(5).any(|w| w == b"data:")
}

/// Patches one JSON object; `None` when nothing changed (or it is a compaction object).
fn patch_json(json: &[u8]) -> Option<Vec<u8>> {
    let mut v = cpa_json::parse(json);
    if v.g("object").str() == "response.compaction" {
        return None;
    }
    let mut changed = ensure_usage_details_at(&mut v, "response.usage");
    changed |= ensure_usage_details_at(&mut v, "usage");
    changed.then(|| cpa_json::to_vec(&v))
}

fn ensure_usage_details_at(body: &mut Value, path: &str) -> bool {
    if !body.g(path).is_object() {
        return false;
    }
    let mut changed = false;
    for (details, field, empty) in [
        ("output_tokens_details", "reasoning_tokens", r#"{"reasoning_tokens":0}"#),
        ("input_tokens_details", "cached_tokens", r#"{"cached_tokens":0}"#),
    ] {
        let details_path = format!("{path}.{details}");
        let node = body.g(&details_path);
        if !node.exists() {
            cpa_json::set(body, &format!("{details_path}.{field}"), 0);
            changed = true;
        } else if node.kind() == Kind::Null || !node.is_object() {
            if let Ok(raw) = serde_json::from_str::<Value>(empty) {
                cpa_json::set(body, &details_path, raw);
                changed = true;
            }
        } else {
            let leaf = body.g(&format!("{details_path}.{field}"));
            if !leaf.exists() || leaf.kind() == Kind::Null {
                cpa_json::set(body, &format!("{details_path}.{field}"), 0);
                changed = true;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_missing_details_in_json_and_sse() {
        let json = br#"{"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":1}}}"#;
        let out = String::from_utf8(ensure_responses_usage_details(json)).unwrap();
        assert!(out.contains(r#""output_tokens_details":{"reasoning_tokens":0}"#));
        assert!(out.contains(r#""input_tokens_details":{"cached_tokens":0}"#));

        let sse = b"event: response.completed\ndata: {\"response\":{\"usage\":{\"output_tokens_details\":null,\"input_tokens_details\":{}}}}\n\n";
        let out = String::from_utf8(ensure_responses_usage_details(sse)).unwrap();
        assert!(out.starts_with("event: response.completed\ndata: {"));
        assert!(out.contains(r#""output_tokens_details":{"reasoning_tokens":0}"#));
        assert!(out.contains(r#""input_tokens_details":{"cached_tokens":0}"#));
        assert!(out.ends_with("}\n\n"));
    }

    #[test]
    fn leaves_complete_and_compaction_payloads_alone() {
        let done = br#" {"usage":{"output_tokens_details":{"reasoning_tokens":2},"input_tokens_details":{"cached_tokens":1}}} "#;
        assert_eq!(ensure_responses_usage_details(done), done.to_vec());
        let compaction = br#"{"object":"response.compaction","usage":{"input_tokens":1}}"#;
        assert_eq!(ensure_responses_usage_details(compaction), compaction.to_vec());
        assert_eq!(ensure_responses_usage_details(b"plain text"), b"plain text".to_vec());
    }
}

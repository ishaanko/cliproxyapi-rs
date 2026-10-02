//! Codex `input[]` item id limits (Go: helps/codex_input_ids.go `SanitizeCodexInputItemIDs`).
//!
//! Codex rejects input item ids longer than 64 characters and expects typed prefixes
//! (`msg_`, `rs_`, `fc_`, ...). Ids are prefixed per item type, encrypted reasoning items with
//! overlong ids are dropped, and other overlong ids are shortened deterministically with a
//! hash suffix. Like the Go code this edits the request as raw text, so unchanged items keep
//! their original bytes.

use std::borrow::Cow;
use std::collections::HashMap;

use sha2::{Digest, Sha256};

use super::tool_schema::gj::{self, Key, Res, Ty};

const CODEX_INPUT_ITEM_ID_LIMIT: usize = 64;

const ID_OCCUPIED: u8 = 1 << 0;
const ID_PRESERVED: u8 = 1 << 1;

/// Normalizes supported input item ids for Codex, removes encrypted reasoning items whose ids
/// exceed the limit, and deterministically shortens other overlong ids. Returns the same bytes
/// when nothing changed.
pub fn sanitize_codex_input_item_ids(body: &[u8]) -> Vec<u8> {
    let Some(input) = gj::get(body, "input").filter(Res::is_array) else {
        return body.to_vec();
    };
    let items = gj::array(input.raw);

    // First pass: which ids exist after prefixing, which are untouched originals, and which
    // already fit the limit.
    let mut id_states: HashMap<String, u8> = HashMap::with_capacity(items.len());
    for item in &items {
        if should_drop_encrypted_reasoning_item(item) {
            continue;
        }
        let Some(item_id) = string_member(item, "id") else {
            continue;
        };
        let id = normalize_input_item_id(item, &item_id);
        let mut state = id_states.get(&id).copied().unwrap_or(0);
        if id == item_id {
            state |= ID_PRESERVED;
        }
        if id.chars().count() <= CODEX_INPUT_ITEM_ID_LIMIT {
            state |= ID_OCCUPIED;
        }
        if state != 0 {
            id_states.insert(id, state);
        }
    }

    let mut mapped: HashMap<String, String> = HashMap::new();
    let mut collision_mapped: HashMap<String, String> = HashMap::new();
    // Kept items: original text, or the edited copy when the id changed.
    let mut kept: Vec<Cow<'_, [u8]>> = Vec::with_capacity(items.len());
    let mut changed = false;
    for item in &items {
        if should_drop_encrypted_reasoning_item(item) {
            changed = true;
            continue;
        }
        let mut raw = Cow::Borrowed(item.raw);
        if let Some(original_id) = string_member(item, "id") {
            let mut id = normalize_input_item_id(item, &original_id);
            if id != original_id && id_states.get(&id).is_some_and(|s| s & ID_PRESERVED != 0) {
                // The prefixed id collides with an untouched original: add a hash suffix.
                let collision_id = match collision_mapped.get(&id) {
                    Some(c) => c.clone(),
                    None => {
                        let mut attempt = 0usize;
                        let collision_id = loop {
                            let candidate = id_with_hash_suffix(&id, attempt);
                            if id_states.get(&candidate).is_some_and(|s| s & ID_OCCUPIED != 0) {
                                attempt += 1;
                                continue;
                            }
                            break candidate;
                        };
                        collision_mapped.insert(id.clone(), collision_id.clone());
                        *id_states.entry(collision_id.clone()).or_insert(0) |= ID_OCCUPIED;
                        collision_id
                    }
                };
                id = collision_id;
            }
            if id.chars().count() > CODEX_INPUT_ITEM_ID_LIMIT {
                let shortened = match mapped.get(&id) {
                    Some(s) => s.clone(),
                    None => {
                        let mut shortened = shorten_input_item_id(&id);
                        let mut attempt = 1usize;
                        while id_states.get(&shortened).is_some_and(|s| s & ID_OCCUPIED != 0) {
                            shortened = shorten_input_item_id_with_attempt(&id, attempt);
                            attempt += 1;
                        }
                        mapped.insert(id.clone(), shortened.clone());
                        *id_states.entry(shortened.clone()).or_insert(0) |= ID_OCCUPIED;
                        shortened
                    }
                };
                id = shortened;
            }

            if id != original_id
                && let Ok(next) = gj::set_string(item.raw, "id", &id)
            {
                raw = Cow::Owned(next);
                changed = true;
            }
        }
        kept.push(raw);
    }
    if !changed {
        return body.to_vec();
    }

    let mut new_input = vec![b'['];
    for (i, raw) in kept.iter().enumerate() {
        if i > 0 {
            new_input.push(b',');
        }
        new_input.extend_from_slice(raw);
    }
    new_input.push(b']');
    gj::set_raw(body, &[Key::plain("input")], &new_input).unwrap_or_else(|()| body.to_vec())
}

/// `item.Get(path)` as a string when the value is a JSON string.
fn string_member(item: &Res<'_>, path: &str) -> Option<String> {
    gj::get(item.raw, path).filter(|v| v.ty == Ty::String).map(|v| v.string())
}

/// Adds the per-type prefix (`msg_`, `rs_`, `fc_`, `ctc_`, `ctco_`) unless the id is empty or
/// already starts with it.
fn normalize_input_item_id(item: &Res<'_>, id: &str) -> String {
    let item_type = gj::get(item.raw, "type").map(|t| t.string()).unwrap_or_default();
    let prefix = match item_type.as_str() {
        "message" => "msg",
        "reasoning" => "rs",
        "function_call" => "fc",
        "custom_tool_call" => "ctc",
        "custom_tool_call_output" => "ctco",
        _ => return id.to_string(),
    };
    if id.is_empty() || id.starts_with(prefix) {
        return id.to_string();
    }
    format!("{prefix}_{id}")
}

/// Encrypted reasoning items with an overlong id cannot be shortened (the signature binds the
/// id), so they are dropped.
fn should_drop_encrypted_reasoning_item(item: &Res<'_>) -> bool {
    if gj::get(item.raw, "type").map(|t| t.string()).as_deref() != Some("reasoning") {
        return false;
    }
    match string_member(item, "id") {
        Some(id) if id.chars().count() > CODEX_INPUT_ITEM_ID_LIMIT => {}
        _ => return false,
    }
    string_member(item, "encrypted_content").is_some_and(|c| !c.is_empty())
}

fn shorten_input_item_id(id: &str) -> String {
    shorten_input_item_id_with_attempt(id, 0)
}

fn shorten_input_item_id_with_attempt(id: &str, attempt: usize) -> String {
    let runes: Vec<char> = id.chars().collect();
    if runes.len() <= CODEX_INPUT_ITEM_ID_LIMIT {
        return id.to_string();
    }
    id_with_hash_suffix_runes(id, &runes, attempt)
}

fn id_with_hash_suffix(id: &str, attempt: usize) -> String {
    let runes: Vec<char> = id.chars().collect();
    id_with_hash_suffix_runes(id, &runes, attempt)
}

/// `prefix[..64-17] + "_" + hex(sha256(id)[..8])`; later attempts hash `id\0attempt`.
fn id_with_hash_suffix_runes(id: &str, runes: &[char], attempt: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id.as_bytes());
    if attempt > 0 {
        hasher.update([0u8]);
        hasher.update(attempt.to_string().as_bytes());
    }
    let sum = hasher.finalize();
    let suffix = format!("_{}", hex::encode(&sum[..8]));
    let prefix_length = (CODEX_INPUT_ITEM_ID_LIMIT - suffix.len()).min(runes.len());
    let mut out: String = runes[..prefix_length].iter().collect();
    out.push_str(&suffix);
    out
}

#[cfg(test)]
mod tests {
    use cpa_json::J;

    use super::*;

    fn id_at(body: &[u8], path: &str) -> String {
        cpa_json::parse(body).g(path).str()
    }

    fn runes(s: &str) -> usize {
        s.chars().count()
    }

    #[test]
    fn boundaries() {
        let id64 = "a".repeat(64);
        let id65 = "b".repeat(65);
        let unicode65 = "界".repeat(65);
        let body = format!(r#"{{"input":[{{"id":"{id64}"}},{{"id":"{id65}"}},{{"id":"{unicode65}"}}]}}"#);

        let got = sanitize_codex_input_item_ids(body.as_bytes());

        assert_eq!(id_at(&got, "input.0.id"), id64, "64-character ID changed");
        for path in ["input.1.id", "input.2.id"] {
            let actual = id_at(&got, path);
            assert_eq!(runes(&actual), 64, "{path} length: {actual:?}");
        }
    }

    #[test]
    fn normalizes_message_ids() {
        let invalid_id = "item_74ec40c883248ebb4885ec84";
        let body = format!(
            r#"{{"input":[{{"type":"message","id":"{invalid_id}","role":"user"}},{{"type":"message","id":"msg-1","role":"assistant"}},{{"type":"function_call","id":"item_call","call_id":"call-1"}}]}}"#
        );

        let first = sanitize_codex_input_item_ids(body.as_bytes());
        let second = sanitize_codex_input_item_ids(body.as_bytes());

        assert_eq!(id_at(&first, "input.0.id"), format!("msg_{invalid_id}"));
        assert_eq!(id_at(&first, "input.1.id"), "msg-1");
        assert_eq!(id_at(&first, "input.2.id"), "fc_item_call");
        assert_eq!(first, second);
    }

    #[test]
    fn normalizes_response_item_ids() {
        let body = br#"{"input":[{"type":"message","id":"item_message"},{"type":"reasoning","id":"item_reasoning"},{"type":"function_call","id":"item_function_call","call_id":"call-1"},{"type":"function_call_output","id":"item_function_call_output","call_id":"call-1"},{"type":"reasoning","id":"rs-existing"},{"type":"function_call","id":"fc-existing","call_id":"call-2"},{"type":"message","id":"msg-existing"}]}"#;

        let got = sanitize_codex_input_item_ids(body);
        let want = [
            "msg_item_message",
            "rs_item_reasoning",
            "fc_item_function_call",
            "item_function_call_output",
            "rs-existing",
            "fc-existing",
            "msg-existing",
        ];
        for (index, expected) in want.iter().enumerate() {
            let path = format!("input.{index}.id");
            assert_eq!(id_at(&got, &path), *expected, "{path}; payload={}", String::from_utf8_lossy(&got));
        }
        assert_eq!(sanitize_codex_input_item_ids(body), got);
    }

    #[test]
    fn avoids_normalization_collisions() {
        for (item_type, prefix) in [
            ("message", "msg_"),
            ("reasoning", "rs_"),
            ("function_call", "fc_"),
            ("custom_tool_call", "ctc_"),
            ("custom_tool_call_output", "ctco_"),
        ] {
            for invalid_id in
                ["item_collision".to_string(), "x".repeat(CODEX_INPUT_ITEM_ID_LIMIT - runes(prefix) + 1)]
            {
                let prefixed_id = format!("{prefix}{invalid_id}");
                for (ids, prefixed_index) in [
                    ([invalid_id.clone(), prefixed_id.clone()], 1usize),
                    ([prefixed_id.clone(), invalid_id.clone()], 0usize),
                ] {
                    let body = format!(
                        r#"{{"input":[{{"type":{item_type:?},"id":{:?}}},{{"type":{item_type:?},"id":{:?}}}]}}"#,
                        ids[0], ids[1]
                    );
                    let first = sanitize_codex_input_item_ids(body.as_bytes());
                    let second = sanitize_codex_input_item_ids(body.as_bytes());
                    let normalized_again = sanitize_codex_input_item_ids(&first);
                    let out = [id_at(&first, "input.0.id"), id_at(&first, "input.1.id")];

                    let ctx = String::from_utf8_lossy(&first).into_owned();
                    assert_ne!(out[0], out[1], "distinct IDs collided: {ctx}");
                    for (index, id) in out.iter().enumerate() {
                        assert!(id.starts_with(prefix), "input.{index}.id = {id:?}, want prefix {prefix:?}");
                        assert!(runes(id) <= CODEX_INPUT_ITEM_ID_LIMIT, "input.{index}.id too long: {id:?}");
                    }
                    if runes(&prefixed_id) <= CODEX_INPUT_ITEM_ID_LIMIT {
                        assert_eq!(out[prefixed_index], prefixed_id, "existing valid ID changed");
                    }
                    assert_eq!(first, second, "not deterministic");
                    assert_eq!(first, normalized_again, "not idempotent");
                }
            }
        }
    }

    #[test]
    fn normalizes_custom_tool_call_ids() {
        let invalid_id = "item_44e13caebc1ddf25f1337cbe";
        let body = format!(
            r#"{{"input":[{{"type":"custom_tool_call","id":"{invalid_id}","call_id":"call-1","name":"lookup","input":"{{}}"}}]}}"#
        );
        let got = sanitize_codex_input_item_ids(body.as_bytes());
        assert_eq!(id_at(&got, "input.0.id"), format!("ctc_{invalid_id}"));
    }

    #[test]
    fn normalizes_custom_tool_call_output_ids() {
        let invalid_id = "item_44e13caebc1ddf25f1337cbe_output";
        let valid_id = "ctco-existing";
        let body = format!(
            r#"{{"input":[{{"type":"custom_tool_call_output","id":"{invalid_id}","call_id":"call-1","output":"done"}},{{"type":"custom_tool_call_output","id":"{valid_id}","call_id":"call-2","output":"done"}}]}}"#
        );
        let first = sanitize_codex_input_item_ids(body.as_bytes());
        let second = sanitize_codex_input_item_ids(body.as_bytes());
        let normalized_again = sanitize_codex_input_item_ids(&first);

        assert_eq!(id_at(&first, "input.0.id"), format!("ctco_{invalid_id}"));
        assert_eq!(id_at(&first, "input.1.id"), valid_id);
        assert_eq!(first, second);
        assert_eq!(first, normalized_again);
    }

    #[test]
    fn drops_overlong_encrypted_reasoning_item() {
        let long_reasoning_id = format!("rs_{}", "a".repeat(64));
        let short_reasoning_id = format!("rs_{}", "b".repeat(48));
        let long_call_id = "call-item-".repeat(8);
        let body = format!(
            r#"{{"input":[{{"type":"message","id":"msg-1","role":"user","content":"before"}},{{"type":"reasoning","id":"{long_reasoning_id}","encrypted_content":"gAAAA-encrypted","summary":[{{"type":"summary_text","text":"drop me"}}]}},{{"type":"reasoning","id":"{short_reasoning_id}","encrypted_content":"gAAAA-encrypted","summary":[]}},{{"type":"function_call","id":"{long_call_id}","call_id":"call-1","name":"lookup","arguments":"{{}}"}}]}}"#
        );
        let got = sanitize_codex_input_item_ids(body.as_bytes());

        assert_eq!(cpa_json::parse(&got).g("input.#").int(), 3, "{}", String::from_utf8_lossy(&got));
        assert_eq!(id_at(&got, "input.0.id"), "msg-1");
        assert_eq!(id_at(&got, "input.1.id"), short_reasoning_id);
        let call_id = id_at(&got, "input.2.id");
        assert!(call_id != long_call_id && runes(&call_id) == 64, "ordinary overlong id was not shortened: {call_id:?}");
    }

    #[test]
    fn shortens_overlong_reasoning_without_encrypted_content() {
        let long_reasoning_id = format!("rs_{}", "a".repeat(64));
        for encrypted_content in ["", r#","encrypted_content":"""#, r#","encrypted_content":null"#] {
            let body = format!(
                r#"{{"input":[{{"type":"reasoning","id":"{long_reasoning_id}"{encrypted_content},"summary":[]}}]}}"#
            );
            let got = sanitize_codex_input_item_ids(body.as_bytes());
            assert_eq!(cpa_json::parse(&got).g("input.#").int(), 1, "{}", String::from_utf8_lossy(&got));
            let got_id = id_at(&got, "input.0.id");
            assert!(
                got_id != long_reasoning_id && runes(&got_id) == 64,
                "overlong reasoning id was not shortened: {got_id:?}"
            );
        }
    }

    #[test]
    fn avoids_existing_id_collision() {
        let long_id = "grok-item-".repeat(10);
        let colliding_valid_id = shorten_input_item_id(&long_id);
        let body = format!(r#"{{"input":[{{"id":"{long_id}"}},{{"id":"{colliding_valid_id}"}}]}}"#);

        let first = sanitize_codex_input_item_ids(body.as_bytes());
        let second = sanitize_codex_input_item_ids(body.as_bytes());

        let shortened = id_at(&first, "input.0.id");
        assert_ne!(shortened, colliding_valid_id, "shortened ID collided with an existing valid ID");
        assert!(runes(&shortened) <= 64);
        assert_eq!(id_at(&first, "input.1.id"), colliding_valid_id, "existing valid ID changed");
        assert_eq!(id_at(&second, "input.0.id"), shortened, "collision resolution is not deterministic");
    }

    #[test]
    fn leaves_unsupported_payloads_unchanged() {
        for body in [
            br#"not-json"#.as_slice(),
            br#"{"input":{"id":"item-1"}}"#,
            br#"{"input":[1,{"id":2},{"id":"item-1"}]}"#,
        ] {
            assert_eq!(sanitize_codex_input_item_ids(body), body, "payload changed");
        }
    }
}

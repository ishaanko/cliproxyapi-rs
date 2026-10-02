//! Claude `cache_control` propagation (Go: common/cache_control.go). Only `{"type":"ephemeral"}`
//! objects are copied.

use cpa_json::{J, Res, Value};

/// A `cache_control` object whose `type` is exactly the string `ephemeral`.
fn is_valid_cache_control(cc: &Res<'_>) -> bool {
    cc.is_object() && cc.g("type").as_str() == Some("ephemeral")
}

/// Copies a Claude-compatible `cache_control` object from `src` onto `dst`. Returns `dst`
/// unchanged when `src` has no valid `cache_control`.
pub fn attach_cache_control(dst: &[u8], src: &Res<'_>) -> Vec<u8> {
    let cc = src.g("cache_control");
    if !is_valid_cache_control(&cc) {
        return dst.to_vec();
    }
    let mut root = cpa_json::parse(dst);
    cpa_json::set(&mut root, "cache_control", cc.value());
    cpa_json::to_vec(&root)
}

/// Applies message-level `cache_control` onto the last content block. Part-level `cache_control`
/// wins when the last block already has one; string content is promoted to a content array so
/// Claude accepts the `cache_control`.
pub fn attach_message_cache_control(msg: &[u8], src: &Res<'_>) -> Vec<u8> {
    let cc = src.g("cache_control");
    if !is_valid_cache_control(&cc) {
        return msg.to_vec();
    }

    let mut root = cpa_json::parse(msg);
    let content = root.g("content");
    if content.is_array() {
        let arr = content.array();
        let Some(last) = arr.last() else {
            return msg.to_vec();
        };
        if last.g("cache_control").exists() {
            return msg.to_vec();
        }
        let path = format!("content.{}.cache_control", arr.len() - 1);
        drop(arr);
        cpa_json::set(&mut root, &path, cc.value());
        return cpa_json::to_vec(&root);
    }

    let Some(text) = content.as_str() else {
        return msg.to_vec();
    };
    let mut text_part = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
    cpa_json::set(&mut text_part, "text", text);
    cpa_json::set(&mut text_part, "cache_control", cc.value());
    cpa_json::set(&mut root, "content", Value::Array(Vec::new()));
    cpa_json::set(&mut root, "content.-1", text_part);
    cpa_json::to_vec(&root)
}

/// Hoists part-level or message-level `cache_control` onto the first `tool_result` block of
/// `msg`'s content. Part-level `cache_control` from `src` content takes precedence.
pub fn attach_tool_message_cache_control(msg: &[u8], src: &Res<'_>) -> Vec<u8> {
    let mut raw_cc = extract_first_part_cache_control(src);
    if raw_cc.is_none() {
        let cc = src.g("cache_control");
        if is_valid_cache_control(&cc) {
            raw_cc = Some(cc.value());
        }
    }
    let Some(raw_cc) = raw_cc else {
        return msg.to_vec();
    };

    let mut root = cpa_json::parse(msg);
    let content = root.g("content");
    if !content.is_array() {
        return msg.to_vec();
    }
    let arr = content.array();
    let Some(target_idx) = arr.iter().position(|block| block.g("type").str() == "tool_result") else {
        return msg.to_vec();
    };
    drop(arr);
    cpa_json::set(&mut root, &format!("content.{target_idx}.cache_control"), raw_cc);
    cpa_json::to_vec(&root)
}

/// The first valid `cache_control` among the parts of `src.content` (or of `src` itself when it
/// has no `content`); for an object, its own `cache_control`.
fn extract_first_part_cache_control(src: &Res<'_>) -> Option<Value> {
    let mut content = src.g("content");
    if !content.exists() {
        content = src.clone();
    }
    if content.is_array() {
        return content.array().iter().find_map(|part| {
            let cc = part.g("cache_control");
            is_valid_cache_control(&cc).then(|| cc.value())
        });
    }
    if content.is_object() {
        let cc = content.g("cache_control");
        if is_valid_cache_control(&cc) {
            return Some(cc.value());
        }
    }
    None
}

//! Claude `cache_control` placement, TTL handling and breakpoint limits, plus
//! `ensure_model_max_tokens` (Go: executor/claude_executor_cloaking.go, from
//! `withEphemeralCacheControl` to the end of the file).
//!
//! Every `pub fn` takes and returns body bytes. Bodies are edited as `serde_json::Value` (key
//! order preserved), and the original bytes are returned untouched whenever nothing changed,
//! like the Go byte-level sjson edits.

use cpa_core::registry::global_registry;
use cpa_json::{J, Map, Value};

/// Go: `defaultModelMaxTokens` (claude_executor.go). Fallback `max_tokens` for registered
/// Claude models that carry no `MaxCompletionTokens`.
pub const DEFAULT_MODEL_MAX_TOKENS: i64 = 1024;

/// Go: `claudeCacheControlTTL1h`, the only non-default ttl native Claude Code selects.
pub const CLAUDE_CACHE_CONTROL_TTL_1H: &str = "1h";

/// Go: `claudeCacheControl`. Serializes as `{"type":..,"ttl":..}` with an empty `ttl` omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeCacheControl {
    pub r#type: &'static str,
    pub ttl: &'static str,
}

/// Go: `claudeCodeCacheControl`, the default Claude Code breakpoint `{"type":"ephemeral"}`.
pub const CLAUDE_CODE_CACHE_CONTROL: ClaudeCacheControl = ClaudeCacheControl {
    r#type: "ephemeral",
    ttl: "",
};

impl ClaudeCacheControl {
    /// The JSON object the Go struct marshals to (`type` first, `ttl` only when set).
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), Value::String(self.r#type.into()));
        if !self.ttl.is_empty() {
            m.insert("ttl".into(), Value::String(self.ttl.into()));
        }
        Value::Object(m)
    }
}

/// Go: `buildTextBlock`, as a `Value` (`{"type":"text","text":..[,"cache_control":..]}`).
/// serde_json never HTML-escapes, matching `marshalJSONStringWithoutHTMLEscape`.
pub fn build_text_block_value(text: &str, cache_control: Option<&ClaudeCacheControl>) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("text".into()));
    m.insert("text".into(), Value::String(text.into()));
    if let Some(cc) = cache_control.filter(|cc| !cc.r#type.is_empty()) {
        m.insert("cache_control".into(), cc.to_value());
    }
    Value::Object(m)
}

/// Parses `payload`, applies `f`, and re-serializes only when `f` reports a change.
fn edit(payload: &[u8], f: impl FnOnce(&mut Value) -> bool) -> Vec<u8> {
    crate::helps::parse_cache::edit(payload, f)
}

fn has_cache_control(item: &Value) -> bool {
    item.get("cache_control").is_some()
}

fn array_of<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Every block that can carry `cache_control`, in Anthropic's evaluation order: tools, then
/// system, then message content blocks.
fn blocks(v: &Value) -> impl Iterator<Item = &Value> {
    let msg_blocks = array_of(v, "messages")
        .iter()
        .flat_map(|m| array_of(m, "content").iter());
    array_of(v, "tools")
        .iter()
        .chain(array_of(v, "system"))
        .chain(msg_blocks)
}

/// Go: `forEachClaudeCacheControlBlock` (mutable visit in the same order as [`blocks`]).
fn for_each_block_mut(v: &mut Value, f: &mut dyn FnMut(&mut Value)) {
    let Some(root) = v.as_object_mut() else {
        return;
    };
    for key in ["tools", "system"] {
        if let Some(Value::Array(items)) = root.get_mut(key) {
            items.iter_mut().for_each(&mut *f);
        }
    }
    if let Some(Value::Array(messages)) = root.get_mut("messages") {
        for message in messages {
            if let Some(Value::Array(content)) = message.get_mut("content") {
                content.iter_mut().for_each(&mut *f);
            }
        }
    }
}

/// Go: `withEphemeralCacheControl`. Stamps `{"type":"ephemeral"}` onto a raw content block;
/// returns the input unchanged when it is not valid JSON.
pub fn with_ephemeral_cache_control(raw_block: &str) -> String {
    if !cpa_json::valid(raw_block.as_bytes()) {
        return raw_block.to_string();
    }
    let mut v = cpa_json::parse_str(raw_block);
    cpa_json::set(
        &mut v,
        "cache_control",
        CLAUDE_CODE_CACHE_CONTROL.to_value(),
    );
    cpa_json::to_string(&v)
}

/// Go: `ensureCacheControl`. Injects default breakpoints (tools fallback, last system block,
/// rolling message) after cloaking; each section injects independently.
pub fn ensure_cache_control(payload: &[u8]) -> Vec<u8> {
    edit(payload, |v| {
        let mut changed = false;
        if !has_cacheable_system(v) {
            changed |= inject_tools(v);
        }
        changed |= inject_system(v);
        changed |= inject_messages(v);
        changed
    })
}

/// Go: `claudePayloadHasCacheableSystem`. False for an absent key, empty array, or blank string.
pub fn claude_payload_has_cacheable_system(payload: &[u8]) -> bool {
    has_cacheable_system(&crate::helps::parse_cache::parse(payload))
}

fn has_cacheable_system(v: &Value) -> bool {
    match v.get("system") {
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::String(s)) => !s.trim().is_empty(),
        _ => false,
    }
}

/// Go: `upgradeClaudeCacheControlTTL`. Adds `ttl` to every existing ttl-less marker, rebuilding
/// the object as `{type, ttl, scope?}`; never creates a marker.
pub fn upgrade_claude_cache_control_ttl(payload: &[u8], ttl: &str) -> Vec<u8> {
    if ttl.is_empty() || payload.is_empty() || !cpa_json::valid(payload) {
        return payload.to_vec();
    }
    edit(payload, |v| {
        let mut changed = false;
        for_each_block_mut(v, &mut |block| {
            let Some(cc) = block.get_mut("cache_control") else {
                return;
            };
            let upgraded = match cc.as_object() {
                Some(m) if !m.contains_key("ttl") => match m.get("type") {
                    Some(Value::String(kind)) => {
                        let mut up = Map::new();
                        up.insert("type".into(), Value::String(kind.clone()));
                        up.insert("ttl".into(), Value::String(ttl.into()));
                        if let Some(scope) = m.get("scope") {
                            up.insert("scope".into(), scope.clone());
                        }
                        up
                    }
                    _ => return,
                },
                _ => return,
            };
            *cc = Value::Object(upgraded);
            changed = true;
        });
        changed
    })
}

/// Go: `stripClaudeCacheControlTTL`. Removes `ttl` from every cache_control object.
pub fn strip_claude_cache_control_ttl(payload: &[u8]) -> Vec<u8> {
    if payload.is_empty() || !cpa_json::valid(payload) {
        return payload.to_vec();
    }
    edit(payload, |v| {
        let mut changed = false;
        for_each_block_mut(v, &mut |block| {
            if let Some(Value::Object(cc)) = block.get_mut("cache_control") {
                changed |= cc.shift_remove("ttl").is_some();
            }
        });
        changed
    })
}

/// Go: `shouldEnsureCacheControl`.
pub fn should_ensure_cache_control(
    payload: &[u8],
    cloaked: bool,
    confirmed_claude_code: bool,
) -> bool {
    !confirmed_claude_code && (cloaked || count_cache_controls(payload) == 0)
}

/// Go: `countCacheControls`.
pub fn count_cache_controls(payload: &[u8]) -> usize {
    count_in(&crate::helps::parse_cache::parse(payload))
}

fn count_in(v: &Value) -> usize {
    blocks(v).filter(|b| has_cache_control(b)).count()
}

/// Go: `normalizeCacheControlTTL`. Once a non-1h marker is seen (tools, system, messages
/// order), every later 1h ttl is deleted.
pub fn normalize_cache_control_ttl(payload: &[u8]) -> Vec<u8> {
    if payload.is_empty() || !cpa_json::valid(payload) {
        return payload.to_vec();
    }
    edit(payload, |v| {
        let mut seen_5m = false;
        let mut modified = false;
        for_each_block_mut(v, &mut |block| {
            let Some(cc) = block.get_mut("cache_control") else {
                return;
            };
            let Value::Object(m) = cc else {
                seen_5m = true;
                return;
            };
            if !matches!(m.get("ttl"), Some(Value::String(s)) if s == "1h") {
                seen_5m = true;
                return;
            }
            if seen_5m {
                m.shift_remove("ttl");
                modified = true;
            }
        });
        modified
    })
}

/// Removes `cache_control` from `items` earliest-first while `excess > 0`. With
/// `preserve_last`, the last marked item is skipped (and nothing happens if none is marked).
fn strip_items(items: &mut [Value], preserve_last: bool, excess: &mut usize) {
    let last = if preserve_last {
        match items.iter().rposition(has_cache_control) {
            Some(i) => Some(i),
            None => return,
        }
    } else {
        None
    };
    for (i, item) in items.iter_mut().enumerate() {
        if *excess == 0 {
            return;
        }
        if Some(i) != last
            && let Value::Object(m) = item
            && m.shift_remove("cache_control").is_some()
        {
            *excess -= 1;
        }
    }
}

fn strip_section(v: &mut Value, key: &str, preserve_last: bool, excess: &mut usize) {
    if let Some(Value::Array(items)) = v.get_mut(key) {
        strip_items(items, preserve_last, excess);
    }
}

/// Go: `enforceCacheControlLimit`. Strips excess markers: system (keep last), tools (keep last),
/// message blocks, then the last system, then the last tool.
pub fn enforce_cache_control_limit(payload: &[u8], max_blocks: usize) -> Vec<u8> {
    if payload.is_empty() || !cpa_json::valid(payload) {
        return payload.to_vec();
    }
    edit(payload, |v| {
        let total = count_in(v);
        if total <= max_blocks {
            return false;
        }
        let mut excess = total - max_blocks;
        strip_section(v, "system", true, &mut excess);
        strip_section(v, "tools", true, &mut excess);
        if let Some(Value::Array(messages)) = v.get_mut("messages") {
            for message in messages {
                if let Some(Value::Array(content)) = message.get_mut("content") {
                    strip_items(content, false, &mut excess);
                }
            }
        }
        strip_section(v, "system", false, &mut excess);
        strip_section(v, "tools", false, &mut excess);
        true
    })
}

/// Go: `injectMessagesCacheControl`. Marks the message the native rolling selector picks.
pub fn inject_messages_cache_control(payload: &[u8]) -> Vec<u8> {
    edit(payload, inject_messages)
}

fn inject_messages(v: &mut Value) -> bool {
    let messages = array_of(v, "messages");
    let Some(last_message_index) = messages.len().checked_sub(1) else {
        return false;
    };
    let Some(last_eligible) = messages.iter().rposition(|m| {
        matches!(m.g("role").str().as_str(), "user" | "assistant")
            && claude_message_eligible_for_rolling_cache(m)
    }) else {
        return false;
    };

    // Final-system special case: non-blank STRING content becomes one freshly marked text block.
    let final_message = &messages[last_message_index];
    if final_message.g("role").str() == "system"
        && let Some(Value::String(text)) = final_message.get("content")
        && !text.trim().is_empty()
    {
        let text = text.clone();
        return inject_final_system(v, last_message_index, &text);
    }

    let content = messages[last_eligible].get("content");
    if content.is_some_and(message_content_has_cache_control) {
        return false;
    }
    match content {
        Some(Value::Array(items)) if !items.is_empty() => {
            let path = format!(
                "messages.{last_eligible}.content.{}.cache_control",
                items.len() - 1
            );
            cpa_json::set(v, &path, CLAUDE_CODE_CACHE_CONTROL.to_value())
        }
        Some(Value::String(text)) => {
            let block = build_text_block_value(text, Some(&CLAUDE_CODE_CACHE_CONTROL));
            cpa_json::set(
                v,
                &format!("messages.{last_eligible}.content"),
                Value::Array(vec![block]),
            )
        }
        _ => false,
    }
}

/// Go: `claudeMessageEligibleForRollingCache`. An assistant turn whose last block is
/// thinking-like cannot host the marker.
pub fn claude_message_eligible_for_rolling_cache(message: &Value) -> bool {
    match message.get("content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(items)) if !items.is_empty() => {
            if message.g("role").str() != "assistant" {
                return true;
            }
            let last_type = items.last().map(|b| b.g("type").str()).unwrap_or_default();
            !matches!(last_type.as_str(), "thinking" | "redacted_thinking")
        }
        _ => false,
    }
}

/// Go: `injectClaudeFinalSystemCacheControl`. Replaces the string content of message
/// `message_index` with a single marked text block.
pub fn inject_claude_final_system_cache_control(
    payload: &[u8],
    message_index: usize,
    text: &str,
) -> Vec<u8> {
    edit(payload, |v| inject_final_system(v, message_index, text))
}

fn inject_final_system(v: &mut Value, message_index: usize, text: &str) -> bool {
    let block = build_text_block_value(text, Some(&CLAUDE_CODE_CACHE_CONTROL));
    cpa_json::set(
        v,
        &format!("messages.{message_index}.content"),
        Value::Array(vec![block]),
    )
}

/// Go: `messageContentHasCacheControl`. Only array content can carry markers.
pub fn message_content_has_cache_control(content: &Value) -> bool {
    content
        .as_array()
        .is_some_and(|a| a.iter().any(has_cache_control))
}

/// Go: `injectToolsCacheControl`. Marks the last non-`defer_loading` tool unless any tool
/// already has a marker.
pub fn inject_tools_cache_control(payload: &[u8]) -> Vec<u8> {
    edit(payload, inject_tools)
}

fn inject_tools(v: &mut Value) -> bool {
    let Some(Value::Array(tools)) = v.get("tools") else {
        return false;
    };
    let mut last_eligible = None;
    for (i, tool) in tools.iter().enumerate() {
        if has_cache_control(tool) {
            return false;
        }
        if !tool.g("defer_loading").bool() {
            last_eligible = Some(i);
        }
    }
    let Some(i) = last_eligible else { return false };
    cpa_json::set(
        v,
        &format!("tools.{i}.cache_control"),
        CLAUDE_CODE_CACHE_CONTROL.to_value(),
    )
}

/// Go: `injectSystemCacheControl`. Marks the last system block (string systems become a text
/// block array) unless a system block already has a marker.
pub fn inject_system_cache_control(payload: &[u8]) -> Vec<u8> {
    edit(payload, inject_system)
}

fn inject_system(v: &mut Value) -> bool {
    match v.get("system") {
        Some(Value::Array(items)) => {
            if items.is_empty() || items.iter().any(has_cache_control) {
                return false;
            }
            let path = format!("system.{}.cache_control", items.len() - 1);
            cpa_json::set(v, &path, CLAUDE_CODE_CACHE_CONTROL.to_value())
        }
        // Blank strings are not cacheable hosts (see `has_cacheable_system`).
        Some(Value::String(s)) if !s.trim().is_empty() => {
            let block = build_text_block_value(s, Some(&CLAUDE_CODE_CACHE_CONTROL));
            cpa_json::set(v, "system", Value::Array(vec![block]))
        }
        _ => false,
    }
}

/// Go: `ensureModelMaxTokens`. Fills a missing `max_tokens` for models the registry knows as
/// Claude (registered `MaxCompletionTokens`, else 1024); unregistered models stay unset.
pub fn ensure_model_max_tokens(body: &[u8], model_id: &str) -> Vec<u8> {
    if body.is_empty() || !cpa_json::valid(body) {
        return body.to_vec();
    }
    let mut v = cpa_json::parse(body);
    if v.g("max_tokens").exists() {
        return body.to_vec();
    }
    let model_id = model_id.trim();
    let registry = global_registry();
    if !registry
        .get_model_providers(model_id)
        .iter()
        .any(|p| p.eq_ignore_ascii_case("claude"))
    {
        return body.to_vec();
    }
    let max_tokens = registry
        .get_model_info(model_id, "claude")
        .map(|info| info.max_completion_tokens)
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MODEL_MAX_TOKENS);
    cpa_json::set(&mut v, "max_tokens", max_tokens);
    cpa_json::to_vec(&v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_core::registry::ModelInfo;

    fn gs(b: &[u8], p: &str) -> String {
        cpa_json::parse(b).g(p).str()
    }
    fn ex(b: &[u8], p: &str) -> bool {
        cpa_json::parse(b).g(p).exists()
    }
    fn text(b: &[u8]) -> &str {
        std::str::from_utf8(b).expect("utf8")
    }

    // Go: TestEnsureCacheControl (caching_verify_test.go)
    #[test]
    fn ensure_system_string_and_array() {
        let out = ensure_cache_control(
            br#"{"model":"m","system":"This is a long system prompt","messages":[]}"#,
        );
        assert_eq!(gs(&out, "system.0.cache_control.type"), "ephemeral");
        assert_eq!(gs(&out, "system.0.text"), "This is a long system prompt");

        let out = ensure_cache_control(
            br#"{"system":[{"type":"text","text":"Part 1"},{"type":"text","text":"Part 2"}],"messages":[]}"#,
        );
        assert!(!ex(&out, "system.0.cache_control"));
        assert_eq!(gs(&out, "system.1.cache_control.type"), "ephemeral");
    }

    #[test]
    fn ensure_tools_not_stamped_when_system_cacheable() {
        let out = ensure_cache_control(
            br#"{"tools":[{"name":"t1"},{"name":"t2"}],"system":"System prompt","messages":[]}"#,
        );
        assert!(!ex(&out, "tools.0.cache_control") && !ex(&out, "tools.1.cache_control"));
        assert_eq!(gs(&out, "system.0.cache_control.type"), "ephemeral");
        assert!(!ex(&out, "system.0.cache_control.ttl"));
        // An existing tool marker does not stop the independent system breakpoint.
        let out = ensure_cache_control(
            br#"{"tools":[{"name":"t","cache_control":{"type":"ephemeral"}}],"system":[{"type":"text","text":"S"}],"messages":[]}"#,
        );
        assert_eq!(gs(&out, "tools.0.cache_control.type"), "ephemeral");
        assert_eq!(gs(&out, "system.0.cache_control.type"), "ephemeral");
    }

    #[test]
    fn ensure_tools_fallback_without_usable_system() {
        let out = ensure_cache_control(
            br#"{"tools":[{"name":"tool1"},{"name":"tool2"}],"messages":[{"role":"user","content":"Hi"}]}"#,
        );
        assert!(!ex(&out, "tools.0.cache_control"));
        assert_eq!(gs(&out, "tools.1.cache_control.type"), "ephemeral");
        assert!(!ex(&out, "tools.1.cache_control.ttl"));
        assert_eq!(
            gs(&out, "messages.0.content.0.cache_control.type"),
            "ephemeral"
        );

        for system in [r#""system":[],"#, r#""system":"","#, r#""system":"   ","#] {
            let body = format!(
                r#"{{{system}"tools":[{{"name":"tool1"}}],"messages":[{{"role":"user","content":"Hi"}}]}}"#
            );
            let out = ensure_cache_control(body.as_bytes());
            assert_eq!(
                gs(&out, "tools.0.cache_control.type"),
                "ephemeral",
                "{system}"
            );
            assert!(
                !text(&out).contains(r#""text":"""#) && !text(&out).contains(r#""text":"   ""#)
            );
            assert!(!ex(&out, "system.0.cache_control"));
            assert_eq!(count_cache_controls(&out), 2, "{system}");
        }
    }

    #[test]
    fn ensure_many_tools_and_inject_tools_helper() {
        let tools: Vec<String> = (0..50)
            .map(|i| format!(r#"{{"name":"tool{i}"}}"#))
            .collect();
        let body = format!(
            r#"{{"tools":[{}],"system":[{{"type":"text","text":"You are Claude Code"}}],"messages":[{{"role":"user","content":"Hello"}}]}}"#,
            tools.join(",")
        );
        let out = ensure_cache_control(body.as_bytes());
        assert!((0..50).all(|i| !ex(&out, &format!("tools.{i}.cache_control"))));
        assert_eq!(
            gs(
                &inject_tools_cache_control(body.as_bytes()),
                "tools.49.cache_control.type"
            ),
            "ephemeral"
        );
        assert_eq!(gs(&out, "system.0.cache_control.type"), "ephemeral");
        assert_eq!(
            gs(&out, "messages.0.content.0.cache_control.type"),
            "ephemeral"
        );
        // Empty tools array is harmless.
        let out = ensure_cache_control(br#"{"tools":[],"system":"Test","messages":[]}"#);
        assert_eq!(gs(&out, "system.0.cache_control.type"), "ephemeral");
    }

    #[test]
    fn ensure_messages_rolling_marker() {
        let out = ensure_cache_control(
            br#"{"messages":[{"role":"user","content":"First"},{"role":"assistant","content":"A"},{"role":"user","content":"Second"},{"role":"assistant","content":"A2"},{"role":"user","content":"Third"}]}"#,
        );
        assert_eq!(
            gs(&out, "messages.4.content.0.cache_control.type"),
            "ephemeral"
        );
        assert!(!ex(&out, "messages.4.content.0.cache_control.ttl"));
        assert!(!ex(&out, "messages.2.content.0.cache_control"));

        // A first-user marker (from cloaking) must not suppress the rolling marker.
        let out = ensure_cache_control(
            br#"{"tools":[{"name":"Read"}],"system":"You are helpful.","messages":[{"role":"user","content":[{"type":"text","text":"currentDate"},{"type":"text","text":"First","cache_control":{"type":"ephemeral"}}]},{"role":"assistant","content":[{"type":"text","text":"A"}]},{"role":"user","content":[{"type":"text","text":"Third"}]}]}"#,
        );
        assert_eq!(
            gs(&out, "messages.0.content.1.cache_control.type"),
            "ephemeral"
        );
        assert_eq!(
            gs(&out, "messages.2.content.0.cache_control.type"),
            "ephemeral"
        );
        assert!(!ex(&out, "tools.0.cache_control"));
        assert_eq!(gs(&out, "system.0.cache_control.type"), "ephemeral");

        // An existing marker on the latest turn is left alone and nothing else is invented.
        let out = ensure_cache_control(
            br#"{"messages":[{"role":"user","content":[{"type":"text","text":"F"}]},{"role":"user","content":[{"type":"text","text":"S","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#,
        );
        assert_eq!(gs(&out, "messages.1.content.0.cache_control.ttl"), "1h");
        assert!(!ex(&out, "messages.0.content.0.cache_control"));
    }

    #[test]
    fn ensure_messages_final_system_and_thinking_rules() {
        let out = ensure_cache_control(
            br#"{"messages":[{"role":"user","content":"User"},{"role":"assistant","content":"Assistant"},{"role":"system","content":"Internal system"}]}"#,
        );
        let c = cpa_json::parse(&out).g("messages.2.content").value();
        assert_eq!(c.as_array().map(Vec::len), Some(1));
        assert_eq!(gs(&out, "messages.2.content.0.text"), "Internal system");
        assert_eq!(
            gs(&out, "messages.2.content.0.cache_control.type"),
            "ephemeral"
        );
        assert!(!ex(&out, "messages.1.content.0.cache_control"));

        // Array content on the trailing system turn falls back to the last eligible turn.
        let out = ensure_cache_control(
            br#"{"messages":[{"role":"user","content":"User"},{"role":"assistant","content":"Assistant"},{"role":"system","content":[{"type":"text","text":"I1"},{"type":"text","text":"I2"}]}]}"#,
        );
        assert!(!ex(&out, "messages.2.content.1.cache_control"));
        assert_eq!(
            gs(&out, "messages.1.content.0.cache_control.type"),
            "ephemeral"
        );

        // Trailing assistant text is promoted in native key order.
        let out = ensure_cache_control(
            br#"{"messages":[{"role":"user","content":"User"},{"role":"assistant","content":"Assistant prefill"}]}"#,
        );
        assert!(text(&out).contains(
            r#"[{"type":"text","text":"Assistant prefill","cache_control":{"type":"ephemeral"}}]"#
        ));
        assert!(!ex(&out, "messages.0.content.0.cache_control"));

        // A trailing assistant thinking block is ineligible; internal system turns are skipped.
        let out = ensure_cache_control(
            br#"{"messages":[{"role":"user","content":"User"},{"role":"system","content":"Internal system"},{"role":"assistant","content":[{"type":"text","text":"A"},{"type":"thinking","thinking":"I"}]}]}"#,
        );
        assert_eq!(
            gs(&out, "messages.0.content.0.cache_control.type"),
            "ephemeral"
        );
        assert!(cpa_json::parse(&out).g("messages.1.content").is_string());
        assert!(!ex(&out, "messages.2.content.1.cache_control"));
    }

    #[test]
    fn ensure_wire_shape_and_escaping() {
        let out = ensure_cache_control(
            br#"{"system":[{"type":"text","text":"System"}],"messages":[{"role":"user","content":[{"type":"text","text":"User"}]}]}"#,
        );
        assert_eq!(
            text(&out)
                .matches(r#""cache_control":{"type":"ephemeral"}"#)
                .count(),
            2
        );
        let up = upgrade_claude_cache_control_ttl(&out, CLAUDE_CACHE_CONTROL_TTL_1H);
        assert_eq!(
            text(&up)
                .matches(r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#)
                .count(),
            2
        );

        let out = ensure_cache_control(
            br#"{"system":"System <tag> &","messages":[{"role":"user","content":"User <tag> &"}]}"#,
        );
        assert!(text(&out).contains(
            r#""system":[{"type":"text","text":"System <tag> &","cache_control":{"type":"ephemeral"}}]"#
        ));
        assert!(text(&out).contains(
            r#""content":[{"type":"text","text":"User <tag> &","cache_control":{"type":"ephemeral"}}]"#
        ));

        let global = br#"{"system":[{"type":"text","text":"Global","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}}],"messages":[{"role":"user","content":"User"}]}"#;
        let out = ensure_cache_control(global);
        assert!(
            text(&out)
                .contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#)
        );
    }

    // Verified against the Go reference (throwaway oracle): a non-object trailing block is
    // replaced by the marker object, unusable messages leave the body byte-identical.
    #[test]
    fn ensure_oracle_edge_cases() {
        let body = br#"{"system":"s","tools":[{"name":"a","defer_loading":"true"},{"name":"b"}],"messages":[{"role":"user","content":[{"type":"text","text":"u"},"str"]}]}"#;
        assert_eq!(
            text(&ensure_cache_control(body)),
            r#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"a","defer_loading":"true"},{"name":"b"}],"messages":[{"role":"user","content":[{"type":"text","text":"u"},{"cache_control":{"type":"ephemeral"}}]}]}"#
        );
        let none = br#"{"messages":[{"role":"user","content":[]},{"role":"assistant","content":[{"type":"thinking","thinking":"x"}]}]}"#;
        assert_eq!(ensure_cache_control(none), none);
        let body = br#"{"tools":[{"name":"a","defer_loading":true}],"messages":[{"role":"user","content":"x"},{"role":"system","content":"final"}]}"#;
        assert_eq!(
            text(&ensure_cache_control(body)),
            r#"{"tools":[{"name":"a","defer_loading":true}],"messages":[{"role":"user","content":"x"},{"role":"system","content":[{"type":"text","text":"final","cache_control":{"type":"ephemeral"}}]}]}"#
        );
    }

    // Go: TestShouldEnsureCacheControl
    #[test]
    fn should_ensure_matrix() {
        let markerless = br#"{"messages":[{"role":"user","content":"x"}]}"#;
        let marked = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}]}"#;
        assert!(!should_ensure_cache_control(markerless, false, true));
        assert!(!should_ensure_cache_control(marked, false, true));
        assert!(should_ensure_cache_control(marked, true, false));
        assert!(should_ensure_cache_control(markerless, false, false));
        assert!(!should_ensure_cache_control(marked, false, false));
    }

    // Go: TestInjectToolsCacheControlSkipsDeferredTools
    #[test]
    fn inject_tools_skips_deferred() {
        let cases: [(&str, Option<usize>); 5] = [
            (
                r#"{"tools":[{"name":"r","defer_loading":false},{"name":"d","defer_loading":true}]}"#,
                Some(0),
            ),
            (
                r#"{"tools":[{"name":"r"},{"name":"d1","defer_loading":true},{"name":"d2","defer_loading":true}]}"#,
                Some(0),
            ),
            (
                r#"{"tools":[{"name":"r1"},{"name":"d","defer_loading":true},{"name":"r2"}]}"#,
                Some(2),
            ),
            (
                r#"{"tools":[{"name":"d1","defer_loading":true},{"name":"d2","defer_loading":true}]}"#,
                None,
            ),
            (
                r#"{"tools":[{"name":"r1","cache_control":{"type":"ephemeral","ttl":"1h"}},{"name":"r2"}]}"#,
                Some(0),
            ),
        ];
        for (input, want) in cases {
            let out = inject_tools_cache_control(input.as_bytes());
            let marked: Vec<usize> = (0..3)
                .filter(|i| ex(&out, &format!("tools.{i}.cache_control")))
                .collect();
            assert_eq!(marked, want.into_iter().collect::<Vec<_>>(), "{input}");
        }
        let out = inject_tools_cache_control(cases[4].0.as_bytes());
        assert_eq!(gs(&out, "tools.0.cache_control.ttl"), "1h");
    }

    // Go: TestUpgradeClaudeCacheControlTTL
    #[test]
    fn upgrade_ttl() {
        let input = br#"{"tools":[{"name":"t","cache_control":{"type":"ephemeral"}},{"name":"u"}],"system":[{"type":"text","text":"s0"},{"type":"text","text":"s1","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b","cache_control":{"type":"ephemeral"}}]}]}"#;
        let out = upgrade_claude_cache_control_ttl(input, "1h");
        for p in ["tools.0", "system.1", "messages.0.content.1"] {
            assert_eq!(gs(&out, &format!("{p}.cache_control.ttl")), "1h", "{p}");
        }
        for p in ["tools.1", "system.0", "messages.0.content.0"] {
            assert!(!ex(&out, &format!("{p}.cache_control")), "{p}");
        }
        assert_eq!(count_cache_controls(&out), 3);

        let kept = br#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"5m"}}]}"#;
        assert_eq!(
            gs(
                &upgrade_claude_cache_control_ttl(kept, "1h"),
                "system.0.cache_control.ttl"
            ),
            "5m"
        );
        let once = upgrade_claude_cache_control_ttl(
            br#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"}}]}"#,
            "1h",
        );
        assert_eq!(upgrade_claude_cache_control_ttl(&once, "1h"), once);

        let scoped = br#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","scope":"global"}}]}"#;
        assert!(
            text(&upgrade_claude_cache_control_ttl(scoped, "1h"))
                .contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#)
        );

        assert_eq!(upgrade_claude_cache_control_ttl(scoped, ""), scoped);
        assert_eq!(
            upgrade_claude_cache_control_ttl(b"not json", "1h"),
            b"not json"
        );

        // Go oracle: extra keys are dropped, a null scope is kept, odd markers are skipped.
        let odd = br#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","extra":1,"scope":null}},{"type":"text","cache_control":{"type":5}},{"type":"text","cache_control":null}]}"#;
        assert_eq!(
            text(&upgrade_claude_cache_control_ttl(odd, "1h")),
            r#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"1h","scope":null}},{"type":"text","cache_control":{"type":5}},{"type":"text","cache_control":null}]}"#
        );
    }

    #[test]
    fn strip_ttl() {
        let body = br#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}}],"tools":[{"cache_control":"x"}]}"#;
        assert_eq!(
            text(&strip_claude_cache_control_ttl(body)),
            r#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","scope":"global"}}],"tools":[{"cache_control":"x"}]}"#
        );
        let clean = br#"{"system":[{"cache_control":{"type":"ephemeral"}}]}"#;
        assert_eq!(strip_claude_cache_control_ttl(clean), clean);
    }

    // Go: TestNormalizeCacheControlTTL_*
    #[test]
    fn normalize_ttl() {
        let body = br#"{"tools":[{"name":"t1","cache_control":{"type":"ephemeral","ttl":"1h"}}],"system":[{"type":"text","text":"s1","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","text":"u1","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        let out = normalize_cache_control_ttl(body);
        assert_eq!(gs(&out, "tools.0.cache_control.ttl"), "1h");
        assert!(!ex(&out, "messages.0.content.0.cache_control.ttl"));

        // Untouched bytes (including HTML characters) when nothing needs to change.
        let same = br#"{"tools":[{"name":"t1","cache_control":{"type":"ephemeral","ttl":"1h"}}],"system":[{"type":"text","text":"<system-reminder>foo & bar</system-reminder>","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
        assert_eq!(normalize_cache_control_ttl(same), same);

        // Top-level key order survives a modification.
        let body = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"u1","cache_control":{"type":"ephemeral","ttl":"1h"}}]}],"tools":[{"name":"t1","cache_control":{"type":"ephemeral"}}],"system":[{"type":"text","text":"s1","cache_control":{"type":"ephemeral"}}]}"#;
        let out = normalize_cache_control_ttl(body);
        assert!(!ex(&out, "messages.0.content.0.cache_control.ttl"));
        let s = text(&out);
        let idx = |k: &str| s.find(k).expect(k);
        assert!(
            idx(r#""model""#) < idx(r#""messages""#)
                && idx(r#""messages""#) < idx(r#""tools""#)
                && idx(r#""tools""#) < idx(r#""system""#)
        );

        // Go oracle: a non-object marker counts as a 5m block.
        let weird = br#"{"system":[{"cache_control":"weird"},{"cache_control":{"type":"ephemeral","ttl":"1h"}},{"cache_control":{"type":"ephemeral","ttl":"5m"}}],"messages":[{"role":"user","content":[{"cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        assert_eq!(
            text(&normalize_cache_control_ttl(weird)),
            r#"{"system":[{"cache_control":"weird"},{"cache_control":{"type":"ephemeral"}},{"cache_control":{"type":"ephemeral","ttl":"5m"}}],"messages":[{"role":"user","content":[{"cache_control":{"type":"ephemeral"}}]}]}"#
        );
    }

    // Go: TestEnforceCacheControlLimit_*
    #[test]
    fn enforce_limit_phases() {
        let body = br#"{"tools":[{"name":"t1","cache_control":{"type":"ephemeral"}},{"name":"t2","cache_control":{"type":"ephemeral"}}],"system":[{"type":"text","text":"s1","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","text":"u1","cache_control":{"type":"ephemeral"}}]},{"role":"user","content":[{"type":"text","text":"u2","cache_control":{"type":"ephemeral"}}]}]}"#;
        let out = enforce_cache_control_limit(body, 4);
        assert_eq!(count_cache_controls(&out), 4);
        assert!(!ex(&out, "tools.0.cache_control") && ex(&out, "tools.1.cache_control"));
        assert!(
            ex(&out, "messages.0.content.0.cache_control")
                && ex(&out, "messages.1.content.0.cache_control")
        );

        let body = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"u1","cache_control":{"type":"ephemeral"}},{"type":"text","text":"u2","cache_control":{"type":"ephemeral"}}]}],"tools":[{"name":"t1","cache_control":{"type":"ephemeral"}},{"name":"t2","cache_control":{"type":"ephemeral"}}],"system":[{"type":"text","text":"s1","cache_control":{"type":"ephemeral"}}]}"#;
        let out = enforce_cache_control_limit(body, 4);
        assert_eq!(count_cache_controls(&out), 4);
        assert!(!ex(&out, "tools.0.cache_control"));
        let s = text(&out);
        let idx = |k: &str| s.find(k).expect(k);
        assert!(
            idx(r#""model""#) < idx(r#""messages""#)
                && idx(r#""messages""#) < idx(r#""tools""#)
                && idx(r#""tools""#) < idx(r#""system""#)
        );

        let tools_only = br#"{"tools":[{"cache_control":{"type":"ephemeral"}},{"cache_control":{"type":"ephemeral"}},{"cache_control":{"type":"ephemeral"}},{"cache_control":{"type":"ephemeral"}},{"cache_control":{"type":"ephemeral"}}]}"#;
        let out = enforce_cache_control_limit(tools_only, 4);
        assert_eq!(count_cache_controls(&out), 4);
        assert!(!ex(&out, "tools.0.cache_control") && ex(&out, "tools.4.cache_control"));

        let under = br#"{"tools":[{"cache_control":{"type":"ephemeral"}}]}"#;
        assert_eq!(enforce_cache_control_limit(under, 4), under);
    }

    // Go oracle: message blocks go before the last system and the last tool, which go last.
    #[test]
    fn enforce_limit_removal_order() {
        let mc = r#"{"cache_control":{"type":"ephemeral"}}"#;
        let body = format!(
            r#"{{"tools":[{mc}],"system":[{mc},{mc}],"messages":[{{"role":"user","content":[{mc},{mc}]}},{{"role":"user","content":[{mc}]}}]}}"#
        );
        assert_eq!(
            text(&enforce_cache_control_limit(body.as_bytes(), 2)),
            r#"{"tools":[{"cache_control":{"type":"ephemeral"}}],"system":[{},{"cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{},{}]},{"role":"user","content":[{}]}]}"#
        );
        let body = format!(
            r#"{{"tools":[{mc}],"system":[{mc},{mc}],"messages":[{{"role":"user","content":[{mc}]}}]}}"#
        );
        assert_eq!(
            text(&enforce_cache_control_limit(body.as_bytes(), 0)),
            r#"{"tools":[{}],"system":[{},{}],"messages":[{"role":"user","content":[{}]}]}"#
        );
    }

    #[test]
    fn with_ephemeral_replaces_in_place() {
        assert_eq!(
            with_ephemeral_cache_control(
                r#"{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"},"z":1}"#
            ),
            r#"{"type":"text","text":"x","cache_control":{"type":"ephemeral"},"z":1}"#
        );
        assert_eq!(
            with_ephemeral_cache_control(r#"{"type":"text","text":"x"}"#),
            r#"{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}"#
        );
        assert_eq!(with_ephemeral_cache_control("nope"), "nope");
    }

    // Go: TestEnsureModelMaxTokens_*
    #[test]
    fn max_tokens_uses_registry() {
        let reg = global_registry();
        let register = |client: &str, model: &str, max: i64| {
            reg.register_client(
                client,
                "claude",
                &[ModelInfo {
                    id: model.into(),
                    r#type: "claude".into(),
                    max_completion_tokens: max,
                    ..Default::default()
                }],
            );
        };
        register("cc-max-client", "cc-max-model", 4096);
        register("cc-default-client", "cc-default-model", 0);

        let body = |model: &str| {
            format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}]}}"#)
        };
        let out = ensure_model_max_tokens(body("cc-max-model").as_bytes(), "cc-max-model");
        assert_eq!(cpa_json::parse(&out).g("max_tokens").int(), 4096);
        let out =
            ensure_model_max_tokens(body("cc-default-model").as_bytes(), " cc-default-model ");
        assert_eq!(
            cpa_json::parse(&out).g("max_tokens").int(),
            DEFAULT_MODEL_MAX_TOKENS
        );

        let explicit = br#"{"model":"cc-max-model","max_tokens":2048}"#;
        assert_eq!(ensure_model_max_tokens(explicit, "cc-max-model"), explicit);
        let unregistered = body("cc-unregistered-model");
        assert!(!ex(
            &ensure_model_max_tokens(unregistered.as_bytes(), "cc-unregistered-model"),
            "max_tokens"
        ));

        reg.unregister_client("cc-max-client");
        reg.unregister_client("cc-default-client");
    }
}

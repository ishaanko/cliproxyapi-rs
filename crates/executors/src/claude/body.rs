//! Pure Claude request/response body helpers (Go: executor/claude_executor_request.go,
//! claude_executor.go). Bodies are `&[u8]` in, `Vec<u8>` out; the input bytes are returned
//! untouched when nothing changes, like the Go byte-level sjson edits.

use cpa_json::{J, Value};

/// Parses `payload`, applies `f`, and re-serializes only when `f` reports a change.
fn edit(payload: &[u8], f: impl FnOnce(&mut Value) -> bool) -> Vec<u8> {
    crate::helps::parse_cache::edit(payload, f)
}

/// Removes a top-level key (order-preserving); true when it existed.
fn remove_key(v: &mut Value, key: &str) -> bool {
    v.as_object_mut()
        .is_some_and(|m| m.shift_remove(key).is_some())
}

/// gjson `Result.Num`: the numeric value for JSON numbers, 0 for every other type.
fn num_field(v: &Value, path: &str) -> Option<f64> {
    let r = v.g(path);
    r.exists()
        .then(|| if r.is_number() { r.float() } else { 0.0 })
}

/// Go: `extractAndRemoveBetas`. Pulls the body `betas` (array or single string, trimmed, blanks
/// dropped) out of the body and returns them with the body minus the `betas` key.
pub fn extract_and_remove_betas(body: &[u8]) -> (Vec<String>, Vec<u8>) {
    let mut betas = Vec::new();
    let out = edit(body, |v| {
        {
            let r = v.g("betas");
            if !r.exists() {
                return false;
            }
            if r.is_array() {
                for item in r.array() {
                    let s = item.str();
                    if !s.trim().is_empty() {
                        betas.push(s.trim().to_string());
                    }
                }
            } else {
                let s = r.str();
                if !s.trim().is_empty() {
                    betas.push(s.trim().to_string());
                }
            }
        }
        cpa_json::delete(v, "betas");
        true
    });
    (betas, out)
}

/// Go: `disableThinkingIfToolChoiceForced`. `tool_choice.type` of `any`/`tool` removes
/// `thinking` and `output_config.effort` (and an emptied `output_config`).
pub fn disable_thinking_if_tool_choice_forced(body: &[u8]) -> Vec<u8> {
    edit(body, |v| {
        if !matches!(v.g("tool_choice.type").str().as_str(), "any" | "tool") {
            return false;
        }
        let mut changed = remove_key(v, "thinking");
        if let Some(Value::Object(oc)) = v.get_mut("output_config") {
            changed |= oc.shift_remove("effort").is_some();
            if oc.is_empty() {
                changed |= remove_key(v, "output_config");
            }
        }
        changed
    })
}

/// Go: `normalizeClaudeSamplingForUpstream`. Non-native callers lose `temperature`/`top_p`
/// (and `top_k` under active thinking); native-owned bodies only drop combinations Anthropic
/// rejects.
pub fn normalize_claude_sampling_for_upstream(body: &[u8], native_owned: bool) -> Vec<u8> {
    edit(body, |v| {
        let thinking_active = matches!(
            v.g("thinking.type").str().trim().to_lowercase().as_str(),
            "enabled" | "adaptive" | "auto"
        );

        if !native_owned {
            let mut changed = remove_key(v, "temperature");
            changed |= remove_key(v, "top_p");
            if thinking_active {
                changed |= remove_key(v, "top_k");
            }
            return changed;
        }

        if thinking_active {
            let mut changed = false;
            if num_field(v, "temperature").is_some_and(|n| n != 1.0) {
                changed |= remove_key(v, "temperature");
            }
            if num_field(v, "top_p").is_some_and(|n| n < 0.95) {
                changed |= remove_key(v, "top_p");
            }
            changed |= remove_key(v, "top_k");
            return changed;
        }
        // Anthropic accepts temperature or top_p but not both; top_p gives way.
        if v.g("temperature").exists() && v.g("top_p").exists() {
            return remove_key(v, "top_p");
        }
        false
    })
}

/// Go: `claudePayloadHasMidSystemMessage`. True when `messages` holds a `system` role turn
/// (case and surrounding whitespace ignored).
pub fn claude_payload_has_mid_system_message(payload: &[u8]) -> bool {
    let v = crate::helps::parse_cache::parse(payload);
    v.get("messages")
        .and_then(Value::as_array)
        .is_some_and(|ms| ms.iter().any(is_system_role))
}

fn is_system_role(message: &Value) -> bool {
    message
        .g("role")
        .str()
        .trim()
        .eq_ignore_ascii_case("system")
}

/// Go: `rebuildMidSystemMessagesToTopLevel`. Folds `system` role turns out of `messages` into
/// the top-level `system` array (existing system text first). Turns without usable text are
/// left in place and nothing changes when no text was found.
pub fn rebuild_mid_system_messages_to_top_level(payload: &[u8]) -> Vec<u8> {
    edit(payload, |v| {
        let Some(Value::Array(messages)) = v.get("messages") else {
            return false;
        };
        let moved: Vec<String> = messages
            .iter()
            .filter(|m| is_system_role(m))
            .flat_map(|m| claude_system_text_parts(&m.g("content")))
            .collect();
        if moved.is_empty() {
            return false;
        }
        let mut system_parts = claude_system_text_parts(&v.g("system"));
        system_parts.extend(moved);
        let _ = cpa_json::set_raw(v, "system", &raw_json_array(&system_parts));
        if let Some(Value::Array(messages)) = v.get_mut("messages") {
            messages.retain(|m| !is_system_role(m));
        }
        true
    })
}

/// Go: `claudeSystemTextParts`. Raw JSON text blocks from a system/message `content`: a
/// non-blank string becomes `{"type":"text","text":..}`, arrays keep non-blank string items
/// (wrapped) and non-blank `text` objects (verbatim); everything else is dropped.
pub fn claude_system_text_parts(content: &cpa_json::Res<'_>) -> Vec<String> {
    let text_block = |text: &str| serde_json::json!({"type": "text", "text": text}).to_string();
    match content.v() {
        Some(Value::String(s)) if !s.trim().is_empty() => vec![text_block(s)],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(s) if !s.trim().is_empty() => Some(text_block(s)),
                Value::Object(_)
                    if item.g("type").str() == "text"
                        && !item.g("text").str().trim().is_empty() =>
                {
                    Some(item.to_string())
                }
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

/// Go: `rawJSONArray`. Joins raw JSON items into a JSON array (`[]` when empty).
pub fn raw_json_array(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

/// Go: `sanitizeClaudeWebSearchDomains`. Deletes empty `allowed_domains`/`blocked_domains`
/// arrays from `web_search_*` tools (Anthropic rejects them as ambiguous).
pub fn sanitize_claude_web_search_domains(body: &[u8]) -> Vec<u8> {
    edit(body, |v| {
        let Some(Value::Array(tools)) = v.get_mut("tools") else {
            return false;
        };
        let mut changed = false;
        for tool in tools {
            if !tool.g("type").str().starts_with("web_search_") {
                continue;
            }
            let Value::Object(m) = tool else { continue };
            for field in ["allowed_domains", "blocked_domains"] {
                if matches!(m.get(field), Some(Value::Array(a)) if a.is_empty()) {
                    m.shift_remove(field);
                    changed = true;
                }
            }
        }
        changed
    })
}

/// Go: `restoreClaudeResponseModel`. Rewrites `model` / `message.model` to `model` in a JSON
/// payload or in a `data: {...}` SSE line (rebuilt as `<prefix>data: <json>`).
pub fn restore_claude_response_model(payload: &[u8], model: &str) -> Vec<u8> {
    if let Some(updated) = set_claude_response_model(payload, model) {
        return updated;
    }
    if !payload.trim_ascii().starts_with(b"data:") {
        return payload.to_vec();
    }
    let Some(data_index) = payload.windows(5).position(|w| w == b"data:") else {
        return payload.to_vec();
    };
    let raw_json = payload[data_index + 5..].trim_ascii();
    let Some(updated) = set_claude_response_model(raw_json, model) else {
        return payload.to_vec();
    };
    let mut rebuilt = Vec::with_capacity(data_index + 6 + updated.len());
    rebuilt.extend_from_slice(&payload[..data_index]);
    rebuilt.extend_from_slice(b"data: ");
    rebuilt.extend_from_slice(&updated);
    rebuilt
}

/// Go: `setClaudeResponseModel` (`Some(updated)` is Go's `changed == true`). Outer
/// whitespace of the payload is kept.
pub fn set_claude_response_model(payload: &[u8], model: &str) -> Option<Vec<u8>> {
    if !crate::helps::parse_cache::valid(payload) {
        return None;
    }
    let mut v = cpa_json::parse(payload);
    let mut changed = false;
    for path in ["model", "message.model"] {
        if v.g(path).exists() {
            changed |= cpa_json::set(&mut v, path, model);
        }
    }
    if !changed {
        return None;
    }
    let is_ws = |b: &u8| !matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let start = payload.iter().position(is_ws).unwrap_or(0);
    let end = payload.iter().rposition(is_ws).map_or(0, |i| i + 1);
    let mut out = Vec::with_capacity(payload.len());
    out.extend_from_slice(&payload[..start]);
    out.extend_from_slice(&cpa_json::to_vec(&v));
    out.extend_from_slice(&payload[end..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gs(b: &[u8], p: &str) -> String {
        cpa_json::parse(b).g(p).str()
    }
    fn ex(b: &[u8], p: &str) -> bool {
        cpa_json::parse(b).g(p).exists()
    }
    fn text(b: &[u8]) -> &str {
        std::str::from_utf8(b).expect("utf8")
    }

    // Go: TestSanitizeClaudeWebSearchDomains*
    #[test]
    fn web_search_domains() {
        let out = sanitize_claude_web_search_domains(
            br#"{"tools":[{"type":"web_search_20250305","name":"web_search","allowed_domains":["anthropic.com"],"blocked_domains":[],"max_uses":8}]}"#,
        );
        assert!(!ex(&out, "tools.0.blocked_domains"));
        assert_eq!(gs(&out, "tools.0.allowed_domains.0"), "anthropic.com");
        assert_eq!(gs(&out, "tools.0.max_uses"), "8");

        let out = sanitize_claude_web_search_domains(
            br#"{"tools":[{"type":"custom","name":"x","blocked_domains":[]},{"type":"web_search_20250305","name":"web_search","blocked_domains":["evil.com"]}]}"#,
        );
        assert!(ex(&out, "tools.0.blocked_domains"));
        assert_eq!(gs(&out, "tools.1.blocked_domains.0"), "evil.com");

        // Go oracle.
        assert_eq!(
            text(&sanitize_claude_web_search_domains(
                br#"{"tools":[{"type":"web_search_20250305","allowed_domains":[],"blocked_domains":[],"name":"w"},{"type":"x","allowed_domains":[]}]}"#
            )),
            r#"{"tools":[{"type":"web_search_20250305","name":"w"},{"type":"x","allowed_domains":[]}]}"#
        );
    }

    // Go: TestApplyClaudeHeaders_FastModeBetaIsConditional (extraction half) + Go oracle.
    #[test]
    fn betas_extraction() {
        let (betas, body) = extract_and_remove_betas(
            br#"{"model":"claude-opus-5","betas":["fast-mode-2026-02-01"]}"#,
        );
        assert_eq!(betas, ["fast-mode-2026-02-01"]);
        assert_eq!(text(&body), r#"{"model":"claude-opus-5"}"#);

        let (betas, body) =
            extract_and_remove_betas(br#"{"model":"m","betas":[" a ","",5,"b"],"x":1}"#);
        assert_eq!(betas, ["a", "5", "b"]);
        assert_eq!(text(&body), r#"{"model":"m","x":1}"#);
        let (betas, body) = extract_and_remove_betas(br#"{"betas":" solo ","x":1}"#);
        assert_eq!(
            (betas, text(&body)),
            (vec!["solo".to_string()], r#"{"x":1}"#)
        );
        let (betas, body) = extract_and_remove_betas(br#"{"betas":{"a":1},"x":1}"#);
        assert_eq!(
            (betas, text(&body)),
            (vec![r#"{"a":1}"#.to_string()], r#"{"x":1}"#)
        );
        let (betas, body) = extract_and_remove_betas(br#"{"betas":null,"x":1}"#);
        assert_eq!((betas.len(), text(&body)), (0, r#"{"x":1}"#));
        let (betas, body) = extract_and_remove_betas(br#"{"x":1}"#);
        assert_eq!((betas.len(), text(&body)), (0, r#"{"x":1}"#));
    }

    // Go: TestNormalizeClaudeSamplingForUpstream_* (non-native)
    #[test]
    fn sampling_non_native() {
        let out = normalize_claude_sampling_for_upstream(
            br#"{"temperature":0,"thinking":{"type":"adaptive"},"output_config":{"effort":"max"}}"#,
            false,
        );
        assert!(!ex(&out, "temperature"));

        let out = normalize_claude_sampling_for_upstream(
            br#"{"temperature":0.2,"top_p":0.9,"top_k":40,"thinking":{"type":"adaptive"}}"#,
            false,
        );
        assert!(!ex(&out, "temperature") && !ex(&out, "top_p") && !ex(&out, "top_k"));

        let out = normalize_claude_sampling_for_upstream(
            br#"{"temperature":0,"top_p":0.9,"top_k":40,"messages":[{"role":"user","content":"hi"}]}"#,
            false,
        );
        assert!(!ex(&out, "temperature") && !ex(&out, "top_p"));
        assert_eq!(gs(&out, "top_k"), "40");

        let out = disable_thinking_if_tool_choice_forced(
            br#"{"temperature":0,"thinking":{"type":"adaptive"},"output_config":{"effort":"max"},"tool_choice":{"type":"any"}}"#,
        );
        let out = normalize_claude_sampling_for_upstream(&out, false);
        assert!(!ex(&out, "thinking") && !ex(&out, "temperature"));
    }

    // Go: TestNormalizeClaudeSamplingForUpstreamNative*
    #[test]
    fn sampling_native_owned() {
        let helper = br#"{"model":"claude-haiku-4-5-20251001","max_tokens":32000,"thinking":{"type":"disabled"},"temperature":1,"stream":true}"#;
        assert_eq!(normalize_claude_sampling_for_upstream(helper, true), helper);

        // (payload, kept field -> value, dropped fields)
        type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [&'a str]);
        let cases: [Case; 9] = [
            (
                r#"{"temperature":0.5,"top_k":40}"#,
                &[("temperature", "0.5"), ("top_k", "40")],
                &[],
            ),
            (
                r#"{"temperature":0.5,"top_p":0.9}"#,
                &[("temperature", "0.5")],
                &["top_p"],
            ),
            (r#"{"top_p":0.9}"#, &[("top_p", "0.9")], &[]),
            (
                r#"{"temperature":1,"thinking":{"type":"disabled"}}"#,
                &[("temperature", "1")],
                &[],
            ),
            (
                r#"{"temperature":1,"thinking":{"type":"enabled","budget_tokens":1024}}"#,
                &[("temperature", "1")],
                &[],
            ),
            (
                r#"{"temperature":0.5,"thinking":{"type":"enabled","budget_tokens":1024}}"#,
                &[],
                &["temperature"],
            ),
            (
                r#"{"top_p":0.99,"thinking":{"type":"enabled","budget_tokens":1024}}"#,
                &[("top_p", "0.99")],
                &[],
            ),
            (
                r#"{"top_p":0.9,"thinking":{"type":"enabled","budget_tokens":1024}}"#,
                &[],
                &["top_p"],
            ),
            (
                r#"{"top_k":40,"thinking":{"type":"enabled","budget_tokens":1024}}"#,
                &[],
                &["top_k"],
            ),
        ];
        for (payload, keep, dropped) in cases {
            let out = normalize_claude_sampling_for_upstream(payload.as_bytes(), true);
            for (field, want) in keep {
                assert_eq!(gs(&out, field), *want, "{payload}");
            }
            for field in dropped {
                assert!(!ex(&out, field), "{payload}: {field}");
            }
        }

        // Go oracle: gjson `Num` is 0 for non-numbers, so string/null values count as "not 1"
        // and "below 0.95"; the thinking type is trimmed and case-insensitive.
        assert_eq!(
            text(&normalize_claude_sampling_for_upstream(
                br#"{"temperature":"1","thinking":{"type":"enabled"}}"#,
                true
            )),
            r#"{"thinking":{"type":"enabled"}}"#
        );
        assert_eq!(
            text(&normalize_claude_sampling_for_upstream(
                br#"{"temperature":null,"top_p":"0.99","thinking":{"type":" Adaptive "}}"#,
                true
            )),
            r#"{"thinking":{"type":" Adaptive "}}"#
        );
        assert_eq!(
            text(&normalize_claude_sampling_for_upstream(
                br#"{"temperature":1.0,"top_p":0.5}"#,
                true
            )),
            r#"{"temperature":1.0}"#
        );
    }

    // Go oracle for disableThinkingIfToolChoiceForced.
    #[test]
    fn tool_choice_forced_disables_thinking() {
        let f = |s: &str| disable_thinking_if_tool_choice_forced(s.as_bytes());
        assert_eq!(
            text(&f(
                r#"{"tool_choice":{"type":"tool","name":"x"},"thinking":{"type":"enabled"},"output_config":{"effort":"high"},"a":1}"#
            )),
            r#"{"tool_choice":{"type":"tool","name":"x"},"a":1}"#
        );
        assert_eq!(
            text(&f(
                r#"{"tool_choice":{"type":"any"},"output_config":{"effort":"high","format":{}},"a":1}"#
            )),
            r#"{"tool_choice":{"type":"any"},"output_config":{"format":{}},"a":1}"#
        );
        assert_eq!(
            text(&f(r#"{"tool_choice":{"type":"any"},"output_config":{}}"#)),
            r#"{"tool_choice":{"type":"any"}}"#
        );
        let auto = br#"{"tool_choice":{"type":"auto"},"thinking":{"type":"enabled"}}"#;
        assert_eq!(disable_thinking_if_tool_choice_forced(auto), auto);
    }

    // Go: TestClaudePayloadHasMidSystemMessage
    #[test]
    fn mid_system_detection() {
        let cases = [
            (
                r#"{"messages":[{"role":"user","content":"a"},{"role":"system","content":"s"}]}"#,
                true,
            ),
            (r#"{"messages":[{"role":"SySTeM","content":"s"}]}"#, true),
            (r#"{"messages":[{"role":" system ","content":"s"}]}"#, true),
            (
                r#"{"messages":[{"role":"user","content":"a"},{"role":"assistant","content":"b"}]}"#,
                false,
            ),
            (
                r#"{"system":[{"type":"text","text":"s"}],"messages":[{"role":"user","content":"a"}]}"#,
                false,
            ),
            (r#"{"model":"claude-haiku-4-5"}"#, false),
            (r#"{"messages":"system"}"#, false),
            (r#"{"messages":["system"]}"#, false),
            (
                r#"{"messages":[{"role":"user","content":"role: system"}]}"#,
                false,
            ),
        ];
        for (payload, want) in cases {
            assert_eq!(
                claude_payload_has_mid_system_message(payload.as_bytes()),
                want,
                "{payload}"
            );
        }
    }

    // Expected values from the Go oracle. Go emits `<` for `<` in strings that sjson
    // must marshal; the JSON values are identical, so the escaping case compares parsed values.
    #[test]
    fn mid_system_rebuild() {
        let out = rebuild_mid_system_messages_to_top_level(
            br#"{"system":"top","messages":[{"role":"user","content":"a"},{"role":"system","content":"mid <x> & \"q\""},{"role":"assistant","content":"b"},{"role":" System ","content":[{"type":"text","text":"t1"},"plain",{"type":"image"},{"type":"text","text":"  "}]}]}"#,
        );
        let want: Value = serde_json::from_str(
            r#"{"system":[{"type":"text","text":"top"},{"type":"text","text":"mid <x> & \"q\""},{"type":"text","text":"t1"},{"type":"text","text":"plain"}],"messages":[{"role":"user","content":"a"},{"role":"assistant","content":"b"}]}"#,
        )
        .expect("json");
        assert_eq!(cpa_json::parse(&out), want);
        assert_eq!(
            text(&out).find("\"system\""),
            Some(1),
            "system keeps its position"
        );

        // Blank-only system turns yield no parts, so nothing changes (the turn stays).
        let blank =
            br#"{"messages":[{"role":"user","content":"a"},{"role":"system","content":"   "}]}"#;
        assert_eq!(rebuild_mid_system_messages_to_top_level(blank), blank);

        // Existing array system keeps its blocks (and markers), moved text is appended.
        assert_eq!(
            text(&rebuild_mid_system_messages_to_top_level(
                br#"{"system":[{"type":"text","text":"s0","cache_control":{"type":"ephemeral"}}],"model":"m","messages":[{"role":"system","content":"x"},{"role":"user","content":"a"}]}"#
            )),
            r#"{"system":[{"type":"text","text":"s0","cache_control":{"type":"ephemeral"}},{"type":"text","text":"x"}],"model":"m","messages":[{"role":"user","content":"a"}]}"#
        );
        // No top-level system: it is appended after messages, which become empty.
        assert_eq!(
            text(&rebuild_mid_system_messages_to_top_level(
                br#"{"messages":[{"role":"system","content":"x"}]}"#
            )),
            r#"{"messages":[],"system":[{"type":"text","text":"x"}]}"#
        );
    }

    #[test]
    fn raw_json_array_joins() {
        assert_eq!(raw_json_array(&[]), "[]");
        assert_eq!(
            raw_json_array(&["1".into(), r#"{"a":2}"#.into()]),
            r#"[1,{"a":2}]"#
        );
    }

    // Expected values from the Go oracle (restoreClaudeResponseModel).
    #[test]
    fn restore_response_model() {
        let r = |s: &str| restore_claude_response_model(s.as_bytes(), "client");
        assert_eq!(
            text(&r(r#"{"id":"1","model":"up","message":{"model":"up2"}}"#)),
            r#"{"id":"1","model":"client","message":{"model":"client"}}"#
        );
        assert_eq!(
            text(&r(
                "data: {\"type\":\"message_start\",\"message\":{\"model\":\"up\"}}\n"
            )),
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"client\"}}"
        );
        // Not an SSE data line (starts with `event:`) and not JSON: untouched.
        let event = "event: x\ndata:{\"model\":\"up\"}";
        assert_eq!(text(&r(event)), event);
        let ping = "data: {\"type\":\"ping\"}";
        assert_eq!(text(&r(ping)), ping);
        // Trailing whitespace of a plain JSON payload is preserved.
        assert_eq!(text(&r("{\"model\":\"up\"}\n")), "{\"model\":\"client\"}\n");
        assert!(set_claude_response_model(b"{\"id\":1}", "x").is_none());
    }
}

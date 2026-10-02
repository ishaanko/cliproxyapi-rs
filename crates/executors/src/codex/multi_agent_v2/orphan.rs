//! Orphan Codex delegation outputs (Go: optimize-multi-agent-v2/orphan_delegation.go).
//!
//! Codex Desktop sub-agent requests can carry `codex_app` `create_thread` /
//! `send_message_to_thread` function outputs whose calls are not part of the request. Strict
//! upstreams reject such unpaired outputs, so they are downgraded to plain user messages.

use std::collections::HashMap;

use cpa_config::Config;
use cpa_json::{J, Value};
use http::HeaderMap;
use serde_json::json;

use super::header_value_case_insensitive;

const CODEX_APP_NAMESPACE: &str = "codex_app";
const CREATE_THREAD_NAME: &str = "create_thread";
const SEND_MESSAGE_TO_THREAD_NAME: &str = "send_message_to_thread";
const APP_CREATE_THREAD_TOOL: &str = "codex_app__create_thread";
const APP_SEND_MESSAGE_TOOL: &str = "codex_app__send_message_to_thread";
const OPENAI_SUBAGENT_HEADER: &str = "X-Openai-Subagent";
const COLLAB_SPAWN_SUBAGENT: &str = "collab_spawn";

/// Converts orphan Codex delegation outputs into standard user messages when `enabled` and the
/// request carries `X-Openai-Subagent: collab_spawn`. Returns the payload bytes unchanged when
/// nothing was rewritten.
pub fn rewrite_orphan_delegation_input(
    headers: &HeaderMap,
    payload: &[u8],
    enabled: bool,
) -> Vec<u8> {
    if !enabled || payload.is_empty() || !is_collab_spawn_subagent(headers) {
        return payload.to_vec();
    }
    let mut root = cpa_json::parse(payload);
    if rewrite_orphan_value(&mut root, payload) {
        cpa_json::to_vec(&root)
    } else {
        payload.to_vec()
    }
}

/// [`rewrite_orphan_delegation_input`] gated on `cfg.codex.orphan-delegation-compatibility`.
pub fn rewrite_orphan_delegation_input_for_config(
    headers: &HeaderMap,
    payload: &[u8],
    cfg: Option<&Config>,
) -> Vec<u8> {
    let enabled = cfg.is_some_and(|c| c.codex.orphan_delegation_compatibility);
    rewrite_orphan_delegation_input(headers, payload, enabled)
}

/// Whether the request is a Codex `collab_spawn` sub-agent call.
pub(super) fn is_collab_spawn_subagent(headers: &HeaderMap) -> bool {
    header_value_case_insensitive(headers, OPENAI_SUBAGENT_HEADER)
        .eq_ignore_ascii_case(COLLAB_SPAWN_SUBAGENT)
}

/// In-place rewrite over an already parsed request. `src` must be the unmodified bytes `root`
/// was parsed from; it supplies the original text of structured outputs. The caller has already
/// checked the enable flag and subagent header.
pub(super) fn rewrite_orphan_value(root: &mut Value, src: &[u8]) -> bool {
    let Some(Value::Array(items)) = root.get("input") else {
        return false;
    };

    let mut available_calls: HashMap<String, i64> = HashMap::new();
    for item in items {
        if item.g("type").str() == "function_call" {
            let call_id = item.g("call_id").str();
            if !call_id.trim().is_empty() {
                *available_calls.entry(call_id).or_insert(0) += 1;
            }
        }
    }

    let mut replacements: Vec<(usize, Value)> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if item.g("type").str() != "function_call_output" {
            continue;
        }
        let call_id = item.g("call_id").str();
        if !call_id.trim().is_empty()
            && let Some(count) = available_calls.get_mut(&call_id)
            && *count > 0
        {
            // Paired with a function call in the same request; consume and preserve.
            *count -= 1;
            continue;
        }
        let Some(tool_label) = match_delegation_tool(item) else {
            continue;
        };
        // Orphan delegation output: downgrade to a user message preserving the exact output.
        replacements.push((
            index,
            build_orphan_user_message(tool_label, item, src, index),
        ));
    }

    if replacements.is_empty() {
        return false;
    }
    if let Some(Value::Array(items)) = root.get_mut("input") {
        for (index, message) in replacements {
            items[index] = message;
        }
    }
    true
}

fn match_delegation_tool(item: &Value) -> Option<&'static str> {
    if item.g("namespace").str() != CODEX_APP_NAMESPACE {
        return None;
    }
    match item.g("name").str().as_str() {
        CREATE_THREAD_NAME => Some(APP_CREATE_THREAD_TOOL),
        SEND_MESSAGE_TO_THREAD_NAME => Some(APP_SEND_MESSAGE_TOOL),
        _ => None,
    }
}

/// `Tool output from <label>:\n<output>` as a user message; string outputs verbatim, other
/// outputs as their original JSON text.
fn build_orphan_user_message(tool_label: &str, item: &Value, src: &[u8], index: usize) -> Value {
    let output = item.g("output");
    let output_text = match output.v() {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => cpa_json::raw_at(src, &format!("input.{index}.output"))
            .map(str::to_owned)
            .unwrap_or_else(|| output.raw()),
    };
    let full_text = format!("Tool output from {tool_label}:\n{output_text}");
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": full_text}]})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collab_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-openai-subagent", "collab_spawn".parse().unwrap());
        h
    }

    fn rewrite(payload: &str, enabled: bool) -> Vec<u8> {
        rewrite_orphan_delegation_input(&collab_headers(), payload.as_bytes(), enabled)
    }

    fn parsed(bytes: &[u8]) -> Value {
        cpa_json::parse(bytes)
    }

    const CREATE_THREAD_ORPHAN: &str = r#"{
        "model": "deepseek-v4-pro",
        "input": [
            {
                "type": "function_call_output",
                "name": "create_thread",
                "namespace": "codex_app",
                "output": "<codex_delegation><message>handoff</message></codex_delegation>"
            }
        ]
    }"#;

    #[test]
    fn disabled_leaves_payload_unchanged() {
        assert_eq!(
            rewrite(CREATE_THREAD_ORPHAN, false),
            CREATE_THREAD_ORPHAN.as_bytes()
        );
    }

    #[test]
    fn missing_or_different_subagent_header_leaves_payload_unchanged() {
        let got = rewrite_orphan_delegation_input(
            &HeaderMap::new(),
            CREATE_THREAD_ORPHAN.as_bytes(),
            true,
        );
        assert_eq!(got, CREATE_THREAD_ORPHAN.as_bytes());

        let mut headers = HeaderMap::new();
        headers.insert("x-openai-subagent", "other_subagent".parse().unwrap());
        let got = rewrite_orphan_delegation_input(&headers, CREATE_THREAD_ORPHAN.as_bytes(), true);
        assert_eq!(got, CREATE_THREAD_ORPHAN.as_bytes());
    }

    #[test]
    fn rewrites_orphan_create_thread_without_call_id() {
        let payload = r#"{
            "model": "deepseek-v4-pro",
            "input": [
                {
                    "type": "function_call_output",
                    "name": "create_thread",
                    "namespace": "codex_app",
                    "output": "<codex_delegation><message>handoff</message></codex_delegation>"
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "please continue"}]
                }
            ]
        }"#;
        let got = parsed(&rewrite(payload, true));
        let item0 = got.g("input.0");
        assert_eq!(item0.g("type").str(), "message");
        assert_eq!(item0.g("role").str(), "user");
        assert_eq!(
            item0.g("content.0.text").str(),
            "Tool output from codex_app__create_thread:\n<codex_delegation><message>handoff</message></codex_delegation>"
        );
        let item1 = got.g("input.1");
        assert_eq!(item1.g("type").str(), "message");
        assert_eq!(item1.g("content.0.text").str(), "please continue");
    }

    #[test]
    fn case_insensitive_header_key_and_value_works() {
        let payload = r#"{"model":"m","input":[{"type":"function_call_output","name":"create_thread","namespace":"codex_app","output":"msg"}]}"#;
        let mut headers = HeaderMap::new();
        headers.insert("x-openai-subagent", "COLLAB_SPAWN".parse().unwrap());
        let got = parsed(&rewrite_orphan_delegation_input(
            &headers,
            payload.as_bytes(),
            true,
        ));
        assert_eq!(got.g("input.0.type").str(), "message");
        assert_eq!(got.g("input.0.role").str(), "user");
    }

    #[test]
    fn rewrites_orphan_send_message_to_thread_with_stale_call_id() {
        let payload = r#"{"input":[{"type":"function_call_output","call_id":"call_stale_123","name":"send_message_to_thread","namespace":"codex_app","output":"<codex_delegation>msg</codex_delegation>"}]}"#;
        let got = parsed(&rewrite(payload, true));
        assert_eq!(got.g("input.0.type").str(), "message");
        assert_eq!(got.g("input.0.role").str(), "user");
        assert_eq!(
            got.g("input.0.content.0.text").str(),
            "Tool output from codex_app__send_message_to_thread:\n<codex_delegation>msg</codex_delegation>"
        );
    }

    #[test]
    fn preserves_paired_create_thread_tool_call() {
        let payload = r#"{"input":[
            {"type":"function_call","call_id":"call_active_123","name":"create_thread","namespace":"codex_app","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_active_123","name":"create_thread","namespace":"codex_app","output":"<codex_delegation>valid</codex_delegation>"}
        ]}"#;
        let got = rewrite(payload, true);
        assert_eq!(got, payload.as_bytes());
    }

    #[test]
    fn preserves_non_whitelisted_tools() {
        let payload = r#"{"input":[
            {"type":"function_call_output","name":"automation_update","namespace":"codex_app","output":"ignored"},
            {"type":"function_call_output","name":"create_thread","namespace":"other_namespace","output":"ignored"}
        ]}"#;
        let got = parsed(&rewrite(payload, true));
        assert_eq!(got.g("input.0.type").str(), "function_call_output");
        assert_eq!(got.g("input.1.type").str(), "function_call_output");
    }

    #[test]
    fn handles_empty_output() {
        let payload = r#"{"input":[{"type":"function_call_output","name":"create_thread","namespace":"codex_app","output":""}]}"#;
        let got = parsed(&rewrite(payload, true));
        assert_eq!(got.g("input.0.type").str(), "message");
        assert_eq!(got.g("input.0.role").str(), "user");
        assert_eq!(
            got.g("input.0.content.0.text").str(),
            "Tool output from codex_app__create_thread:\n"
        );
    }

    #[test]
    fn call_id_whitespace_difference_is_not_paired() {
        let payload = r#"{"input":[
            {"type":"function_call","call_id":"call_1","name":"create_thread","namespace":"codex_app","arguments":"{}"},
            {"type":"function_call_output","call_id":" call_1 ","name":"create_thread","namespace":"codex_app","output":"mismatch"}
        ]}"#;
        let got = parsed(&rewrite(payload, true));
        assert_eq!(got.g("input.1.type").str(), "message");
    }

    #[test]
    fn preserves_structured_output_as_exact_text() {
        let raw_output = r#"[{"type":"input_text","text":"diagram"},{"type":"input_image","image_url":"https://example.com/img.png"}]"#;
        let payload = format!(
            r#"{{"model":"deepseek-v4-pro","input":[{{"type":"function_call_output","name":"create_thread","namespace":"codex_app","output": {raw_output}
            }}]}}"#
        );
        let got = parsed(&rewrite(&payload, true));
        assert_eq!(got.g("input.0.type").str(), "message");
        assert_eq!(
            got.g("input.0.content.0.text").str(),
            format!("Tool output from codex_app__create_thread:\n{raw_output}")
        );
    }

    #[test]
    fn call_and_output_pair_regardless_of_order() {
        let payload = r#"{"input":[
            {"type":"function_call_output","call_id":"call_future_1","name":"create_thread","namespace":"codex_app","output":"early"},
            {"type":"function_call","call_id":"call_future_1","name":"create_thread","namespace":"codex_app","arguments":"{}"}
        ]}"#;
        let got = parsed(&rewrite(payload, true));
        assert_eq!(got.g("input.0.type").str(), "function_call_output");
        assert_eq!(got.g("input.1.type").str(), "function_call");
    }

    #[test]
    fn duplicate_output_consumes_call_once() {
        let payload = r#"{"input":[
            {"type":"function_call","call_id":"call_once","name":"create_thread","namespace":"codex_app","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_once","name":"create_thread","namespace":"codex_app","output":"first"},
            {"type":"function_call_output","call_id":"call_once","name":"create_thread","namespace":"codex_app","output":"second"}
        ]}"#;
        let got = parsed(&rewrite(payload, true));
        assert_eq!(got.g("input.1.type").str(), "function_call_output");
        assert_eq!(got.g("input.2.type").str(), "message");
    }

    #[test]
    fn custom_tool_call_does_not_pair_with_function_call_output() {
        let payload = r#"{"input":[
            {"type":"custom_tool_call","call_id":"call_custom","name":"create_thread","input":"{}"},
            {"type":"function_call_output","call_id":"call_custom","name":"create_thread","namespace":"codex_app","output":"orphan"}
        ]}"#;
        let got = parsed(&rewrite(payload, true));
        assert_eq!(got.g("input.1.type").str(), "message");
    }

    #[test]
    fn assistant_message_tool_calls_do_not_pair_with_function_call_output() {
        let payload = r#"{"input":[
            {"type":"message","role":"assistant","tool_calls":[{"id":"call_in_msg","type":"function","function":{"name":"create_thread"}}]},
            {"type":"function_call_output","call_id":"call_in_msg","name":"create_thread","namespace":"codex_app","output":"orphan"}
        ]}"#;
        let got = parsed(&rewrite(payload, true));
        assert_eq!(got.g("input.1.type").str(), "message");
    }

    #[test]
    fn exact_namespace_and_name_required() {
        let payload = r#"{"input":[
            {"type":"function_call_output","name":"codex_app__create_thread","output":"orphan"},
            {"type":"function_call_output","name":"create_thread","namespace":" codex_app ","output":"orphan"},
            {"type":"function_call_output","name":"other_tool","namespace":"codex_app","output":"orphan"}
        ]}"#;
        let got = parsed(&rewrite(payload, true));
        for i in 0..3 {
            assert_eq!(
                got.g(&format!("input.{i}.type")).str(),
                "function_call_output"
            );
        }
    }

    // Responses-target cases of TestTranslateRequestWithCodexMultiAgentV2OrphanDelegation: the
    // translate wrapper only calls the config-gated rewrite for a Responses source and target.
    fn orphan_cfg(enabled: bool) -> Config {
        let mut cfg = Config::default();
        cfg.codex.orphan_delegation_compatibility = enabled;
        cfg
    }

    const TRANSLATE_PAYLOAD: &str = r#"{
        "model": "test-model",
        "stream": false,
        "input": [
            {
                "type": "function_call_output",
                "name": "create_thread",
                "namespace": "codex_app",
                "output": "<codex_delegation><message>handoff</message></codex_delegation>"
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "please continue"}]
            }
        ]
    }"#;

    #[test]
    fn config_gate_requires_flag_and_collab_header() {
        let cfg = orphan_cfg(true);
        let p = TRANSLATE_PAYLOAD.as_bytes();
        let got = parsed(&rewrite_orphan_delegation_input_for_config(
            &HeaderMap::new(),
            p,
            Some(&cfg),
        ));
        assert_eq!(got.g("input.0.type").str(), "function_call_output");

        let mut wrong = HeaderMap::new();
        wrong.insert("x-openai-subagent", "other_agent".parse().unwrap());
        let got = parsed(&rewrite_orphan_delegation_input_for_config(
            &wrong,
            p,
            Some(&cfg),
        ));
        assert_eq!(got.g("input.0.type").str(), "function_call_output");

        let got = parsed(&rewrite_orphan_delegation_input_for_config(
            &collab_headers(),
            p,
            Some(&cfg),
        ));
        assert_eq!(got.g("input.0.type").str(), "message");
        assert_eq!(got.g("input.0.role").str(), "user");
        assert_eq!(
            got.g("input.0.content.0.text").str(),
            "Tool output from codex_app__create_thread:\n<codex_delegation><message>handoff</message></codex_delegation>"
        );

        let off = orphan_cfg(false);
        let got = parsed(&rewrite_orphan_delegation_input_for_config(
            &collab_headers(),
            p,
            Some(&off),
        ));
        assert_eq!(got.g("input.0.type").str(), "function_call_output");
        let got = parsed(&rewrite_orphan_delegation_input_for_config(
            &collab_headers(),
            p,
            None,
        ));
        assert_eq!(got.g("input.0.type").str(), "function_call_output");
    }

    #[test]
    fn interleaved_valid_pair_and_orphan_retains_order_and_pairing() {
        let payload = r#"{
            "model": "test-model",
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"test\"}"},
                {"type": "function_call_output", "call_id": "call_1", "name": "lookup", "output": "result1"},
                {"type": "function_call_output", "name": "create_thread", "namespace": "codex_app", "output": "delegated"}
            ]
        }"#;
        let cfg = orphan_cfg(true);
        let got = parsed(&rewrite_orphan_delegation_input_for_config(
            &collab_headers(),
            payload.as_bytes(),
            Some(&cfg),
        ));
        assert_eq!(got.g("input.0.type").str(), "function_call");
        assert_eq!(got.g("input.1.type").str(), "function_call_output");
        assert_eq!(got.g("input.1.call_id").str(), "call_1");
        assert_eq!(got.g("input.2.type").str(), "message");
        assert_eq!(got.g("input.2.role").str(), "user");
    }
}

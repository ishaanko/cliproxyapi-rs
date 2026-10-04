//! Handler-level Codex preparation of Responses requests: multi-agent v2 tool preparation and the
//! orphan-delegation rewrite run before routing (ports of Go `openai_responses_multi_agent_test.go`).

mod common;

use common::{Script, harness};
use serde_json::Value;

const CODEX_UA: (&str, &str) = ("user-agent", "codex_cli_rs/0.144.1");
const SUBAGENT: (&str, &str) = ("x-openai-subagent", "collab_spawn");

const COMPLETED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"output\":[]}}\n\n";

fn collaboration_tools() -> &'static str {
    r#""tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}}]}]"#
}

fn orphan_input() -> &'static str {
    r#""input":[{"type":"function_call_output","name":"create_thread","namespace":"codex_app","output":"<codex_delegation>msg</codex_delegation>"},{"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}]"#
}

fn last_seen(h: &common::Harness) -> Value {
    let seen = h.exec.seen.lock();
    serde_json::from_str(&seen.last().expect("executor call").1).expect("payload json")
}

#[tokio::test]
async fn http_and_sse_prepare_collaboration_tools_and_mark_the_request() {
    for stream in [false, true] {
        let h = harness(if stream { "prep-sse" } else { "prep-http" }, |c| c.client.codex.optimize_multi_agent_v2 = true).await;
        if stream {
            h.script("collaboration", Script::Stream(vec![Ok(COMPLETED)]));
        }
        let body = format!(r#"{{"model":"{}","stream":{stream},{}}}"#, h.model, collaboration_tools());
        let (status, _, out) = h.call("POST", "/v1/responses", &[CODEX_UA], &body).await;
        assert_eq!(status, 200, "{out}");
        let sent = last_seen(&h);
        assert_eq!(sent["tools"][0]["name"], "collaboration");
        assert!(sent["tools"][0]["tools"][0]["parameters"]["properties"]["message"].get("encrypted").is_none(), "{sent}");
        assert_eq!(*h.exec.prepared.lock(), [true]);
    }
}

#[tokio::test]
async fn other_clients_are_not_prepared() {
    let h = harness("prep-other", |c| c.client.codex.optimize_multi_agent_v2 = true).await;
    let body = format!(r#"{{"model":"{}",{}}}"#, h.model, collaboration_tools());
    let (status, _, _) = h.call("POST", "/v1/responses", &[("user-agent", "curl/8.7.1")], &body).await;
    assert_eq!(status, 200);
    let sent = last_seen(&h);
    assert_eq!(sent["tools"][0]["tools"][0]["parameters"]["properties"]["message"]["encrypted"], true);
    assert_eq!(*h.exec.prepared.lock(), [false]);
}

#[tokio::test]
async fn orphan_delegation_is_rewritten_before_routing_only_for_collab_spawn() {
    let h = harness("orphan", |c| c.codex.orphan_delegation_compatibility = true).await;
    let body = format!(r#"{{"model":"{}","stream":false,{}}}"#, h.model, orphan_input());
    let (status, _, out) = h.call("POST", "/v1/responses", &[SUBAGENT], &body).await;
    assert_eq!(status, 200, "{out}");
    let sent = last_seen(&h);
    assert_eq!(sent["input"][0]["type"], "message");
    assert_eq!(sent["input"][0]["role"], "user");
    assert_eq!(
        sent["input"][0]["content"][0]["text"],
        "Tool output from codex_app__create_thread:\n<codex_delegation>msg</codex_delegation>"
    );

    let (status, _, _) = h.call("POST", "/v1/responses", &[], &body).await;
    assert_eq!(status, 200);
    assert_eq!(last_seen(&h)["input"][0]["type"], "function_call_output");
}

/// Compact applies the orphan rewrite but never tool preparation.
#[tokio::test]
async fn compact_rewrites_orphans_without_preparing_tools() {
    let h = harness("orphan-compact", |c| {
        c.codex.orphan_delegation_compatibility = true;
        c.client.codex.optimize_multi_agent_v2 = true;
    })
    .await;
    let body = format!(r#"{{"model":"{}","stream":false,{},{}}}"#, h.model, orphan_input(), collaboration_tools());
    let (status, _, out) = h.call("POST", "/v1/responses/compact", &[CODEX_UA, SUBAGENT], &body).await;
    assert_eq!(status, 200, "{out}");
    let sent = last_seen(&h);
    assert_eq!(sent["input"][0]["type"], "message");
    assert_eq!(sent["tools"][0]["tools"][0]["parameters"]["properties"]["message"]["encrypted"], true);
    assert_eq!(*h.exec.prepared.lock(), [false]);
}

/// `TestClientMultiAgentPreparationDoesNotWaitForOAuthCredential`: both steps run with no
/// credential-kind gate.
#[tokio::test]
async fn preparation_does_not_wait_for_credential_selection() {
    let h = harness("prep-both", |c| {
        c.client.codex.optimize_multi_agent_v2 = true;
        c.codex.orphan_delegation_compatibility = true;
    })
    .await;
    let body = format!(r#"{{"model":"{}",{},{}}}"#, h.model, orphan_input(), collaboration_tools());
    let (status, _, _) = h.call("POST", "/v1/responses", &[CODEX_UA, SUBAGENT], &body).await;
    assert_eq!(status, 200);
    let sent = last_seen(&h);
    assert_eq!(sent["input"][0]["type"], "message");
    assert!(sent["tools"][0]["tools"][0]["parameters"]["properties"]["message"].get("encrypted").is_none());
    assert_eq!(*h.exec.prepared.lock(), [true]);
}

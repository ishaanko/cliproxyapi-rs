//! Handler-level Codex preparation of Responses requests: the multi-agent v2 tool preparation and
//! the orphan-delegation rewrite run at the handler (HTTP, compact and websocket) before routing,
//! with `X-Openai-Subagent: collab_spawn` and an official Codex user agent.

use serde_json::{Value, json};

use super::bodies::Family;
use super::profiles;
use crate::client::{HttpReq, Step as Req, WsReq};
use crate::mock::script::{Content, Script};
use crate::scenario::Scenario;

const CODEX_UA: &str = "codex_cli_rs/0.144.1";

fn orphan_output() -> Value {
    json!({"type": "function_call_output", "call_id": "call_orphan_1", "name": "create_thread", "namespace": "codex_app",
           "output": "<codex_delegation>task</codex_delegation>"})
}

fn user(text: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
}

fn collaboration_tools() -> Value {
    json!([{"type": "namespace", "name": "collaboration", "tools": [
        {"type": "function", "name": "spawn_agent", "description": "Spawns an agent.",
         "parameters": {"type": "object", "properties": {"message": {"type": "string", "encrypted": true}}}},
        {"type": "function", "name": "send_message",
         "parameters": {"type": "object", "properties": {"message": {"type": "string", "encrypted": true}}}}
    ]}])
}

fn body(model: &str, stream: bool) -> Value {
    json!({"model": model, "stream": stream, "input": [orphan_output(), user("continue")], "tools": collaboration_tools()})
}

fn post(path: &str, body: Value, subagent: bool) -> Req {
    let mut req = HttpReq::post(path, body).header("user-agent", CODEX_UA);
    if subagent {
        req = req.header("x-openai-subagent", "collab_spawn");
    }
    Req::Http(req)
}

fn ws(messages: Vec<Value>, subagent: bool) -> Req {
    let mut req = WsReq::new("/v1/responses", messages).header("user-agent", CODEX_UA);
    if subagent {
        req = req.header("x-openai-subagent", "collab_spawn");
    }
    Req::Ws(req)
}

pub fn scenarios() -> Vec<Scenario> {
    let model = Family::Codex.model();
    let text = || Script::ok(Content::Text);
    let mut out = vec![];
    for (id, desc, stream) in [
        ("json", "orphan delegation output becomes a user message before routing (non-stream)", false),
        ("stream", "orphan delegation output becomes a user message before routing (SSE)", true),
    ] {
        out.push(
            Scenario::new(format!("responses.codex.collab_spawn.{id}"), desc, text(), vec![post("/v1/responses", body(model, stream), true)])
                .profile(profiles::codex_collab_prepare),
        );
    }
    out.push(
        Scenario::new(
            "responses.codex.collab_spawn.compact",
            "compact rewrites orphan delegations at the handler (tools are prepared by the executor only)",
            text(),
            vec![post("/v1/responses/compact", body(model, false), true)],
        )
        .profile(profiles::codex_collab_prepare),
    );
    out.push(
        Scenario::new(
            "responses.codex.collab_spawn.no_subagent_header",
            "without X-Openai-Subagent the orphan output is untouched while tools are still prepared",
            text(),
            vec![post("/v1/responses", body(model, false), false)],
        )
        .profile(profiles::codex_collab_prepare),
    );
    out.push(
        Scenario::new(
            "ws.fallback.codex.collab_spawn",
            "orphan delegation with a call_id is rewritten before the tool-call cache; the next turn replays the rewritten input",
            text(),
            vec![ws(
                vec![
                    json!({"type": "response.create", "model": model, "input": [orphan_output(), user("continue")], "tools": collaboration_tools()}),
                    json!({"type": "response.append", "input": [user("second")]}),
                ],
                true,
            )],
        )
        .profile(profiles::codex_collab_prepare),
    );
    out.push(
        Scenario::new(
            "ws.fallback.codex.collab_spawn_no_header",
            "the same websocket request without the sub-agent header keeps the orphan output untouched",
            text(),
            vec![ws(
                vec![json!({"type": "response.create", "model": model, "input": [orphan_output(), user("continue")], "tools": collaboration_tools()})],
                false,
            )],
        )
        .profile(profiles::codex_collab_prepare),
    );
    out
}

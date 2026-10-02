//! Client-facing Responses WebSocket (`GET /v1/responses`) scenarios, with HTTP and WebSocket
//! Codex upstreams.

use serde_json::{Value, json};

use super::bodies::{Family, FAMILIES};
use super::profiles;
use crate::client::{Auth, HttpReq, Step as Req, WsReq};
use crate::mock::script::{Content, Reply, Script, Step};
use crate::scenario::Scenario;

const PATH: &str = "/v1/responses";

fn user_input(text: &str) -> Value {
    json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}])
}

fn create(model: &str, text: &str) -> Value {
    json!({"type": "response.create", "model": model, "input": user_input(text)})
}

fn append(text: &str) -> Value {
    json!({"type": "response.append", "input": user_input(text)})
}

fn ws(messages: Vec<Value>) -> Req {
    Req::Ws(WsReq::new(PATH, messages))
}

fn ok(content: Content) -> Script {
    Script::ok(content)
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    let codex = Family::Codex.model();

    // HTTP upstream: the handler emulates websocket continuation by replaying the transcript.
    for f in FAMILIES {
        out.push(Scenario::new(
            format!("ws.fallback.{}.text", f.label()),
            "one response.create over the client websocket",
            ok(Content::Text),
            vec![ws(vec![create(f.model(), "hello")])],
        ));
    }
    out.push(Scenario::new(
        "ws.fallback.codex.thinking",
        "reasoning events over the websocket",
        ok(Content::Thinking),
        vec![ws(vec![create(codex, "think")])],
    ));
    out.push(Scenario::new(
        "ws.fallback.codex.two_turns",
        "response.append merges the previous turn into the next upstream request",
        ok(Content::Text),
        vec![ws(vec![create(codex, "first"), append("second")])],
    ));
    out.push(Scenario::new(
        "ws.fallback.codex.tool_roundtrip",
        "function call output continues a tool-calling turn",
        Script::steps(vec![Step::once(Reply::ok(Content::ToolCall))]),
        vec![ws(vec![
            json!({"type": "response.create", "model": codex, "input": user_input("weather?"),
                   "tools": [{"type": "function", "name": "get_weather", "parameters": {"type": "object"}}]}),
            json!({"type": "response.create", "previous_response_id": "resp_mock01",
                   "input": [{"type": "function_call_output", "call_id": "call_mock01", "output": "sunny"}]}),
        ])],
    ));
    out.push(Scenario::new(
        "ws.fallback.codex.prewarm",
        "generate:false is answered locally without an upstream call",
        ok(Content::Text),
        vec![ws(vec![json!({"type": "response.create", "model": codex, "input": user_input("warm"), "generate": false}), create(codex, "real")])],
    ));
    out.push(Scenario::new(
        "ws.fallback.client_errors",
        "invalid client frames produce error frames and keep the socket open",
        ok(Content::Text),
        vec![ws(vec![
            json!({"type": "response.bogus"}),
            append("before create"),
            json!({"type": "response.create", "input": []}),
            json!({"type": "response.create", "model": codex, "input": "not-an-array"}),
            json!({"type": "response.create", "model": codex, "previous_response_id": "resp_unknown", "input": []}),
            create(codex, "finally fine"),
        ])],
    ));
    for (id, reply) in [
        ("upstream_400", Reply::error(400)),
        ("upstream_401", Reply::error(401)),
        ("upstream_429", Reply::error_with(429, &[("retry-after", "30")], None)),
        ("upstream_500", Reply::error(500)),
    ] {
        out.push(Scenario::new(
            format!("ws.fallback.codex.{id}"),
            "upstream HTTP error before the first event",
            Script::steps(vec![Step::always(reply)]),
            vec![ws(vec![create(codex, "hello")])],
        ));
    }
    out.push(Scenario::new(
        "ws.fallback.codex.stream_cut",
        "upstream stream ends before response.completed",
        Script::steps(vec![Step::always(Reply::Cut { content: Content::Text, after: 6, abort: false })]),
        vec![ws(vec![create(codex, "hello")])],
    ));
    out.push(Scenario::new(
        "ws.fallback.codex.mid_error",
        "upstream failure event after some output",
        Script::steps(vec![Step::always(Reply::StreamError { content: Content::Text, after: 6 })]),
        vec![ws(vec![create(codex, "hello")])],
    ));

    // WebSocket upstream: frames are passed through on the pinned upstream connection.
    let upstream = |id: &str, desc: &str, script: Script, messages: Vec<Value>| {
        Scenario::new(format!("ws.upstream.{id}"), desc, script, vec![ws(messages)]).profile(profiles::codex_websockets)
    };
    out.push(upstream("text", "response.create forwarded over an upstream websocket", ok(Content::Text), vec![create(codex, "hello")]));
    out.push(upstream("thinking", "reasoning events over both websockets", ok(Content::Thinking), vec![create(codex, "think")]));
    out.push(upstream("two_turns", "second turn continues on the same upstream socket", ok(Content::Text), vec![create(codex, "first"), append("second")]));
    out.push(upstream(
        "previous_response_id",
        "previous_response_id is passed through to the upstream socket",
        ok(Content::Text),
        vec![create(codex, "first"), json!({"type": "response.create", "previous_response_id": "resp_mock01", "input": user_input("next")})],
    ));
    for (id, reply) in [
        ("error_400", Reply::error(400)),
        ("error_401", Reply::error(401)),
        ("error_429", Reply::error_with(429, &[("retry-after", "30")], None)),
        ("error_500", Reply::error(500)),
    ] {
        out.push(upstream(id, "upstream error frame", Script::steps(vec![Step::always(reply)]), vec![create(codex, "hello")]));
    }
    out.push(upstream(
        "mid_error",
        "upstream response.failed after some output",
        Script::steps(vec![Step::always(Reply::StreamError { content: Content::Text, after: 6 })]),
        vec![create(codex, "hello")],
    ));
    out.push(upstream(
        "cut_abort",
        "upstream drops its websocket mid-turn",
        Script::steps(vec![Step::always(Reply::Cut { content: Content::Text, after: 6, abort: true })]),
        vec![create(codex, "hello")],
    ));
    out.push(upstream(
        "close_clean",
        "upstream closes its websocket cleanly mid-turn",
        Script::steps(vec![Step::always(Reply::Cut { content: Content::Text, after: 6, abort: false })]),
        vec![create(codex, "hello")],
    ));
    out.push(
        Scenario::new(
            "ws.upstream.non_codex_model_falls_back",
            "a Claude model on a websocket-enabled setup still goes over HTTP upstream",
            ok(Content::Text),
            vec![ws(vec![create(Family::Claude.model(), "hello")])],
        )
        .profile(profiles::codex_websockets),
    );

    // Handshake and routing.
    let hs = |id: &str, desc: &str, req: Req| Scenario::new(format!("ws.handshake.{id}"), desc, ok(Content::Text), vec![req]);
    out.push(hs("missing_key", "upgrade without credentials", Req::Ws(WsReq::new(PATH, vec![]).auth(Auth::None))));
    out.push(hs("invalid_key", "upgrade with a wrong key", Req::Ws(WsReq::new(PATH, vec![]).auth(Auth::Bearer("wrong")))));
    out.push(hs("query_key", "key in the query string", Req::Ws(WsReq::new("/v1/responses?key=e2e-client-key", vec![create(codex, "hi")]).auth(Auth::None))));
    out.push(hs("codex_backend_path", "/backend-api/codex/responses alias", Req::Ws(WsReq::new("/backend-api/codex/responses", vec![create(codex, "hi")]))));
    out.push(hs(
        "turn_state_echo",
        "x-codex-turn-state is echoed on the upgrade response",
        Req::Ws(WsReq::new(PATH, vec![]).header("x-codex-turn-state", "state-abc")),
    ));
    out.push(hs("plain_get", "GET /v1/responses without an upgrade", Req::Http(HttpReq::get(PATH))));
    out
}

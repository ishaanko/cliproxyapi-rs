//! Request-log files (`request-log: true`, and the forced error logs with it off): the golden
//! holds the normalized file text, so the upstream `API REQUEST` / `API RESPONSE n` /
//! `API WEBSOCKET TIMELINE` sections are compared against the reference.

use serde_json::{Value, json};

use super::bodies::{self, FAMILIES, Family, Kind};
use super::profiles;
use crate::client::{HttpReq, Step as Req, WsReq};
use crate::mock::script::{Content, Reply, Script, Step};
use crate::scenario::Scenario;

fn chat(f: Family, stream: bool) -> Req {
    Req::Http(HttpReq::post("/v1/chat/completions", bodies::chat(f.model(), stream, Kind::Text)))
}

fn user_input(text: &str) -> Value {
    json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}])
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    for f in FAMILIES {
        for stream in [false, true] {
            let mode = if stream { "stream" } else { "json" };
            out.push(
                Scenario::new(
                    format!("reqlog.{}.{mode}", f.label()),
                    "request-log file of a successful call",
                    Script::ok(Content::Text),
                    vec![chat(f, stream)],
                )
                .profile(profiles::request_log)
                .with_logs(),
            );
        }
    }
    out.push(
        Scenario::new(
            "reqlog.claude.messages",
            "request-log file of a Claude-dialect call routed to Claude",
            Script::ok(Content::Text),
            vec![Req::Http(HttpReq::post("/v1/messages", bodies::claude(Family::Claude.model(), false, Kind::Text)))],
        )
        .profile(profiles::request_log)
        .with_logs(),
    );
    out.push(
        Scenario::new(
            "reqlog.claude.failover",
            "two upstream attempts (500 then success) produce API REQUEST 1/2 and API RESPONSE 1/2",
            Script::steps(vec![Step::once(Reply::error(500)), Step::always(Reply::ok(Content::Text))]),
            vec![chat(Family::Claude, false)],
        )
        .profile(profiles::request_log)
        .with_logs(),
    );
    out.push(
        Scenario::new(
            "reqlog.codex.failover_stream",
            "stream bootstrap failover logs both attempts",
            Script::steps(vec![Step::once(Reply::error(500)), Step::always(Reply::ok(Content::Text))]),
            vec![chat(Family::Codex, true)],
        )
        .profile(profiles::request_log)
        .with_logs(),
    );
    out.push(
        Scenario::new(
            "reqlog.gemini.upstream_400",
            "upstream client error with request-log on",
            Script::steps(vec![Step::always(Reply::error(400))]),
            vec![chat(Family::Gemini, false)],
        )
        .profile(profiles::request_log)
        .with_logs(),
    );
    out.push(
        Scenario::new(
            "reqlog.compat.mid_error",
            "stream failing after some output",
            Script::steps(vec![Step::always(Reply::StreamError { content: Content::Text, after: 3 })]),
            vec![chat(Family::Compat, true)],
        )
        .profile(profiles::request_log)
        .with_logs(),
    );
    // Request log off: only failing requests produce `error-*.log`, carrying the deferred API REQUEST.
    for f in [Family::Claude, Family::Codex] {
        out.push(
            Scenario::new(
                format!("reqlog.forced.{}.upstream_500", f.label()),
                "error-only log of a failing request (deferred API REQUEST)",
                Script::steps(vec![Step::always(Reply::error(500))]),
                vec![chat(f, false)],
            )
            .with_logs(),
        );
    }
    out.push(
        Scenario::new("reqlog.off.success", "request-log off and a successful call: no file", Script::ok(Content::Text), vec![chat(Family::Claude, false)])
            .with_logs(),
    );
    // Codex upstream websocket: API WEBSOCKET TIMELINE.
    let codex = Family::Codex.model();
    let create = |text: &str| json!({"type": "response.create", "model": codex, "input": user_input(text)});
    out.push(
        Scenario::new(
            "reqlog.ws.upstream_text",
            "Responses websocket client over a Codex upstream websocket",
            Script::ok(Content::Text),
            vec![Req::Ws(WsReq::new("/v1/responses", vec![create("hello")]))],
        )
        .profile(profiles::request_log_codex_ws)
        .with_logs(),
    );
    out.push(
        Scenario::new(
            "reqlog.ws.fallback_text",
            "Responses websocket client over an HTTP Codex upstream",
            Script::ok(Content::Text),
            vec![Req::Ws(WsReq::new("/v1/responses", vec![create("hello")]))],
        )
        .profile(profiles::request_log)
        .with_logs(),
    );
    out
}

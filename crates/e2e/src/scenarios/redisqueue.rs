//! Redis-protocol usage output on the API port (`AUTH`, `SUBSCRIBE`, `LPOP`/`RPOP`) and its
//! coexistence with HTTP and the management usage-queue endpoint.

use super::bodies::{self, FAMILIES, Family, Kind};
use super::profiles;
use crate::client::{Auth, HttpReq, RespAct, RespReq, Step as Req};
use crate::config::MGMT_SECRET;
use crate::mock::script::{Content, Reply, Script, Step};
use crate::scenario::Scenario;

const SETTLE_MS: u64 = 500;

fn cmd(args: &[&str]) -> RespAct {
    RespAct::Cmd(args.iter().map(|a| a.to_string()).collect())
}

fn auth() -> RespAct {
    cmd(&["AUTH", MGMT_SECRET])
}

fn session(acts: Vec<RespAct>) -> Req {
    Req::Resp(RespReq { acts })
}

fn chat() -> HttpReq {
    HttpReq::post("/v1/chat/completions", bodies::chat(Family::Claude.model(), false, Kind::Text))
}

fn ok_script() -> Script {
    Script::ok(Content::Text)
}

/// A successful then a failing upstream call, so the queue holds a success and a failure record.
fn mixed_script() -> Script {
    Script::steps(vec![Step::once(Reply::ok(Content::Text)), Step::once(Reply::error(400))])
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    let s = |id: &str, desc: &str, script: Script, steps: Vec<Req>| Scenario::new(format!("redis.{id}"), desc, script, steps);

    out.push(s(
        "auth.commands",
        "NOAUTH gate, AUTH argument handling and unknown commands",
        ok_script(),
        vec![session(vec![
            cmd(&["PING"]),
            cmd(&["LPOP", "usage"]),
            cmd(&["AUTH"]),
            cmd(&["AUTH", "user", "wrong-key"]),
            cmd(&["AUTH", "wrong-key"]),
            RespAct::Raw("*0\r\n".into()),
            cmd(&["AUTH", "default", MGMT_SECRET]),
            cmd(&["FLUSHALL"]),
            cmd(&["PING"]),
        ])],
    ));
    out.push(s(
        "auth.ip_ban",
        "five failed attempts ban the client on the Redis path too",
        ok_script(),
        vec![
            session(vec![
                cmd(&["AUTH", "bad"]),
                cmd(&["AUTH", "bad"]),
                cmd(&["AUTH", "bad"]),
                cmd(&["AUTH", "bad"]),
                cmd(&["AUTH", "bad"]),
                cmd(&["AUTH", MGMT_SECRET]),
                cmd(&["PING"]),
            ]),
            Req::Http(HttpReq::get("/v0/management/config").auth(Auth::Mgmt)),
        ],
    ));
    out.push(s(
        "auth.noauth_counts_failures",
        "unauthenticated commands count as failed attempts",
        ok_script(),
        vec![session(vec![
            cmd(&["PING"]),
            cmd(&["PING"]),
            cmd(&["PING"]),
            cmd(&["PING"]),
            cmd(&["PING"]),
            cmd(&["PING"]),
        ])],
    ));
    out.push(
        s(
            "disabled.no_management",
            "without a management secret the RESP connection is closed",
            ok_script(),
            vec![session(vec![cmd(&["PING"])]), session(vec![cmd(&["AUTH", MGMT_SECRET])])],
        )
        .profile(profiles::no_management),
    );
    out.push(s(
        "protocol.errors",
        "malformed frames answer ERR and close",
        ok_script(),
        vec![
            session(vec![RespAct::Raw("*x\r\n".into())]),
            session(vec![RespAct::Raw("+PING\r\n".into())]),
            session(vec![RespAct::Raw("*1\r\n!3\r\n".into())]),
            session(vec![RespAct::Raw("$4\r\nPING\r\n".into())]),
        ],
    ));
    out.push(
        s(
            "pop.usage",
            "LPOP/RPOP usage after proxied requests",
            mixed_script(),
            vec![
                Req::Http(chat()),
                Req::Http(chat()),
                Req::Pause(SETTLE_MS),
                session(vec![
                    auth(),
                    cmd(&["LPOP", "usage"]),
                    cmd(&["RPOP", "usage", "5"]),
                    cmd(&["LPOP", "usage"]),
                    cmd(&["RPOP", "usage", "2"]),
                ]),
            ],
        )
        .profile(profiles::usage_stats),
    );
    out.push(s(
        "pop.arguments",
        "LPOP/RPOP argument and channel validation",
        ok_script(),
        vec![session(vec![
            auth(),
            cmd(&["LPOP"]),
            cmd(&["LPOP", "usage", "1", "2"]),
            cmd(&["RPOP", "usage", "0"]),
            cmd(&["RPOP", "usage", "-3"]),
            cmd(&["RPOP", "usage", "many"]),
            cmd(&["LPOP", "errors"]),
            cmd(&["RPOP", " USAGE ", "2"]),
            cmd(&["RPOP", "nope", "2"]),
        ])],
    ));
    out.push(
        s(
            "subscribe.usage",
            "SUBSCRIBE usage: support refresh, live records, PING, UNSUBSCRIBE",
            mixed_script(),
            vec![session(vec![
                auth(),
                cmd(&["SUBSCRIBE", "usage"]),
                RespAct::Read,
                RespAct::Http(chat()),
                RespAct::Read,
                cmd(&["PING"]),
                cmd(&["PING", "hello"]),
                cmd(&["GET", "x"]),
                RespAct::Http(chat()),
                RespAct::Read,
                cmd(&["UNSUBSCRIBE"]),
            ])],
        )
        .profile(profiles::usage_stats),
    );
    out.push(s(
        "subscribe.arguments",
        "SUBSCRIBE validation, channel case, QUIT while subscribed",
        ok_script(),
        vec![
            session(vec![auth(), cmd(&["SUBSCRIBE"]), cmd(&["SUBSCRIBE", "a", "b"]), cmd(&["SUBSCRIBE", "nope"])]),
            session(vec![auth(), cmd(&["SUBSCRIBE", " Usage "]), RespAct::Read, cmd(&["QUIT"])]),
            session(vec![auth(), cmd(&["SUBSCRIBE", "errors"]), RespAct::Raw("*0\r\n".into()), RespAct::Raw("garbage\r\n".into())]),
        ],
    ));
    out.push(s(
        "subscribe.errors",
        "SUBSCRIBE errors: upstream failures arrive as error events",
        Script::steps(vec![Step::always(Reply::error(401))]),
        vec![session(vec![auth(), cmd(&["SUBSCRIBE", "errors"]), RespAct::Http(chat()), RespAct::Read])],
    ));
    out.push(
        s(
            "mux.http_and_resp",
            "HTTP and RESP clients share the port; the queue feeds both consumers",
            mixed_script(),
            vec![
                Req::Http(HttpReq::get("/v1/models")),
                Req::Http(chat()),
                Req::Pause(SETTLE_MS),
                session(vec![auth(), cmd(&["LPOP", "usage"])]),
                Req::Http(chat()),
                Req::Pause(SETTLE_MS),
                Req::Http(HttpReq::get("/v0/management/usage-queue?count=5").auth(Auth::Mgmt)),
                session(vec![auth(), cmd(&["RPOP", "usage", "5"])]),
                Req::Http(HttpReq::get("/v1/models")),
            ],
        )
        .profile(profiles::usage_stats),
    );
    // Cache read/creation tokens, the served model and the provider's token-breakdown semantics
    // (Claude buckets are independent, OpenAI and Gemini nest cache inside input) per family.
    for f in FAMILIES {
        let cached_chat = |stream| Req::Http(HttpReq::post("/v1/chat/completions", bodies::chat(f.model(), stream, Kind::Cached)));
        out.push(
            s(
                &format!("usage.cached.{}", f.label()),
                &format!("queued usage of cached-token responses, {} upstream, json and stream", f.label()),
                Script::ok(Content::Cached),
                vec![
                    cached_chat(false),
                    cached_chat(true),
                    Req::Pause(SETTLE_MS),
                    session(vec![auth(), cmd(&["LPOP", "usage"]), cmd(&["LPOP", "usage"])]),
                ],
            )
            .profile(profiles::usage_stats),
        );
    }
    // Failed attempts keep the upstream response headers (Go: the response-headers holder is
    // read when the failure record is published), whatever the executor.
    for f in FAMILIES {
        let failing_chat = |stream| Req::Http(HttpReq::post("/v1/chat/completions", bodies::chat(f.model(), stream, Kind::Text)));
        out.push(
            s(
                &format!("usage.failed.{}", f.label()),
                &format!("queued usage of failed upstream calls, {} upstream, json and stream", f.label()),
                Script::steps(vec![Step::always(Reply::error(400))]),
                vec![
                    failing_chat(false),
                    failing_chat(true),
                    Req::Pause(SETTLE_MS),
                    session(vec![auth(), cmd(&["LPOP", "usage"]), cmd(&["LPOP", "usage"])]),
                ],
            )
            .profile(profiles::usage_stats),
        );
    }
    // A client that hangs up mid-request: the upstream call is cancelled and the attempt's usage
    // record is a failure (499, "context canceled") with whatever was observed so far.
    for f in FAMILIES {
        let chat = |stream| HttpReq::post("/v1/chat/completions", bodies::chat(f.model(), stream, Kind::Text));
        out.push(
            s(
                &format!("usage.abort.stream.{}", f.label()),
                &format!("queued usage of a stream the client abandons after the first chunk, {} upstream", f.label()),
                Script::steps(vec![Step::always(Reply::ok(Content::Text)).stalled(3000)]),
                vec![
                    Req::Http(chat(true).abort_after_first_chunk()),
                    Req::Pause(SETTLE_MS),
                    session(vec![auth(), cmd(&["LPOP", "usage"])]),
                ],
            )
            .profile(profiles::usage_stats),
        );
        out.push(
            s(
                &format!("usage.abort.json.{}", f.label()),
                &format!("queued usage of a non-stream request the client abandons while waiting, {} upstream", f.label()),
                Script::steps(vec![Step::always(Reply::ok(Content::Text)).delayed(3000)]),
                vec![
                    Req::Http(chat(false).abort_after_ms(300)),
                    Req::Pause(SETTLE_MS),
                    session(vec![auth(), cmd(&["LPOP", "usage"])]),
                ],
            )
            .profile(profiles::usage_stats),
        );
    }
    out.push(s(
        "usage_disabled",
        "no usage records are queued with usage-statistics-enabled off",
        mixed_script(),
        vec![Req::Http(chat()), Req::Pause(SETTLE_MS), session(vec![auth(), cmd(&["RPOP", "usage", "5"])])],
    ));
    out
}

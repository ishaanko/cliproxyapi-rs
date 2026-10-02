//! Upstream failure and failover scenarios, per client dialect and upstream family.

use super::bodies::{self, FAMILIES, Family, Kind};
use crate::client::{HttpReq, Step as Req};
use crate::mock::script::{Content, Reply, Script, Step};
use crate::scenario::Scenario;

#[derive(Clone, Copy)]
enum Case {
    /// Every upstream call answers 401.
    Unauthorized,
    /// Every upstream call answers 400 (a client fault: no failover).
    BadRequest,
    /// First upstream call 429 with Retry-After, then success; two client requests.
    RateLimitFailover,
    /// Every upstream call 429 with Retry-After; two client requests.
    AllRateLimited,
    /// First upstream call 500, then success.
    ServerErrorFailover,
    /// Every upstream call 500.
    AllServerError,
    /// Streaming request; first upstream call 500, then a good stream.
    StreamBootstrapFailover,
    /// Streaming request; upstream sends a few events then an in-band error.
    StreamMidError,
    /// Streaming request; upstream drops the connection mid-stream.
    StreamCutAbort,
    /// Streaming request; upstream ends the stream cleanly without a terminal event.
    StreamCutClean,
    /// Non-streaming request; upstream drops the connection mid-body.
    JsonCutAbort,
}

impl Case {
    fn label(self) -> &'static str {
        match self {
            Case::Unauthorized => "upstream401",
            Case::BadRequest => "upstream400",
            Case::RateLimitFailover => "429_failover",
            Case::AllRateLimited => "429_all",
            Case::ServerErrorFailover => "500_failover",
            Case::AllServerError => "500_all",
            Case::StreamBootstrapFailover => "stream_500_failover",
            Case::StreamMidError => "stream_mid_error",
            Case::StreamCutAbort => "stream_cut_abort",
            Case::StreamCutClean => "stream_cut_clean",
            Case::JsonCutAbort => "json_cut_abort",
        }
    }

    fn streaming(self) -> bool {
        matches!(self, Case::StreamBootstrapFailover | Case::StreamMidError | Case::StreamCutAbort | Case::StreamCutClean)
    }

    fn requests(self) -> usize {
        match self {
            Case::RateLimitFailover | Case::AllRateLimited => 2,
            _ => 1,
        }
    }

    /// Events delivered before the cut/error, chosen so some payload has already been sent.
    fn after(f: Family) -> usize {
        match f {
            Family::Claude => 4,
            Family::Codex => 6,
            Family::Gemini => 1,
            Family::Compat => 3,
        }
    }

    fn script(self, f: Family) -> Script {
        let retry_after = [("retry-after", "30")];
        let after = Self::after(f);
        match self {
            Case::Unauthorized => Script::steps(vec![Step::always(Reply::error(401))]),
            Case::BadRequest => Script::steps(vec![Step::always(Reply::error(400))]),
            Case::RateLimitFailover => Script::steps(vec![Step::once(Reply::error_with(429, &retry_after, None))]),
            Case::AllRateLimited => Script::steps(vec![Step::always(Reply::error_with(429, &retry_after, None))]),
            Case::ServerErrorFailover | Case::StreamBootstrapFailover => Script::steps(vec![Step::once(Reply::error(500))]),
            Case::AllServerError => Script::steps(vec![Step::always(Reply::error(500))]),
            Case::StreamMidError => Script::steps(vec![Step::always(Reply::StreamError { content: Content::Text, after })]),
            Case::StreamCutAbort => Script::steps(vec![Step::always(Reply::Cut { content: Content::Text, after, abort: true })]),
            Case::StreamCutClean => Script::steps(vec![Step::always(Reply::Cut { content: Content::Text, after, abort: false })]),
            Case::JsonCutAbort => Script::steps(vec![Step::always(Reply::Cut { content: Content::Text, after: 1, abort: true })]),
        }
    }
}

struct Dialect {
    name: &'static str,
    request: fn(Family, bool) -> HttpReq,
    cases: &'static [Case],
}

fn chat(f: Family, stream: bool) -> HttpReq {
    HttpReq::post("/v1/chat/completions", bodies::chat(f.model(), stream, Kind::Text))
}

fn claude(f: Family, stream: bool) -> HttpReq {
    HttpReq::post("/v1/messages", bodies::claude(f.model(), stream, Kind::Text))
}

fn responses(f: Family, stream: bool) -> HttpReq {
    HttpReq::post("/v1/responses", bodies::responses(f.model(), stream, Kind::Text))
}

fn gemini(f: Family, stream: bool) -> HttpReq {
    let method = if stream { "streamGenerateContent" } else { "generateContent" };
    HttpReq::post(&bodies::gemini_path(f.model(), method), bodies::gemini(Kind::Text))
}

const ALL_CASES: &[Case] = &[
    Case::Unauthorized,
    Case::BadRequest,
    Case::RateLimitFailover,
    Case::AllRateLimited,
    Case::ServerErrorFailover,
    Case::AllServerError,
    Case::StreamBootstrapFailover,
    Case::StreamMidError,
    Case::StreamCutAbort,
    Case::StreamCutClean,
    Case::JsonCutAbort,
];

const COMMON_CASES: &[Case] = &[
    Case::Unauthorized,
    Case::RateLimitFailover,
    Case::AllServerError,
    Case::ServerErrorFailover,
    Case::StreamBootstrapFailover,
    Case::StreamMidError,
    Case::StreamCutAbort,
];

const DIALECTS: [Dialect; 4] = [
    Dialect { name: "chat", request: chat, cases: ALL_CASES },
    Dialect { name: "claude", request: claude, cases: COMMON_CASES },
    Dialect { name: "responses", request: responses, cases: COMMON_CASES },
    Dialect { name: "gemini", request: gemini, cases: &[Case::Unauthorized, Case::RateLimitFailover, Case::AllServerError, Case::StreamMidError, Case::StreamCutAbort] },
];

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    for d in &DIALECTS {
        for f in FAMILIES {
            for &c in d.cases {
                let id = format!("err.{}.{}.{}", d.name, f.label(), c.label());
                let desc = format!("{} client -> {} upstream: {}", d.name, f.label(), c.label());
                let steps = (0..c.requests()).map(|_| Req::Http((d.request)(f, c.streaming()))).collect();
                out.push(Scenario::new(id, desc, c.script(f), steps));
            }
        }
    }
    out
}

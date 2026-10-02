//! Dialect x family x stream x content matrix: every client dialect against every upstream
//! provider family, successful responses only.

use super::bodies::{self, FAMILIES, Family, KINDS, Kind};
use crate::client::{HttpReq, Step};
use crate::mock::script::Script;
use crate::scenario::Scenario;

/// A client dialect: how to build the request for a family/stream/kind combination.
struct Dialect {
    name: &'static str,
    request: fn(Family, bool, Kind) -> HttpReq,
    /// Whether the dialect can express tool calls and thinking.
    full: bool,
}

fn chat(f: Family, stream: bool, k: Kind) -> HttpReq {
    HttpReq::post("/v1/chat/completions", bodies::chat(f.model(), stream, k))
}

fn completions(f: Family, stream: bool, _: Kind) -> HttpReq {
    HttpReq::post("/v1/completions", bodies::completions(f.model(), stream))
}

fn responses(f: Family, stream: bool, k: Kind) -> HttpReq {
    HttpReq::post("/v1/responses", bodies::responses(f.model(), stream, k))
}

fn claude(f: Family, stream: bool, k: Kind) -> HttpReq {
    HttpReq::post("/v1/messages", bodies::claude(f.model(), stream, k))
}

fn gemini(f: Family, stream: bool, k: Kind) -> HttpReq {
    let method = if stream { "streamGenerateContent" } else { "generateContent" };
    HttpReq::post(&bodies::gemini_path(f.model(), method), bodies::gemini(k))
}

fn gemini_sse(f: Family, stream: bool, k: Kind) -> HttpReq {
    let method = if stream { "streamGenerateContent?alt=sse" } else { "generateContent" };
    HttpReq::post(&bodies::gemini_path(f.model(), method), bodies::gemini(k))
}

const DIALECTS: [Dialect; 6] = [
    Dialect { name: "chat", request: chat, full: true },
    Dialect { name: "completions", request: completions, full: false },
    Dialect { name: "responses", request: responses, full: true },
    Dialect { name: "claude", request: claude, full: true },
    Dialect { name: "gemini", request: gemini, full: true },
    // Same dialect with `alt=sse`: only differs for streaming.
    Dialect { name: "geminisse", request: gemini_sse, full: false },
];

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    for d in &DIALECTS {
        for f in FAMILIES {
            for stream in [false, true] {
                // `alt=sse` equals the plain form for non-stream calls, so skip those.
                if d.name == "geminisse" && !stream {
                    continue;
                }
                let kinds: &[Kind] = if d.full { &KINDS } else { &KINDS[..1] };
                for &k in kinds {
                    let mode = if stream { "stream" } else { "json" };
                    let id = format!("{}.{}.{}.{}", d.name, f.label(), mode, k.label());
                    let desc = format!("{} client -> {} upstream, {mode}, {}", d.name, f.label(), k.label());
                    out.push(Scenario::new(id, desc, Script::ok(k.content()), vec![Step::Http((d.request)(f, stream, k))]));
                }
            }
        }
    }
    out
}

//! Streams whose events arrive split mid-line or several per network write: the server must
//! re-frame them identically to whole-event writes.

use super::bodies::{self, FAMILIES, Family, Kind};
use crate::client::{HttpReq, Step as Req};
use crate::mock::script::{Chunking, Content, Reply, Script, Step};
use crate::scenario::Scenario;

fn request(dialect: &str, f: Family) -> HttpReq {
    match dialect {
        "chat" => HttpReq::post("/v1/chat/completions", bodies::chat(f.model(), true, Kind::Thinking)),
        "responses" => HttpReq::post("/v1/responses", bodies::responses(f.model(), true, Kind::Thinking)),
        "claude" => HttpReq::post("/v1/messages", bodies::claude(f.model(), true, Kind::Thinking)),
        _ => HttpReq::post(&bodies::gemini_path(f.model(), "streamGenerateContent"), bodies::gemini(Kind::Thinking)),
    }
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    for dialect in ["chat", "responses", "claude", "gemini"] {
        for f in FAMILIES {
            for (name, chunking) in [("split", Chunking::Split), ("merged", Chunking::Merged)] {
                out.push(Scenario::new(
                    format!("chunking.{name}.{dialect}.{}", f.label()),
                    format!("upstream stream written {name}: {dialect} client -> {} upstream", f.label()),
                    Script::steps(vec![Step::always(Reply::ok(Content::Thinking)).chunked(chunking)]),
                    vec![Req::Http(request(dialect, f))],
                ));
            }
        }
    }
    out
}

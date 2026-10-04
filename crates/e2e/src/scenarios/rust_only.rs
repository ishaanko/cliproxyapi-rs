//! Scenarios for Rust-only extensions. The Go reference has no such behavior, so these are
//! skipped by `record` (unless `--rust-only`) and their goldens are produced by the Rust server.

use serde_json::json;

use super::bodies::{self, Family, Kind};
use crate::client::{HttpReq, Step as Req};
use crate::config::ConfigSpec;
use crate::mock::script::{Content, Reply, Script, Step};
use crate::scenario::Scenario;

const FIVE_HOUR: &str = "anthropic-ratelimit-unified-5h-utilization";

/// `smart-quota` strategy with session affinity.
fn smart_quota_affinity(s: &mut ConfigSpec) {
    s.strategy = Some("smart-quota".into());
    s.session_affinity = true;
}

/// One chat request in the session `session` (explicit `prompt_cache_key`).
fn chat_in(session: &str) -> Req {
    let mut body = bodies::chat(Family::Claude.model(), false, Kind::Text);
    body["prompt_cache_key"] = json!(session);
    HttpReq::post("/v1/chat/completions", body).into()
}

pub fn scenarios() -> Vec<Scenario> {
    // sk-claude-1 reports its 5h window 90% used, sk-claude-2 10% used, on every response.
    let script = Script {
        steps: vec![
            Step { credential: Some("sk-claude-1".into()), ..Step::always(Reply::ok(Content::Text)).with_headers(&[(FIVE_HOUR, "0.9")]) },
            Step { credential: Some("sk-claude-2".into()), ..Step::always(Reply::ok(Content::Text)).with_headers(&[(FIVE_HOUR, "0.1")]) },
        ],
        fallback: Reply::ok(Content::Text),
    };
    vec![
        Scenario::new(
            "rust.smart_quota.claude",
            "smart-quota: a cold session lands on key 1 (tie), then new sessions avoid it (90% used) while the bound session stays",
            script,
            // 1: s1 binds to key 1 (all unknown, ties go to the lowest id). 2-3: new sessions
            // go to key 2 (10% used). 4-5: s1 stays on key 1. 6: another new session: key 2.
            vec![chat_in("s1"), chat_in("n1"), chat_in("n2"), chat_in("s1"), chat_in("s1"), chat_in("n3")],
        )
        .profile(smart_quota_affinity)
        .rust_only(),
    ]
}

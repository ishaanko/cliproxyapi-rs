//! Differential test: sequences of generated Gemini chunks must convert to identical bytes through
//! the public entry (fast path first) and through the general path alone, state included.

use std::collections::HashMap;

use super::{convert_gemini_response_to_openai, convert_general, ChatParams};
use crate::common::test_gen::{Gen, Json};
use crate::registry::{Ctx, Param};

fn text(g: &mut Gen) -> Json {
    const POOL: &[&str] = &["", "hello", "@@0001700000000000 token ", "line\nbreak \"q\"", "é→日本", "emoji 😀", "a/b", "tab\t", "\u{1}"];
    Json::s(g.pick(POOL))
}

fn part(g: &mut Gen) -> Json {
    match g.below(14) {
        0..=5 => Json::obj(vec![("text", text(g))]),
        6 | 7 => Json::obj(vec![("text", text(g)), ("thought", Json::Raw("true".into()))]),
        8 => Json::obj(vec![("text", text(g)), ("thoughtSignature", Json::s("sig"))]),
        9 => Json::obj(vec![("thoughtSignature", Json::s("sig"))]),
        10 => Json::obj(vec![("functionCall", Json::obj(vec![("name", Json::s("get_weather")), ("args", Json::obj(vec![("city", Json::s("Paris"))]))]))]),
        11 => Json::obj(vec![("inlineData", Json::obj(vec![("mimeType", Json::s("image/png")), ("data", Json::s("QUJD"))]))]),
        12 => Json::obj(vec![("text", Json::Raw(["null", "5"][g.below(2)].into()))]),
        _ => Json::obj(vec![("text", text(g)), ("thought", Json::Raw(["false", "\"true\""][g.below(2)].into()))]),
    }
}

fn candidate(g: &mut Gen, index: usize) -> Json {
    let mut f = vec![];
    if g.chance(70) {
        f.push(("index", Json::Raw(index.to_string())));
    }
    if g.chance(30) {
        f.push(("finishReason", Json::s(["STOP", "MAX_TOKENS", "stop", "SAFETY"][g.below(4)])));
    }
    if g.chance(90) {
        let parts = Json::Arr((0..g.below(3)).map(|_| part(g)).collect());
        f.push(("content", Json::obj(if g.chance(95) { vec![("role", Json::s("model")), ("parts", parts)] } else { vec![("role", Json::s("model"))] })));
    }
    Json::obj(f)
}

fn chunk(g: &mut Gen) -> String {
    let mut f = vec![];
    if g.chance(93) {
        let n = 1 + g.below(2);
        let cands = (0..n)
            .map(|i| {
                let index = if g.chance(95) { i } else { 0 };
                candidate(g, index)
            })
            .collect();
        f.push(("candidates", Json::Arr(cands)));
    }
    if g.chance(25) {
        let mut u = vec![("promptTokenCount", Json::Raw("11".into())), ("candidatesTokenCount", Json::Raw("7".into()))];
        if g.chance(80) {
            u.push(("totalTokenCount", Json::Raw("18".into())));
        }
        if g.chance(30) {
            u.push(("thoughtsTokenCount", Json::Raw(["3", "0"][g.below(2)].into())));
        }
        if g.chance(30) {
            u.push(("cachedContentTokenCount", Json::Raw("4".into())));
        }
        if g.chance(8) {
            u.push(("promptTokenCount", Json::Raw("1.5".into())));
        }
        f.push(("usageMetadata", Json::obj(u)));
    }
    if g.chance(80) {
        f.push(("modelVersion", Json::s("gemini-2.5-flash")));
    }
    if g.chance(60) {
        f.push(("responseId", Json::s("resp\"1")));
    }
    if g.chance(15) {
        f.push(("createTime", Json::s(["2025-01-02T03:04:05Z", "garbage", "2025-01-02T03:04:05.123456Z"][g.below(3)])));
    }
    let body = Json::obj(f).emit(g);
    match g.below(8) {
        0 => body,
        1 => format!("data:{body}"),
        2 => String::new(),
        3 => "data: [DONE]".to_string(),
        _ => format!("data: {body}"),
    }
}

/// Function call ids carry a clock reading and a counter.
fn mask(frames: Vec<Vec<u8>>) -> Vec<String> {
    let re = regex::Regex::new(r#""id":"[^"]*-\d{10,}-\d+""#).expect("regex");
    frames.into_iter().map(|f| re.replace_all(&String::from_utf8_lossy(&f), r#""id":"FC""#).into_owned()).collect()
}

#[test]
fn fast_matches_general_on_generated_streams() {
    let total: u64 = std::env::var("FAST_DIFF_N").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    for seed in 1..=total {
        let mut g = Gen::new(seed);
        let mut param = Param::default();
        let mut general = ChatParams {
            unix_timestamp: 0,
            function_index: HashMap::new(),
            saw_tool_call: HashMap::new(),
            upstream_finish_reason: HashMap::new(),
            sanitized_name_map: HashMap::new(),
        };
        let mut history = Vec::new();
        for _ in 0..1 + g.below(6) {
            let line = chunk(&mut g);
            history.push(line.clone());
            let got = mask(convert_gemini_response_to_openai(&Ctx::default(), "m", b"{}", b"{}", line.as_bytes(), &mut param));
            let mut raw = line.as_bytes();
            if let Some(rest) = raw.strip_prefix(b"data:") {
                raw = rest.trim_ascii();
            }
            let want = if raw == b"[DONE]" { vec![] } else { mask(convert_general(&mut general, raw)) };
            assert_eq!(got, want, "seed {seed} history {history:?}");
        }
    }
}

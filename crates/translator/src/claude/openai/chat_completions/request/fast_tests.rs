//! Differential test: whenever the fast path accepts a body it must produce exactly the bytes of
//! the general `Value` conversion. Bodies are generated pseudo-randomly with many shapes the fast
//! path declines (so declines are exercised too) and string literals in non-canonical encodings.

use super::{convert_general, fast};
use crate::common::test_gen::{Gen, Json};

fn string_pool(g: &mut Gen) -> String {
    const POOL: &[&str] = &[
        "",
        "hello",
        "line1\nline2",
        "quote \"s\" and \\ back",
        "tab\there",
        "\u{1}ctl\u{1f}",
        "é→日本語",
        "emoji 😀 pair",
        "a/b/c",
        "<tag> & done",
        "  padded  ",
        "x",
        "get_weather",
        "tool.name:with/odd chars",
        "data:image/png;base64,iVBORw0KGgo=",
        "data:;base64,AAAA",
        "data:image/jpeg",
        "https://example.com/a.png",
        "\u{7f}del",
        "\u{2028}sep",
    ];
    if g.chance(70) {
        g.pick(POOL).to_string()
    } else {
        let n = g.below(40);
        (0..n).map(|_| char::from(32 + g.below(95) as u8)).collect()
    }
}

fn text(g: &mut Gen) -> Json {
    Json::Str(string_pool(g))
}

fn schema(g: &mut Gen) -> Json {
    let mut props = vec![];
    for i in 0..g.below(4) {
        props.push((format!("p{i}"), Json::obj(vec![("type", Json::s("string")), ("description", text(g))])));
    }
    let mut fields = vec![("type", Json::s("object")), ("properties", Json::Obj(props))];
    if g.chance(30) {
        fields.push(("required", Json::Arr(vec![Json::s("p0")])));
    }
    if g.chance(15) {
        fields.insert(0, ("anyOf", Json::Arr(vec![Json::obj(vec![("type", Json::s("object")), ("properties", Json::obj(vec![("z", Json::obj(vec![("type", Json::s("number"))]))]))])])));
    }
    Json::obj(fields)
}

fn part(g: &mut Gen) -> Json {
    match g.below(12) {
        0..=4 => Json::obj(vec![("type", Json::s("text")), ("text", text(g))]),
        5 => Json::obj(vec![("type", Json::s("text"))]),
        6 | 7 => Json::obj(vec![("type", Json::s("image_url")), ("image_url", Json::obj(vec![("url", text(g))]))]),
        8 => Json::obj(vec![("type", Json::s("file")), ("file", Json::obj(vec![("file_data", Json::s("data:application/pdf;base64,QQ=="))]))]),
        9 => Json::obj(vec![("type", Json::s("mystery")), ("text", text(g))]),
        10 => Json::obj(vec![("type", Json::s("text")), ("text", text(g)), ("cache_control", Json::obj(vec![("type", Json::s("ephemeral"))]))]),
        _ => Json::Raw("7".into()),
    }
}

fn content(g: &mut Gen) -> Option<Json> {
    Some(match g.below(10) {
        0..=3 => text(g),
        4..=6 => Json::Arr((0..g.below(4)).map(|_| part(g)).collect()),
        7 => Json::Raw("null".into()),
        8 => return None,
        _ => Json::Raw(["5", "true", "{\"a\":1}"][g.below(3)].into()),
    })
}

fn tool_call(g: &mut Gen) -> Json {
    let args = match g.below(6) {
        0 => Json::s(""),
        1 => Json::s("not json"),
        2 => Json::s("[1,2]"),
        3 => Json::Raw("{\"a\":1}".into()),
        _ => Json::s(r#"{"path": "src/a.rs",  "n": 1.50, "nested": {"k": "v\n"}, "path": "dup"}"#),
    };
    let mut fields = vec![("id", if g.chance(90) { Json::Str(format!("call_{}", g.below(4))) } else { Json::s("") })];
    fields.push(("type", if g.chance(90) { Json::s("function") } else { Json::s("other") }));
    let mut f = vec![("name", text(g))];
    if g.chance(90) {
        f.push(("arguments", args));
    }
    fields.push(("function", Json::obj(f)));
    Json::obj(fields)
}

fn message(g: &mut Gen) -> Json {
    let role = ["system", "developer", "user", "user", "user", "assistant", "assistant", "tool", "tool", "User", "function"][g.below(11)];
    let mut f: Vec<(&str, Json)> = vec![("role", Json::s(role))];
    if let Some(c) = content(g) {
        f.push(("content", c));
    }
    if role == "assistant" && g.chance(50) {
        f.push(("tool_calls", Json::Arr((0..1 + g.below(3)).map(|_| tool_call(g)).collect())));
    }
    if role == "tool" || g.chance(3) {
        f.push(("tool_call_id", if g.chance(92) { Json::Str(format!("call_{}", g.below(4))) } else { Json::s("") }));
    }
    if g.chance(3) {
        f.push(("cache_control", Json::obj(vec![("type", Json::s("ephemeral"))])));
    }
    Json::obj(f)
}

fn tool(g: &mut Gen) -> Json {
    let mut f = vec![("name", text(g))];
    if g.chance(85) {
        f.push(("description", text(g)));
    }
    match g.below(8) {
        0 => {}
        1 => f.push(("parametersJsonSchema", schema(g))),
        2 => f.push(("parameters", Json::Raw("null".into()))),
        _ => f.push(("parameters", schema(g))),
    }
    if g.chance(15) {
        f.push(("strict", Json::Raw(["true", "false", "\"true\"", "null"][g.below(4)].into())));
    }
    let mut t = vec![("type", if g.chance(90) { Json::s("function") } else { Json::s("custom") }), ("function", Json::obj(f))];
    if g.chance(10) {
        t.push(("strict", Json::Raw(["true", "false"][g.below(2)].into())));
    }
    if g.chance(3) {
        t.push(("cache_control", Json::obj(vec![("type", Json::s("ephemeral"))])));
    }
    Json::obj(t)
}

fn request(g: &mut Gen) -> Json {
    let mut f: Vec<(&str, Json)> = vec![];
    if g.chance(90) {
        f.push(("model", Json::s("gpt-x")));
    }
    if g.chance(95) {
        f.push(("messages", Json::Arr((0..g.below(8)).map(|_| message(g)).collect())));
    }
    if g.chance(50) {
        f.push(("tools", Json::Arr((0..1 + g.below(3)).map(|_| tool(g)).collect())));
    }
    if g.chance(30) {
        let choice = match g.below(9) {
            0 => Json::s("auto"),
            1 => Json::s("none"),
            2 => Json::s("required"),
            3 => Json::s("weird"),
            4 => Json::obj(vec![("type", Json::s("function")), ("function", Json::obj(vec![("name", text(g))]))]),
            5 => Json::obj(vec![("type", Json::s("function"))]),
            6 => Json::obj(vec![("type", Json::s("allowed_tools")), ("allowed_tools", Json::obj(vec![("mode", Json::s("auto")), ("tools", Json::Arr(vec![]))]))]),
            7 => Json::Raw("null".into()),
            _ => Json::obj(vec![("type", Json::s("any"))]),
        };
        f.push(("tool_choice", choice));
    }
    if g.chance(20) {
        f.push(("parallel_tool_calls", Json::Raw(["false", "true", "null", "\"false\""][g.below(4)].into())));
    }
    if g.chance(30) {
        f.push((["max_tokens", "max_completion_tokens"][g.below(2)], Json::Raw(["100", "0", "-5", "1.5", "null", "99999999999999999999", "\"7\""][g.below(7)].into())));
    }
    if g.chance(20) {
        f.push(("top_p", Json::Raw(["0.95", "1", "0", "1e2", "0.30000000000000004", "null", "\"0.5\"", "-0.5"][g.below(8)].into())));
    }
    if g.chance(20) {
        let stop = match g.below(5) {
            0 => text(g),
            1 => Json::Arr(vec![text(g), text(g)]),
            2 => Json::Arr(vec![]),
            3 => Json::Arr(vec![Json::Raw("5".into())]),
            _ => Json::Raw("null".into()),
        };
        f.push(("stop", stop));
    }
    if g.chance(15) {
        f.push(("user", [text(g), Json::Raw("5".into()), Json::s("  ")][g.below(3)].clone()));
    }
    if g.chance(10) {
        f.push(("stream_options", Json::obj(vec![("include_usage", Json::Raw("true".into()))])));
    }
    if g.chance(15) {
        const RARE: &[&str] = &[
            "metadata", "prompt_cache_key", "session_id", "conversation", "conversation_id", "input", "contents", "instructions", "system", "reasoning_effort",
            "response_format", "thinking", "reasoning", "extra_body", "include_reasoning", "request", "temperature",
        ];
        f.push((g.pick(RARE), text(g)));
    }
    Json::obj(f)
}

#[test]
fn fast_matches_general_on_generated_requests() {
    let mut accepted = 0;
    let total = std::env::var("FAST_DIFF_N").ok().and_then(|v| v.parse().ok()).unwrap_or(6000);
    for seed in 1..=total {
        let mut g = Gen::new(seed);
        let body = request(&mut g).emit(&mut g);
        let stream = g.chance(50);
        if let Some(fast_out) = fast::convert("claude-x", body.as_bytes(), stream) {
            accepted += 1;
            let general = convert_general("claude-x", body.as_bytes(), stream, false);
            assert_eq!(String::from_utf8_lossy(&fast_out), String::from_utf8_lossy(&general), "seed {seed} body {body}");
        }
    }
    eprintln!("fast path accepted {accepted}/{total}");
    assert!(accepted > total / 5, "fast path accepted only {accepted}/{total}");
}

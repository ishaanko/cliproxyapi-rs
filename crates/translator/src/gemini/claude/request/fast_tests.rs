//! Differential test: whenever the fast path accepts a body it must produce exactly the bytes of
//! the general `Value` conversion, over generated bodies (with odd shapes and non-canonical string
//! encodings, so declines are exercised too).

use super::{convert_general, fast};
use crate::common::test_gen::{Gen, Json};

fn string_pool(g: &mut Gen) -> String {
    const POOL: &[&str] = &[
        "",
        "hello",
        "line1\nline2",
        "quote \"s\" and \\ back",
        "tab\there",
        "\u{1}ctl",
        "é→日本語",
        "emoji 😀 pair",
        "a/b",
        "<tag> & done",
        "  padded  ",
        "get_weather",
        "mcp.server:tool/name",
        "{\"k\": [1, 2]}",
        "[1,2,3]",
        "plain result text",
        "x-anthropic-billing-header: cc_version=1",
        "  x-anthropic-billing-header: padded",
        "see {\"$ref\": \"#/x\"}",
    ];
    if g.chance(75) {
        g.pick(POOL).to_string()
    } else {
        let n = g.below(30);
        (0..n).map(|_| char::from(32 + g.below(95) as u8)).collect()
    }
}

fn text(g: &mut Gen) -> Json {
    Json::Str(string_pool(g))
}

fn ids() -> [&'static str; 4] {
    ["toolu_1", "toolu_2", "my_tool-3", "plain"]
}

fn block(g: &mut Gen) -> Json {
    match g.below(14) {
        0..=3 => Json::obj(vec![("type", Json::s("text")), ("text", text(g))]),
        4 => Json::obj(vec![("type", Json::s("text"))]),
        5 | 6 => {
            let input = match g.below(6) {
                0 => Json::Raw("{}".into()),
                1 => text(g),
                2 => Json::Raw("[1]".into()),
                3 => Json::Raw("5".into()),
                _ => Json::obj(vec![("path", text(g)), ("n", Json::Raw("1.50".into())), ("nested", Json::obj(vec![("k", text(g))]))]),
            };
            let mut f = vec![("type", Json::s("tool_use")), ("id", Json::s(g.pick(&ids()))), ("name", text(g))];
            if g.chance(90) {
                f.push(("input", input));
            }
            Json::obj(f)
        }
        7..=9 => {
            let content = match g.below(9) {
                0..=2 => text(g),
                3 => Json::Arr(vec![Json::obj(vec![("type", Json::s("text")), ("text", text(g))])]),
                4 => Json::Arr(vec![
                    Json::obj(vec![("type", Json::s("text")), ("text", text(g))]),
                    Json::obj(vec![("type", Json::s("image")), ("source", Json::obj(vec![("type", Json::s("base64")), ("media_type", Json::s("image/png")), ("data", Json::s("QUJD"))]))]),
                ]),
                5 => Json::Arr(vec![Json::obj(vec![("type", Json::s("image")), ("source", Json::obj(vec![("type", Json::s("base64")), ("media_type", Json::s("image/png")), ("data", Json::s("QUJD"))]))])]),
                6 => Json::obj(vec![("type", Json::s("text")), ("text", text(g))]),
                7 => Json::Raw(["null", "5", "true"][g.below(3)].into()),
                _ => Json::Arr(vec![Json::obj(vec![("type", Json::s("text")), ("text", text(g))]), Json::obj(vec![("type", Json::s("text")), ("text", text(g))])]),
            };
            let mut f = vec![("type", Json::s("tool_result"))];
            if g.chance(95) {
                f.push(("tool_use_id", Json::s(g.pick(&ids()))));
            }
            if g.chance(90) {
                f.push(("content", content));
            }
            if g.chance(10) {
                f.push(("is_error", Json::Raw("true".into())));
            }
            Json::obj(f)
        }
        10 => Json::obj(vec![("type", Json::s("image")), ("source", Json::obj(vec![("type", Json::s("base64")), ("media_type", Json::s("image/jpeg")), ("data", Json::s("QUJD"))]))]),
        11 => Json::obj(vec![("type", Json::s("image")), ("source", Json::obj(vec![("type", Json::s("url")), ("url", Json::s("https://x/y.png"))]))]),
        12 => Json::obj(vec![("type", Json::s("thinking")), ("thinking", text(g)), ("signature", Json::s("sig"))]),
        _ => Json::obj(vec![("type", Json::s("mystery")), ("text", text(g))]),
    }
}

fn message(g: &mut Gen) -> Json {
    let role = ["user", "user", "user", "assistant", "assistant", "system", "developer", "tool"][g.below(8)];
    let mut f = vec![("role", if g.chance(97) { Json::s(role) } else { Json::Raw("5".into()) })];
    match g.below(10) {
        0..=2 => f.push(("content", text(g))),
        3..=7 => f.push(("content", Json::Arr((0..g.below(5)).map(|_| block(g)).collect()))),
        8 => f.push(("content", Json::Raw("null".into()))),
        _ => {}
    }
    Json::obj(f)
}

fn tool(g: &mut Gen) -> Json {
    let mut f = vec![("name", text(g))];
    if g.chance(80) {
        f.push(("description", text(g)));
    }
    if g.chance(90) {
        f.push(("input_schema", Json::obj(vec![("type", Json::s("object")), ("properties", Json::obj(vec![("a", Json::obj(vec![("type", Json::s("string"))]))]))])));
    }
    if g.chance(15) {
        f.push(("strict", Json::Raw(["true", "false"][g.below(2)].into())));
    }
    if g.chance(10) {
        f.push(("cache_control", Json::obj(vec![("type", Json::s("ephemeral"))])));
    }
    Json::obj(f)
}

fn request(g: &mut Gen) -> Json {
    let mut f: Vec<(&str, Json)> = vec![("model", Json::s("claude-x")), ("max_tokens", Json::Raw("100".into()))];
    match g.below(6) {
        0 => f.push(("system", text(g))),
        1 | 2 => f.push(("system", Json::Arr((0..g.below(3)).map(|_| Json::obj(vec![("type", Json::s("text")), ("text", text(g))])).collect()))),
        _ => {}
    }
    if g.chance(95) {
        f.push(("messages", Json::Arr((0..g.below(8)).map(|_| message(g)).collect())));
    }
    if g.chance(40) {
        f.push(("tools", Json::Arr((0..1 + g.below(3)).map(|_| tool(g)).collect())));
    }
    if g.chance(25) {
        f.push((
            "tool_choice",
            match g.below(5) {
                0 => Json::obj(vec![("type", Json::s("auto"))]),
                1 => Json::obj(vec![("type", Json::s("any"))]),
                2 => Json::obj(vec![("type", Json::s("tool")), ("name", text(g))]),
                3 => Json::obj(vec![("type", Json::s("none"))]),
                _ => Json::s("auto"),
            },
        ));
    }
    if g.chance(20) {
        f.push((
            "thinking",
            match g.below(3) {
                0 => Json::obj(vec![("type", Json::s("enabled")), ("budget_tokens", Json::Raw("2048".into()))]),
                1 => Json::obj(vec![("type", Json::s("adaptive"))]),
                _ => Json::obj(vec![("type", Json::s("disabled"))]),
            },
        ));
        if g.chance(50) {
            f.push(("output_config", Json::obj(vec![("effort", Json::s("high"))])));
        }
    }
    for k in ["temperature", "top_p", "top_k"] {
        if g.chance(20) {
            f.push((k, Json::Raw(["0.5", "1", "40", "null", "\"x\""][g.below(5)].into())));
        }
    }
    Json::obj(f)
}

#[test]
fn fast_matches_general_on_generated_requests() {
    let mut accepted = 0;
    let total: u64 = std::env::var("FAST_DIFF_N").ok().and_then(|v| v.parse().ok()).unwrap_or(6000);
    for seed in 1..=total {
        let mut g = Gen::new(seed);
        let body = request(&mut g).emit(&mut g);
        if let Some(fast_out) = fast::convert("gemini-x", body.as_bytes(), false) {
            accepted += 1;
            let general = convert_general("gemini-x", body.as_bytes(), false, false);
            assert_eq!(String::from_utf8_lossy(&fast_out), String::from_utf8_lossy(&general), "seed {seed} body {body}");
        }
    }
    eprintln!("fast path accepted {accepted}/{total}");
    assert!(accepted > total / 5, "fast path accepted only {accepted}/{total}");
}

/// Derived serde structs would fill fields by position from an array; gjson sees no fields, so
/// every struct position must decline (and the general path must cope with the shape).
#[test]
fn array_for_object_declines() {
    for body in [
        r#"{"messages":[["user","hello"]]}"#,
        r#"["user"]"#,
        r#"{"messages":[{"role":"user","content":[["text","hi"]]}]}"#,
        r#"{"messages":[{"role":"user","content":[{"type":"image","source":["base64","image/png","AAAA"]}]}]}"#,
    ] {
        assert!(fast::convert("gemini-x", body.as_bytes(), false).is_none(), "fast path accepted {body}");
        let _ = convert_general("gemini-x", body.as_bytes(), false, false);
    }
}

/// Documents nested beyond `cpa_json::MAX_DEPTH` parse to `Null` in the general path; serde_json
/// would still skip them, so the fast path must decline them. Deep but allowed ones may be accepted.
#[test]
fn deep_nesting_declines() {
    for depth in [130, cpa_json::MAX_DEPTH - 5, cpa_json::MAX_DEPTH + 10] {
        let body = format!(r#"{{"messages":[{{"role":"user","content":"hi"}}],"x":{}{}}}"#, "[".repeat(depth), "]".repeat(depth));
        let fast_out = fast::convert("gemini-x", body.as_bytes(), false);
        if depth > cpa_json::MAX_DEPTH {
            assert!(fast_out.is_none(), "depth {depth}");
        } else if let Some(out) = fast_out {
            let general = convert_general("gemini-x", body.as_bytes(), false, false);
            assert_eq!(String::from_utf8_lossy(&out), String::from_utf8_lossy(&general), "depth {depth}");
        }
    }
}

//! Expectations recorded from tidwall gjson/sjson (Go) for the same inputs.
use super::*;

fn s(j: &str, p: &str, v: Value) -> String {
    let mut x = if j.is_empty() { Value::Null } else { parse_str(j) };
    set(&mut x, p, v);
    to_string(&x)
}
fn d(j: &str, p: &str) -> String {
    let mut x = parse_str(j);
    delete(&mut x, p);
    to_string(&x)
}

#[test]
fn sjson_set() {
    assert_eq!(s("", "a.b", json!("x")), r#"{"a":{"b":"x"}}"#);
    assert_eq!(s("", "a.0", json!("x")), r#"{"a":["x"]}"#);
    assert_eq!(s("", "a.1", json!("x")), r#"{"a":[null,"x"]}"#);
    assert_eq!(s("", "a.-1", json!("x")), r#"{"a":["x"]}"#);
    assert_eq!(s(r#"{"a":[1]}"#, "a.-1", json!(2)), r#"{"a":[1,2]}"#);
    assert_eq!(s(r#"{"a":[1]}"#, "a.3", json!(2)), r#"{"a":[1,null,null,2]}"#);
    assert_eq!(s(r#"{"a":{}}"#, "a.0", json!(2)), r#"{"a":{"0":2}}"#);
    assert_eq!(s(r#"{"a":1}"#, "a.b", json!(2)), r#"{"a":{"b":2}}"#);
    assert_eq!(s(r#"{"a":"s"}"#, "a.0", json!(2)), r#"{"a":[2]}"#);
    assert_eq!(s(r#"{"b":1,"a":2}"#, "b", json!(3)), r#"{"b":3,"a":2}"#);
    assert_eq!(s("{}", "x", num_f64(1.5)), r#"{"x":1.5}"#);
    assert_eq!(s("{}", "x", num_f64(1.0)), r#"{"x":1}"#);
    assert_eq!(s("{}", "x", num_f64(1e20)), r#"{"x":100000000000000000000}"#);
    assert_eq!(s("{}", r"a\.b", json!(1)), r#"{"a.b":1}"#);
    assert_eq!(s("[]", "0", json!(1)), "[1]");
    assert_eq!(s("[]", "-1", json!(1)), "[1]");
    assert_eq!(s(r#"{"a":null}"#, "a.b", json!(1)), r#"{"a":{"b":1}}"#);
}

#[test]
fn sjson_delete() {
    assert_eq!(d(r#"{"a":1,"b":2,"c":3}"#, "b"), r#"{"a":1,"c":3}"#);
    assert_eq!(d(r#"{"a":[1,2,3]}"#, "a.1"), r#"{"a":[1,3]}"#);
    assert_eq!(d(r#"{"a":[1,2,3]}"#, "a.-1"), r#"{"a":[1,2]}"#);
    assert_eq!(d(r#"{"a":1}"#, "zz"), r#"{"a":1}"#);
    assert_eq!(d(r#"{"a":1}"#, "a.b"), r#"{"a":1}"#);
}

#[test]
fn gjson_paths() {
    let j = parse_str(r#"{"a":[{"t":"x","v":1},{"t":"y","v":2}]}"#);
    assert_eq!(j.g("a.#").int(), 2);
    assert_eq!(j.g("a.#.v").raw(), "[1,2]");
    assert_eq!(j.g(r#"a.#(t=="y").v"#).int(), 2);
    assert_eq!(j.g("a.#(t==y)").raw(), r#"{"t":"y","v":2}"#);
    assert_eq!(j.g("a.#(t=y)").raw(), r#"{"t":"y","v":2}"#);
    assert_eq!(j.g("a.#(v>1)#").raw(), r#"[{"t":"y","v":2}]"#);
    assert_eq!(j.g("a.#(t)").raw(), r#"{"t":"x","v":1}"#);
    assert_eq!(j.g(r#"a.#(t%"x*")"#).raw(), r#"{"t":"x","v":1}"#);
    assert!(!parse_str(r#"{"a":[1,2]}"#).g("a.-1").exists());
    assert_eq!(parse_str(r#"{"a":[{"b":1},{"c":2}]}"#).g("a.#.b").raw(), "[1]");
    assert_eq!(parse_str(r#"{"a.b":1}"#).g(r"a\.b").int(), 1);
    assert!(!parse_str(r#"{"a":{"b":1}}"#).g("a.b.c").exists());
    assert!(!parse_str(r#"{"a":"x"}"#).g("a.0").exists());
    assert_eq!(parse_str("[1,2]").g("1").int(), 2);
    assert_eq!(parse_str(r#"{"a":1}"#).g("@this").raw(), r#"{"a":1}"#);
    assert_eq!(parse_str(r#"{"0":"z"}"#).g("0").str(), "z");
    assert_eq!(parse_str(r#"{"a":{"1":"z"}}"#).g("a.1").str(), "z");
    assert!(!parse_str(r#"{"a":1}"#).g("").exists());
    assert_eq!(parse_str(r#"{"ab":{"c":1}}"#).g("a*.c").int(), 1);
}

#[test]
fn gjson_coercions() {
    let r = |j: &str| parse_str(j);
    let a = r(r#"{"a":"12abc"}"#);
    assert_eq!((a.g("a").int(), a.g("a").float(), a.g("a").bool()), (0, 0.0, false));
    let a = r(r#"{"a":"1.9"}"#);
    assert_eq!((a.g("a").int(), a.g("a").float()), (0, 1.9));
    let a = r(r#"{"a":1.9}"#);
    assert_eq!((a.g("a").str(), a.g("a").int(), a.g("a").bool()), ("1.9".into(), 1, true));
    assert_eq!(r(r#"{"a":-1.9}"#).g("a").int(), -1);
    let a = r(r#"{"a":1e3}"#);
    // serde_json normalizes the raw exponent form (1e3 -> 1e+3); value semantics match gjson.
    assert_eq!((a.g("a").str(), a.g("a").int()), ("1000".into(), 1000));
    assert!(r(r#"{"a":"true"}"#).g("a").bool());
    assert!(r(r#"{"a":"t"}"#).g("a").bool());
    let a = r(r#"{"a":"1"}"#);
    assert_eq!((a.g("a").int(), a.g("a").bool()), (1, true));
    assert_eq!(r(r#"{"a":0.5}"#).g("a").int(), 0);
    let a = r(r#"{"a":null}"#);
    assert_eq!((a.g("a").kind(), a.g("a").str(), a.g("a").raw(), a.g("a").exists()), (Kind::Null, "".into(), "null".into(), true));
    assert!(r(r#"{"a":[]}"#).g("a").is_array());
    let a = r(r#"{"a":9007199254740993}"#);
    assert_eq!((a.g("a").str(), a.g("a").int()), ("9007199254740993".into(), 9007199254740993));
    assert_eq!(r(r#"{"a":"9007199254740993"}"#).g("a").int(), 9007199254740993);
    assert_eq!(r(r#"{"a":{"b" : 1}}"#).g("a").str(), r#"{"b":1}"#);
}

#[test]
fn array_and_foreach() {
    let j = parse_str(r#"{"a":"s","n":null,"arr":[1,2]}"#);
    assert_eq!(j.g("a").array().len(), 1);
    assert_eq!(j.g("n").array().len(), 0);
    assert_eq!(j.g("missing").array().len(), 0);
    assert_eq!(j.g("arr").array().len(), 2);
    let mut keys = vec![];
    j.g("@this").for_each(|k, _| {
        keys.push(k.str());
        true
    });
    assert_eq!(keys, ["a", "n", "arr"]);
}

#[test]
fn sjson_refuses_named_key_into_array() {
    let mut x = parse_str(r#"{"a":[1]}"#);
    assert!(!set(&mut x, "a.b", json!(2)));
    assert!(!set(&mut x, "a.b.c", json!(2)));
    assert_eq!(to_string(&x), r#"{"a":[1]}"#);
    assert_eq!(s(r#"{"a":[{"x":1}]}"#, "a.0.b", json!(2)), r#"{"a":[{"x":1,"b":2}]}"#);
    assert_eq!(s(r#"{"a":{}}"#, "a.-1", json!(2)), r#"{"a":{"-1":2}}"#);
}

#[test]
fn gjson_big_integers_wrap_like_go() {
    let w = |j: &str| parse_str(&format!(r#"{{"a":{j}}}"#));
    assert_eq!(w("99999999999999999999").g("a").int(), 7766279631452241919);
    assert_eq!(w("-99999999999999999999").g("a").int(), -7766279631452241919);
    assert_eq!(w("-99999999999999999999").g("a").uint(), 9223372036854775808);
    assert_eq!(w("18446744073709551615").g("a").int(), -1);
    assert_eq!(w("18446744073709551615").g("a").uint(), 18446744073709551615);
    assert_eq!(w("1e30").g("a").int(), i64::MIN);
    assert_eq!(w("1e30").g("a").uint(), 9223372036854775808);
    assert_eq!(w(r#""99999999999999999999""#).g("a").int(), 7766279631452241919);
}

#[test]
fn judge_regressions() {
    // Non-ASCII query keys must not panic.
    let j = parse_str(r#"{"a":[{"nämé":1,"x":2}]}"#);
    assert_eq!(j.g("a.#(nämé==1).x").int(), 2);
    assert!(!j.g(r"a.#(\é==1)").exists());
    // `@type` is a key, `@this` a modifier.
    let j = parse_str(r#"{"properties":{"@type":{"type":"string"}}}"#);
    assert_eq!(j.g("properties.@type.type").str(), "string");
    let mut m = j.clone();
    set(&mut m, "properties.@type.description", json!("d"));
    assert_eq!(m.g("properties.@type.description").str(), "d");
    // Parentheses outside #(...) are ordinary key characters.
    let j = parse_str(r#"{"p":{"a(b":{"c":1}}}"#);
    assert_eq!(j.g("p.a(b.c").int(), 1);
    // Deep documents within MAX_DEPTH parse; beyond it they are rejected.
    let deep = |n: usize| format!("{}1{}", "[".repeat(n), "]".repeat(n));
    assert!(parse_str(&deep(500)).is_array());
    assert!(parse_str(&deep(MAX_DEPTH + 1)).is_null());
    assert!(valid(deep(500).as_bytes()));
    assert!(!valid(b"{} trailing"));
}

#[test]
fn lone_surrogates_and_raw_at() {
    assert_eq!(parse(br#"{"a":"\ud800"}"#).g("a").str(), "\u{fffd}");
    assert_eq!(parse(br#"{"a":"x\udc00y"}"#).g("a").str(), "x\u{fffd}y");
    assert_eq!(parse(br#"{"a":"\ud83d\ude00"}"#).g("a").str(), "\u{1f600}");
    assert_eq!(parse(br#"{"a":"\\ud800"}"#).g("a").str(), "\\ud800");
    let src = br#"{ "a" : { "b" : [ 1 , {"c": {"x": 1,  "x": 2}} ] }, "k.d": true }"#;
    assert_eq!(raw_at(src, "a.b.1.c"), Some(r#"{"x": 1,  "x": 2}"#));
    assert_eq!(raw_at(src, "a.b.0"), Some("1"));
    assert_eq!(raw_at(src, r"k\.d"), Some("true"));
    assert_eq!(raw_at(src, "a.zz"), None);
    assert_eq!(raw_at(b"[1,2", "5"), None);
}

#[test]
fn raw_children_and_deep_malformed_input() {
    let src = br#"{"a":[ {"x": 1} , [ 2 ],"s" ], "o": {"k" : 1, "j": [ ]}}"#;
    assert_eq!(raw_children(src, "a"), vec![r#"{"x": 1}"#, "[ 2 ]", r#""s""#]);
    assert_eq!(raw_children(src, "o"), vec!["1", "[ ]"]);
    assert!(raw_children(src, "missing").is_empty());
    // Malformed and very deep: must not overflow the stack (tolerant fallback is depth-checked).
    let mut evil = br#"{"input":"#.to_vec();
    evil.extend(std::iter::repeat_n(b'[', 200_000));
    assert!(parse(&evil).is_null());
}

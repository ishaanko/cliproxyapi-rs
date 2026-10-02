//! Focused tests for the tricky parts of the util port. Expected values were produced by running
//! the Go implementation (they pin quirks such as `((nullable))`, hash-suffixed names and
//! sorted-key re-marshalling), not derived from first principles.

use std::collections::HashMap;

use super::*;
use cpa_json::{J, Value};

/// Compares a cleaned schema to the Go output as compact JSON text (key order matters).
fn assert_same_json(actual: &str, expected: &str) {
    let normalize = |s: &str| {
        serde_json::to_string(&serde_json::from_str::<Value>(s).expect("valid json")).unwrap()
    };
    assert_eq!(normalize(actual), normalize(expected));
}

#[test]
fn function_name_sanitization() {
    assert_eq!(
        sanitize_function_name("9lives.tool name"),
        "_9lives.tool_name"
    );
    assert_eq!(
        sanitize_function_name(&"9".repeat(64)),
        format!("_{}", "9".repeat(63))
    );
    assert_eq!(sanitize_function_name(""), "");
    assert_eq!(sanitize_function_name("日本語"), "___");
    assert_eq!(
        sanitize_claude_function_name("mcp__srv.tool:name"),
        "mcp__srv_tool_name"
    );
    assert_eq!(sanitize_claude_function_name(&"x".repeat(70)).len(), 64);
}

#[test]
fn colliding_sanitized_names_get_hash_suffixes() {
    let body = br#"{"tools":[{"name":"a b"},{"name":"a_b"},{"name":"a b"},{"function":{"name":"ok.name"}},{"functionDeclarations":[{"name":"x y"}]}]}"#;
    let forward = sanitized_function_name_map(body);
    assert_eq!(forward["a b"], "a_b_eebd4769fc73");
    assert_eq!(forward["a_b"], "a_b_3fab944de3f4");
    assert_eq!(forward["ok.name"], "ok.name");
    assert_eq!(forward["x y"], "x_y");

    let reverse = disambiguated_tool_name_map(body);
    assert_eq!(
        reverse.len(),
        3,
        "unchanged names are not in the reverse map"
    );
    assert_eq!(reverse["a_b_eebd4769fc73"], "a b");
    assert_eq!(restore_sanitized_tool_name(&reverse, "x_y"), "x y");
    assert_eq!(restore_sanitized_tool_name(&reverse, "other"), "other");
    assert_eq!(
        map_sanitized_function_name(&forward, "a b"),
        "a_b_eebd4769fc73"
    );
    assert_eq!(
        map_sanitized_function_name(&forward, "unknown name"),
        "unknown_name"
    );

    // The legacy map only keeps the first of colliding names.
    assert_eq!(
        sanitized_tool_name_map(body),
        HashMap::from([("a_b".to_string(), "a b".to_string())])
    );
}

#[test]
fn claude_tool_name_map_is_case_and_underscore_insensitive() {
    let map = tool_name_map_from_claude_request(
        br#"{"tools":[{"name":"Read"},{"function":{"name":"_Bash"}},{"name":"read"}]}"#,
    );
    assert_eq!(map_tool_name(&map, "READ"), "Read");
    assert_eq!(map_tool_name(&map, "bash"), "_Bash");
    assert_eq!(map_tool_name(&map, "unknown"), "unknown");
}

#[test]
fn gemini_tool_use_id_hashes_go_canonical_args() {
    // Go re-marshals args: sorted keys, float64 numbers (1.0 -> 1), HTML escaping.
    let id = gemini_claude_tool_use_id("call_1", "bash", r#"{"b":1,"a":[1.0,"<x>"]}"#);
    assert_eq!(id, "cpa_gemini_7ff8f44c724a5d4ba2783430d28c006c");
    assert_eq!(
        id,
        gemini_claude_tool_use_id(" call_1 ", "bash", r#"{ "a": [1, "<x>"], "b": 1 }"#)
    );
    assert!(is_gemini_claude_tool_use_id(&id));
    assert!(!is_gemini_claude_tool_use_id("cpa_gemini_abc"));
    assert_eq!(gemini_claude_tool_use_id("", "bash", ""), "");
}

#[test]
fn gemini_cleaner_flattens_and_hints() {
    let out = clean_json_schema_for_gemini(
        r##"{"type":"object","properties":{"a":{"type":["string","null"],"minLength":2},"m":{"const":"fixed"},
        "u":{"anyOf":[{"type":"string"},{"type":"null"}],"description":"d"},"e":{"type":"integer","enum":[1,2]},
        "r":{"$ref":"#/$defs/R"}},"required":["a","ghost"],"additionalProperties":false,"$defs":{"R":{"type":"string"}}}"##,
    );
    assert_same_json(
        &out,
        r#"{"type":"object","properties":{"a":{"type":"string","description":"minLength: 2 ((nullable))"},"m":{"enum":["fixed"],"type":"string"},"u":{"type":"string","description":"d (Accepts: string | null)"},"e":{"type":"string","enum":["1","2"],"description":"Allowed: 1, 2"},"r":{"type":"object","description":"See: R"}},"description":"No extra properties allowed"}"#,
    );
}

#[test]
fn cleaner_preserves_properties_named_like_keywords() {
    let out = clean_json_schema_for_gemini(
        r#"{"type":"object","properties":{"properties":{"type":"object","properties":{"title":{"type":"string"}},"propertyNames":{"pattern":"^a"}}}}"#,
    );
    // The unsupported `propertyNames` inside the schema of a property named "properties" is
    // dropped, while the property called "title" survives.
    assert_same_json(
        &out,
        r#"{"type":"object","properties":{"properties":{"type":"object","properties":{"title":{"type":"string"}}}}}"#,
    );
}

#[test]
fn cleaner_repairs_malformed_mcp_schemas_with_sorted_keys() {
    // Bare property map + boolean `required` (Asana-style MCP tools); repair re-marshals sorted.
    let out = clean_json_schema_for_gemini(
        r#"{"a":{"type":"string"},"b":{"type":"integer","required":true}}"#,
    );
    assert_eq!(
        out,
        r#"{"properties":{"a":{"type":"string"},"b":{"type":"integer"}},"required":["b"],"type":"object"}"#
    );
}

#[test]
fn cleaner_merges_all_of() {
    let out = clean_json_schema_for_gemini(
        r#"{"allOf":[{"properties":{"a":{"type":"string"}},"required":["a"]},{"properties":{"b":{"type":"integer"}},"required":["b"]}]}"#,
    );
    assert_same_json(
        &out,
        r#"{"properties":{"a":{"type":"string"},"b":{"type":"integer"}},"required":["a","b"]}"#,
    );
}

#[test]
fn antigravity_placeholder_and_cyclic_refs() {
    assert_same_json(
        &clean_json_schema_for_antigravity(r#"{"type":"object","properties":{}}"#),
        r#"{"type":"object","properties":{"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"}},"required":["reason"]}"#,
    );
    // A self-referencing $def terminates as a "See: N" hint; inlining re-marshals with sorted keys.
    let out = clean_json_schema_for_antigravity(
        r##"{"type":"object","properties":{"n":{"$ref":"#/$defs/N"}},"$defs":{"N":{"type":"object","properties":{"c":{"$ref":"#/$defs/N"}}}}}"##,
    );
    assert_eq!(
        out,
        r#"{"properties":{"n":{"properties":{"c":{"description":"See: N","type":"object","properties":{"reason":{"type":"string","description":"Brief explanation of why you are calling this tool"}},"required":["reason"]},"_":{"type":"boolean"}},"type":"object","required":["_"]}},"type":"object"}"#
    );
}

#[test]
fn antigravity_response_keeps_additional_properties_false() {
    let out = clean_json_schema_for_antigravity_response(
        r#"{"type":"object","properties":{"a":{"type":["string","null"],"minimum":1}},"additionalProperties":false}"#,
    );
    assert_same_json(
        &out,
        r#"{"type":"object","properties":{"a":{"type":"string","description":"minimum: 1 ((nullable))","nullable":true}},"additionalProperties":false}"#,
    );
}

#[test]
fn gemini_json_schema_carrier_keeps_standard_constraints() {
    let input = r#"{"type":"object","properties":{"s":{"type":"string","pattern":"^a","minLength":1}},"additionalProperties":{"type":"string"}}"#;
    assert_same_json(&clean_json_schema_for_gemini_json_schema(input), input);
}

#[test]
fn hint_quotes_the_original_raw_json() {
    // `Raw` in Go keeps the input's whitespace; pretty-printed constraint values keep it.
    let out = clean_json_schema_for_gemini(
        "{\"type\":\"object\",\"properties\":{\"t\":{\"type\":\"array\",\"items\":{\"type\":\"string\"},\"default\":[ 1,  2 ]}}}",
    );
    assert_eq!(
        cpa_json::parse_str(&out)
            .g("properties.t.description")
            .str(),
        "default: [ 1,  2 ]"
    );
}

#[test]
fn claude_schema_union_flattening() {
    let out = normalize_claude_tool_input_schema(
        br#"{"anyOf":[{"type":"object","properties":{"z":{"type":"string"}}},{"type":"string"},{"properties":{"a":{"type":"integer"},"z":{"type":"null"}}}],"allOf":[{"required":["z"]},{"required":["z","a"]}],"description":"d"}"#,
    );
    assert_eq!(
        String::from_utf8(out).unwrap(),
        r#"{"description":"d","properties":{"a":{"type":"integer"},"z":{"type":"string"}},"required":["z","a"],"type":"object"}"#
    );
    assert_eq!(
        normalize_claude_tool_input_schema(b"[1]"),
        br#"{"type":"object","properties":{}}"#
    );
    assert_eq!(
        normalize_claude_tool_input_schema(b""),
        br#"{"type":"object","properties":{}}"#
    );
}

#[test]
fn unicode_property_escape_detection() {
    for (pattern, expected) in [
        (r"\p{L}+", true),
        (r"\P{N}", true),
        (r"^[^\0]*$", true),
        (r"\\p{L}", false),
        (r"\pL", false),
        (r"\x00", false),
        (r"abc\", false),
    ] {
        assert_eq!(
            has_unsupported_unicode_property_escape(pattern),
            expected,
            "{pattern}"
        );
    }
}

#[test]
fn fix_json_converts_single_quoted_strings() {
    assert_eq!(
        fix_json(r#"{'a': 'it\'s "q"', 'b': "x"}"#),
        r#"{"a": "it's \"q\"", "b": "x"}"#
    );
    assert_eq!(fix_json("{'a': 'unterminated"), r#"{"a": "unterminated""#);
    assert_eq!(fix_json(r"{'a':'é\u12'}"), r#"{"a":"é\u12"}"#);
}

#[test]
fn responses_tool_precedence_and_namespaces() {
    let root = cpa_json::parse_str(
        r#"{"tools":[{"type":"function","name":"dup","description":"top"},
            {"type":"namespace","name":"ns","tools":[{"type":"function","name":"dup","description":"child"},{"type":"custom","name":"apply_patch"}]}],
            "input":[{"type":"additional_tools","tools":[{"type":"function","name":"dup","description":"extra"}]}]}"#,
    );
    let (decls, forward, reverse) = build_gemini_function_declarations(&root);
    let names: Vec<String> = decls.iter().map(|d| d.g("name").str()).collect();
    assert_eq!(names, ["dup", "ns__dup", "ns__apply_patch"]);
    assert_eq!(
        decls[0].g("description").str(),
        "top",
        "top-level direct declaration wins"
    );
    assert_eq!(
        forward["apply_patch"], "ns__apply_patch",
        "local name resolves to the qualified tool"
    );
    assert!(reverse["ns__apply_patch"].apply_patch && reverse["ns__apply_patch"].custom);
    assert_eq!(reverse["ns__dup"].namespace, "ns");
    assert_eq!(decls[2].g("parametersJsonSchema.required.0").str(), "input");

    let winners = collect_responses_tool_winners(&root);
    assert_eq!(
        (
            winners["dup"].source_priority,
            winners["dup"].direct,
            winners["dup"].order
        ),
        (0, true, 0)
    );

    assert_eq!(
        qualify_responses_namespace_tool_name("functions__", "exec"),
        "functions__exec"
    );
    assert_eq!(
        qualify_responses_namespace_tool_name("ns", "mcp__x"),
        "mcp__x"
    );
    assert_eq!(
        qualify_responses_namespace_tool_name("ns", "ns__x"),
        "ns__x"
    );
}

#[test]
fn responses_tool_choice_and_custom_input() {
    let forward = HashMap::from([("ns__fn".to_string(), "ns__fn_g".to_string())]);
    let choice = cpa_json::parse_str(r#"{"type":"function","name":"fn","namespace":"ns"}"#);
    let cfg = convert_responses_tool_choice_to_gemini(Some(&choice), &forward).unwrap();
    assert_eq!(
        cfg.to_string(),
        r#"{"mode":"ANY","allowedFunctionNames":["ns__fn_g"]}"#
    );
    assert!(
        convert_responses_tool_choice_to_gemini(Some(&Value::String("bogus".into())), &forward)
            .is_none()
    );
    assert!(convert_responses_tool_choice_to_gemini(None, &forward).is_none());

    assert_eq!(
        unwrap_responses_custom_tool_input(r#"{"input":"abc"}"#),
        "abc"
    );
    assert_eq!(
        unwrap_responses_custom_tool_input(r#"{"input":{"a":1}}"#),
        r#"{"a":1}"#
    );
    assert_eq!(unwrap_responses_custom_tool_input("{}"), "");
    assert_eq!(unwrap_responses_custom_tool_input("raw text"), "raw text");
}

#[test]
fn claude_tool_result_conversion() {
    let content = cpa_json::parse_str(
        r#"[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAA"}},{"type":"text","text":"t"},{"type":"text","text":"u"}]"#,
    );
    let r = convert_claude_tool_result_content(Some(&content));
    assert!(r.result_is_raw);
    assert_eq!(
        r.result,
        r#"[{"type":"text","text":"t"},{"type":"text","text":"u"}]"#
    );
    assert_eq!(
        r.images,
        [ClaudeToolResultImage {
            mime_type: "image/png".into(),
            data: "AAA".into()
        }]
    );
    assert_eq!(
        convert_claude_tool_result_content(Some(&Value::String("hi".into()))).result,
        "hi"
    );
    assert_eq!(
        convert_claude_tool_result_content(None),
        ClaudeToolResult::default()
    );
}

#[test]
fn attribution_blocks_are_stripped() {
    let body = br#"{"system":[{"type":"text","text":" x-anthropic-billing-header: cc=1"},{"type":"text","text":"keep"}],"messages":[]}"#;
    let out = strip_claude_code_attribution_system(body);
    assert_eq!(
        out,
        br#"{"system":[{"type":"text","text":"keep"}],"messages":[]}"#
    );
    let untouched = br#"{ "system": "keep" }"#;
    assert_eq!(strip_claude_code_attribution_system(untouched), untouched);
}

#[test]
fn declarations_dedupe_by_name() {
    let out = deduplicate_function_declarations(
        br#"[{"name":"a","x":1},{"name":"a"},{"description":"unnamed"},{"name":"b"}]"#,
    );
    assert_eq!(
        String::from_utf8(out).unwrap(),
        r#"[{"name":"a","x":1},{"description":"unnamed"},{"name":"b"}]"#
    );
}

#[test]
fn credential_masking() {
    assert_eq!(hide_api_key("sk-1234567890abcdef"), "sk-1...cdef");
    assert_eq!(hide_api_key("abcdef"), "ab...ef");
    assert_eq!(hide_api_key("abc"), "a...c");
    assert_eq!(hide_api_key("ab"), "ab");
    assert_eq!(
        mask_sensitive_header_value("Authorization", "Bearer abcdefghijkl"),
        "Bearer abcd...ijkl"
    );
    assert_eq!(
        mask_sensitive_query("a=b&api_key=abcdefghijk&x=1"),
        "a=b&api_key=abcd...hijk&x=1"
    );
    assert_eq!(mask_sensitive_query("a=b"), "a=b");
}

#[test]
fn custom_headers_resolve_client_variables_and_session_id() {
    use http::HeaderMap;
    let attrs = HashMap::from([
        ("header:X-Static".to_string(), "v".to_string()),
        ("header:X-From-Client".to_string(), "$Abc".to_string()),
        ("header:X-Missing".to_string(), "$Nope".to_string()),
        (
            "header:X-Session".to_string(),
            "pre-$cpa-session-id-post".to_string(),
        ),
        ("other".to_string(), "ignored".to_string()),
    ]);
    let mut client = HeaderMap::new();
    client.insert("abc", "from-client".parse().unwrap());
    let mut out = HeaderMap::new();
    apply_custom_headers_from_attrs(&mut out, &attrs, Some(&client), Some("sess-1"));
    assert_eq!(out["x-static"], "v");
    assert_eq!(out["x-from-client"], "from-client");
    assert_eq!(out["x-session"], "pre-sess-1-post");
    assert!(!out.contains_key("x-missing"));

    // An explicit empty session id clears the variable, so the header is omitted.
    let mut cleared = HeaderMap::new();
    apply_custom_headers_from_attrs(&mut cleared, &attrs, Some(&client), Some(""));
    assert!(!cleared.contains_key("x-session"));
}

#[test]
fn auth_dir_resolution() {
    assert_eq!(resolve_auth_dir("/a/b/../c/").unwrap(), "/a/c");
    assert_eq!(filepath_clean("a//b/./../c"), "a/c");
    assert_eq!(filepath_clean("/.."), "/");
    assert_eq!(filepath_clean(""), ".");
}

#[test]
fn go_json_helpers_match_go_encoding() {
    assert_eq!(
        go_json_string("a<b>&\u{2028}\n"),
        "\"a\\u003cb\\u003e\\u0026\\u2028\\n\""
    );
    assert_eq!(
        go_json_canonicalize(r#"{"b":1.0,"a":[1e21,1e-7,0.1,-0.0]}"#).unwrap(),
        r#"{"a":[1e+21,1e-7,0.1,-0],"b":1}"#
    );
    assert_eq!(
        go_json_canonicalize("12345678901234567890").unwrap(),
        "12345678901234567000"
    );
    assert!(go_json_canonicalize("not json").is_none());
}

#[test]
fn white_image_is_a_png_of_the_right_size() {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(create_white_image_base64("16:9").unwrap())
        .unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 1344);
    assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 768);
}

//! Codex client tool integer normalization (Go: internal/client/codex/tool-schema
//! `IsCodexUserAgent` / `NormalizeCodexToolIntegerTypes`, called from helps/payload_helpers.go).
//!
//! Codex clients deserialize some tool parameters as integers; schemas that declare them as
//! `number` (for example after a Gemini round trip) are rewritten to `integer` for a fixed set
//! of well-known tools. Only the payload pipeline needs this, so the rest of the Go tool-schema
//! package stays with the Codex executor.

use std::collections::HashSet;

use http::HeaderMap;
use serde_json::Value;

/// Whether the `User-Agent` header (first non-blank value) names a Codex client.
pub fn is_codex_user_agent(headers: Option<&HeaderMap>) -> bool {
    let Some(headers) = headers else {
        return false;
    };
    headers
        .get_all(http::header::USER_AGENT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .is_some_and(|ua| ua.to_lowercase().contains("codex"))
}

/// Integer parameters per tool. Entries are explicit schema paths relative to
/// `parameters.properties` (gjson syntax, array indexes included), not recursive field names.
fn target_fields(tool_name: &str) -> &'static [&'static str] {
    let name = tool_name.trim();
    let name = name
        .strip_prefix("functions__")
        .or_else(|| name.strip_prefix("collab__"))
        .unwrap_or(name);
    let name = match name {
        "multi_agent_v1__wait_agent" | "collaboration__wait_agent" => "wait_agent",
        "collaboration__get_channels"
        | "collaboration__list_threads"
        | "collaboration__search_posts"
        | "collaboration__read_thread"
        | "collaboration__read_post" => &name["collaboration__".len()..],
        other => other,
    };
    match name {
        "exec_command" => &["yield_time_ms", "max_output_tokens", "timeout_ms"],
        "write_stdin" => &["session_id", "yield_time_ms", "max_output_tokens"],
        "sleep" => &["duration_ms"],
        "wait_agent" => &["timeout_ms"],
        "wait" => &["yield_time_ms", "max_tokens"],
        "tool_search" => &["limit"],
        "test_sync_tool" => &[
            "sleep_before_ms",
            "sleep_after_ms",
            "participants",
            "timeout_ms",
            "barrier.properties.participants",
            "barrier.properties.timeout_ms",
        ],
        "create_goal" => &["token_budget"],
        "get_channels" => &["limit"],
        "list_threads" | "search_posts" | "read_thread" => &["limit", "max_chars_per_post"],
        "read_post" => &["offset_chars", "limit_chars"],
        "memories__list" => &["max_results"],
        "memories__read" => &["line_offset", "max_lines"],
        "memories__search" => &["context_lines", "max_results"],
        "history__list_windows" => &["limit"],
        "history__list_items" => &["limit", "max_chars_per_item"],
        "history__read_item" => &["offset_chars", "limit_chars"],
        "history__search_contents" => &["limit"],
        "notes__list_files_by_prefix" => &["max_results"],
        // Codex declares signed line numbers in the first nullable union branch.
        "notes__read_file" => &["start_line", "stop_line", "start_line.anyOf.0", "stop_line.anyOf.0"],
        "notes__search_contents" => &["max_matches_per_file", "max_files"],
        "image_gen__imagegen" => &["num_last_images_to_include"],
        "web__run" => &[
            "search_query.items.properties.recency",
            "image_query.items.properties.recency",
            "open.items.properties.lineno",
            "click.items.properties.id",
            "screenshot.items.properties.pageno",
            "weather.items.properties.duration",
            "sports.items.properties.num_games",
        ],
        _ => &[],
    }
}

/// Walks a dotted gjson-style `path` (object keys and array indexes) below `node`.
fn property_at_mut<'a>(node: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut cur = node;
    for segment in path.split('.') {
        cur = match cur {
            Value::Object(map) => map.get_mut(segment)?,
            Value::Array(items) => items.get_mut(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Rewrites `number` to `integer` in the declared types at the `fields` paths; true when anything
/// changed.
fn normalize_field_types(params: &mut Value, fields: &[&str]) -> bool {
    let Some(properties) = params.get_mut("properties").filter(|p| p.is_object()) else {
        return false;
    };
    let mut changed = false;
    for field in fields {
        let Some(type_val) = property_at_mut(properties, field).and_then(|p| p.get_mut("type")) else {
            continue;
        };
        match type_val {
            Value::String(s) if s == "number" => {
                *s = "integer".into();
                changed = true;
            }
            Value::Array(items) => {
                let mut has_number = false;
                let mut seen: HashSet<String> = HashSet::new();
                let mut next = Vec::with_capacity(items.len());
                for item in items.iter() {
                    // Go's gjson `String()` stringifies non-string items (null becomes "").
                    let mut s = match item {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    if s == "number" {
                        has_number = true;
                        s = "integer".into();
                    }
                    if seen.insert(s.clone()) {
                        next.push(Value::String(s));
                    }
                }
                if has_number {
                    *type_val = Value::Array(next);
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

/// `namespace` is the enclosing namespace tool's name ("" at the top level); its tools match as
/// `<namespace>__<name>`.
fn normalize_array(tools: &mut Value, namespace: &str) -> bool {
    let Some(items) = tools.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for tool in items {
        changed |= normalize_element(tool, namespace);
    }
    changed
}

fn normalize_element(tool: &mut Value, namespace: &str) -> bool {
    if tool.get("type").and_then(Value::as_str) == Some("namespace") {
        // Only one namespace level with a name is meaningful.
        if !namespace.is_empty() {
            return false;
        }
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if name.is_empty() {
            return false;
        }
        return tool.get_mut("tools").is_some_and(|nested| normalize_array(nested, &name));
    }
    for key in ["function_declarations", "functionDeclarations"] {
        if tool.get(key).is_some_and(Value::is_array) {
            return tool.get_mut(key).is_some_and(|decls| normalize_array(decls, namespace));
        }
    }
    let name = tool.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let is_object = |v: Option<&Value>| v.is_some_and(Value::is_object);
    let (path, name): (&[&str], String) = if is_object(tool.get("parameters")) {
        (&["parameters"], name)
    } else if is_object(tool.pointer("/function/parameters")) {
        let name = if name.is_empty() {
            tool.pointer("/function/name").and_then(Value::as_str).unwrap_or("").to_string()
        } else {
            name
        };
        (&["function", "parameters"], name)
    } else if is_object(tool.get("input_schema")) {
        (&["input_schema"], name)
    } else if is_object(tool.get("parametersJsonSchema")) {
        (&["parametersJsonSchema"], name)
    } else {
        return false;
    };
    let name = if namespace.is_empty() { name } else { format!("{namespace}__{name}") };
    let fields = target_fields(&name);
    if fields.is_empty() {
        return false;
    }
    let mut cur = tool;
    for key in path {
        match cur.get_mut(*key) {
            Some(next) => cur = next,
            None => return false,
        }
    }
    normalize_field_types(cur, fields)
}

/// Normalizes the well-known tool parameters to integers across `tools` and
/// `input[].additional_tools`, for Codex clients only. Returns `body` untouched (same bytes)
/// when the request is not from Codex or nothing changed.
pub fn normalize_codex_tool_integer_types(body: &[u8], headers: Option<&HeaderMap>) -> Vec<u8> {
    if body.is_empty() || !is_codex_user_agent(headers) {
        return body.to_vec();
    }
    let mut root = cpa_json::parse(body);
    if !root.is_object() {
        return body.to_vec();
    }
    let mut changed = false;
    if let Some(tools) = root.get_mut("tools") {
        changed |= normalize_array(tools, "");
    }
    if let Some(items) = root.get_mut("input").and_then(Value::as_array_mut) {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools")
                && let Some(tools) = item.get_mut("tools")
            {
                changed |= normalize_array(tools, "");
            }
        }
    }
    if !changed {
        return body.to_vec();
    }
    tracing::debug!("codex: normalized target tool number types to integer");
    cpa_json::to_vec(&root)
}

#[cfg(test)]
mod tests {
    //! Ported from tool_schema_integer_fields_test.go. The Go tests compare bytes; this port
    //! re-serializes, so outputs are compared as compact, order-preserving JSON.

    use super::*;
    use cpa_json::J;
    use http::HeaderValue;

    fn headers(ua: &str) -> Option<HeaderMap> {
        if ua.is_empty() {
            return None;
        }
        let mut h = HeaderMap::new();
        h.insert(http::header::USER_AGENT, HeaderValue::from_str(ua).expect("ua"));
        Some(h)
    }

    fn compact(raw: &[u8]) -> Vec<u8> {
        cpa_json::to_vec(&cpa_json::parse(raw))
    }

    fn set_raw(body: &[u8], path: &str, value: &str) -> Vec<u8> {
        let mut root = cpa_json::parse(body);
        cpa_json::set_raw(&mut root, path, value).expect("set_raw");
        cpa_json::to_vec(&root)
    }

    fn normalize(input: &[u8], ua: &str) -> Vec<u8> {
        compact(&normalize_codex_tool_integer_types(input, headers(ua).as_ref()))
    }

    #[test]
    fn source_fields() {
        // Paths follow the input schemas and Deserialize types in codex-rs core and ext.
        let cases: &[(&str, &[&str])] = &[
            ("test_sync_tool", &["barrier.properties.participants", "barrier.properties.timeout_ms"]),
            ("create_goal", &["token_budget"]),
            ("get_channels", &["limit"]),
            ("list_threads", &["limit", "max_chars_per_post"]),
            ("search_posts", &["limit", "max_chars_per_post"]),
            ("read_thread", &["limit", "max_chars_per_post"]),
            ("read_post", &["offset_chars", "limit_chars"]),
            ("memories__list", &["max_results"]),
            ("memories__read", &["line_offset", "max_lines"]),
            ("memories__search", &["context_lines", "max_results"]),
            ("history__list_windows", &["limit"]),
            ("history__list_items", &["limit", "max_chars_per_item"]),
            ("history__read_item", &["offset_chars", "limit_chars"]),
            ("history__search_contents", &["limit"]),
            ("notes__list_files_by_prefix", &["max_results"]),
            ("notes__read_file", &["start_line", "stop_line", "start_line.anyOf.0", "stop_line.anyOf.0"]),
            ("notes__search_contents", &["max_matches_per_file", "max_files"]),
            ("image_gen__imagegen", &["num_last_images_to_include"]),
            (
                "web__run",
                &[
                    "search_query.items.properties.recency",
                    "image_query.items.properties.recency",
                    "open.items.properties.lineno",
                    "click.items.properties.id",
                    "screenshot.items.properties.pageno",
                    "weather.items.properties.duration",
                    "sports.items.properties.num_games",
                ],
            ),
            ("collaboration__wait_agent", &["timeout_ms"]),
            ("multi_agent_v1__wait_agent", &["timeout_ms"]),
            ("functions__create_goal", &["token_budget"]),
            ("collab__read_post", &["offset_chars", "limit_chars"]),
            ("collaboration__get_channels", &["limit"]),
            ("collaboration__list_threads", &["limit", "max_chars_per_post"]),
            ("collaboration__search_posts", &["limit", "max_chars_per_post"]),
            ("collaboration__read_thread", &["limit", "max_chars_per_post"]),
            ("collaboration__read_post", &["offset_chars", "limit_chars"]),
        ];
        for (name, fields) in cases {
            for type_json in [r#""number""#, r#"["number","integer","null"]"#, r#""integer""#, r#""string""#] {
                let mut input = compact(
                    format!(
                        r#"{{"tools":[{{"type":"function","name":"{name}","parameters":{{"type":"object","properties":{{"unrelated":{{"type":"number","default":1.5}}}}}},"output_schema":{{"properties":{{"wall_time_seconds":{{"type":"number"}}}}}}}}],"input":[{{"type":"function_call","arguments":"{{\"limit\":1.5}}"}}]}}"#
                    )
                    .as_bytes(),
                );
                for field in *fields {
                    let prop = format!(r#"{{"type":{type_json},"description":"Keep metadata","minimum":0,"default":1}}"#);
                    input = set_raw(&input, &format!("tools.0.parameters.properties.{field}"), &prop);
                }
                for ua in ["codex-tui/0.154.0", "curl/8.7.1", ""] {
                    let mut want = input.clone();
                    if ua == "codex-tui/0.154.0" {
                        let want_type = match type_json {
                            r#""number""# => r#""integer""#,
                            r#"["number","integer","null"]"# => r#"["integer","null"]"#,
                            other => other,
                        };
                        for field in *fields {
                            want = set_raw(&want, &format!("tools.0.parameters.properties.{field}.type"), want_type);
                        }
                    }
                    let out = normalize(&input, ua);
                    assert_eq!(String::from_utf8_lossy(&out), String::from_utf8_lossy(&want), "{name} UA {ua:?} type {type_json}");
                    assert_eq!(normalize(&out, ua), out, "normalization is not idempotent");
                }
            }
        }
    }

    #[test]
    fn namespace_formats() {
        let schema = r#"{"type":"object","properties":{"line_offset":{"type":"number"},"max_lines":{"type":["number","null"]},"max_tokens":{"type":"number"}}}"#;
        let cases = [
            ("responses", r#"{"tools":[{"type":"function","name":"memories__read","parameters":SCHEMA}]}"#, "tools.0.parameters"),
            ("chat", r#"{"tools":[{"type":"function","function":{"name":"memories__read","parameters":SCHEMA}}]}"#, "tools.0.function.parameters"),
            ("claude", r#"{"tools":[{"name":"memories__read","input_schema":SCHEMA}]}"#, "tools.0.input_schema"),
            ("gemini", r#"{"tools":[{"function_declarations":[{"name":"memories__read","parameters":SCHEMA}]}]}"#, "tools.0.function_declarations.0.parameters"),
            ("gemini JSON schema", r#"{"tools":[{"functionDeclarations":[{"name":"memories__read","parametersJsonSchema":SCHEMA}]}]}"#, "tools.0.functionDeclarations.0.parametersJsonSchema"),
            ("namespace", r#"{"tools":[{"type":"namespace","name":"memories","tools":[{"type":"function","name":"read","parameters":SCHEMA}]}]}"#, "tools.0.tools.0.parameters"),
            ("additional namespace", r#"{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"memories","tools":[{"type":"function","name":"read","parameters":SCHEMA}]}]}]}"#, "input.0.tools.0.tools.0.parameters"),
        ];
        for (name, body, path) in cases {
            let input = compact(body.replace("SCHEMA", schema).as_bytes());
            let want = set_raw(&input, &format!("{path}.properties.line_offset.type"), r#""integer""#);
            let want = set_raw(&want, &format!("{path}.properties.max_lines.type"), r#"["integer","null"]"#);
            assert_eq!(String::from_utf8_lossy(&normalize(&input, "Codex/1.0")), String::from_utf8_lossy(&want), "{name}");
        }
    }

    #[test]
    fn preserves_unproven_fields() {
        let cases = [
            ("unknown_tool", ""),
            ("mcp__server__read_post", ""),
            ("read", ""),
            ("run", ""),
            ("imagegen", ""),
            ("read_post", "user_tools"),
            ("wait_agent", "mcp__server"),
            ("read", "skills"),
            ("read", "user_tools"),
            ("read_post", "multi_agent_v1"),
            ("read_post", "arbitrary_collaboration"),
        ];
        for (name, namespace) in cases {
            let mut tool = format!(
                r#"{{"type":"function","name":"{name}","parameters":{{"properties":{{"limit":{{"type":"number"}},"offset_chars":{{"type":"number"}},"line_offset":{{"type":"number"}},"timeout_ms":{{"type":"number"}}}}}}}}"#
            );
            if !namespace.is_empty() {
                tool = format!(r#"{{"type":"namespace","name":"{namespace}","tools":[{tool}]}}"#);
            }
            let input = compact(format!(r#"{{"tools":[{tool}]}}"#).as_bytes());
            assert_eq!(normalize(&input, "codex"), input, "unknown tool changed: {namespace}/{name}");
        }
        let input = compact(
            br#"{"tools":[{"name":"test_sync_tool","parameters":{"properties":{"barrier.participants":{"type":"number"},"other":{"properties":{"participants":{"type":"number"}}},"barrier":{"properties":{"participants":{"type":"number"},"ratio":{"type":"number"}}}}}}]}"#,
        );
        let want = set_raw(&input, "tools.0.parameters.properties.barrier.properties.participants.type", r#""integer""#);
        assert_eq!(normalize(&input, "codex"), want, "nested path changed unrelated fields");
    }

    #[test]
    fn history_notes_schemas() {
        // Parameter schemas copied from codex-rs/ext/history-notes/src/tools.rs.
        let fixture = cpa_json::parse(include_bytes!("testdata/history_notes_tools.json"));
        for namespace in fixture.g("tools").array() {
            let tool = namespace.g("tools.0");
            let name = format!("{}__{}", namespace.g("name").str(), tool.g("name").str());
            let params = tool.g("parameters").raw();
            let degrade = |s: &str| s.replace(r#""type":"integer""#, r#""type":"number""#);
            let formats = [
                ("namespace", format!(r#"{{"tools":[{}]}}"#, namespace.raw())),
                ("flat", format!(r#"{{"tools":[{{"name":"{name}","parameters":{params}}}]}}"#)),
                ("additional", format!(r#"{{"input":[{{"type":"additional_tools","tools":[{}]}}]}}"#, namespace.raw())),
            ];
            for (format_name, body) in &formats {
                // Simulate integer declarations degraded to number, retaining the real schema shape.
                let input = compact(degrade(body).as_bytes());
                for ua in ["codex", "curl", ""] {
                    let want = if ua == "codex" { compact(body.as_bytes()) } else { input.clone() };
                    let out = normalize(&input, ua);
                    assert_eq!(String::from_utf8_lossy(&out), String::from_utf8_lossy(&want), "{name} {format_name} UA {ua:?}");
                    assert_eq!(normalize(&out, ua), out, "source schema is not stable");
                }
            }
            for unknown in [tool.g("name").str(), format!("mcp__server__{name}"), format!("user__{name}")] {
                let input = compact(format!(r#"{{"tools":[{{"name":"{unknown}","parameters":{}}}]}}"#, degrade(&params)).as_bytes());
                assert_eq!(normalize(&input, "codex"), input, "unknown tool {unknown:?} changed");
            }
        }
    }

    #[test]
    fn notes_explicit_union_paths() {
        let input = compact(
            br#"{"tools":[{"name":"notes__read_file","parameters":{"type":"object","properties":{"start_line":{"anyOf":[{"type":"number"},{"type":"null"}],"default":-3},"stop_line":{"anyOf":[{"type":"number"},{"type":"number"}],"default":-1},"ratio":{"anyOf":[{"type":"number"},{"type":"null"}]},"other":{"properties":{"start_line":{"anyOf":[{"type":"number"}]}}}},"required":["path"]}}],"input":[{"type":"function_call","arguments":"{\"start_line\":-3,\"stop_line\":-1}"}]}"#,
        );
        let want = set_raw(&input, "tools.0.parameters.properties.start_line.anyOf.0.type", r#""integer""#);
        let want = set_raw(&want, "tools.0.parameters.properties.stop_line.anyOf.0.type", r#""integer""#);
        assert_eq!(String::from_utf8_lossy(&normalize(&input, "codex")), String::from_utf8_lossy(&want));
    }
}

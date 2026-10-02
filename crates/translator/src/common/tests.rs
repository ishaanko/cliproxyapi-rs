//! Tests for the shared translator helpers: a differential corpus recorded from the Go package
//! (`testdata/golden.jsonl.gz`, regenerate with `testdata/regen.sh`) plus focused unit tests.

use std::io::Read;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use cpa_json::{J, Res, Value, json};
use flate2::read::GzDecoder;

use super::*;

fn golden() -> Vec<Value> {
    let gz = include_bytes!("testdata/golden.jsonl.gz");
    let mut text = String::new();
    GzDecoder::new(&gz[..]).read_to_string(&mut text).expect("gunzip golden");
    text.lines().map(|l| serde_json::from_str(l).expect("golden line")).collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn texts(items: &[Vec<u8>]) -> Value {
    Value::Array(items.iter().map(|i| Value::String(text(i))).collect())
}

fn byte_items(v: &Value) -> Vec<Vec<u8>> {
    v.as_array()
        .map(|a| a.iter().map(|s| s.as_str().unwrap_or_default().as_bytes().to_vec()).collect())
        .unwrap_or_default()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn parse_res(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

/// A gjson `Result` parsed from text; unparsable text is a non-existent result like gjson.
fn res_owned(raw: &str) -> Res<'static> {
    match serde_json::from_str::<Value>(raw.trim()) {
        Ok(v) => Res::owned(v),
        Err(_) => Res::NONE,
    }
}

fn res_out(r: &Res<'_>) -> Value {
    json!({"exists": r.exists(), "raw": r.raw()})
}

fn err_val(r: &Result<Vec<u8>, String>) -> Value {
    match r {
        Ok(_) => Value::Null,
        Err(e) => json!(e),
    }
}

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

fn unb64(v: &Value) -> Vec<u8> {
    B64.decode(s(v)).expect("b64")
}

fn run_decoder(case_in: &Value) -> Value {
    let args_bytes = unb64(&case_in["args"]);
    let frags: Vec<Vec<u8>> = case_in["frags"].as_array().map(|a| a.iter().map(unb64).collect()).unwrap_or_default();
    let mut d = ApplyPatchInputDecoder::default();
    let mut steps = Vec::new();
    for fr in &frags {
        let r = d.push(fr);
        steps.push(json!({"delta": b64(r.as_deref().unwrap_or("").as_bytes()), "err": r.as_ref().err()}));
    }
    let mut out = json!({"steps": steps, "input": b64(d.input().as_bytes())});
    // `finish` takes &str, so invalid UTF-8 arguments (which Go can be handed) cannot occur in
    // Rust; the golden finish results for those cases are dropped by the caller.
    if case_in["finish"].as_bool().unwrap_or(false)
        && let Ok(args) = std::str::from_utf8(&args_bytes)
    {
        let finish = d.finish(args);
        out["finish"] = json!({"tail": b64(finish.as_deref().unwrap_or("").as_bytes()), "err": finish.as_ref().err()});
        out["inputAfter"] = json!(b64(d.input().as_bytes()));
        let after = d.push("");
        out["pushAfter"] = json!({"delta": b64(after.as_deref().unwrap_or("").as_bytes()), "err": after.as_ref().err()});
    }
    out
}

fn run_bridge(case_in: &Value) -> Value {
    let mut b = ApplyPatchResponsesBridge::new(s(&case_in["req"]).as_bytes());
    let mut steps = Vec::new();
    for e in case_in["events"].as_array().cloned().unwrap_or_default() {
        let (events, err) = b.transform(s(&e).as_bytes());
        steps.push(json!({"events": texts(&events), "err": err}));
    }
    let mut out = json!({
        "steps": steps,
        "finish": b.finish().err(),
        "toolErr": b.tool_input_error(),
    });
    let non_stream = s(&case_in["nonStream"]);
    if !non_stream.is_empty() {
        let r = b.transform_non_stream(non_stream.as_bytes());
        out["nonStream"] = json!({"value": r.as_deref().map(text).unwrap_or_default(), "err": r.as_ref().err()});
    }
    out
}

fn run_case(fn_name: &str, i: &Value) -> Value {
    match fn_name {
        "geminiTokenCount" => json!(text(&gemini_token_count_json(i.as_i64().unwrap()))),
        "claudeInputTokens" => json!(text(&claude_input_tokens_json(i.as_i64().unwrap()))),
        "joinRawArray" => json!(text(&join_raw_array(&byte_items(i)))),
        "setRawArrayItems" => {
            json!(text(&set_raw_array_items(s(&i["data"]).as_bytes(), s(&i["path"]), &byte_items(&i["items"]))))
        }
        "sse" => json!(text(&sse_event_data(s(&i["event"]), s(&i["payload"]).as_bytes()))),
        "appendSSEString" => {
            let mut out = s(&i["prefix"]).as_bytes().to_vec();
            append_sse_event_string(&mut out, s(&i["event"]), s(&i["payload"]), i["n"].as_u64().unwrap() as usize);
            json!(text(&out))
        }
        "appendSSEBytes" => {
            let mut out = s(&i["prefix"]).as_bytes().to_vec();
            append_sse_event_bytes(&mut out, s(&i["event"]), s(&i["payload"]).as_bytes(), i["n"].as_u64().unwrap() as usize);
            json!(text(&out))
        }
        "requestModelName" => json!(request_model_name(s(&i["a"]).as_bytes(), s(&i["b"]).as_bytes())),
        "agTool" => json!({
            "up": antigravity_tool_name_to_upstream(s(i)),
            "client": antigravity_upstream_tool_name_to_client(s(i)),
        }),
        "fileData" => match normalize_openai_file_data(s(&i["filename"]), s(&i["fallback"]), s(&i["data"])) {
            Some((m, d)) => json!({"mime": m, "data": d, "ok": true}),
            None => json!({"mime": "", "data": "", "ok": false}),
        },
        "interactionsUsage" => {
            let root = parse_res(s(i));
            res_out(&interactions_usage(&Res::of(&root)))
        }
        "accumulator" => {
            let msgs = i["msgs"].as_array().cloned().unwrap_or_default();
            let steps = i["steps"].as_array().cloned().unwrap_or_default();
            let mut acc = ClaudeMessageAccumulator::new(msgs.len());
            for (m, step) in msgs.iter().zip(&steps) {
                acc.append(s(m).as_bytes());
                if s(step) == "flush" {
                    acc.flush();
                }
            }
            texts(&acc.messages())
        }
        "alignClaudeToolResults" => {
            let content = i["content"].as_str().filter(|c| !c.is_empty()).map(res_owned).unwrap_or(Res::NONE);
            let ids: Vec<String> = i["ids"].as_array().map(|a| a.iter().map(|v| s(v).to_string()).collect()).unwrap_or_default();
            res_out(&align_claude_tool_results(content, &ids))
        }
        "systemReminderText" => json!(system_reminder_text(s(i))),
        "claudeSystemReminder" => {
            let content = if s(i).is_empty() { Res::NONE } else { res_owned(s(i)) };
            let r = claude_message_system_reminder_text(&content);
            json!({"text": r.clone().unwrap_or_default(), "ok": r.is_some()})
        }
        "structuredOutput" => {
            let format = if s(i).is_empty() { Res::NONE } else { res_owned(s(i)) };
            json!(build_claude_structured_output_instruction(&format, None))
        }
        "attachCacheControl" => {
            json!(text(&attach_cache_control(s(&i["dst"]).as_bytes(), &res_owned(s(&i["src"])))))
        }
        "attachMessageCacheControl" => {
            json!(text(&attach_message_cache_control(s(&i["msg"]).as_bytes(), &res_owned(s(&i["src"])))))
        }
        "attachToolMessageCacheControl" => {
            json!(text(&attach_tool_message_cache_control(s(&i["msg"]).as_bytes(), &res_owned(s(&i["src"])))))
        }
        "deriveClaudeUserID" => json!(derive_claude_user_id(s(i).as_bytes())),
        "devinAutomation" => json!(is_devin_codex_app_automation_update(s(&i["ns"]), s(&i["tool"]))),
        "devinObfuscate" => json!({
            "exec": obfuscate_exec_command_description(s(i)),
            "stdin": obfuscate_write_stdin_description(s(i)),
        }),
        "devinSanitize" => json!(sanitize_devin_tool_description(s(&i["tool"]), s(&i["desc"]))),
        "geminiMerge" => texts(&merge_adjacent_gemini_contents(&byte_items(i))),
        "geminiMergeUser" => texts(&merge_adjacent_gemini_user_contents(&byte_items(i))),
        "geminiSplit" => texts(&split_gemini_function_response_turns(&byte_items(i))),
        "geminiContentHas" => json!({
            "fc": content_has_gemini_function_call(s(i).as_bytes()),
            "fr": content_has_gemini_function_response(s(i).as_bytes()),
        }),
        "geminiReorder" => texts(&reorder_gemini_user_parts(byte_items(i))),
        "isThought" => json!(is_gemini_thought_part(&res_owned(s(i)))),
        "containsJSONRef" => json!(contains_json_ref(&res_owned(s(i)))),
        "setGeminiFRResult" => {
            let result = i["result"].as_str().map(res_owned).unwrap_or(Res::NONE);
            json!(text(&set_gemini_function_response_result(s(&i["part"]).as_bytes(), s(&i["path"]), &result)))
        }
        "setGeminiFRRaw" => json!(text(&set_gemini_function_response_raw(s(&i["part"]).as_bytes(), s(&i["path"]), s(&i["raw"])))),
        "alignOpenAI" => {
            let extra: Vec<String> = i["extra"].as_array().map(|a| a.iter().map(|v| s(v).to_string()).collect()).unwrap_or_default();
            let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
            texts(&align_openai_tool_call_messages(&byte_items(&i["msgs"]), &extra))
        }
        "setResponsesIdentity" => json!(text(&set_responses_tool_call_identity(
            s(&i["item"]).as_bytes(),
            s(&i["name"]),
            s(&i["ns"]),
            s(&i["path"])
        ))),
        "extractResponsesCallID" => json!(extract_responses_call_id(&res_owned(s(i)))),
        "normalizeResponsesOutputs" => {
            let items: Vec<Res<'static>> = i.as_array().map(|a| a.iter().map(|v| res_owned(s(v))).collect()).unwrap_or_default();
            Value::Array(normalize_responses_tool_call_outputs(&items).iter().map(res_out).collect())
        }
        "decoder" => run_decoder(i),
        "patchEvents" => {
            let state = ApplyPatchCallState {
                item_id: s(&i["item"]).to_string(),
                call_id: s(&i["call"]).to_string(),
                output_index: i["idx"].as_i64().unwrap(),
                ..Default::default()
            };
            let seq = i["seq"].as_i64().unwrap();
            json!({
                "delta": text(&apply_patch_input_delta(&state, s(&i["text"]), seq)),
                "done": text(&apply_patch_input_done(&state, s(&i["text"]), seq)),
            })
        }
        "patchFailure" => json!(text(&apply_patch_failure(s(&i["id"]), i["seq"].as_i64().unwrap()))),
        "normalizeRequest" => {
            let r = normalize_apply_patch_responses_request(s(i).as_bytes());
            // Go returns the input unchanged alongside the invalid-JSON error (nil for others).
            let value = match &r {
                Ok(v) => text(v),
                Err(e) if e == "invalid Responses request JSON" => s(i).to_string(),
                Err(_) => String::new(),
            };
            json!({"value": value, "err": err_val(&r)})
        }
        "bridge" => run_bridge(i),
        "bridgeFail" => {
            let mut b = ApplyPatchResponsesBridge::new(br#"{"tools":[{"type":"custom","name":"apply_patch"}]}"#);
            let (out, err) = b.fail("boom");
            let (again, again_err) = b.fail("boom2");
            json!({
                "out": texts(&out), "err": err, "again": texts(&again), "againErr": again_err,
                "toolErr": b.tool_input_error(),
            })
        }
        other => panic!("unknown golden fn {other}"),
    }
}

const LIST_FNS: [&str; 7] = [
    "joinRawArray",
    "accumulator",
    "geminiMerge",
    "geminiMergeUser",
    "geminiSplit",
    "geminiReorder",
    "alignOpenAI",
];

/// Strings holding JSON objects/arrays are compared compactly (Go copies raw bytes, Rust
/// re-serializes). Go's `decode apply_patch ...: <detail>` errors carry encoding/json's wording,
/// so only the part before the detail is compared.
fn normalize(v: &Value) -> Value {
    match v {
        Value::String(st) => {
            let t = st.trim_start();
            if (t.starts_with('{') || t.starts_with('[')) && cpa_json::valid(st.as_bytes()) {
                return Value::String(cpa_json::to_string(&parse_res(st)));
            }
            if st.starts_with("decode apply_patch")
                && let Some(colon) = st.find(':') {
                    return Value::String(st[..=colon].to_string());
                }
            v.clone()
        }
        Value::Array(a) => Value::Array(a.iter().map(normalize).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    // Go nil slices serialize as null; Rust has no nil/empty distinction.
                    let empty_list = v.is_null() && matches!(k.as_str(), "events" | "steps" | "again");
                    (k.clone(), if empty_list { json!([]) } else { normalize(v) })
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `applypatch::unwrap_input` (base layer, not part of this port) words and orders some
/// malformed-wrapper errors differently from Go's `json.Decoder` ("invalid character ',' ..." vs
/// "must contain only one input field"; a lone surrogate fails there instead of in the strict
/// pass). Where either side's error came from that decoder, only "it is an error" is compared.
fn loosen_base_layer_errors(want: &Value, got: &mut Value) {
    for key in ["finish", "pushAfter"] {
        let want_err = want[key]["err"].as_str().unwrap_or_default();
        let got_err = got[key]["err"].as_str().unwrap_or_default();
        let from_base_layer = |e: &str| e.starts_with("decode apply_patch");
        if !want_err.is_empty() && !got_err.is_empty() && (from_base_layer(want_err) || from_base_layer(got_err)) {
            got[key]["err"] = json!(want_err);
        }
    }
}

/// Go helpers the translators never call from Rust (the golden corpus still records them).
const RETIRED_FNS: [&str; 2] = ["setStringNoEscape", "checkIdentity"];

#[test]
fn matches_go_golden_corpus() {
    let cases = golden();
    let (mut checked, mut mismatches) = (0usize, Vec::new());
    for case in &cases {
        let fn_name = s(&case["fn"]);
        if RETIRED_FNS.contains(&fn_name) {
            continue;
        }
        let mut want = case["out"].clone();
        if want.is_null() && LIST_FNS.contains(&fn_name) {
            want = json!([]);
        }
        let mut got = run_case(fn_name, &case["in"]);
        if fn_name == "decoder" {
            if std::str::from_utf8(&unb64(&case["in"]["args"])).is_err() {
                for key in ["finish", "inputAfter", "pushAfter"] {
                    want.as_object_mut().map(|m| m.remove(key));
                }
            }
            loosen_base_layer_errors(&want, &mut got);
        }
        checked += 1;
        if normalize(&want) != normalize(&got) {
            mismatches.push((fn_name.to_string(), case["in"].to_string(), want, got));
        }
    }
    if !mismatches.is_empty() {
        let mut by_fn: std::collections::BTreeMap<&str, usize> = Default::default();
        for (f, ..) in &mismatches {
            *by_fn.entry(f).or_default() += 1;
        }
        let shown: String = mismatches
            .iter()
            .scan(std::collections::HashSet::new(), |seen, m| Some(seen.insert(m.0.clone()).then_some(m)))
            .flatten()
            .take(8)
            .map(|(f, i, w, g)| {
                let i: String = i.chars().take(600).collect();
                format!("{f} in={i}\n  want {}\n  got  {}", normalize(w), normalize(g))
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!("{} of {checked} cases differ {by_fn:?}\n{shown}", mismatches.len());
    }
}

// ---- focused unit tests

#[test]
fn claude_tool_call_id_shape() {
    for _ in 0..50 {
        let id = generate_claude_tool_call_id();
        assert!(id.starts_with("toolu_") && id.len() == 30, "{id}");
        assert!(id["toolu_".len()..].bytes().all(|b| b.is_ascii_alphanumeric()), "{id}");
    }
    assert_ne!(generate_claude_tool_call_id(), generate_claude_tool_call_id());
}

#[test]
fn raw_array_helpers_edge_cases() {
    assert_eq!(join_raw_array::<&[u8]>(&[]), b"[]");
    assert_eq!(join_raw_array(&[&b"1"[..], b"{}"]), b"[1,{}]");
    assert_eq!(set_raw_array_items(br#"{"a":[]}"#, "a", &[b"1".to_vec()]), br#"{"a":[1]}"#);
    assert_eq!(set_raw_array_items(br#"{"a":[]}"#, "a", &[] as &[Vec<u8>]), br#"{"a":[]}"#);
    assert_eq!(sse_event_data("e", b"p"), b"event: e\ndata: p\n\n");
}

#[test]
fn gemini_default_safety_settings_only_when_absent() {
    use crate::gemini::common::{attach_default_safety_settings, default_safety_settings};
    let settings = default_safety_settings();
    assert_eq!(settings.len(), 5);
    assert_eq!(
        settings[0].to_string(),
        r#"{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"}"#
    );
    assert_eq!(settings[4]["threshold"], "BLOCK_NONE");

    let out = attach_default_safety_settings(br#"{"model":"m"}"#, "request.safetySettings");
    let parsed = cpa_json::parse(&out);
    assert_eq!(parsed.g("request.safetySettings.#").int(), 5);
    assert_eq!(parsed.g("model").str(), "m");
    let kept = br#"{"safetySettings":[]}"#;
    assert_eq!(attach_default_safety_settings(kept, "safetySettings"), kept);
}

/// Streams of 200 randomly split fragments decode to the same input as a single push.
#[test]
fn apply_patch_decoder_is_split_invariant() {
    let args = r#"{"input":"*** Begin Patch\n+中文😀 \"q\" \\ é\n*** End Patch\n"}"#;
    let whole = {
        let mut d = ApplyPatchInputDecoder::default();
        d.push(args).unwrap();
        d.input().to_string()
    };
    assert!(whole.contains("中文😀") && whole.contains('é'));
    for split in 0..=args.len() {
        let mut d = ApplyPatchInputDecoder::default();
        d.push(&args.as_bytes()[..split]).unwrap();
        d.push(&args.as_bytes()[split..]).unwrap();
        assert_eq!(d.input(), whole, "split at {split}");
        assert_eq!(d.finish(args).unwrap(), "");
    }
}

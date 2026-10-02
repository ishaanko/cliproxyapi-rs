//! Differential tests of the smaller Go functions: outputs recorded by running the Go executor
//! package (accumulator, stream aggregation, 429 decisions, replay scopes, schema sanitizing,
//! envelope, sensitive words, turn boundaries, token estimate, compaction).

use std::collections::HashMap;

use cpa_core::cache::{
    clear_antigravity_reasoning_replay_cache, get_antigravity_reasoning_replay_items,
    get_antigravity_reasoning_replay_items_with_snapshot_required,
};
use cpa_json::J;
use cpa_runtime::executor::{Options, Request};
use cpa_translator::Format;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use crate::helps::claude_input_tokens::count_claude_input_tokens;
use super::compaction as compaction_mod;
use super::credits::{decide_429, has_explicit_credits_balance_exhausted_reason, inject_enabled_credit_types};
use super::execute::convert_stream_to_non_stream;
use crate::helps::cloak_obfuscate::{SensitiveWordMatcher, obfuscate_sensitive_words_in_system_instruction};
use crate::helps::gemini_content_turns::{ensure_gemini_boundary_user_content, ensure_gemini_leading_user_content};
use super::replay::{ReplayScope, scope_from_request};
use super::replay_capture::ReplayAccumulator;
use super::request::{gemini_to_antigravity, request_needs_schema_sanitization, sanitize_request_schemas};
use super::tests::SERIAL;

fn fixture() -> Value {
    serde_json::from_slice(&super::tests::gunzip(include_bytes!("testdata/misc.json.gz"))).expect("misc fixture")
}

fn list(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn bytes(v: &Value) -> Vec<u8> {
    cpa_json::to_vec(v)
}

fn report(what: &str, diffs: Vec<String>) {
    assert!(diffs.is_empty(), "{} {what} mismatches:\n{}", diffs.len(), diffs.iter().take(5).cloned().collect::<Vec<_>>().join("\n"));
}

#[test]
fn accumulator_ledger_matches_go() {
    let _guard = SERIAL.blocking_lock();
    clear_antigravity_reasoning_replay_cache();
    let doc = fixture();
    let mut diffs = Vec::new();
    for (i, case) in list(&doc["acc"]).iter().enumerate() {
        let (_, snapshot) = get_antigravity_reasoning_replay_items_with_snapshot_required("m", &format!("session:acc-{i}"));
        let scope = ReplayScope { model_name: "m".into(), session_key: format!("session:acc-{i}"), snapshot };
        let request = bytes(&case["request"]);
        let Some(mut acc) = ReplayAccumulator::new(&scope, &request) else {
            diffs.push(format!("case {i}: no accumulator"));
            continue;
        };
        for line in list(&case["lines"]) {
            acc.observe_sse_line(line.as_str().unwrap_or("").as_bytes());
        }
        acc.commit();
        let got: Vec<Value> = get_antigravity_reasoning_replay_items("m", &format!("session:acc-{i}"))
            .unwrap_or_default()
            .iter()
            .map(|b| cpa_json::parse(b))
            .collect();
        if got != list(&case["items"]) {
            diffs.push(format!("case {i}: ledger differs\n  request: {}\n  lines: {}\n  go:   {}\n  rust: {}", case["request"], case["lines"], case["items"], Value::Array(got)));
        }
    }
    report("accumulator", diffs);
}

#[test]
fn stream_aggregation_matches_go() {
    let doc = fixture();
    let mut diffs = Vec::new();
    for (i, case) in list(&doc["streams"]).iter().enumerate() {
        let got = cpa_json::parse(&convert_stream_to_non_stream(case["stream"].as_str().unwrap_or("").as_bytes()));
        if got != case["out"] {
            diffs.push(format!("case {i}: stream {:?}\n  go:   {}\n  rust: {got}", case["stream"], case["out"]));
        }
    }
    report("stream aggregation", diffs);
}

#[test]
fn quota_decisions_match_go() {
    let doc = fixture();
    let mut diffs = Vec::new();
    for case in list(&doc["decisions"]) {
        let body = case["body"].as_str().unwrap_or("").as_bytes().to_vec();
        let d = decide_429(&body);
        let kind = match d.kind {
            super::credits::Decision429Kind::SoftRetry => "soft_retry",
            super::credits::Decision429Kind::InstantRetrySameAuth => "instant_retry_same_auth",
            super::credits::Decision429Kind::ShortCooldownSwitchAuth => "short_cooldown_switch_auth",
            super::credits::Decision429Kind::FullQuotaExhausted => "full_quota_exhausted",
        };
        let retry_ms = d.retry_after.map(|r| r.as_millis() as i64);
        let want_retry = case["has_retry"].as_bool().unwrap_or(false).then(|| case["retry_ms"].as_i64().unwrap_or(0));
        let injected = inject_enabled_credit_types(&body).map(|b| cpa_json::parse(&b));
        let want_injected = case.get("injected").and_then(Value::as_str).map(|s| cpa_json::parse(s.as_bytes()));
        if kind != case["kind"].as_str().unwrap_or("")
            || retry_ms != want_retry
            || d.reason != case["reason"].as_str().unwrap_or("")
            || has_explicit_credits_balance_exhausted_reason(&body) != case["explicit_credits"].as_bool().unwrap_or(false)
            || injected != want_injected
        {
            diffs.push(format!("body {}: rust kind={kind} retry={retry_ms:?} reason={:?}; go {case}", case["body"], d.reason));
        }
    }
    report("429 decision", diffs);
}

fn headers_of(v: &Value) -> HeaderMap {
    let mut map = HeaderMap::new();
    if let Value::Object(h) = v {
        for (k, val) in h {
            if let (Ok(n), Ok(x)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(val.as_str().unwrap_or(""))) {
                map.insert(n, x);
            }
        }
    }
    map
}

fn metadata_of(v: &Value) -> HashMap<String, Value> {
    v.as_object().map(|m| m.iter().map(|(k, x)| (k.clone(), x.clone())).collect()).unwrap_or_default()
}

#[test]
fn replay_scope_keys_match_go() {
    let doc = fixture();
    let mut diffs = Vec::new();
    for (i, case) in list(&doc["scopes"]).iter().enumerate() {
        let payload = if case["payload"].is_string() { Vec::new() } else { bytes(&case["payload"]) };
        let mut opts = Options::new(Format::OpenAI);
        opts.headers = headers_of(&case["headers"]);
        opts.metadata = metadata_of(&case["metadata"]);
        if !case["orig"].is_null() {
            opts.original_request = bytes(&case["orig"]).into();
        }
        let req = Request {
            model: "gemini-3.7-flash".into(),
            payload: payload.clone().into(),
            format: Format::OpenAI,
            metadata: metadata_of(&case["req_metadata"]),
        };
        let scope = scope_from_request("gemini-3.7-flash", &req, &opts, &payload);
        let (key, valid) = (scope.session_key.clone(), scope.valid());
        let want_key = case["key"].as_str().unwrap_or("");
        // Without an explicit id or user text Go falls back to a random session id: shape only.
        let parsed = cpa_json::parse(&payload);
        let explicit = ["sessionId", "session_id", "request.sessionId", "request.session_id"].iter().any(|p| !parsed.g(p).str().trim().is_empty());
        let has_text = parsed.g("request.contents").array().iter().any(|c| c.g("role").str() == "user" && !c.g("parts.0.text").str().is_empty());
        let random_expected = want_key.starts_with("session:") && !explicit && !has_text;
        let key_ok = key == want_key || (random_expected && key.starts_with("session:"));
        if !key_ok || valid != case["valid"].as_bool().unwrap_or(false) {
            diffs.push(format!("case {i}: rust key={key:?} valid={valid}; go {}", case["key"]));
        }
    }
    report("replay scope", diffs);
}

#[test]
fn schema_sanitization_matches_go() {
    let doc = fixture();
    let mut diffs = Vec::new();
    for (i, case) in list(&doc["schemas"]).iter().enumerate() {
        let mut payload = case["payload"].clone();
        if request_needs_schema_sanitization(&payload) != case["needs_sanitization"].as_bool().unwrap_or(false) {
            diffs.push(format!("case {i}: needs_sanitization differs"));
        }
        sanitize_request_schemas(&mut payload, case["antigravity"].as_bool().unwrap_or(false));
        if payload != case["out"] {
            diffs.push(format!("case {i}: antigravity={}\n  in:   {}\n  go:   {}\n  rust: {payload}", case["antigravity"], case["payload"], case["out"]));
        }
    }
    report("schema sanitization", diffs);
}

#[test]
fn envelope_matches_go() {
    let doc = fixture();
    let mut diffs = Vec::new();
    for (i, case) in list(&doc["envelopes"]).iter().enumerate() {
        let derived: Vec<String> = vec![case["derived"].as_str().unwrap_or("").to_string()];
        let out = cpa_json::parse(&gemini_to_antigravity(
            case["model"].as_str().unwrap_or(""),
            &bytes(&case["payload"]),
            case["project"].as_str().unwrap_or(""),
            &derived,
        ));
        let mut want = case["out"].clone();
        let mut got = out;
        // requestId is random; the session id is random unless text, payload or derived id fix it.
        let deterministic_session = case["payload"].g("request.sessionId").exists()
            || !case["derived"].as_str().unwrap_or("").is_empty()
            || case["payload"].g("request.contents").array().iter().any(|c| c.g("role").str() == "user" && !c.g("parts.0.text").str().is_empty());
        for v in [&mut want, &mut got] {
            if v.g("requestId").exists() {
                cpa_json::set(v, "requestId", "<id>");
            }
            if !deterministic_session && v.g("request.sessionId").exists() {
                cpa_json::set(v, "request.sessionId", "<sid>");
            }
        }
        if got != want {
            diffs.push(format!("case {i}: model {}\n  go:   {want}\n  rust: {got}", case["model"]));
        }
    }
    report("envelope", diffs);
}

#[test]
fn small_helpers_match_go() {
    let doc = fixture();
    let m = &doc["misc"];
    let mut diffs = Vec::new();

    let words: Vec<String> = list(&m["words"]).iter().filter_map(|w| w.as_str().map(String::from)).collect();
    let matcher = SensitiveWordMatcher::new(&words).expect("matcher");
    for (i, (input, want)) in list(&m["instructions"]).iter().zip(list(&m["obfuscated"])).enumerate() {
        let got = cpa_json::parse(&obfuscate_sensitive_words_in_system_instruction(&bytes(input), Some(&matcher)));
        if got != want {
            diffs.push(format!("obfuscate {i}:\n  go:   {want}\n  rust: {got}"));
        }
    }
    for (i, input) in list(&m["boundary_in"]).iter().enumerate() {
        let got = cpa_json::parse(&ensure_gemini_boundary_user_content(&bytes(input), "request.contents"));
        if got != m["boundary_out"][i] {
            diffs.push(format!("boundary {i}:\n  go:   {}\n  rust: {got}", m["boundary_out"][i]));
        }
        let got = cpa_json::parse(&ensure_gemini_leading_user_content(&bytes(input), "request.contents"));
        if got != m["leading_out"][i] {
            diffs.push(format!("leading {i}:\n  go:   {}\n  rust: {got}", m["leading_out"][i]));
        }
    }
    for (i, req) in list(&m["claude_reqs"]).iter().enumerate() {
        let want = m["claude_tokens"][i].as_i64().unwrap_or(-2);
        let got = count_claude_input_tokens(req.as_str().unwrap_or("").as_bytes()).unwrap_or(-1);
        if got != want {
            diffs.push(format!("claude tokens {i}: rust={got} go={want}"));
        }
    }

    let c = &m["compaction"];
    // A capsule sealed by Go opens here and expands to the same developer message.
    let capsule = c["capsule"].as_str().unwrap_or("");
    if compaction_mod::unseal_compaction(capsule).as_deref() != Ok("summary text <b>&") {
        diffs.push("unseal of Go capsule failed".into());
    }
    let input = json!({"input": [{"type": "compaction", "encrypted_content": capsule}, {"type": "message", "role": "user", "content": "hi"}]});
    match compaction_mod::expand_compaction_capsules(&bytes(&input)) {
        Ok(out) if cpa_json::parse(&out) == c["expanded"] => {}
        other => diffs.push(format!("expand differs: {other:?}")),
    }
    let sealed = compaction_mod::seal_compaction("round trip", "m").expect("seal");
    if compaction_mod::unseal_compaction(&sealed).as_deref() != Ok("round trip") || !sealed.starts_with("cpa-ag-compact-v1:") {
        diffs.push("seal/unseal round trip failed".into());
    }
    let s1 = br#"{"model":"m","stream":true,"tools":[1],"input":[{"type":"message","role":"user","content":"do"},{"type":"compaction_trigger"}],"metadata":{"a":1},"instructions":"i"}"#;
    if cpa_json::parse(&compaction_mod::prepare_summary_payload(s1, "m")) != c["summary1"] {
        diffs.push("summary payload (array input) differs".into());
    }
    if cpa_json::parse(&compaction_mod::prepare_summary_payload(br#"{"model":"m","input":"just text"}"#, "m")) != c["summary2"] {
        diffs.push("summary payload (string input) differs".into());
    }
    let extract = |s: &str| compaction_mod::extract_summary_text(s.as_bytes()).unwrap_or_else(|e| format!("ERR:{e}"));
    for (key, input) in [
        ("extract_responses", r#"{"output":[{"type":"reasoning"},{"type":"message","content":[{"type":"output_text","text":"A"},{"type":"output_text","text":"B"}]},{"type":"message","content":"C"}]}"#),
        ("extract_gemini", r#"{"response":{"candidates":[{"content":{"parts":[{"text":"x","thought":true},{"text":"G1"},{"text":"G2"}]}}]}}"#),
        ("extract_claude", r#"{"content":[{"type":"text","text":"c1"},{"type":"tool_use"},{"type":"text","text":"c2"}]}"#),
        ("extract_chat", r#"{"choices":[{"message":{"content":"chat"}}]}"#),
        ("extract_none", r#"{"a":1}"#),
    ] {
        let got = extract(input);
        if got != c[key].as_str().unwrap_or("") {
            diffs.push(format!("{key}: rust={got:?} go={}", c[key]));
        }
    }
    if compaction_mod::has_responses_compaction_trigger(br#"{"input":[{"type":"compaction_trigger"}]}"#) != c["has_trigger"].as_bool().unwrap_or(false)
        || compaction_mod::has_responses_compaction_item(br#"{"input":[{"type":"compaction"}]}"#) != c["has_item"].as_bool().unwrap_or(false)
        || compaction_mod::has_responses_compaction_item(br#"{"input":"x"}"#) != c["has_item_neg"].as_bool().unwrap_or(true)
    {
        diffs.push("compaction item detection differs".into());
    }
    let bad = compaction_mod::expand_compaction_capsules(br#"{"input":[{"type":"compaction","encrypted_content":"nope"}]}"#);
    if bad.err().as_deref() != c["bad_unseal"].as_str() {
        diffs.push("invalid capsule error text differs".into());
    }
    report("helper", diffs);
}

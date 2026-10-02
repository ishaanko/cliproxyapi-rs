//! Antigravity executor tests.
//!
//! `oracle_cases_match_go` replays `testdata/oracle.json`: executor-level cases whose
//! expectations were recorded by running the Go `AntigravityExecutor` against an httptest server
//! (upstream request line, selected headers and body, and what the client receives). Here the
//! same cases run against a local axum mock. Random ids (`requestId`, translator ids and
//! timestamps) are masked before comparing; everything else must match.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request as HttpRequest, Response as HttpResponse, StatusCode};
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_json::J;
use cpa_runtime::conductor::ANTIGRAVITY_CREDITS_METADATA_KEY;
use cpa_runtime::executor::{ExecError, Executor, Metadata, Options, Request};
use cpa_translator::Format;
use http::{HeaderMap, HeaderName, HeaderValue};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::AntigravityExecutor;

const ORACLE: &str = include_str!("testdata/oracle.json");

/// Tests that touch the process-wide replay ledger or cooldown state run one at a time.
pub(crate) static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default, Clone)]
struct Captured {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// Local stand-in for cloudcode-pa: records the generate/stream/count call, answers canned data.
struct MockUpstream {
    base_url: String,
    captured: Arc<Mutex<Option<Captured>>>,
}

async fn start_mock(status: u16, body: String) -> MockUpstream {
    let captured: Arc<Mutex<Option<Captured>>> = Arc::new(Mutex::new(None));
    let seen = captured.clone();
    let app = Router::new().fallback(move |req: HttpRequest<Body>| {
        let seen = seen.clone();
        let body = body.clone();
        async move {
            let (parts, b) = req.into_parts();
            let bytes = to_bytes(b, usize::MAX).await.unwrap_or_default();
            if parts.uri.path().contains("loadCodeAssist") {
                return HttpResponse::new(Body::from("{}"));
            }
            let mut headers = HashMap::new();
            for k in ["content-type", "authorization", "user-agent", "accept-encoding", "connection", "x-custom"] {
                if let Some(v) = parts.headers.get(k).and_then(|v| v.to_str().ok()) {
                    headers.insert(k.to_string(), v.to_string());
                }
            }
            *seen.lock() = Some(Captured {
                method: parts.method.to_string(),
                path: parts.uri.path().to_string(),
                query: parts.uri.query().unwrap_or("").to_string(),
                headers,
                body: bytes.to_vec(),
            });
            let mut resp = HttpResponse::new(Body::from(body));
            *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            resp
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    MockUpstream { base_url: format!("http://{addr}"), captured }
}

fn executor_with(cfg: Config) -> AntigravityExecutor {
    let (tx, rx) = watch::channel(Arc::new(cfg));
    // Keep the sender alive for the executor's lifetime.
    std::mem::forget(tx);
    AntigravityExecutor::new(rx)
}

fn test_auth(id: &str, base_url: &str) -> Auth {
    let mut auth = Auth::new(id, "antigravity");
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.metadata.insert("access_token".into(), json!("token-123"));
    auth.metadata
        .insert("expired".into(), json!((chrono::Utc::now() + chrono::Duration::hours(24)).to_rfc3339()));
    auth.metadata.insert("project_id".into(), json!("project-1"));
    auth
}

fn map_of(v: Option<&Value>) -> Metadata {
    match v {
        Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => Metadata::new(),
    }
}

// ---------------------------------------------------------------- normalization

fn is_volatile_id(s: &str) -> bool {
    let timestamped = regex::Regex::new(r"^.+-\d{10,}-\d+$").expect("static regex");
    (["chatcmpl-", "resp_", "msg_", "agent-", "image_gen/", "toolu_", "call_", "cmp_", "fc_call_", "interaction_"].iter().any(|p| s.starts_with(p))
        && s.len() > 12)
        || timestamped.is_match(s)
}

/// Masks values that legitimately differ between runs (ids, timestamps, durations).
fn mask(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, x) in map.iter_mut() {
                match (k.as_str(), &*x) {
                    ("created" | "created_at" | "completed_at" | "timestamp", Value::Number(_)) => *x = json!(0),
                    ("created" | "updated", Value::String(_)) => *x = json!("<ts>"),
                    ("requestId", _) => *x = json!("<id>"),
                    ("encrypted_content", Value::String(_)) => *x = json!("<capsule>"),
                    (_, Value::String(s)) if is_volatile_id(s) && matches!(k.as_str(), "id" | "item_id" | "call_id" | "tool_use_id" | "response_id" | "requestId" | "tool_call_id" | "interaction_id") => {
                        *x = json!("<id>")
                    }
                    _ => mask(x),
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(mask),
        _ => {}
    }
}

/// Parses text that is JSON, or SSE frames whose `data:` lines are JSON, into comparable values.
fn normalize_text(s: &str) -> Value {
    if let Ok(mut v) = serde_json::from_str::<Value>(s) {
        mask(&mut v);
        return v;
    }
    let lines: Vec<Value> = s
        .lines()
        .map(|line| match line.strip_prefix("data: ") {
            Some(rest) => match serde_json::from_str::<Value>(rest) {
                Ok(mut v) => {
                    mask(&mut v);
                    json!({"data": v})
                }
                Err(_) => json!(line),
            },
            None => json!(line),
        })
        .collect();
    Value::Array(lines)
}

fn has_user_text(body: &Value) -> bool {
    body.g("request.contents").array().iter().any(|c| c.g("role").str() == "user" && !c.g("parts.0.text").str().is_empty())
}

fn mask_duration(s: &str) -> String {
    let re = regex::Regex::new(r"[0-9.]+(ms|s|m)? remaining").expect("static regex");
    re.replace_all(s, "<d> remaining").into_owned()
}

fn json_or_string(bytes: &[u8]) -> Value {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(mut v) => {
            mask(&mut v);
            v
        }
        Err(_) => json!(String::from_utf8_lossy(bytes)),
    }
}

// ---------------------------------------------------------------- oracle replay

struct Outcome {
    payload: Option<String>,
    chunks: Vec<String>,
    error: Option<ExecError>,
}

async fn run_case(case: &Value) -> (Option<Captured>, Outcome) {
    let up = &case["upstream"];
    let mock = start_mock(up["status"].as_u64().unwrap_or(200) as u16, up["body"].as_str().unwrap_or("").to_string()).await;

    let mut cfg = Config::default();
    if let Some(words) = case.get("sensitive_words").and_then(Value::as_array) {
        cfg.antigravity.sensitive_words = words.iter().filter_map(|w| w.as_str().map(String::from)).collect();
    }
    cfg.quota_exceeded.antigravity_credits = case.get("credits_enabled").and_then(Value::as_bool).unwrap_or(false);
    let ex = executor_with(cfg);

    let mut auth = test_auth(case.get("auth_id").and_then(Value::as_str).unwrap_or(""), &mock.base_url);
    if let Some(Value::Object(attrs)) = case.get("auth_attributes") {
        for (k, v) in attrs {
            auth.attributes.insert(k.clone(), v.as_str().unwrap_or("").to_string());
        }
    }
    if let Some(Value::Object(m)) = case.get("auth_metadata") {
        for (k, v) in m {
            auth.metadata.insert(k.clone(), v.clone());
        }
    }

    let kind = case["kind"].as_str().unwrap_or("");
    let source = Format::parse(case["source_format"].as_str().unwrap_or("openai")).expect("source format");
    let mut opts = Options::new(source);
    opts.stream = kind == "stream";
    opts.alt = case.get("alt").and_then(Value::as_str).unwrap_or("").to_string();
    opts.response_format = case.get("response_format").and_then(Value::as_str).and_then(Format::parse);
    if let Some(Value::Object(h)) = case.get("headers") {
        let mut map = HeaderMap::new();
        for (k, v) in h {
            if let (Ok(n), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v.as_str().unwrap_or(""))) {
                map.insert(n, val);
            }
        }
        opts.headers = map;
    }
    opts.metadata = map_of(case.get("metadata"));
    if case.get("credits_requested").and_then(Value::as_bool).unwrap_or(false) {
        opts.metadata.insert(ANTIGRAVITY_CREDITS_METADATA_KEY.into(), json!(true));
    }
    if let Some(orig) = case.get("original_payload") {
        opts.original_request = serde_json::to_vec(orig).unwrap_or_default().into();
    }
    let req = Request {
        model: case["model"].as_str().unwrap_or("").to_string(),
        payload: serde_json::to_vec(&case["payload"]).unwrap_or_default().into(),
        format: source,
        metadata: map_of(case.get("req_metadata")),
    };

    let mut out = Outcome { payload: None, chunks: Vec::new(), error: None };
    match kind {
        "execute" => match ex.execute(&auth, req, opts).await {
            Ok(r) => out.payload = Some(String::from_utf8_lossy(&r.payload).into_owned()),
            Err(e) => out.error = Some(e),
        },
        "count" => match ex.count_tokens(&auth, req, opts).await {
            Ok(r) => out.payload = Some(String::from_utf8_lossy(&r.payload).into_owned()),
            Err(e) => out.error = Some(e),
        },
        _ => match ex.execute_stream(&auth, req, opts).await {
            Ok(mut s) => {
                while let Some(chunk) = s.chunks.recv().await {
                    match chunk {
                        Ok(b) => out.chunks.push(String::from_utf8_lossy(&b).into_owned()),
                        Err(e) => {
                            out.error = Some(e);
                            break;
                        }
                    }
                }
            }
            Err(e) => out.error = Some(e),
        },
    }
    // Let background probes (loadCodeAssist) settle before the mock goes away.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let captured = mock.captured.lock().clone();
    (captured, out)
}

fn compare_case(case: &Value, captured: Option<Captured>, out: Outcome) -> Vec<String> {
    let name = case["name"].as_str().unwrap_or("?");
    let expect = &case["expect"];
    let mut diffs: Vec<String> = Vec::new();
    macro_rules! check {
        ($what:expr, $got:expr, $want:expr) => {{
            let (got, want): (Value, Value) = ($got, $want);
            if got != want {
                diffs.push(format!("{name}: {}\n  go:   {want}\n  rust: {got}", $what));
            }
        }};
    }

    match (&expect["request"], &captured) {
        (Value::Null, None) => {}
        (Value::Null, Some(c)) => diffs.push(format!("{name}: rust sent an upstream request to {} but Go did not", c.path)),
        (want, None) => diffs.push(format!("{name}: Go sent {} but rust sent nothing", want["path"])),
        (want, Some(c)) => {
            check!("request method", json!(c.method), want["method"].clone());
            check!("request path", json!(c.path), want["path"].clone());
            check!("request query", json!(c.query), want["query"].clone());
            let mut got_headers = serde_json::to_value(&c.headers).unwrap_or(Value::Null);
            let mut want_headers = want["headers"].clone();
            // Neither client sends "connection" by default; ignore it if a hop adds one.
            for h in [&mut got_headers, &mut want_headers] {
                if let Value::Object(m) = h {
                    m.remove("connection");
                }
            }
            check!("request headers", got_headers, want_headers);
            let mut got_body = json_or_string(&c.body);
            let mut want_body = want["body"].clone();
            mask(&mut want_body);
            mask(&mut got_body);
            // Without user text Go falls back to a random session id.
            if !has_user_text(&want_body) {
                for b in [&mut want_body, &mut got_body] {
                    if b.g("request.sessionId").exists() {
                        cpa_json::set(b, "request.sessionId", "<sid>");
                    }
                }
            }
            check!("request body", got_body, want_body);
        }
    }

    let want_result = &expect["result"];
    match (&out.error, want_result.get("error")) {
        (None, None) => {}
        (Some(e), Some(w)) => {
            check!("error status", json!(e.status), w["status"].clone());
            check!(
                "error message",
                json!(mask_duration(&e.message)),
                json!(mask_duration(w["message"].as_str().unwrap_or("")))
            );
            let got_ms = e.retry_after.map(|d| d.as_millis() as i64).unwrap_or(0);
            let want_ms = w.get("retry_after_ms").and_then(Value::as_i64).unwrap_or(0);
            if w["message"].as_str().unwrap_or("").contains("remaining") {
                // Cooldown remainders shrink with wall time; both must be positive and close.
                if got_ms <= 0 || want_ms <= 0 || (got_ms - want_ms).abs() > 5000 {
                    diffs.push(format!("{name}: cooldown retry_after go={want_ms} rust={got_ms}"));
                }
            } else {
                check!("error retry_after_ms", json!(got_ms), json!(want_ms));
            }
        }
        (Some(e), None) => diffs.push(format!("{name}: rust errored ({} {}) but Go did not", e.status, e.message)),
        (None, Some(w)) => diffs.push(format!("{name}: Go errored ({w}) but rust did not")),
    }
    if let Some(payload) = &out.payload {
        check!(
            "payload",
            normalize_text(payload),
            normalize_text(want_result.get("payload").and_then(Value::as_str).unwrap_or(""))
        );
    }
    let want_chunks: Vec<Value> = want_result
        .get("chunks")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|c| normalize_text(c.as_str().unwrap_or(""))).collect())
        .unwrap_or_default();
    let got_chunks: Vec<Value> = out.chunks.iter().map(|c| normalize_text(c)).collect();
    check!("chunks", Value::Array(got_chunks), Value::Array(want_chunks));
    diffs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oracle_cases_match_go() {
    let _guard = SERIAL.lock().await;
    let cases: Vec<Value> = serde_json::from_str(ORACLE).expect("oracle fixture");
    cpa_core::cache::clear_antigravity_reasoning_replay_cache();
    let filter = std::env::var("ORACLE_FILTER").unwrap_or_default();
    let mut all_diffs = Vec::new();
    let mut ran = 0;
    for case in &cases {
        let name = case["name"].as_str().unwrap_or("");
        if !filter.is_empty() && !name.contains(&filter) {
            // Sequence cases share cache state, so a filter only makes sense for stateless ones.
            continue;
        }
        let (captured, out) = run_case(case).await;
        all_diffs.extend(compare_case(case, captured, out));
        ran += 1;
    }
    assert!(ran > 0, "no oracle cases ran");
    if !all_diffs.is_empty() {
        let shown: Vec<String> = all_diffs.iter().take(20).cloned().collect();
        panic!("{} oracle mismatches (showing {}):\n{}", all_diffs.len(), shown.len(), shown.join("\n"));
    }
}

//! Kimi tests. `testdata/golden.json` and `testdata/exec_golden.json` were recorded by running the
//! Go implementation (normalizers, replay accumulator, and a full executor against an httptest
//! server) on the same inputs; the tests replay them against the Rust port.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes as AxBytes;
use axum::extract::State;
use axum::http::{HeaderMap as AxHeaders, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::any;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_runtime::executor::{DynExecutor, ExecError, Options, Request, Response, StreamResult};
use cpa_translator::Format;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::watch;

use super::normalize::*;
use super::{new, new_with_claude};

fn cfg_rx() -> crate::ConfigRx {
    watch::channel(Arc::new(Config::default())).1
}

/// Same JSON structure and key order, ignoring whitespace; non-JSON compares as text.
fn same_json(a: &str, b: &str) -> bool {
    if cpa_json::valid(a.as_bytes()) && cpa_json::valid(b.as_bytes()) {
        return cpa_json::to_string(&cpa_json::parse(a.as_bytes())) == cpa_json::to_string(&cpa_json::parse(b.as_bytes()));
    }
    a == b
}

#[derive(Deserialize)]
struct GoldenCase {
    #[serde(rename = "fn")]
    func: String,
    input: Value,
    output: Value,
}

fn golden() -> Vec<GoldenCase> {
    let raw = include_str!("testdata/golden.json");
    serde_json::from_str(raw).expect("golden.json parses")
}

fn text(v: &Value) -> &str {
    v.as_str().expect("string")
}

#[test]
fn normalizers_match_go() {
    let mut seen = 0;
    for case in golden() {
        seen += 1;
        let ctx = format!("{} {}", case.func, case.input);
        match case.func.as_str() {
            "upstream_model" => assert_eq!(normalize_kimi_upstream_model(text(&case.input)), text(&case.output), "{ctx}"),
            "replay_family" => assert_eq!(super::replay::model_family(text(&case.input)), text(&case.output), "{ctx}"),
            "tool_message_links" => {
                let got = String::from_utf8(normalize_kimi_tool_message_links(text(&case.input).as_bytes())).unwrap();
                assert!(same_json(&got, text(&case.output)), "{ctx}\n got {got}\nwant {}", text(&case.output));
            }
            "tools" => {
                let got = String::from_utf8(normalize_kimi_tools(text(&case.input).as_bytes())).unwrap();
                assert!(same_json(&got, text(&case.output)), "{ctx}\n got {got}\nwant {}", text(&case.output));
            }
            "temperature" => {
                let got = String::from_utf8(normalize_kimi_temperature(text(&case.input).as_bytes())).unwrap();
                assert!(same_json(&got, text(&case.output)), "{ctx}\n got {got}\nwant {}", text(&case.output));
            }
            "responses_input" => {
                let got = String::from_utf8(normalize_kimi_responses_input(text(&case.input).as_bytes())).unwrap();
                assert!(same_json(&got, text(&case.output)), "{ctx}\n got {got}\nwant {}", text(&case.output));
            }
            "urls" => {
                let mut auth = Auth::new(case.input["id"].as_str().unwrap_or("kimi-1"), case.input["provider"].as_str().unwrap_or("kimi"));
                if let Some(id) = case.input["id"].as_str() {
                    auth.file_name = id.to_string();
                }
                if let Some(attrs) = case.input["attributes"].as_object() {
                    for (k, v) in attrs {
                        auth.attributes.insert(k.clone(), v.as_str().unwrap().to_string());
                    }
                }
                if let Some(meta) = case.input["metadata"].as_object() {
                    for (k, v) in meta {
                        auth.metadata.insert(k.clone(), v.clone());
                    }
                }
                let a = Some(&auth);
                assert_eq!(resolve_kimi_base_url(a), text(&case.output["base"]), "{ctx}");
                assert_eq!(resolve_kimi_chat_url(a), text(&case.output["chat"]), "{ctx}");
                assert_eq!(resolve_kimi_responses_url(a), text(&case.output["responses"]), "{ctx}");
                assert_eq!(resolve_kimi_claude_base_url(a), text(&case.output["claude"]), "{ctx}");
            }
            "restore" => {
                let body = text(&case.input["body"]);
                let cached = text(&case.input["cached"]);
                let got = super::replay::restore_content(body.as_bytes(), cached.as_bytes());
                assert_eq!(got.is_some(), case.output["restored"].as_bool().unwrap(), "{ctx}");
                if let Some(got) = got {
                    assert!(same_json(&String::from_utf8(got).unwrap(), text(&case.output["body"])), "{ctx}");
                }
            }
            "replayable" => {
                assert_eq!(super::replay::content_is_replayable(text(&case.input).as_bytes()), case.output.as_bool().unwrap(), "{ctx}")
            }
            "stream_accumulator" => {
                let mut acc = super::replay::StreamAccumulator::default();
                for chunk in case.input.as_array().unwrap() {
                    acc.observe(text(chunk).as_bytes());
                }
                let content = acc.content();
                assert_eq!(content.is_some(), case.output["ok"].as_bool().unwrap(), "{ctx}");
                assert_eq!(acc.upstream_error, case.output["upstream_error"].as_bool().unwrap(), "{ctx}");
                if let Some(content) = content {
                    assert!(same_json(&String::from_utf8(content).unwrap(), text(&case.output["content"])), "{ctx}");
                }
            }
            other => panic!("unknown golden fn {other}"),
        }
    }
    assert!(seen > 100);
}

// ---------------------------------------------------------------- executor against a mock upstream

#[derive(Default)]
struct Captured {
    called: bool,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Clone)]
struct Mock {
    status: u16,
    content_type: String,
    body: String,
    captured: Arc<Mutex<Captured>>,
}

async fn mock_handler(State(mock): State<Mock>, uri: Uri, headers: AxHeaders, body: AxBytes) -> impl IntoResponse {
    {
        let mut c = mock.captured.lock();
        c.called = true;
        c.path = uri.path().to_string();
        c.headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
        c.body = String::from_utf8_lossy(&body).into_owned();
    }
    (
        StatusCode::from_u16(mock.status).unwrap(),
        [("content-type", mock.content_type.clone())],
        mock.body.clone(),
    )
}

/// Serves `mock` on a loopback port; returns its base URL.
async fn serve(mock: Mock) -> String {
    let app = Router::new().fallback(any(mock_handler)).with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn kimi_auth(base: &str) -> Auth {
    let mut auth = Auth::new("kimi-test", "kimi");
    auth.attributes.insert("base_url".into(), format!("{base}/coding"));
    auth.attributes.insert("header:X-Custom".into(), "yes".into());
    auth.metadata.insert("access_token".into(), Value::from("tok-123"));
    auth.metadata.insert("device_id".into(), Value::from("dev-1"));
    auth
}

#[derive(Deserialize)]
struct ExecScenario {
    name: String,
    source: String,
    #[serde(default)]
    response: String,
    stream: bool,
    model: String,
    #[serde(default)]
    alt: String,
    payload: String,
    upstream_status: u16,
    upstream_content_type: String,
    upstream_body: String,
    got_path: String,
    got_headers: Option<std::collections::HashMap<String, String>>,
    got_body: String,
    out_payload: String,
    out_chunks: Option<Vec<String>>,
    err_status: u16,
    err_msg: String,
    called: bool,
}

fn header_of(c: &Captured, name: &str) -> String {
    c.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap_or_default()
}

fn request_for(sc: &ExecScenario) -> (Request, Options) {
    let source = Format::parse(&sc.source).unwrap();
    let mut opts = Options::new(source);
    opts.stream = sc.stream;
    opts.alt = sc.alt.clone();
    opts.original_request = Bytes::from(sc.payload.clone());
    if !sc.response.is_empty() {
        opts.response_format = Format::parse(&sc.response);
    }
    let req = Request {
        model: sc.model.clone(),
        payload: Bytes::from(sc.payload.clone()),
        format: source,
        metadata: Default::default(),
    };
    (req, opts)
}

async fn drain(mut result: StreamResult) -> (Vec<String>, Option<ExecError>) {
    let mut chunks = Vec::new();
    let mut err = None;
    while let Some(item) = result.chunks.recv().await {
        match item {
            Ok(b) => chunks.push(String::from_utf8_lossy(&b).into_owned()),
            Err(e) => err = Some(e),
        }
    }
    (chunks, err)
}

#[tokio::test]
async fn executor_matches_go_recordings() {
    let scenarios: Vec<ExecScenario> = serde_json::from_str(include_str!("testdata/exec_golden.json")).unwrap();
    for sc in scenarios {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let base = serve(Mock {
            status: sc.upstream_status,
            content_type: sc.upstream_content_type.clone(),
            body: sc.upstream_body.clone(),
            captured: captured.clone(),
        })
        .await;
        let exec: DynExecutor = new(cfg_rx());
        let auth = kimi_auth(&base);
        let (req, opts) = request_for(&sc);

        let (payload, chunks, err): (Option<Response>, Option<Vec<String>>, Option<ExecError>) = if sc.stream {
            match exec.execute_stream(&auth, req, opts).await {
                Ok(result) => {
                    let (chunks, err) = drain(result).await;
                    (None, Some(chunks), err)
                }
                Err(e) => (None, None, Some(e)),
            }
        } else {
            match exec.execute(&auth, req, opts).await {
                Ok(resp) => (Some(resp), None, None),
                Err(e) => (None, None, Some(e)),
            }
        };

        let c = captured.lock();
        assert_eq!(c.called, sc.called, "{}: upstream called", sc.name);
        if sc.called {
            assert_eq!(c.path, sc.got_path, "{}: path", sc.name);
            for (name, want) in sc.got_headers.as_ref().unwrap() {
                assert_eq!(&header_of(&c, name), want, "{}: header {name}", sc.name);
            }
            assert!(same_json(&c.body, &sc.got_body), "{}: upstream body\n got {}\nwant {}", sc.name, c.body, sc.got_body);
        }
        assert_eq!(err.as_ref().map(|e| e.status).unwrap_or(0), sc.err_status, "{}: error status", sc.name);
        assert_eq!(err.as_ref().map(|e| e.message.as_str()).unwrap_or(""), sc.err_msg, "{}: error message", sc.name);
        if let Some(resp) = payload {
            let got = String::from_utf8(resp.payload.to_vec()).unwrap();
            assert!(same_json(&got, &sc.out_payload), "{}: payload\n got {got}\nwant {}", sc.name, sc.out_payload);
        }
        if let Some(chunks) = chunks {
            let want = sc.out_chunks.clone().unwrap_or_default();
            assert_eq!(chunks.len(), want.len(), "{}: chunk count {chunks:?} vs {want:?}", sc.name);
            for (i, (g, w)) in chunks.iter().zip(&want).enumerate() {
                assert!(same_json_sse(g, w), "{}: chunk {i}\n got {g:?}\nwant {w:?}", sc.name);
            }
        }
    }
}

/// SSE chunks compare exactly except that JSON `data:` payloads compare structurally.
fn same_json_sse(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let strip = |s: &str| {
        s.lines()
            .map(|l| match l.strip_prefix("data: ") {
                Some(rest) if cpa_json::valid(rest.as_bytes()) => {
                    format!("data: {}", cpa_json::to_string(&cpa_json::parse(rest.as_bytes())))
                }
                _ => l.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    strip(a) == strip(b)
}

// ---------------------------------------------------------------- usage

#[tokio::test]
async fn usage_is_reported_for_stream_and_non_stream() {
    let scenarios: Vec<ExecScenario> = serde_json::from_str(include_str!("testdata/exec_golden.json")).unwrap();
    for name in ["chat_tools", "chat_stream", "responses", "responses_stream"] {
        let sc = scenarios.iter().find(|s| s.name == name).unwrap();
        let base = serve(Mock {
            status: sc.upstream_status,
            content_type: sc.upstream_content_type.clone(),
            body: sc.upstream_body.clone(),
            captured: Default::default(),
        })
        .await;
        let exec = new(cfg_rx());
        let (req, opts) = request_for(sc);
        let usage = if sc.stream {
            let mut result = exec.execute_stream(&kimi_auth(&base), req, opts).await.unwrap();
            let rx = result.usage.take().expect("stream usage channel");
            while result.chunks.recv().await.is_some() {}
            rx.await.expect("usage sent")
        } else {
            exec.execute(&kimi_auth(&base), req, opts).await.unwrap().metadata["usage"].clone()
        };
        let want_total = if name.starts_with("responses") { 10 } else { 5 };
        assert_eq!(usage["total_tokens"], want_total, "{name}");
    }
}

// ---------------------------------------------------------------- claude delegation and replay

/// A stand-in Claude executor that records requests and answers with canned bodies.
struct FakeClaude {
    seen: Mutex<Vec<(Vec<String>, String)>>,
    non_stream: Mutex<Vec<Result<Response, ExecError>>>,
    stream_body: Mutex<Vec<String>>,
}

impl FakeClaude {
    fn new() -> Arc<Self> {
        Arc::new(FakeClaude { seen: Mutex::default(), non_stream: Mutex::default(), stream_body: Mutex::default() })
    }
}

#[async_trait::async_trait]
impl cpa_runtime::executor::Executor for FakeClaude {
    fn identifier(&self) -> &str {
        "claude"
    }

    async fn execute(&self, auth: &Auth, req: Request, _opts: Options) -> Result<Response, ExecError> {
        self.seen.lock().push((vec![auth.attr("base_url")], String::from_utf8_lossy(&req.payload).into_owned()));
        self.non_stream.lock().remove(0)
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, _opts: Options) -> Result<StreamResult, ExecError> {
        self.seen.lock().push((vec![auth.attr("base_url")], String::from_utf8_lossy(&req.payload).into_owned()));
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let chunks: Vec<String> = self.stream_body.lock().drain(..).collect();
        for chunk in chunks {
            tx.send(Ok(Bytes::from(chunk))).await.unwrap();
        }
        Ok(StreamResult::new(Default::default(), rx))
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        Ok(auth.clone())
    }

    async fn count_tokens(&self, auth: &Auth, req: Request, _opts: Options) -> Result<Response, ExecError> {
        self.seen.lock().push((vec![auth.attr("base_url")], String::from_utf8_lossy(&req.payload).into_owned()));
        Ok(Response { payload: Bytes::from_static(b"{\"input_tokens\":7}"), ..Default::default() })
    }
}

const SIGNED_CONTENT: &str = r#"[{"type":"thinking","thinking":"plan","signature":"sig-1"},{"type":"text","text":"Calling."},{"type":"tool_use","id":"toolu_1","name":"lookup","input":{"q":"x"}}]"#;

fn claude_request(session: &str, assistant_content: &str, model: &str) -> (Request, Options) {
    let payload = format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}},{{"role":"assistant","content":{assistant_content}}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}}]}}]}}"#
    );
    let mut opts = Options::new(Format::Claude);
    opts.metadata.insert("client_api_key".into(), Value::from("client-key"));
    opts.metadata.insert("execution_session_id".into(), Value::from(session));
    let req = Request { model: model.into(), payload: Bytes::from(payload), format: Format::Claude, metadata: Default::default() };
    (req, opts)
}

fn ok_response(content: &str) -> Result<Response, ExecError> {
    Ok(Response { payload: Bytes::from(format!(r#"{{"type":"message","role":"assistant","content":{content}}}"#)), ..Default::default() })
}

#[tokio::test]
async fn claude_path_sets_base_url_and_replays_thinking_across_k3_variants() {
    let fake = FakeClaude::new();
    let exec = new_with_claude(cfg_rx(), Some(fake.clone()));
    let auth = Auth::new("kimi-a", "kimi");
    let unthinking = r#"[{"type":"text","text":"Calling."},{"type":"tool_use","id":"toolu_1","name":"lookup","input":{"q":"x"}}]"#;

    // First turn: the response is cached under the K3 family.
    fake.non_stream.lock().push(ok_response(SIGNED_CONTENT));
    let (req, opts) = claude_request("sess-replay", unthinking, "kimi-k3");
    exec.execute(&auth, req, opts).await.unwrap();
    {
        let seen = fake.seen.lock();
        assert_eq!(seen[0].0[0], "https://api.kimi.com/coding", "claude base_url drops /v1");
        // Nothing cached yet, so the request is untouched.
        assert!(!seen[0].1.contains("sig-1"));
    }

    // Second turn on a K3 variant: the client dropped the thinking, the cache restores it.
    fake.non_stream.lock().push(ok_response(SIGNED_CONTENT));
    let (req, opts) = claude_request("sess-replay", unthinking, "k3-256k");
    exec.execute(&auth, req, opts).await.unwrap();
    assert!(fake.seen.lock()[1].1.contains(r#""signature":"sig-1""#), "replayed thinking missing");

    // A non-K3 family does not share the cache.
    fake.non_stream.lock().push(ok_response(SIGNED_CONTENT));
    let (req, opts) = claude_request("sess-replay", unthinking, "kimi-k2.6");
    exec.execute(&auth, req, opts).await.unwrap();
    assert!(!fake.seen.lock()[2].1.contains("sig-1"));
}

#[tokio::test]
async fn upstream_rejection_clears_applied_replay() {
    let fake = FakeClaude::new();
    let exec = new_with_claude(cfg_rx(), Some(fake.clone()));
    let auth = Auth::new("kimi-a", "kimi");
    let unthinking = r#"[{"type":"text","text":"Calling."},{"type":"tool_use","id":"toolu_1","name":"lookup","input":{"q":"x"}}]"#;

    fake.non_stream.lock().push(ok_response(SIGNED_CONTENT));
    let (req, opts) = claude_request("sess-clear", unthinking, "kimi-k3");
    exec.execute(&auth, req, opts).await.unwrap();

    // The replayed request is rejected with 400: the entry is cleared.
    fake.non_stream.lock().push(Err(ExecError::new(400, "bad")));
    let (req, opts) = claude_request("sess-clear", unthinking, "kimi-k3");
    assert_eq!(exec.execute(&auth, req, opts).await.unwrap_err().status, 400);
    assert!(fake.seen.lock()[1].1.contains("sig-1"));

    fake.non_stream.lock().push(ok_response("[]"));
    let (req, opts) = claude_request("sess-clear", unthinking, "kimi-k3");
    exec.execute(&auth, req, opts).await.unwrap();
    assert!(!fake.seen.lock()[2].1.contains("sig-1"), "cleared entry must not replay");
}

fn sse(events: &[&str]) -> Vec<String> {
    events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect()
}

#[tokio::test]
async fn claude_stream_caches_complete_content_and_replays_it() {
    let fake = FakeClaude::new();
    let exec = new_with_claude(cfg_rx(), Some(fake.clone()));
    let auth = Auth::new("kimi-a", "kimi");
    let unthinking = r#"[{"type":"text","text":"Calling."},{"type":"tool_use","id":"toolu_1","name":"lookup","input":{"q":"x"}}]"#;

    *fake.stream_body.lock() = sse(&[
        r#"{"type":"message_start","message":{"id":"m"}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-9"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Calling."}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"lookup","input":{}}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"x\"}"}}"#,
        r#"{"type":"content_block_stop","index":2}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let (req, opts) = claude_request("sess-stream", unthinking, "kimi-k3");
    let mut result = exec.execute_stream(&auth, req, opts).await.unwrap();
    let mut forwarded = 0;
    while result.chunks.recv().await.is_some() {
        forwarded += 1;
    }
    assert_eq!(forwarded, 12, "chunks are forwarded unchanged");
    // The cache write happens after the last chunk is forwarded.
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let (req, opts) = claude_request("sess-stream", unthinking, "kimi-k3");
        fake.non_stream.lock().push(ok_response("[]"));
        exec.execute(&auth, req, opts).await.unwrap();
        if fake.seen.lock().last().unwrap().1.contains("sig-9") {
            return;
        }
    }
    panic!("streamed thinking was never cached");
}

#[tokio::test]
async fn claude_format_without_delegate_is_501_and_count_tokens_uses_delegate() {
    let auth = Auth::new("kimi-a", "kimi");
    let exec = new(cfg_rx());
    let (req, opts) = claude_request("s", "[]", "kimi-k3");
    assert_eq!(exec.execute(&auth, req, opts).await.unwrap_err().status, 501);

    let fake = FakeClaude::new();
    let exec = new_with_claude(cfg_rx(), Some(fake.clone()));
    let mut auth = Auth::new("kimi-ai", "kimi-ai");
    auth.metadata.insert("domain".into(), Value::from("kimi.ai"));
    let (req, mut opts) = claude_request("s", "[]", "kimi-k3");
    opts.source_format = Format::Claude;
    let resp = exec.count_tokens(&auth, req, opts).await.unwrap();
    assert_eq!(resp.payload, Bytes::from_static(b"{\"input_tokens\":7}"));
    assert_eq!(fake.seen.lock()[0].0[0], "https://api.kimi.ai/coding");
}

// ---------------------------------------------------------------- refresh

#[tokio::test]
async fn refresh_rotates_tokens_and_keeps_kimi_ai_identity() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = serve(Mock {
        status: 200,
        content_type: "application/json".into(),
        body: r#"{"access_token":"new-ai-token","refresh_token":"new-ai-refresh","token_type":"Bearer","expires_in":3600}"#.into(),
        captured: captured.clone(),
    })
    .await;
    let exec = super::KimiExecutor { cfg: cfg_rx(), claude: None, api_key_scope: false, oauth_host: Some(base) };
    let mut auth = Auth::new("kimi-ai-refresh-test", "kimi-ai");
    auth.metadata.insert("type".into(), Value::from("kimi-ai"));
    auth.metadata.insert("access_token".into(), Value::from("old-token"));
    auth.metadata.insert("refresh_token".into(), Value::from("old-refresh"));
    auth.storage = Some(cpa_auth::storage::TokenStorage::Kimi(cpa_auth::storage::KimiTokenStorage {
        access_token: "old-token".into(),
        refresh_token: "old-refresh".into(),
        type_: "kimi-ai".into(),
        domain: "kimi.ai".into(),
        ..Default::default()
    }));

    let refreshed = cpa_runtime::executor::Executor::refresh(&exec, &auth).await.unwrap();
    assert_eq!(captured.lock().path, "/api/oauth/token");
    assert!(captured.lock().body.contains("grant_type=refresh_token") && captured.lock().body.contains("refresh_token=old-refresh"));
    assert_eq!(refreshed.metadata["access_token"], "new-ai-token");
    assert_eq!(refreshed.metadata["refresh_token"], "new-ai-refresh");
    assert_eq!(refreshed.metadata["type"], "kimi-ai");
    assert!(refreshed.metadata["expired"].as_str().is_some_and(|s| s.ends_with('Z')));
    let Some(cpa_auth::storage::TokenStorage::Kimi(s)) = &refreshed.storage else { panic!("storage kept") };
    assert_eq!((s.access_token.as_str(), s.type_.as_str(), s.domain.as_str()), ("new-ai-token", "kimi-ai", "kimi.ai"));
}

#[tokio::test]
async fn refresh_without_refresh_token_is_a_noop_and_rejection_has_no_status() {
    let exec = new(cfg_rx());
    let mut auth = Auth::new("a", "kimi");
    auth.metadata.insert("access_token".into(), Value::from("t"));
    let same = exec.refresh(&auth).await.unwrap();
    assert_eq!(same.metadata["access_token"], "t");

    let base = serve(Mock { status: 401, content_type: "text/plain".into(), body: "no".into(), captured: Default::default() }).await;
    let exec = super::KimiExecutor { cfg: cfg_rx(), claude: None, api_key_scope: false, oauth_host: Some(base) };
    auth.metadata.insert("refresh_token".into(), Value::from("r"));
    let err = cpa_runtime::executor::Executor::refresh(&exec, &auth).await.unwrap_err();
    assert_eq!(err.status, 0);
    assert!(err.message.contains("refresh token rejected (status 401)"), "{}", err.message);
}

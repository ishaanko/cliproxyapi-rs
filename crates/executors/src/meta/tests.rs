//! Meta tests. The golden files were recorded by running the Go implementation (full executor
//! against an httptest server, credential resolution, DCA mint refresh) on the same inputs; the
//! tests replay them against the Rust port.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes as AxBytes;
use axum::extract::State;
use axum::http::{HeaderMap as AxHeaders, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::any;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_runtime::executor::{ExecError, Executor, Options, Request};
use cpa_translator::Format;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::watch;

use super::creds::meta_creds;
use super::{MetaExecutor, new};

fn cfg_rx() -> crate::ConfigRx {
    watch::channel(Arc::new(Config::default())).1
}

/// Wall-clock fields the translators stamp into responses.
fn mask_volatile(s: &str) -> String {
    regex::Regex::new(r#""(created|created_at)":\s*[0-9]+"#).unwrap().replace_all(s, r#""$1":0"#).into_owned()
}

/// Same JSON structure and key order, ignoring whitespace and wall-clock stamps; non-JSON
/// compares as text.
fn same_json(a: &str, b: &str) -> bool {
    let (a, b) = (mask_volatile(a), mask_volatile(b));
    let (a, b) = (a.as_str(), b.as_str());
    if cpa_json::valid(a.as_bytes()) && cpa_json::valid(b.as_bytes()) {
        return cpa_json::to_string(&cpa_json::parse(a.as_bytes())) == cpa_json::to_string(&cpa_json::parse(b.as_bytes()));
    }
    a == b
}

/// SSE chunks compare exactly except that JSON `data:` payloads compare structurally.
fn same_json_sse(a: &str, b: &str) -> bool {
    let (a, b) = (mask_volatile(a), mask_volatile(b));
    let (a, b) = (a.as_str(), b.as_str());
    if a == b {
        return true;
    }
    let norm = |s: &str| {
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
    norm(a) == norm(b)
}

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
    (StatusCode::from_u16(mock.status).unwrap(), [("content-type", mock.content_type.clone())], mock.body.clone())
}

async fn serve(mock: Mock) -> String {
    let app = Router::new().fallback(any(mock_handler)).with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn header_of(c: &Captured, name: &str) -> String {
    c.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap_or_default()
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn mask_epochs(s: &str) -> String {
    regex::Regex::new(r"\b1[0-9]{9}\b").unwrap().replace_all(s, "<epoch>").into_owned()
}

#[derive(Deserialize)]
struct Scenario {
    name: String,
    op: String,
    source: String,
    #[serde(default)]
    response: String,
    model: String,
    #[serde(default)]
    alt: String,
    #[serde(default)]
    req_headers: HashMap<String, String>,
    payload: String,
    upstream_status: u16,
    upstream_content_type: String,
    upstream_body: String,
    #[serde(default)]
    resets_in_secs: i64,
    #[serde(default)]
    orphan_compat: bool,
    got_path: String,
    got_headers: Option<HashMap<String, String>>,
    got_body: String,
    out_payload: String,
    out_chunks: Option<Vec<String>>,
    err_status: u16,
    err_msg: String,
    err_retry_secs: i64,
    err_has_retry: bool,
    err_cred_scoped: bool,
    called: bool,
}

fn meta_auth(base: &str) -> Auth {
    let mut auth = Auth::new("meta-test", "meta");
    auth.attributes.insert("api_key".into(), "meta-token".into());
    auth.attributes.insert("base_url".into(), base.into());
    auth.attributes.insert("header:X-Custom".into(), "yes".into());
    auth
}

fn request_for(sc: &Scenario) -> (Request, Options) {
    let source = Format::parse(&sc.source).unwrap();
    let mut opts = Options::new(source);
    opts.stream = sc.op == "stream";
    opts.alt = sc.alt.clone();
    opts.original_request = Bytes::from(sc.payload.clone());
    for (k, v) in &sc.req_headers {
        opts.headers.insert(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), http::HeaderValue::from_str(v).unwrap());
    }
    if !sc.response.is_empty() {
        opts.response_format = Format::parse(&sc.response);
    }
    let req = Request { model: sc.model.clone(), payload: Bytes::from(sc.payload.clone()), format: source, metadata: Default::default() };
    (req, opts)
}

#[tokio::test]
async fn executor_matches_go_recordings() {
    let scenarios: Vec<Scenario> = serde_json::from_str(include_str!("testdata/exec_golden.json")).unwrap();
    assert!(scenarios.len() > 30);
    for sc in scenarios {
        let upstream_body = sc.upstream_body.replace("{{RESETS}}", &(now_secs() + sc.resets_in_secs).to_string());
        let captured = Arc::new(Mutex::new(Captured::default()));
        let base = serve(Mock {
            status: sc.upstream_status,
            content_type: sc.upstream_content_type.clone(),
            body: upstream_body,
            captured: captured.clone(),
        })
        .await;
        let mut config = Config::default();
        config.codex.orphan_delegation_compatibility = sc.orphan_compat;
        let exec = new(watch::channel(Arc::new(config)).1);
        let auth = meta_auth(&base);
        let (req, opts) = request_for(&sc);

        let mut payload = None;
        let mut chunks = None;
        let mut err: Option<ExecError> = None;
        match sc.op.as_str() {
            "stream" => match exec.execute_stream(&auth, req, opts).await {
                Ok(mut result) => {
                    let mut got = Vec::new();
                    while let Some(item) = result.chunks.recv().await {
                        match item {
                            Ok(b) => got.push(String::from_utf8_lossy(&b).into_owned()),
                            Err(e) => err = Some(e),
                        }
                    }
                    chunks = Some(got);
                }
                Err(e) => err = Some(e),
            },
            "count" => match exec.count_tokens(&auth, req, opts).await {
                Ok(r) => payload = Some(r.payload),
                Err(e) => err = Some(e),
            },
            _ => match exec.execute(&auth, req, opts).await {
                Ok(r) => payload = Some(r.payload),
                Err(e) => err = Some(e),
            },
        }

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
        assert_eq!(
            mask_epochs(err.as_ref().map(|e| e.message.as_str()).unwrap_or("")),
            mask_epochs(&sc.err_msg),
            "{}: error message",
            sc.name
        );
        if let Some(e) = &err {
            assert_eq!(e.retry_after.is_some(), sc.err_has_retry, "{}: retry-after presence", sc.name);
            if let Some(d) = e.retry_after {
                let got = d.as_secs() as i64;
                assert!((got - sc.err_retry_secs).abs() <= 90, "{}: retry-after {got}s vs {}s", sc.name, sc.err_retry_secs);
            }
            assert_eq!(e.credential_scoped, sc.err_cred_scoped, "{}: credential scope", sc.name);
        }
        if let Some(p) = payload {
            let got = String::from_utf8(p.to_vec()).unwrap();
            assert!(same_json(&got, &sc.out_payload), "{}: payload\n got {got}\nwant {}", sc.name, sc.out_payload);
        }
        if let Some(got) = chunks {
            let want = sc.out_chunks.clone().unwrap_or_default();
            assert_eq!(got.len(), want.len(), "{}: chunk count\n got {got:?}\nwant {want:?}", sc.name);
            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                assert!(same_json_sse(g, w), "{}: chunk {i}\n got {g:?}\nwant {w:?}", sc.name);
            }
        }
    }
}

#[derive(Deserialize)]
struct CredCase {
    auth: Value,
    base: String,
    token: String,
    dca: String,
    should_prepare: bool,
}

#[test]
fn credential_resolution_matches_go() {
    let cases: Vec<CredCase> = serde_json::from_str(include_str!("testdata/creds_golden.json")).unwrap();
    let exec = MetaExecutor { cfg: cfg_rx(), api_key_scope: false, mint_url: None };
    for case in cases {
        let mut auth = Auth::new("m", "meta");
        if let Some(attrs) = case.auth["attributes"].as_object() {
            for (k, v) in attrs {
                auth.attributes.insert(k.clone(), v.as_str().unwrap().to_string());
            }
        }
        if let Some(meta) = case.auth["metadata"].as_object() {
            for (k, v) in meta {
                auth.metadata.insert(k.clone(), v.clone());
            }
        }
        let ctx = case.auth.to_string();
        let (base, token) = meta_creds(Some(&auth));
        assert_eq!((base, token), (case.base.clone(), case.token.clone()), "{ctx}");
        assert_eq!(cpa_auth::meta::extract_dca_token(&auth), case.dca, "{ctx}");
        assert_eq!(exec.should_prepare_request_auth(&auth), case.should_prepare, "{ctx}");
    }
    assert_eq!(meta_creds(None), ("https://api.meta.ai/v1".to_string(), String::new()));
}

#[tokio::test]
async fn refresh_mints_key_and_prepare_request_auth_installs_it() {
    let golden: Value = serde_json::from_str(include_str!("testdata/mint_golden.json")).unwrap();
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = serve(Mock {
        status: 200,
        content_type: "application/json".into(),
        body: r#"{"api_key":"minted-key","base_url":"https://minted.example/v1","user_email":"me@example.com","user_full_name":"Me Myself","subs_tier_name":"Pro","subs_tier_id":"t1","is_subs_active":true,"has_payment_method":true}"#.into(),
        captured: captured.clone(),
    })
    .await;
    let exec = MetaExecutor { cfg: cfg_rx(), api_key_scope: false, mint_url: Some(format!("{base}/muse-code/key")) };
    let mut auth = Auth::new("m", "meta");
    for (k, v) in [
        ("type", "meta"),
        ("access_token", "dca:token-1"),
        ("expired", "2030-01-01T00:00:00Z"),
        ("subs_tier_name", "Old"),
        ("base_url", "https://old.example/v1"),
    ] {
        auth.metadata.insert(k.into(), Value::from(v));
    }
    assert!(exec.should_prepare_request_auth(&auth));
    let refreshed = exec.prepare_request_auth(&auth).await.unwrap().expect("minted");

    {
        let c = captured.lock();
        assert_eq!(header_of(&c, "authorization"), golden["got_auth"].as_str().unwrap());
        assert_eq!(header_of(&c, "user-agent"), golden["got_ua"].as_str().unwrap());
        assert_eq!(c.body, golden["got_body"].as_str().unwrap());
    }

    let mut metadata = serde_json::to_value(&refreshed.metadata).unwrap();
    let mut want = golden["metadata"].clone();
    for v in [&mut metadata, &mut want] {
        v.as_object_mut().unwrap().remove("last_refresh");
    }
    assert_eq!(metadata, want);
    let attrs: HashMap<String, String> = refreshed.attributes.into_iter().collect();
    let want_attrs: HashMap<String, String> = serde_json::from_value(golden["attributes"].clone()).unwrap();
    assert_eq!(attrs, want_attrs);

    // A usable key is left alone.
    assert!(!exec.should_prepare_request_auth(&refreshed_with_key()));
    assert!(exec.prepare_request_auth(&refreshed_with_key()).await.unwrap().is_none());
}

fn refreshed_with_key() -> Auth {
    let mut auth = Auth::new("m", "meta");
    auth.attributes.insert("api_key".into(), "k".into());
    auth
}

#[tokio::test]
async fn refresh_without_credentials_is_401_and_keyed_auth_is_unchanged() {
    let exec = MetaExecutor { cfg: cfg_rx(), api_key_scope: false, mint_url: None };
    let err = exec.refresh(&Auth::new("m", "meta")).await.unwrap_err();
    assert_eq!((err.status, err.message.as_str()), (401, "meta executor: missing API key or DCA token"));
    let kept = exec.refresh(&refreshed_with_key()).await.unwrap();
    assert_eq!(kept.attr("api_key"), "k");

    let base = serve(Mock { status: 500, content_type: "text/plain".into(), body: "nope".into(), captured: Default::default() }).await;
    let exec = MetaExecutor { cfg: cfg_rx(), api_key_scope: false, mint_url: Some(format!("{base}/mint")) };
    let mut auth = Auth::new("m", "meta");
    auth.metadata.insert("access_token".into(), Value::from("dca:x"));
    let err = exec.refresh(&auth).await.unwrap_err();
    assert_eq!(err.status, 0);
    assert!(err.message.starts_with("meta executor: mint API key failed:"), "{}", err.message);
}

#[tokio::test]
async fn execute_recovers_dca_only_credentials_by_minting() {
    // The upstream sees the minted key as a bearer token.
    let upstream_captured = Arc::new(Mutex::new(Captured::default()));
    let upstream = serve(Mock {
        status: 200,
        content_type: "text/event-stream".into(),
        body: "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n".into(),
        captured: upstream_captured.clone(),
    })
    .await;
    let mint_server = serve(Mock {
        status: 200,
        content_type: "application/json".into(),
        body: format!(r#"{{"api_key":"fresh-key","base_url":"{upstream}"}}"#),
        captured: Default::default(),
    })
    .await;
    let exec = MetaExecutor { cfg: cfg_rx(), api_key_scope: false, mint_url: Some(format!("{mint_server}/key")) };
    let mut auth = Auth::new("m", "meta");
    auth.metadata.insert("access_token".into(), Value::from("dca:needs-mint"));
    let mut opts = Options::new(Format::OpenAI);
    opts.original_request = Bytes::from_static(b"{}");
    let req = Request {
        model: "muse-spark-1.3".into(),
        payload: Bytes::from_static(br#"{"model":"muse-spark-1.3","messages":[{"role":"user","content":"hi"}]}"#),
        format: Format::OpenAI,
        metadata: Default::default(),
    };
    let resp = exec.execute(&auth, req, opts).await.unwrap();
    assert_eq!(header_of(&upstream_captured.lock(), "authorization"), "Bearer fresh-key");
    assert_eq!(resp.metadata["usage"]["total_tokens"], 2);
}

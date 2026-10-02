//! Devin HTTP flow and `LoginSession` end to end (management callbacks, cancel, CLI callback server),
//! all against in-process mocks.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::claude::{ClaudeEndpoints, credential_file_name};
use crate::codex::CodexEndpoints;
use crate::devin::{DevinAuthService, pb};
use crate::error::AuthFlowError;
use crate::login::{LoginEndpoints, LoginOptions, LoginSession, LoginStatus, Provider};
use crate::manager::Manager;
use crate::sessions::{CallbackPayload, CallbackRequest};
use crate::store::{FileTokenStore, Store};
use crate::testutil::{MockResponse, MockServer, RecordedRequest, make_jwt};
use crate::util::sha256_hex_prefix;

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().expect("client")
}

// ---------------- Devin ----------------

fn pb_string(num: u32, v: &str) -> Vec<u8> {
    let mut b = Vec::new();
    pb::append_string_field(&mut b, num, v);
    b
}

fn pb_nested(num: u32, inner: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    pb::append_bytes_field(&mut b, num, inner);
    b
}

#[tokio::test]
async fn devin_exchange_profile_status_and_auth_record() {
    let plan_info = pb_string(2, "Pro");
    let mut plan_status = pb_nested(1, &plan_info);
    pb::append_varint(&mut plan_status, 14 << 3);
    pb::append_varint(&mut plan_status, 42);
    let user = [pb_string(3, "alice"), pb_string(7, "alice@x.io"), pb_nested(13, &plan_status), pb_string(36, "uid-1")].concat();
    let status_body = pb_nested(1, &user);

    let srv = MockServer::start(move |req| match req.path() {
        "/auth/cli/token" => MockResponse::json(200, json!({"token": " eyJraw "})),
        "/v3/self" => MockResponse::json(200, json!({"user_name": "alice", "user_id": 77, "org_id": "org-1"})),
        p if p.ends_with("/GetUserStatus") => MockResponse::raw(200, status_body.clone()),
        _ => MockResponse::raw(404, vec![]),
    })
    .await;
    let mut svc = DevinAuthService::with_client(http());
    svc.set_api_base_url(&srv.url);
    svc.set_server_base_url(&srv.url);

    let token = svc.exchange_code_for_token(" code ", "verifier").await.unwrap();
    assert_eq!(token, "eyJraw");
    assert_eq!(srv.last().body_json(), json!({"code": "code", "code_verifier": "verifier"}));

    let auth = svc.create_auth_record(&token).await.unwrap();
    let reqs = srv.requests();
    let status_req = reqs.iter().find(|r| r.path().ends_with("/GetUserStatus")).unwrap();
    assert_eq!(status_req.header("authorization"), Some("Basic devin-session-token$eyJraw-devin-session-token$eyJraw"));
    assert_eq!(status_req.header("connect-protocol-version"), Some("1"));
    assert_eq!(status_req.header("content-type"), Some("application/proto"));

    assert_eq!(auth.id, "devin-alice.json");
    assert_eq!(auth.attr("session_token"), "devin-session-token$eyJraw");
    assert_eq!(auth.attr("user_id"), "77", "numeric ids are stringified like gjson");
    assert_eq!(auth.attr("org_id"), "org-1");
    assert_eq!(auth.metadata["email"], "alice@x.io");
    assert_eq!(auth.quota.signals["daily_quota_remaining_percent"], "42%");
    assert_eq!(auth.label, "Devin (alice - alice@x.io)");
}

#[tokio::test]
async fn devin_record_survives_failed_profile_and_status_lookups() {
    let srv = MockServer::start(|_| MockResponse::raw(500, vec![])).await;
    let mut svc = DevinAuthService::with_client(http());
    svc.set_api_base_url(&srv.url);
    svc.set_server_base_url(&srv.url);
    let auth = svc.create_auth_record("devin-session-token$abc").await.unwrap();
    assert!(auth.id.starts_with("devin-user-"), "{}", auth.id);
    assert!(auth.quota.signals.is_empty());
}

// ---------------- LoginSession end to end ----------------

fn manager(dir: &std::path::Path) -> Manager {
    Manager::new(Arc::new(FileTokenStore::with_dir(dir)))
}

fn claude_endpoints(base: &str) -> ClaudeEndpoints {
    ClaudeEndpoints {
        token_url: format!("{base}/token"),
        refresh_url: format!("{base}/token"),
        profile_url: format!("{base}/profile"),
        roles_url: format!("{base}/roles"),
    }
}

fn claude_mock(token_status: u16) -> impl Fn(&RecordedRequest) -> MockResponse + Send + Sync + 'static {
    move |req| match req.path() {
        "/token" if token_status != 200 => MockResponse::raw(token_status, b"nope".to_vec()),
        "/token" => MockResponse::json(
            200,
            json!({"access_token": "sk-ant-oat01-a", "refresh_token": "sk-ant-ort01-b", "expires_in": 3600,
                   "organization": {"uuid": "ORG-1", "name": "Org"}, "account": {"uuid": "ACC-1", "email_address": "e@x.com"}}),
        ),
        "/profile" => MockResponse::json(
            200,
            json!({"account": {"uuid": "ACC-1", "email": "e@x.com"}, "organization": {"uuid": "ORG-1", "name": "Org"}}),
        ),
        _ => MockResponse::json(200, json!({})),
    }
}

async fn wait_status(session: &LoginSession, want: impl Fn(&LoginStatus) -> bool) -> LoginStatus {
    for _ in 0..200 {
        let s = session.status();
        if want(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("status never reached the expected state, last: {:?}", session.status());
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn browser_get(port: u16, target: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

fn mgmt_claude_opts(base: &str) -> LoginOptions {
    let mut opts = LoginOptions::management("");
    opts.endpoints = LoginEndpoints { claude: Some(claude_endpoints(base)), ..Default::default() };
    opts
}

#[tokio::test]
async fn management_claude_login_via_callback_endpoint_saves_file() {
    let srv = MockServer::start(claude_mock(200)).await;
    let dir = tempfile::tempdir().unwrap();
    // A legacy email-only credential for the same org is absorbed and removed.
    std::fs::write(
        dir.path().join("claude-e@x.com.json"),
        r#"{"type":"claude","email":"e@x.com","organization_uuid":"ORG-1","proxy_url":"http://keep","access_token":"old"}"#,
    )
    .unwrap();
    let mgr = manager(dir.path());

    let session = mgr.start_login(Provider::Claude, mgmt_claude_opts(&srv.url)).await.unwrap();
    let info = session.start_info().clone();
    assert!(info.url.starts_with("https://claude.ai/oauth/authorize?") && info.url.contains(&format!("state={}", info.state)));
    assert_eq!(session.poll_json(), (200, json!({"status": "wait"})));

    // The /anthropic/callback route hands the redirect to the session; claude codes may carry #state.
    let req = CallbackRequest {
        provider: "anthropic".into(),
        state: info.state.clone(),
        code: "thecode#ignored".into(),
        ..Default::default()
    };
    assert_eq!(mgr.sessions().handle_oauth_callback(Some(dir.path()), &req), (200, json!({"status": "ok"})));

    let outcome = session.wait().await.unwrap();
    let expected_name = credential_file_name("e@x.com", "ORG-1", "ACC-1");
    assert_eq!(outcome.auth.id, expected_name);
    assert!(expected_name.contains(&sha256_hex_prefix("ORG-1", 8)));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join(&expected_name)).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "sk-ant-oat01-a");
    assert_eq!(saved["type"], "claude");
    assert_eq!(saved["proxy_url"], "http://keep", "operator fields survive the legacy migration");
    assert!(!dir.path().join("claude-e@x.com.json").exists());
    // The management flow strips the #state fragment before exchanging.
    let exchange = srv.requests().into_iter().find(|r| r.path() == "/token").unwrap().body_json();
    assert_eq!(exchange["code"], "thecode");
    assert_eq!(exchange["state"], info.state);

    assert_eq!(mgr.sessions().poll_status(&info.state), (200, json!({"status": "ok"})));
    assert_eq!(FileTokenStore::with_dir(dir.path()).list().unwrap().len(), 1);
}

#[tokio::test]
async fn management_exchange_failure_surfaces_in_poll_and_saves_nothing() {
    let srv = MockServer::start(claude_mock(400)).await;
    let dir = tempfile::tempdir().unwrap();
    let mgr = manager(dir.path());
    let session = mgr.start_login(Provider::Claude, mgmt_claude_opts(&srv.url)).await.unwrap();
    let state = session.state().to_string();
    let req = CallbackRequest { state: state.clone(), code: "c".into(), ..Default::default() };
    assert_eq!(mgr.sessions().handle_oauth_callback(None, &req).0, 200);

    let status = wait_status(&session, |s| matches!(s, LoginStatus::Failed(_))).await;
    assert_eq!(status, LoginStatus::Failed("Failed to exchange authorization code for tokens".into()));
    assert_eq!(
        mgr.sessions().poll_status(&state).1,
        json!({"status": "error", "error": "Failed to exchange authorization code for tokens"})
    );
    assert!(session.wait().await.is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn management_provider_error_callback_sets_bad_request() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = manager(dir.path());
    let session = mgr.start_login(Provider::Claude, mgmt_claude_opts("http://127.0.0.1:1")).await.unwrap();
    let req = CallbackRequest { state: session.state().into(), error: "access_denied".into(), ..Default::default() };
    assert_eq!(mgr.sessions().handle_oauth_callback(None, &req).0, 200);
    let status = wait_status(&session, |s| matches!(s, LoginStatus::Failed(_))).await;
    assert_eq!(status, LoginStatus::Failed("Bad request".into()));
}

#[tokio::test]
async fn cancelling_a_pending_login_stops_it_without_saving() {
    let srv = MockServer::start(claude_mock(200)).await;
    let dir = tempfile::tempdir().unwrap();
    let mgr = manager(dir.path());
    let session = mgr.start_login(Provider::Claude, mgmt_claude_opts(&srv.url)).await.unwrap();
    let state = session.state().to_string();

    assert_eq!(mgr.sessions().cancel_status(&state), (200, json!({"status": "ok", "cancelled": true})));
    assert_eq!(session.status(), LoginStatus::Gone);
    assert!(matches!(session.wait().await, Err(AuthFlowError::Cancelled)));
    assert_eq!(srv.request_count(), 0);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    // A callback for a cancelled session is rejected.
    let req = CallbackRequest { state, code: "c".into(), ..Default::default() };
    assert_eq!(mgr.sessions().handle_oauth_callback(None, &req).0, 404);
}

fn codex_id_token() -> String {
    make_jwt(&json!({
        "email": "dev@example.com",
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1", "chatgpt_plan_type": "plus"}
    }))
}

#[tokio::test]
async fn cli_codex_login_uses_local_callback_server() {
    let id_token = codex_id_token();
    let srv = MockServer::start(move |_| {
        MockResponse::json(200, json!({"access_token": "at", "refresh_token": "rt", "id_token": id_token, "expires_in": 3600}))
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mgr = manager(dir.path());
    let port = free_port();
    let mut opts = LoginOptions::cli();
    opts.callback_port = Some(port);
    opts.endpoints = LoginEndpoints {
        codex: Some(CodexEndpoints {
            token_url: format!("{}/oauth/token", srv.url),
            device_user_code_url: String::new(),
            device_token_url: String::new(),
        }),
        ..Default::default()
    };

    let session = mgr.start_login(Provider::Codex, opts).await.unwrap();
    let state = session.state().to_string();
    assert_eq!(session.start_info().callback_port, Some(port));

    let page = browser_get(port, &format!("/auth/callback?code=thecode&state={state}")).await;
    assert!(page.starts_with("HTTP/1.1 302"), "{page}");
    let outcome = session.wait().await.unwrap();

    let hash = sha256_hex_prefix("acct-1", 8);
    assert_eq!(outcome.auth.id, format!("codex-{hash}-dev@example.com-plus.json"));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(outcome.saved_path.unwrap()).unwrap()).unwrap();
    assert_eq!(saved["type"], "codex");
    assert_eq!(saved["account_id"], "acct-1");
    assert_eq!(saved["plan_type"], "plus");
    assert!(saved["expired"].is_string() && saved["last_refresh"].is_string());
    assert!(srv.last().form().iter().any(|(k, v)| k == "code" && v == "thecode"));
}

#[tokio::test]
async fn cli_state_mismatch_is_an_invalid_state_error() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = manager(dir.path());
    let port = free_port();
    let mut opts = LoginOptions::cli();
    opts.callback_port = Some(port);
    let session = mgr.start_login(Provider::Codex, opts).await.unwrap();
    let _ = browser_get(port, "/auth/callback?code=c&state=someone-elses").await;
    let err = session.wait().await.unwrap_err();
    assert!(err.to_string().starts_with("invalid_state: OAuth state parameter is invalid"), "{err}");
}

#[tokio::test]
async fn cli_login_accepts_a_pasted_callback_when_the_browser_cannot_reach_the_server() {
    let srv = MockServer::start(claude_mock(200)).await;
    let dir = tempfile::tempdir().unwrap();
    let mgr = manager(dir.path());
    let mut opts = LoginOptions::cli();
    opts.callback_port = Some(free_port());
    opts.endpoints = LoginEndpoints { claude: Some(claude_endpoints(&srv.url)), ..Default::default() };
    let session = mgr.start_login(Provider::Claude, opts).await.unwrap();

    // The interactive prompt appears after 15 s in the real flow; feed the inbox directly (the
    // same path a pasted URL takes) to keep the test fast.
    session
        .submit_callback(CallbackPayload { code: "pasted".into(), state: session.state().into(), error: String::new() })
        .unwrap();
    let outcome = session.wait().await.unwrap();
    assert_eq!(outcome.auth.provider, "claude");
    assert!(outcome.saved_path.unwrap().exists());
}

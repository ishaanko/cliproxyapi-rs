//! Provider flows against an in-process mock server: request shapes (bodies, headers), response
//! handling and error paths. No real network.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;

use crate::antigravity::{AntigravityAuth, AntigravityEndpoints};
use crate::claude::{ClaudeAuth, ClaudeEndpoints};
use crate::codex::{self, CodexAuth, CodexEndpoints};
use crate::error::AuthFlowError;
use crate::kimi::{self, DeviceFlowClient};
use crate::meta::{self, MetaAuth};
use crate::pkce::PkceCodes;
use crate::testutil::{MockResponse, MockServer, make_jwt};
use crate::xai::{self, XaiAuth};

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
}

fn pkce() -> PkceCodes {
    PkceCodes {
        code_verifier: "ver".into(),
        code_challenge: "chal".into(),
    }
}

// ---------------- Claude ----------------

fn claude_endpoints(base: &str) -> ClaudeEndpoints {
    ClaudeEndpoints {
        token_url: format!("{base}/token"),
        refresh_url: format!("{base}/token"),
        profile_url: format!("{base}/profile"),
        roles_url: format!("{base}/roles"),
    }
}

#[tokio::test]
async fn claude_exchange_sends_ordered_json_and_profile_wins() {
    let srv = MockServer::start(|req| match req.path() {
        "/token" => MockResponse::json(
            200,
            json!({
                "access_token": "sk-ant-oat01-x", "refresh_token": "sk-ant-ort01-y", "expires_in": 28800,
                "organization": {"uuid": "org-token", "name": "Token Org"},
                "account": {"uuid": "acc-token", "email_address": "token@x.com"}
            }),
        ),
        "/profile" => MockResponse::json(
            200,
            json!({"account": {"uuid": "acc-prof", "email": " prof@x.com "}, "organization": {"uuid": "org-prof", "name": ""}}),
        ),
        // The roles lookup is advisory: its failure must not fail the login.
        "/roles" => MockResponse::json(500, json!({})),
        _ => MockResponse::raw(404, vec![]),
    })
    .await;
    let svc = ClaudeAuth::with_client(http()).with_endpoints(claude_endpoints(&srv.url));

    let bundle = svc
        .exchange_code_for_tokens("thecode#frag-state", "session-state", &pkce())
        .await
        .unwrap();

    let reqs = srv.requests();
    let token_req = reqs.iter().find(|r| r.path() == "/token").unwrap();
    assert_eq!(token_req.method, "POST");
    assert_eq!(
        token_req.body_text(),
        r#"{"grant_type":"authorization_code","code":"thecode","redirect_uri":"http://localhost:54545/callback","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","code_verifier":"ver","state":"frag-state"}"#
    );
    assert_eq!(token_req.header("user-agent"), Some("axios/1.15.2"));
    assert_eq!(token_req.header("content-type"), Some("application/json"));
    let profile_req = reqs.iter().find(|r| r.path() == "/profile").unwrap();
    assert_eq!(
        profile_req.header("authorization"),
        Some("Bearer sk-ant-oat01-x")
    );
    assert_eq!(profile_req.header("cache-control"), Some("no-cache"));

    let d = &bundle.token_data;
    assert_eq!(d.access_token, "sk-ant-oat01-x");
    // Profile identity overrides the token response where non-empty (org name stays from token).
    assert_eq!(
        (d.email.as_str(), d.account_uuid.as_str()),
        ("prof@x.com", "acc-prof")
    );
    assert_eq!(
        (d.organization_uuid.as_str(), d.organization_name.as_str()),
        ("org-prof", "Token Org")
    );
    assert!(chrono::DateTime::parse_from_rfc3339(&d.expire).is_ok());
    assert_eq!(bundle.device_ids.len(), 1);

    let storage = svc.create_token_storage(&bundle);
    assert_eq!(storage.device_ids, bundle.device_ids);
}

#[tokio::test]
async fn claude_exchange_failure_reports_status_and_body() {
    let srv = MockServer::start(|_| MockResponse::raw(400, b"bad code".to_vec())).await;
    let svc = ClaudeAuth::with_client(http()).with_endpoints(claude_endpoints(&srv.url));
    let err = svc
        .exchange_code_for_tokens("c", "s", &pkce())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "token exchange failed with status 400: bad code"
    );
}

#[tokio::test]
async fn claude_refresh_body_keeps_old_refresh_token_and_fetches_profile() {
    let srv = MockServer::start(|req| match req.path() {
        "/token" => MockResponse::json(200, json!({"access_token": "new-at", "expires_in": 3600})),
        "/profile" => MockResponse::json(200, json!({"account": {"uuid": "a1", "email": "e@x"}, "organization": {"uuid": "o1", "name": "O"}})),
        _ => MockResponse::raw(404, vec![]),
    })
    .await;
    let svc = ClaudeAuth::with_client(http()).with_endpoints(claude_endpoints(&srv.url));
    let td = svc.refresh_tokens("rt-keep-old").await.unwrap();
    assert_eq!(
        srv.requests()[0].body_text(),
        r#"{"client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","grant_type":"refresh_token","refresh_token":"rt-keep-old","scope":"user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"}"#
    );
    assert_eq!(td.access_token, "new-at");
    assert_eq!(
        td.refresh_token, "rt-keep-old",
        "missing refresh_token keeps the old one"
    );
    assert_eq!(
        (td.email.as_str(), td.organization_name.as_str()),
        ("e@x", "O")
    );

    // A failing profile lookup after refresh keeps the tokens and just skips identity.
    let srv2 = MockServer::start(|req| match req.path() {
        "/token" => MockResponse::json(
            200,
            json!({"access_token": "at2", "refresh_token": "rt2", "expires_in": 60}),
        ),
        _ => MockResponse::raw(403, vec![]),
    })
    .await;
    let svc2 = ClaudeAuth::with_client(http()).with_endpoints(claude_endpoints(&srv2.url));
    let td2 = svc2.refresh_tokens("rt-no-profile").await.unwrap();
    assert_eq!(
        (
            td2.access_token.as_str(),
            td2.refresh_token.as_str(),
            td2.email.as_str()
        ),
        ("at2", "rt2", "")
    );
}

#[tokio::test]
async fn claude_429_blocks_further_refreshes_without_hitting_the_server() {
    let srv = MockServer::start(|_| {
        MockResponse::raw(429, b"slow down".to_vec()).with_header("Retry-After", "60")
    })
    .await;
    let svc = ClaudeAuth::with_client(http()).with_endpoints(claude_endpoints(&srv.url));

    let err = svc
        .refresh_tokens_with_retry("rt-429", 3)
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("token refresh failed after 3 attempts"),
        "{err}"
    );
    assert_eq!(srv.request_count(), 1, "429 is non-retryable");

    match svc.refresh_tokens("rt-429").await.unwrap_err() {
        AuthFlowError::Refresh {
            status: 429,
            retryable: false,
            ..
        } => {}
        other => panic!("expected blocked 429, got {other:?}"),
    }
    assert_eq!(
        srv.request_count(),
        1,
        "blocked refresh must not reach the server"
    );
}

#[tokio::test]
async fn claude_5xx_is_retryable_4xx_is_not() {
    let srv = MockServer::start(|_| MockResponse::raw(503, vec![])).await;
    let svc = ClaudeAuth::with_client(http()).with_endpoints(claude_endpoints(&srv.url));
    let err = svc.refresh_tokens("rt-503").await.unwrap_err();
    assert!(err.is_retryable_refresh());

    let srv = MockServer::start(|_| MockResponse::raw(400, vec![])).await;
    let svc = ClaudeAuth::with_client(http()).with_endpoints(claude_endpoints(&srv.url));
    assert!(
        !svc.refresh_tokens("rt-400")
            .await
            .unwrap_err()
            .is_retryable_refresh()
    );
}

// ---------------- Codex ----------------

fn codex_endpoints(base: &str) -> CodexEndpoints {
    CodexEndpoints {
        token_url: format!("{base}/oauth/token"),
        device_user_code_url: format!("{base}/usercode"),
        device_token_url: format!("{base}/devtoken"),
    }
}

fn codex_id_token() -> String {
    make_jwt(&json!({
        "email": "dev@example.com",
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1", "chatgpt_plan_type": "plus"}
    }))
}

#[tokio::test]
async fn codex_exchange_form_and_id_token_identity() {
    let id_token = codex_id_token();
    let it = id_token.clone();
    let srv = MockServer::start(move |_| {
        MockResponse::json(200, json!({"access_token": "at", "refresh_token": "rt", "id_token": it, "expires_in": 3600}))
    })
    .await;
    let svc = CodexAuth::with_client(http()).with_endpoints(codex_endpoints(&srv.url));
    let bundle = svc
        .exchange_code_for_tokens("the-code", &pkce())
        .await
        .unwrap();

    let req = srv.last();
    assert_eq!(
        req.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(req.header("accept"), Some("application/json"));
    assert_eq!(
        req.body_text(),
        "client_id=app_EMoamEEZ73f0CkXaXp7hrann&code=the-code&code_verifier=ver&grant_type=authorization_code&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
    );
    let d = &bundle.token_data;
    assert_eq!(
        (
            d.email.as_str(),
            d.account_id.as_str(),
            d.plan_type.as_str()
        ),
        ("dev@example.com", "acct-1", "plus")
    );
    assert_eq!(d.id_token, id_token);

    let storage = svc.create_token_storage(&bundle);
    assert_eq!(storage.plan_type, "plus");
}

#[tokio::test]
async fn codex_exchange_error_and_unparseable_id_token_defaults() {
    let srv =
        MockServer::start(|_| MockResponse::raw(400, b"{\"error\":\"invalid_grant\"}".to_vec()))
            .await;
    let svc = CodexAuth::with_client(http()).with_endpoints(codex_endpoints(&srv.url));
    let err = svc
        .exchange_code_for_tokens("c", &pkce())
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("token exchange failed with status 400:"),
        "{err}"
    );

    let srv = MockServer::start(|_| {
        MockResponse::json(
            200,
            json!({"access_token": "at", "id_token": "garbage", "expires_in": 1}),
        )
    })
    .await;
    let svc = CodexAuth::with_client(http()).with_endpoints(codex_endpoints(&srv.url));
    let bundle = svc.exchange_code_for_tokens("c", &pkce()).await.unwrap();
    assert_eq!(
        (
            bundle.token_data.email.as_str(),
            bundle.token_data.plan_type.as_str()
        ),
        ("", "free")
    );
}

#[tokio::test]
async fn codex_refresh_form_and_reuse_is_terminal() {
    let it = codex_id_token();
    let srv = MockServer::start(move |req| {
        if req.body_text().contains("refresh_token=reused-rt") {
            MockResponse::raw(400, b"{\"error\":{\"code\":\"refresh_token_reused\"}}".to_vec())
        } else {
            MockResponse::json(200, json!({"access_token": "at2", "refresh_token": "rt2", "id_token": it, "expires_in": 3600}))
        }
    })
    .await;
    let svc = CodexAuth::with_client(http()).with_endpoints(codex_endpoints(&srv.url));

    let td = svc.refresh_tokens("good-rt").await.unwrap();
    assert_eq!(
        srv.last().body_text(),
        "client_id=app_EMoamEEZ73f0CkXaXp7hrann&grant_type=refresh_token&refresh_token=good-rt&scope=openid+profile+email"
    );
    assert_eq!(
        (
            td.access_token.as_str(),
            td.refresh_token.as_str(),
            td.plan_type.as_str()
        ),
        ("at2", "rt2", "plus")
    );

    let before = srv.request_count();
    let err = svc
        .refresh_tokens_with_retry("reused-rt", 3)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("refresh_token_reused"));
    assert_eq!(
        srv.request_count(),
        before + 1,
        "reused token aborts without retrying"
    );
}

#[tokio::test]
async fn codex_device_flow_end_to_end() {
    let polls = Arc::new(AtomicUsize::new(0));
    let (p, it) = (polls.clone(), codex_id_token());
    let srv = MockServer::start(move |req| match req.path() {
        "/usercode" => MockResponse::json(200, json!({"device_auth_id": "dev-1", "usercode": "ABCD-1234", "interval": "1"})),
        "/devtoken" => {
            if p.fetch_add(1, Ordering::SeqCst) == 0 {
                MockResponse::raw(403, vec![])
            } else {
                MockResponse::json(200, json!({"authorization_code": "auth-code", "code_verifier": "v", "code_challenge": "c"}))
            }
        }
        "/oauth/token" => MockResponse::json(200, json!({"access_token": "at", "refresh_token": "rt", "id_token": it, "expires_in": 60})),
        _ => MockResponse::raw(404, vec![]),
    })
    .await;
    let svc = CodexAuth::with_client(http()).with_endpoints(codex_endpoints(&srv.url));

    let code = svc.request_device_user_code().await.unwrap();
    assert_eq!(
        srv.requests()[0].body_json(),
        json!({"client_id": "app_EMoamEEZ73f0CkXaXp7hrann"})
    );
    assert_eq!(
        (code.user_code.as_str(), code.device_auth_id.as_str()),
        ("ABCD-1234", "dev-1")
    );
    assert_eq!(code.poll_interval, Duration::from_secs(1));

    let fast = codex::DeviceUserCode {
        poll_interval: Duration::from_millis(10),
        ..code
    };
    let token = svc
        .poll_device_token_for(&fast, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(polls.load(Ordering::SeqCst), 2, "403 means pending");
    assert_eq!(
        srv.requests()[1].body_json(),
        json!({"device_auth_id": "dev-1", "user_code": "ABCD-1234"})
    );

    let bundle = svc.exchange_device_code(&token).await.unwrap();
    let form = srv.last().body_text();
    assert!(
        form.contains("redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback"),
        "{form}"
    );
    assert!(form.contains("code=auth-code") && form.contains("code_verifier=v"));
    assert_eq!(bundle.token_data.email, "dev@example.com");
}

#[tokio::test]
async fn codex_device_errors() {
    let srv = MockServer::start(|req| match req.path() {
        "/usercode" => MockResponse::raw(404, vec![]),
        _ => MockResponse::raw(500, b"boom".to_vec()),
    })
    .await;
    let svc = CodexAuth::with_client(http()).with_endpoints(codex_endpoints(&srv.url));
    let err = svc.request_device_user_code().await.unwrap_err();
    assert!(err.to_string().contains("endpoint is unavailable"));
    let code = codex::DeviceUserCode {
        device_auth_id: "d".into(),
        user_code: "u".into(),
        poll_interval: Duration::from_millis(5),
    };
    let err = svc
        .poll_device_token_for(&code, Duration::from_secs(2))
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("polling failed with status 500: boom")
    );
    let missing = codex::DeviceTokenResponse {
        authorization_code: "a".into(),
        ..Default::default()
    };
    assert!(svc.exchange_device_code(&missing).await.is_err());
}

// ---------------- Antigravity ----------------

fn ag_endpoints(base: &str) -> AntigravityEndpoints {
    AntigravityEndpoints {
        token: format!("{base}/token"),
        user_info: format!("{base}/userinfo"),
        api: format!("{base}/api"),
        daily_api: format!("{base}/daily"),
    }
}

#[tokio::test]
async fn antigravity_exchange_userinfo_and_project_from_load_code_assist() {
    let srv = MockServer::start(|req| match req.path() {
        "/token" => MockResponse::json(200, json!({"access_token": "ya29.at", "refresh_token": "1//rt", "expires_in": 3599, "token_type": "Bearer"})),
        "/userinfo" => MockResponse::json(200, json!({"email": " user@gmail.com "})),
        "/api/v1internal:loadCodeAssist" => MockResponse::json(200, json!({"cloudaicompanionProject": "cogent-snow-4mnnp"})),
        _ => MockResponse::raw(404, vec![]),
    })
    .await;
    let svc = AntigravityAuth::with_client(http())
        .with_client_secret("test-secret")
        .with_endpoints(ag_endpoints(&srv.url));

    let token = svc
        .exchange_code_for_tokens("auth-code", "http://localhost:51121/oauth-callback")
        .await
        .unwrap();
    assert_eq!(token.access_token, "ya29.at");
    let form = srv.last().body_text();
    assert_eq!(
        form,
        "client_id=1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com&client_secret=test-secret&code=auth-code&grant_type=authorization_code&redirect_uri=http%3A%2F%2Flocalhost%3A51121%2Foauth-callback"
    );

    assert_eq!(
        svc.fetch_user_info("ya29.at").await.unwrap(),
        "user@gmail.com"
    );
    let ui = srv.last();
    assert_eq!(ui.header("authorization"), Some("Bearer ya29.at"));
    assert!(
        ui.header("user-agent")
            .unwrap()
            .starts_with("antigravity/hub/")
    );

    assert_eq!(
        svc.fetch_project_id("ya29.at").await.unwrap(),
        "cogent-snow-4mnnp"
    );
    let lca = srv.last();
    assert_eq!(
        lca.body_json(),
        json!({"metadata": {"ideType": "ANTIGRAVITY"}})
    );
    assert_eq!(lca.header("accept"), Some("*/*"));
    assert!(lca.header("x-goog-api-client").is_none());
    assert!(
        !lca.header("user-agent")
            .unwrap()
            .contains("google-api-nodejs-client")
    );
}

#[tokio::test]
async fn antigravity_onboard_fallback_polls_until_done() {
    let onboard_calls = Arc::new(AtomicUsize::new(0));
    let oc = onboard_calls.clone();
    let srv = MockServer::start(move |req| match req.path() {
        "/api/v1internal:loadCodeAssist" => MockResponse::json(200, json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]})),
        "/daily/v1internal:onboardUser" => {
            if oc.fetch_add(1, Ordering::SeqCst) == 0 {
                MockResponse::json(200, json!({"done": false}))
            } else {
                MockResponse::json(200, json!({"done": true, "response": {"cloudaicompanionProject": {"id": "proj-9", "name": "proj-9"}}}))
            }
        }
        _ => MockResponse::raw(404, vec![]),
    })
    .await;
    let svc = AntigravityAuth::with_client(http())
        .with_client_secret("s")
        .with_endpoints(ag_endpoints(&srv.url));

    // fetch_project_id uses the real 2 s poll; drive onboard_user directly with a short poll.
    let project = svc
        .onboard_user_with_poll("tok", "free-tier", Duration::from_millis(10))
        .await
        .unwrap();
    assert_eq!(project, "proj-9");
    let req = srv.last();
    let body = req.body_json();
    assert_eq!(body["tier_id"], "free-tier");
    assert_eq!(body["metadata"]["ide_type"], "ANTIGRAVITY");
    assert_eq!(body["metadata"]["ide_name"], "antigravity");
    assert_eq!(req.header("x-goog-api-client"), Some("gl-node/22.21.1"));
    assert!(
        req.header("user-agent")
            .unwrap()
            .contains("google-api-nodejs-client/10.3.0")
    );
    assert_eq!(onboard_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn antigravity_upstream_403_keeps_status_code() {
    let srv = MockServer::start(|_| {
        MockResponse::raw(
            403,
            br#"{"error":{"code":403,"message":"no permission"}}"#.to_vec(),
        )
    })
    .await;
    let svc = AntigravityAuth::with_client(http())
        .with_client_secret("s")
        .with_endpoints(ag_endpoints(&srv.url));
    let err = svc.fetch_project_id("tok").await.unwrap_err();
    assert_eq!(err.status_code(), Some(403));
}

#[tokio::test]
async fn antigravity_refresh_uses_go_user_agent_and_form() {
    let srv = MockServer::start(|_| {
        MockResponse::json(
            200,
            json!({"access_token": "new", "expires_in": 3600, "token_type": "Bearer"}),
        )
    })
    .await;
    let svc = AntigravityAuth::with_client(http())
        .with_client_secret("s3cret")
        .with_endpoints(ag_endpoints(&srv.url));
    let t = svc.refresh_tokens(" 1//rt ").await.unwrap();
    assert_eq!(t.access_token, "new");
    let req = srv.last();
    assert_eq!(req.header("user-agent"), Some("Go-http-client/2.0"));
    assert_eq!(
        req.form_value("grant_type").as_deref(),
        Some("refresh_token")
    );
    assert_eq!(req.form_value("refresh_token").as_deref(), Some("1//rt"));
    assert_eq!(req.form_value("client_secret").as_deref(), Some("s3cret"));

    let mut auth = crate::types::Auth::new("antigravity-a.json", "antigravity");
    crate::antigravity::apply_refresh_to_auth(&mut auth, &t);
    assert_eq!(auth.metadata["access_token"], "new");
    assert!(
        !auth.metadata.contains_key("refresh_token"),
        "empty refresh_token keeps the old one (absent here)"
    );
    assert_eq!(auth.metadata["type"], "antigravity");

    let err = svc.refresh_tokens("  ").await.unwrap_err();
    assert_eq!(err.status_code(), Some(401));
}

// ---------------- xAI ----------------

fn xai_for(base: &str) -> XaiAuth {
    XaiAuth::with_client(http()).for_tests(
        &format!("{base}/.well-known/openid-configuration"),
        Duration::from_millis(10),
    )
}

#[tokio::test]
async fn xai_device_flow_pending_slow_down_then_token() {
    let polls = Arc::new(AtomicUsize::new(0));
    let (p, it) = (
        polls.clone(),
        make_jwt(&json!({"email": "grok@x.ai", "sub": "sub-7"})),
    );
    let srv_url = Arc::new(parking_lot::Mutex::new(String::new()));
    let su = srv_url.clone();
    let srv = MockServer::start(move |req| match req.path() {
        "/.well-known/openid-configuration" => {
            let base = su.lock().clone();
            MockResponse::json(
                200,
                json!({"device_authorization_endpoint": format!("{base}/device"), "token_endpoint": format!("{base}/token")}),
            )
        }
        "/device" => MockResponse::json(
            200,
            json!({"device_code": "dc", "user_code": "WXYZ", "verification_uri": "https://x.ai/device", "verification_uri_complete": "https://x.ai/device?c=WXYZ", "expires_in": 600, "interval": 0}),
        ),
        "/token" => match req.form_value("grant_type").as_deref() {
            Some("refresh_token") => MockResponse::json(200, json!({"access_token": "refreshed", "expires_in": 3600})),
            _ => match p.fetch_add(1, Ordering::SeqCst) {
                0 => MockResponse::json(400, json!({"error": "authorization_pending"})),
                1 => MockResponse::json(400, json!({"error": "slow_down"})),
                _ => MockResponse::json(200, json!({"access_token": "at", "refresh_token": "rt", "id_token": it, "token_type": "Bearer", "expires_in": 3600})),
            },
        },
        _ => MockResponse::raw(404, vec![]),
    })
    .await;
    *srv_url.lock() = srv.url.clone();
    let svc = xai_for(&srv.url);

    let device = svc.start_device_flow().await.unwrap();
    assert_eq!(device.verification_url(), "https://x.ai/device?c=WXYZ");
    assert_eq!(
        srv.requests()[1].form_value("scope").as_deref(),
        Some(xai::SCOPE)
    );

    let bundle = svc.wait_for_authorization(&device).await.unwrap();
    assert_eq!(polls.load(Ordering::SeqCst), 3);
    let req = srv.last();
    assert_eq!(
        req.form_value("grant_type").as_deref(),
        Some(xai::DEVICE_CODE_GRANT_TYPE)
    );
    assert_eq!(req.form_value("device_code").as_deref(), Some("dc"));
    assert_eq!(
        (
            bundle.token_data.email.as_str(),
            bundle.token_data.subject.as_str()
        ),
        ("grok@x.ai", "sub-7")
    );
    assert_eq!(bundle.base_url, xai::DEFAULT_API_BASE_URL);
    assert_eq!(bundle.token_endpoint, format!("{}/token", srv.url));

    let td = svc
        .refresh_tokens("rt", &bundle.token_endpoint)
        .await
        .unwrap();
    assert_eq!(td.access_token, "refreshed");
    let storage = svc.create_token_storage(&bundle);
    let auth = xai::build_auth_record(storage).unwrap();
    assert_eq!(auth.id, "xai-grok@x.ai.json");
}

#[tokio::test]
async fn xai_device_denied_and_expired() {
    for (err_code, expect) in [
        ("access_denied", "xai device authorization denied"),
        ("expired_token", "xai device code expired"),
    ] {
        let srv =
            MockServer::start(move |_| MockResponse::json(400, json!({"error": err_code}))).await;
        let svc = xai_for(&srv.url);
        let device = xai::DeviceCodeResponse {
            device_code: "dc".into(),
            token_endpoint: format!("{}/token", srv.url),
            ..Default::default()
        };
        let err = svc.poll_for_token(&device).await.unwrap_err();
        assert_eq!(err.to_string(), expect);
    }
}

// ---------------- Kimi ----------------

#[tokio::test]
async fn kimi_device_flow_headers_poll_and_refresh() {
    let polls = Arc::new(AtomicUsize::new(0));
    let p = polls.clone();
    let srv = MockServer::start(move |req| match req.path() {
        "/api/oauth/device_authorization" => MockResponse::json(
            200,
            json!({"device_code": "dc", "user_code": "KIMI-1", "verification_uri_complete": "https://www.kimi.com/code/authorize_device?user_code=KIMI-1", "expires_in": 600, "interval": 5}),
        ),
        "/api/oauth/token" => match req.form_value("grant_type").as_deref() {
            Some("refresh_token") => {
                if req.form_value("refresh_token").as_deref() == Some("revoked") {
                    MockResponse::raw(401, vec![])
                } else {
                    MockResponse::json(200, json!({"access_token": "at2", "refresh_token": "rt2", "token_type": "Bearer", "expires_in": 7200.0}))
                }
            }
            _ => {
                if p.fetch_add(1, Ordering::SeqCst) == 0 {
                    MockResponse::json(400, json!({"error": "authorization_pending"}))
                } else {
                    MockResponse::json(200, json!({"access_token": "at", "refresh_token": "rt", "token_type": "Bearer", "expires_in": 3600.5, "scope": "kimi-code"}))
                }
            }
        },
        _ => MockResponse::raw(404, vec![]),
    })
    .await;
    let client =
        DeviceFlowClient::with_client(http(), "kimi.com", "device-xyz").with_oauth_host(&srv.url);

    let device = client.request_device_code().await.unwrap();
    let first = &srv.requests()[0];
    assert_eq!(
        first.form_value("client_id").as_deref(),
        Some("17e5f671-d194-4dfb-9706-5516cb48c098")
    );
    assert_eq!(first.header("x-msh-platform"), Some("CLIProxyAPI"));
    assert_eq!(first.header("x-msh-device-id"), Some("device-xyz"));
    assert!(
        first.header("x-msh-device-model").is_some() && first.header("x-msh-device-name").is_some()
    );
    assert_eq!(
        device.verification_url(),
        "https://www.kimi.com/code/authorize_device?user_code=KIMI-1"
    );

    let token = client
        .poll_for_token_with_min_interval(
            &kimi::DeviceCodeResponse {
                interval: 0,
                ..device
            },
            Duration::from_millis(10),
        )
        .await
        .unwrap();
    assert_eq!(
        (token.access_token.as_str(), token.scope.as_str()),
        ("at", "kimi-code")
    );
    assert!(token.expires_at > chrono::Utc::now().timestamp());
    assert_eq!(polls.load(Ordering::SeqCst), 2);

    let td = client.refresh_token("rt").await.unwrap();
    assert_eq!(
        (td.access_token.as_str(), td.refresh_token.as_str()),
        ("at2", "rt2")
    );
    let err = client.refresh_token("revoked").await.unwrap_err();
    assert_eq!(err.status_code(), Some(401));
    assert!(
        err.to_string()
            .contains("refresh token rejected (status 401)")
    );
}

#[tokio::test]
async fn kimi_access_denied_and_empty_token() {
    let srv = MockServer::start(|req| match req.form_value("device_code").as_deref() {
        Some("denied") => MockResponse::json(400, json!({"error": "access_denied"})),
        _ => MockResponse::json(200, json!({"access_token": ""})),
    })
    .await;
    let client = DeviceFlowClient::with_client(http(), "kimi.ai", "d").with_oauth_host(&srv.url);
    let mk = |code: &str| kimi::DeviceCodeResponse {
        device_code: code.into(),
        ..Default::default()
    };
    let min = Duration::from_millis(5);
    assert_eq!(
        client
            .poll_for_token_with_min_interval(&mk("denied"), min)
            .await
            .unwrap_err()
            .to_string(),
        "kimi: access denied by user"
    );
    assert!(
        client
            .poll_for_token_with_min_interval(&mk("x"), min)
            .await
            .unwrap_err()
            .to_string()
            .contains("empty access token")
    );
}

// ---------------- Meta ----------------

#[tokio::test]
async fn meta_device_flow_mints_api_key() {
    let polls = Arc::new(AtomicUsize::new(0));
    let p = polls.clone();
    let srv = MockServer::start(move |req| match req.path() {
        "/device" => MockResponse::json(
            200,
            json!({"device_code": "dc", "user_code": "META-1", "verification_uri": "https://meta/activate", "expires_in": 600, "interval": 1}),
        ),
        "/token" => {
            if p.fetch_add(1, Ordering::SeqCst) == 0 {
                MockResponse::json(400, json!({"error": "authorization_pending"}))
            } else {
                MockResponse::json(200, json!({"access_token": "dca:tok", "token_type": "Bearer", "expires_in": 3600}))
            }
        }
        "/mint" => {
            assert_eq!(req.header("authorization"), Some("Bearer dca:tok"));
            MockResponse::json(
                200,
                json!({"api_key": "meta-key", "base_url": "https://api.meta.ai/v1", "user_email": "m@x.com", "user_full_name": "M", "subs_tier_name": "pro", "subs_tier_id": "t1", "is_subs_active": true}),
            )
        }
        _ => MockResponse::raw(404, vec![]),
    })
    .await;
    let mut svc = MetaAuth::with_client(http())
        .with_endpoints(
            &format!("{}/device", srv.url),
            &format!("{}/token", srv.url),
        )
        .with_poll_floor(Duration::from_millis(10));
    svc.set_mint_url(&format!("{}/mint", srv.url));

    let device = svc.start_device_flow().await.unwrap();
    assert_eq!(device.verification_url(), "https://meta/activate");
    assert_eq!(
        srv.requests()[0].header("user-agent"),
        Some("muse-code/1.0.2")
    );
    assert_eq!(
        srv.requests()[0].form_value("client_id").as_deref(),
        Some("1031625952748946")
    );

    let bundle = svc.wait_for_authorization(&device).await.unwrap();
    assert_eq!(bundle.email, "m@x.com");
    assert_eq!(
        srv.requests()
            .iter()
            .filter(|r| r.path() == "/token")
            .count(),
        2
    );
    assert_eq!(srv.last().body_json(), json!({"dca_token": "dca:tok"}));

    let storage = svc.create_token_storage(&bundle);
    let auth = meta::build_auth_record(storage, &bundle).unwrap();
    assert_eq!(auth.metadata["access_token"], "meta-key");
    assert_eq!(auth.metadata["dca_token"], "dca:tok");
    assert_eq!(auth.metadata["subs_tier_name"], "pro");
}

#[tokio::test]
async fn meta_mint_failure_falls_back_to_dca_token() {
    let srv = MockServer::start(|req| match req.path() {
        "/token" => MockResponse::json(200, json!({"access_token": "dca:t", "expires_in": 60})),
        _ => MockResponse::raw(500, b"nope".to_vec()),
    })
    .await;
    let mut svc = MetaAuth::with_client(http())
        .with_endpoints(
            &format!("{}/device", srv.url),
            &format!("{}/token", srv.url),
        )
        .with_poll_floor(Duration::from_millis(5));
    svc.set_mint_url(&format!("{}/mint", srv.url));
    let device = meta::DeviceCodeResponse {
        device_code: "dc".into(),
        token_endpoint: format!("{}/token", srv.url),
        ..Default::default()
    };
    let bundle = svc.wait_for_authorization(&device).await.unwrap();
    assert!(bundle.minted_key.is_none());
    let storage = svc.create_token_storage(&bundle);
    assert_eq!(
        (storage.access_token.as_str(), storage.api_key.as_str()),
        ("dca:t", "")
    );
    assert!(
        !storage.expired.is_empty(),
        "DCA-only credentials carry an expiry"
    );

    let err = svc.mint_api_key("dca:t").await.unwrap_err();
    assert!(
        err.to_string().contains("mint key failed (HTTP 500)"),
        "{err}"
    );
}

// ---------------- Hostile server values and error semantics ----------------

#[tokio::test]
async fn hostile_expires_in_never_panics() {
    // Claude / Codex / Antigravity tokens with absurd lifetimes.
    let srv = MockServer::start(|req| match req.path() {
        "/profile" => MockResponse::json(200, json!({"account": {"uuid": "a", "email": "e"}})),
        _ => MockResponse::json(
            200,
            json!({"access_token": "at", "refresh_token": "rt", "expires_in": i64::MAX}),
        ),
    })
    .await;
    let svc = ClaudeAuth::with_client(http()).with_endpoints(claude_endpoints(&srv.url));
    let td = svc.refresh_tokens("rt-hostile").await.unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&td.expire).is_ok(),
        "{}",
        td.expire
    );

    let ag = AntigravityAuth::with_client(http())
        .with_client_secret("s")
        .with_endpoints(ag_endpoints(&srv.url));
    let t = ag.refresh_tokens("rt").await.unwrap();
    let auth = {
        let mut a = crate::types::Auth::new("a.json", "antigravity");
        crate::antigravity::apply_refresh_to_auth(&mut a, &t);
        a
    };
    assert!(auth.metadata["expired"].is_string());
    // A huge relative expiry in a credential file saturates instead of panicking.
    let mut file = crate::types::Auth::new("f.json", "x");
    file.metadata.insert("expires_in".into(), json!(i64::MAX));
    file.metadata
        .insert("timestamp".into(), json!(1_700_000_000_000i64));
    assert!(file.expiration_time().is_some());
}

#[tokio::test]
async fn xai_and_kimi_device_codes_with_absurd_timings_do_not_panic() {
    let srv = MockServer::start(|_| {
        MockResponse::json(200, json!({"access_token": "at", "expires_in": 1e30}))
    })
    .await;
    let client = DeviceFlowClient::with_client(http(), "kimi.com", "d").with_oauth_host(&srv.url);
    let device = kimi::DeviceCodeResponse {
        device_code: "dc".into(),
        expires_in: i64::MAX,
        interval: 0,
        ..Default::default()
    };
    let token = client
        .poll_for_token_with_min_interval(&device, Duration::from_millis(5))
        .await
        .unwrap();
    assert!(token.expires_at > 0);

    let srv = MockServer::start(|_| {
        MockResponse::json(200, json!({"access_token": "at", "expires_in": i64::MAX}))
    })
    .await;
    let svc = xai_for(&srv.url);
    let device = xai::DeviceCodeResponse {
        device_code: "dc".into(),
        expires_in: i64::MAX,
        interval: i64::MAX,
        token_endpoint: format!("{}/token", srv.url),
        ..Default::default()
    };
    // First attempt is immediate, so the absurd interval is never slept on.
    let td = svc.poll_for_token(&device).await.unwrap();
    assert!(chrono::DateTime::parse_from_rfc3339(&td.expire).is_ok());
}

#[tokio::test]
async fn antigravity_refresh_429_carries_the_parsed_retry_delay() {
    let srv = MockServer::start(|_| {
        MockResponse::json(
            429,
            json!({"error": {"code": 429, "message": "quota", "details": [
                {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "17s"}]}}),
        )
    })
    .await;
    let svc = AntigravityAuth::with_client(http())
        .with_client_secret("s")
        .with_endpoints(ag_endpoints(&srv.url));
    let err = svc.refresh_tokens("rt-429").await.unwrap_err();
    assert_eq!(err.status_code(), Some(429));
    assert_eq!(err.retry_after(), Some(Duration::from_secs(17)));
}

#[tokio::test]
async fn exhausted_retries_keep_the_last_error_semantics_and_block_message_has_a_time() {
    let srv = MockServer::start(|_| MockResponse::raw(502, b"bad gateway".to_vec())).await;
    // Codex with 1 attempt is enough to observe the wrapper (no sleeping between attempts).
    let svc = CodexAuth::with_client(http()).with_endpoints(codex_endpoints(&srv.url));
    let err = svc
        .refresh_tokens_with_retry("rt-502", 1)
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthFlowError::RetriesExhausted { attempts: 1, .. }),
        "{err:?}"
    );
    assert_eq!(err.status_code(), Some(502));
    assert!(err.to_string().starts_with(
        "token refresh failed after 1 attempts: token refresh failed with status 502"
    ));

    let srv =
        MockServer::start(|_| MockResponse::raw(429, vec![]).with_header("Retry-After", "30"))
            .await;
    let svc = ClaudeAuth::with_client(http()).with_endpoints(claude_endpoints(&srv.url));
    let _ = svc.refresh_tokens("rt-blocked-msg").await;
    let blocked = svc
        .refresh_tokens("rt-blocked-msg")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        blocked.contains("refresh temporarily blocked until 20"),
        "{blocked}"
    );
}

#[test]
fn antigravity_missing_secret_warning_only_when_credentials_exist() {
    if std::env::var(crate::antigravity::CLIENT_SECRET_ENV).is_ok() {
        return;
    }
    let none = [crate::types::Auth::new("c.json", "claude")];
    assert!(!crate::antigravity::warn_if_client_secret_missing(&none));
    let some = [crate::types::Auth::new("antigravity-a.json", "Antigravity")];
    assert!(crate::antigravity::warn_if_client_secret_missing(&some));
}

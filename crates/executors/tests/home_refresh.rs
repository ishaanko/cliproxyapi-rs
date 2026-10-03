//! Ports of internal/runtime/executor/helps/home_refresh_test.go against a mock Home. The Home
//! client is process-global, so every test holds `LOCK` and installs its own mock.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_executors::helps::home_refresh::refresh_auth_via_home;
use cpa_executors::helps::usage::reporter::access_token_sha256;
use cpa_home::Client;
use cpa_home::testing::{self as t, MockHome, Reply};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::const_new(());

fn enabled_config() -> Config {
    let mut cfg = Config::default();
    cfg.home.enabled = true;
    cfg
}

fn codex_auth() -> Auth {
    let mut auth = Auth::new("home-auth", "codex");
    auth.index = "home-auth".into();
    auth
}

struct Home {
    _guard: MutexGuard<'static, ()>,
    mock: MockHome,
    client: Arc<Client>,
}

/// Installs a mock Home whose `GET` (the refresh request) answers `reply`.
async fn home_replying(reply: impl Fn() -> Reply + Send + Sync + 'static) -> Home {
    let guard = LOCK.lock().await;
    let mock = MockHome::start(move |args| if args[0].eq_ignore_ascii_case("GET") { reply() } else { t::err("ERR unknown command") }).await;
    let cfg = cpa_config::HomeConfig { enabled: true, host: "127.0.0.1".into(), port: i64::from(mock.port()), ..Default::default() };
    let client = Arc::new(Client::new(cfg));
    client.set_test_operation_timeout(Duration::from_secs(2));
    client.set_heartbeat_ok_for_tests(true);
    cpa_home::kv::set_current(client.clone());
    Home { _guard: guard, mock, client }
}

async fn home_with_body(body: impl Into<Vec<u8>>) -> Home {
    let body = body.into();
    home_replying(move || t::bulk(&body)).await
}

#[tokio::test]
async fn disabled_home_leaves_refresh_to_the_executor() {
    let cfg = Config::default();
    assert!(refresh_auth_via_home(&cfg, &codex_auth()).await.is_none());
}

#[tokio::test]
async fn unavailable_control_center_is_a_503() {
    let h = home_with_body(b"{}".to_vec()).await;
    h.client.set_heartbeat_ok_for_tests(false);
    let err = refresh_auth_via_home(&enabled_config(), &codex_auth()).await.unwrap().unwrap_err();
    assert_eq!((err.status, err.message.as_str()), (503, "home control center unavailable"));
    assert!(h.mock.commands().is_empty());

    cpa_home::kv::clear_current();
    let err = refresh_auth_via_home(&enabled_config(), &codex_auth()).await.unwrap().unwrap_err();
    assert_eq!(err.status, 503);
}

#[tokio::test]
async fn transport_failure_is_a_generic_503_without_a_direct_response() {
    let _h = home_replying(|| Reply::Close).await;
    let err = refresh_auth_via_home(&enabled_config(), &codex_auth()).await.unwrap().unwrap_err();
    assert_eq!((err.status, err.message.as_str()), (503, "home refresh temporarily unavailable"));
    assert!(err.body.is_none());
}

#[tokio::test]
async fn error_envelopes_map_to_generic_client_errors() {
    for (raw, status, message) in [
        // Legacy envelope: the untrusted message never reaches the client.
        (r#"{"error":{"type":"error","message":"provider response: refresh_token=provider-secret"}}"#, 503, "credential refresh temporarily unavailable"),
        (r#"{"error":{"type":"refresh_temporarily_unavailable","message":"database unavailable: provider-secret"}}"#, 503, "credential refresh temporarily unavailable"),
        (r#"{"error":{"type":"authentication_error","message":"codex refresh: invalid_grant refresh_token=provider-secret"}}"#, 401, "credential unauthorized"),
        (r#"{"error":{"code":"unauthorized"}}"#, 401, "credential unauthorized"),
        (r#"{"error":{"type":"model_not_found"}}"#, 404, "credential refresh target not found"),
        (r#"{"error":{"type":"home_unavailable","diagnostic":"antigravity refresh failed: stage=transport err=EOF"}}"#, 503, "credential refresh temporarily unavailable"),
    ] {
        let _h = home_with_body(raw).await;
        let err = refresh_auth_via_home(&enabled_config(), &codex_auth()).await.unwrap().unwrap_err();
        assert_eq!((err.status, err.message.as_str()), (status, message), "{raw}");
        assert!(err.body.is_none(), "{raw}");
        assert!(!err.message.contains("provider-secret"));
    }
}

#[tokio::test]
async fn upstream_status_and_body_pass_through_exactly() {
    for (status, body) in [
        (400u16, b"{\"error\":\"invalid_request\"}".to_vec()),
        (401, b"{\"error\":{\"message\":\"access token expired\"}}".to_vec()),
        (502, b"provider unavailable".to_vec()),
        (429, b"first line\r\nsecond line\n".to_vec()),
        (401, Vec::new()),
    ] {
        let raw = json!({"error": {
            "type": "refresh_temporarily_unavailable",
            "message": "credential refresh temporarily unavailable",
            "diagnostic": "antigravity refresh failed: stage=upstream_response status=400",
            "upstream": {"status": status, "body": base64::engine::general_purpose::STANDARD.encode(&body)},
        }});
        let _h = home_with_body(serde_json::to_vec(&raw).unwrap()).await;
        let err = refresh_auth_via_home(&enabled_config(), &codex_auth()).await.unwrap().unwrap_err();
        assert_eq!(err.status, status);
        assert_eq!(err.message.as_bytes(), body.as_slice());
        assert_eq!(err.body.as_deref(), Some(body.as_slice()));
    }
}

#[tokio::test]
async fn auth_envelope_is_accepted_and_the_request_carries_index_and_token_hash() {
    let reply = json!({
        "auth": {"id": "home-auth-1", "provider": "antigravity", "metadata": {"access_token": "new-access-token"}},
        "auth_index": "home-index-1",
    });
    let h = home_with_body(serde_json::to_vec(&reply).unwrap()).await;
    let mut auth = Auth::new("home-auth-1", "antigravity");
    auth.index = "home-index-1".into();
    auth.metadata.insert("access_token".into(), Value::String("old-access-token".into()));
    auth.metadata.insert("refresh_token".into(), Value::String("refresh-token".into()));

    let updated = refresh_auth_via_home(&enabled_config(), &auth).await.unwrap().unwrap();
    assert_eq!(updated.metadata.get("access_token"), Some(&Value::String("new-access-token".into())));
    assert_eq!(updated.index, "home-index-1");

    let gets: Vec<_> = h.mock.commands().into_iter().filter(|c| c[0].eq_ignore_ascii_case("GET")).collect();
    assert_eq!(gets.len(), 1);
    let request: Value = serde_json::from_str(&gets[0][1]).unwrap();
    assert_eq!(request["type"], "refresh");
    assert_eq!(request["auth_index"], "home-index-1");
    assert_eq!(request["access_token_sha256"], access_token_sha256(&auth));
}

#[tokio::test]
async fn bare_auth_object_is_accepted_and_gets_an_index() {
    let reply = json!({"id": "bare", "provider": "codex", "metadata": {"access_token": "t"}});
    let _h = home_with_body(serde_json::to_vec(&reply).unwrap()).await;
    let auth = Auth::new("bare", "codex");
    let updated = refresh_auth_via_home(&enabled_config(), &auth).await.unwrap().unwrap();
    assert_eq!(updated.id, "bare");
    assert_eq!(updated.index.len(), 16, "index derived from the auth: {}", updated.index);
}

#[tokio::test]
async fn disabled_or_invalid_payloads_are_rejected() {
    let _h = home_with_body(br#"{"id":"x","provider":"codex","disabled":true}"#.to_vec()).await;
    let err = refresh_auth_via_home(&enabled_config(), &codex_auth()).await.unwrap().unwrap_err();
    assert_eq!((err.status, err.message.as_str()), (401, "credential unauthorized"));
    drop(_h);

    let _h = home_with_body(br#"{"id":"x","provider":"codex","status":"disabled"}"#.to_vec()).await;
    let err = refresh_auth_via_home(&enabled_config(), &codex_auth()).await.unwrap().unwrap_err();
    assert_eq!(err.status, 401);
    drop(_h);

    let _h = home_with_body(b"[1,2]".to_vec()).await;
    let err = refresh_auth_via_home(&enabled_config(), &codex_auth()).await.unwrap().unwrap_err();
    assert_eq!((err.status, err.message.as_str()), (502, "home returned invalid auth payload"));
}

// Go: antigravity_executor_auth.go Refresh / ensureAccessToken: with Home enabled the credential
// is refreshed at Home, never locally.
fn antigravity_executor() -> cpa_runtime::executor::DynExecutor {
    cpa_executors::antigravity::new(tokio::sync::watch::channel(Arc::new(enabled_config())).1)
}

#[tokio::test]
async fn antigravity_refresh_goes_through_home() {
    let reply = json!({"auth": {"id": "ag", "provider": "antigravity", "metadata": {"access_token": "from-home"}}});
    let h = home_with_body(serde_json::to_vec(&reply).unwrap()).await;
    let mut auth = Auth::new("ag", "antigravity");
    auth.index = "ag".into();
    auth.metadata.insert("refresh_token".into(), Value::String("local-only".into()));
    let updated = antigravity_executor().refresh(&auth).await.unwrap();
    assert_eq!(updated.metadata.get("access_token"), Some(&Value::String("from-home".into())));
    assert_eq!(h.mock.commands().iter().filter(|c| c[0].eq_ignore_ascii_case("GET")).count(), 1);
}

#[tokio::test]
async fn antigravity_request_auth_preparation_rejects_a_home_refresh_without_token() {
    let reply = json!({"auth": {"id": "ag", "provider": "antigravity", "metadata": {}}});
    let _h = home_with_body(serde_json::to_vec(&reply).unwrap()).await;
    let mut auth = Auth::new("ag", "antigravity");
    auth.index = "ag".into();
    let err = antigravity_executor().prepare_request_auth(&auth).await.unwrap_err();
    assert_eq!((err.status, err.message.as_str()), (401, "missing access token"));
}

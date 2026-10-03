//! Home-mode glue over the real router against a mock Home: the heartbeat gate, model lists and
//! request-log forwarding. The Home client is process-global, so every test holds `LOCK`.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use cpa_auth::{FileTokenStore, OAuthSessions};
use cpa_config::{Config, HomeConfig};
use cpa_home::testing::{self as t, MockHome};
use cpa_home::{Client, kv};
use cpa_runtime::conductor::Manager;
use cpa_runtime::usage::UsageTracker;
use cpa_server::reqlog::RequestLogger;
use cpa_server::{AppState, build_router};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard, watch};
use tower::ServiceExt;

static LOCK: Mutex<()> = Mutex::const_new(());

const MODELS: &str = r#"{
    "claude": [{"id": "claude-sonnet-4-6", "created": 1771372800, "owned_by": "anthropic", "display_name": "Claude Sonnet", "context_length": 200000}],
    "gemini": [{"name": "models/gemini-3-pro", "displayName": "Gemini 3 Pro"}],
    "codex": [{"id": "gpt-5.5", "owned_by": "openai"}]
}"#;

/// Holds the global Home client slot for one test and clears it afterwards.
struct HomeSlot {
    _guard: MutexGuard<'static, ()>,
    client: Arc<Client>,
}

impl HomeSlot {
    async fn install(mock: &MockHome, heartbeat: bool) -> HomeSlot {
        let guard = LOCK.lock().await;
        let cfg = HomeConfig { enabled: true, host: "127.0.0.1".into(), port: i64::from(mock.port()) as _, ..Default::default() };
        let client = Arc::new(Client::new(cfg));
        client.set_test_operation_timeout(Duration::from_secs(2));
        client.set_heartbeat_ok_for_tests(heartbeat);
        kv::set_current(client.clone());
        HomeSlot { _guard: guard, client }
    }
}

impl Drop for HomeSlot {
    fn drop(&mut self) {
        kv::clear_current_if(&self.client);
    }
}

fn mock_home(models: &'static str) -> impl Fn(&[String]) -> t::Reply + Send + Sync + 'static {
    move |args| match args[0].to_uppercase().as_str() {
        "GET" => t::bulk(models),
        "RPUSH" => t::int(1),
        _ => t::err("ERR unexpected command"),
    }
}

struct App {
    router: Router,
    dir: tempfile::TempDir,
    /// Keeps the live-config channel open for the router's lifetime.
    _config: watch::Sender<Arc<Config>>,
}

fn app(edit: impl FnOnce(&mut Config)) -> App {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config { api_keys: vec!["k1".into()], auth_dir: dir.path().display().to_string(), ..Config::default() };
    cfg.home.enabled = true;
    edit(&mut cfg);
    let (config, rx) = watch::channel(Arc::new(cfg));
    let mut state = AppState::new(
        rx.clone(),
        Arc::new(Manager::default()),
        Arc::new(FileTokenStore::with_dir(dir.path())),
        Arc::new(OAuthSessions::default()),
        Arc::new(UsageTracker::default()),
    );
    state.request_logger = Some(Arc::new(RequestLogger::new(rx, None)));
    App { router: build_router(state), dir, _config: config }
}

async fn call(app: &App, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(path).header("x-api-key", "k1");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app.router.clone().oneshot(req.body(Body::from(body.to_string())).expect("request")).await.expect("infallible");
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.expect("body");
    (parts.status, parts.headers, String::from_utf8_lossy(&bytes).into_owned())
}

fn json_of(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("json body {body:?}: {e}"))
}

async fn wait_for_command(mock: &MockHome, name: &str, key: &str) -> Vec<String> {
    for _ in 0..400 {
        if let Some(cmd) = mock.commands().into_iter().find(|c| c[0].eq_ignore_ascii_case(name) && c.get(1).is_some_and(|k| k == key)) {
            return cmd;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no {name} {key} command arrived: {:?}", mock.commands());
}

// ------------------------------------------------------------------ heartbeat gate

#[tokio::test]
async fn heartbeat_gate_answers_503_until_home_is_healthy() {
    let mock = MockHome::start(mock_home(MODELS)).await;
    let slot = HomeSlot::install(&mock, false).await;
    let app = app(|_| {});

    let (status, headers, body) = call(&app, "GET", "/healthz", &[], "").await;
    assert_eq!((status, body.as_str()), (StatusCode::SERVICE_UNAVAILABLE, ""));
    assert_eq!(headers["access-control-allow-origin"], "*", "the gate sits behind CORS");
    let (status, _, _) = call(&app, "GET", "/v1/models", &[], "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // Management, plugin resources and the panel page are exempt (they 404 here, never 503).
    for path in ["/v0/management/config", "/v8/management", "/v0/resource/plugins/x", "/management.html"] {
        let (status, _, _) = call(&app, "GET", path, &[], "").await;
        assert_ne!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
    }

    slot.client.set_heartbeat_ok_for_tests(true);
    let (status, _, body) = call(&app, "GET", "/healthz", &[], "").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, r#"{"status":"ok"}"#));
}

#[tokio::test]
async fn heartbeat_gate_needs_a_home_client_and_is_off_without_home_mode() {
    let _guard = LOCK.lock().await;
    kv::clear_current();
    let gated = app(|_| {});
    let (status, _, _) = call(&gated, "GET", "/healthz", &[], "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "no client yet");

    let open = app(|cfg| cfg.home.enabled = false);
    let (status, _, _) = call(&open, "GET", "/healthz", &[], "").await;
    assert_eq!(status, StatusCode::OK);
}

// ------------------------------------------------------------------ model lists

#[tokio::test]
async fn openai_and_claude_lists_come_from_home() {
    let mock = MockHome::start(mock_home(MODELS)).await;
    let _slot = HomeSlot::install(&mock, true).await;
    let app = app(|_| {});

    let (status, headers, body) = call(&app, "GET", "/v1/models?foo=Bar", &[("x-extra", "v")], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json; charset=utf-8");
    assert_eq!(
        body,
        r#"{"data":[{"created":1771372800,"id":"claude-sonnet-4-6","object":"model","owned_by":"anthropic"},{"id":"gemini-3-pro","object":"model"},{"id":"gpt-5.5","object":"model","owned_by":"openai"}],"object":"list"}"#
    );
    // Home saw a lower-cased header/query snapshot of the client request.
    let sent = mock.commands().into_iter().find(|c| c[0].eq_ignore_ascii_case("GET")).expect("GET sent to home");
    let request: Value = serde_json::from_str(&sent[1]).expect("request json");
    assert_eq!(request["type"], "models");
    assert_eq!(request["headers"]["x-extra"], "v");
    assert_eq!(request["query"]["foo"], "Bar");

    let (status, _, body) = call(&app, "GET", "/v1/models", &[("anthropic-version", "2023-06-01")], "").await;
    assert_eq!(status, StatusCode::OK);
    let list = json_of(&body);
    assert_eq!(list["has_more"], false);
    let first = &list["data"][0];
    assert_eq!(first["type"], "model");
    assert_eq!(first["created_at"], "2026-02-18T00:00:00Z");
    assert_eq!(first["max_input_tokens"], 200000);
    assert_eq!(first["display_name"], "Claude Sonnet");
}

#[tokio::test]
async fn gemini_lists_and_single_lookup_come_from_home() {
    let mock = MockHome::start(mock_home(MODELS)).await;
    let _slot = HomeSlot::install(&mock, true).await;
    let app = app(|_| {});

    let (status, _, body) = call(&app, "GET", "/v1beta/models", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let models = json_of(&body)["models"].clone();
    assert_eq!(models.as_array().map(Vec::len), Some(3));
    assert_eq!(models[1], json!({"name": "models/gemini-3-pro", "displayName": "Gemini 3 Pro", "description": "Gemini 3 Pro", "supportedGenerationMethods": ["generateContent"]}));

    for path in ["/v1beta/models/gemini-3-pro", "/v1beta/models/models/gemini-3-pro"] {
        let (status, _, body) = call(&app, "GET", path, &[], "").await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(json_of(&body)["name"], "models/gemini-3-pro");
    }
    let (status, _, body) = call(&app, "GET", "/v1beta/models/nope", &[], "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, r#"{"error":{"message":"Not Found","type":"not_found"}}"#);
}

#[tokio::test]
async fn codex_client_catalog_is_built_from_home_ids() {
    let mock = MockHome::start(mock_home(MODELS)).await;
    let _slot = HomeSlot::install(&mock, true).await;
    let app = app(|cfg| cfg.client.codex.enable_apply_patch = true);

    let (status, headers, body) = call(&app, "GET", "/v1/models?client_version=0.153.4", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json; charset=utf-8");
    assert!(!body.contains('\n'), "catalog is compact JSON");
    let catalog = json_of(&body);
    let slugs: Vec<&str> = catalog["models"].as_array().expect("models").iter().filter_map(|m| m["slug"].as_str()).collect();
    for want in ["claude-sonnet-4-6", "gemini-3-pro", "gpt-5.5"] {
        assert!(slugs.contains(&want), "{want} missing from {slugs:?}");
    }
    // No executors are registered, so Home's routing evidence never advertises apply_patch.
    assert!(catalog["models"].as_array().expect("models").iter().all(|m| m["apply_patch_tool_type"].is_null()));
}

#[tokio::test]
async fn grok_shell_gets_the_grok_shape_from_home() {
    let mock = MockHome::start(mock_home(MODELS)).await;
    let _slot = HomeSlot::install(&mock, true).await;
    let app = app(|_| {});

    let (status, _, body) = call(&app, "GET", "/v1/models", &[("user-agent", "grok-shell/0.2.119 (macos; aarch64)")], "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with(r#"{"object":"list","data":[{"id":"claude-sonnet-4-6","model":"claude-sonnet-4-6","name":"Claude Sonnet","context_window":200000,"api_backend":"responses","supported_in_api":true}"#), "{body}");
}

#[tokio::test]
async fn model_list_failures_map_to_json_errors() {
    let app = app(|_| {});
    let mock = MockHome::start(|args| match args[0].to_uppercase().as_str() {
        "GET" => t::nil(),
        _ => t::ok(),
    })
    .await;
    let slot = HomeSlot::install(&mock, true).await;
    let (status, _, body) = call(&app, "GET", "/v1/models", &[], "").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body, r#"{"error":{"message":"home models not found","type":"server_error"}}"#);
    drop(slot);

    for (payload, want_status, want_message) in [
        (r#"{"error":{"type":"no_credentials","message":"Missing API key"}}"#, 401, "Missing API key"),
        (r#"{"error":{"type":"internal_error","message":"boom"}}"#, 502, "boom"),
    ] {
        let mock = MockHome::start(move |args| if args[0].eq_ignore_ascii_case("GET") { t::bulk(payload) } else { t::ok() }).await;
        let _slot = HomeSlot::install(&mock, true).await;
        let (status, _, body) = call(&app, "GET", "/v1/models", &[], "").await;
        assert_eq!(status.as_u16(), want_status);
        assert_eq!(body, format!(r#"{{"error":{{"message":"{want_message}","type":"authentication_error"}}}}"#));
    }

    let mock = MockHome::start(|args| if args[0].eq_ignore_ascii_case("GET") { t::bulk("{}") } else { t::ok() }).await;
    let _slot = HomeSlot::install(&mock, true).await;
    let (status, _, body) = call(&app, "GET", "/v1/models", &[], "").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body, r#"{"error":{"message":"home models payload has no sections","type":"server_error"}}"#);
}

// ------------------------------------------------------------------ request logs

#[tokio::test]
async fn request_logs_are_pushed_to_home_instead_of_files() {
    let mock = MockHome::start(mock_home(MODELS)).await;
    let _slot = HomeSlot::install(&mock, true).await;
    let app = app(|cfg| cfg.request_log = true);

    let (status, _, _) = call(&app, "POST", "/v1/chat/completions", &[("content-type", "application/json")], r#"{"model":"nope","messages":[]}"#).await;
    assert!(status.is_client_error(), "{status}");

    let cmd = wait_for_command(&mock, "RPUSH", "request-log").await;
    let payload = json_of(&cmd[2]);
    assert_eq!(payload["headers"]["X-Api-Key"][0], "k1", "raw request headers are forwarded");
    assert_eq!(payload["headers"]["Content-Type"][0], "application/json");
    let request_id = payload["request_id"].as_str().expect("request id");
    assert_eq!(request_id.len(), 36, "full UUID, not the 8 character short id");
    let log = payload["request_log"].as_str().expect("request log");
    assert!(log.starts_with("=== REQUEST INFO ===\n"), "{log}");
    assert!(log.contains("URL: /v1/chat/completions") && log.contains(r#"{"model":"nope","messages":[]}"#), "{log}");
    assert!(log.contains("=== RESPONSE ===\nStatus: "), "{log}");
    // The Authorization-style headers are masked in the log text but raw in `headers`.
    assert!(log.contains("X-Api-Key: "), "{log}");

    let logs_dir = app.dir.path().join("logs");
    assert!(!logs_dir.exists() || std::fs::read_dir(&logs_dir).map(|d| d.count()).unwrap_or(0) == 0, "no local log files in Home mode");
}

#[tokio::test]
async fn forced_error_logs_stay_local_when_request_log_is_off() {
    let mock = MockHome::start(mock_home(MODELS)).await;
    let _slot = HomeSlot::install(&mock, true).await;
    let app = app(|cfg| cfg.request_log = false);

    let (status, _, _) = call(&app, "POST", "/v1/chat/completions", &[("content-type", "application/json")], r#"{"model":"nope","messages":[]}"#).await;
    assert!(status.is_client_error(), "{status}");

    let logs_dir = app.dir.path().join("logs");
    let mut found = false;
    for _ in 0..400 {
        if let Ok(entries) = std::fs::read_dir(&logs_dir) {
            found = entries.flatten().any(|e| e.file_name().to_string_lossy().starts_with("error-"));
        }
        if found {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(found, "expected a local error-*.log in {}", logs_dir.display());
    assert_eq!(mock.count("RPUSH", Some("request-log")), 0, "forced error logs are not forwarded to Home");
}

#[tokio::test]
async fn request_logs_are_dropped_without_a_healthy_home() {
    let mock = MockHome::start(mock_home(MODELS)).await;
    let slot = HomeSlot::install(&mock, true).await;
    let app = app(|cfg| cfg.request_log = true);
    let (status, _, _) = call(&app, "POST", "/v1/chat/completions", &[], r#"{"model":"nope","messages":[]}"#).await;
    assert!(status.is_client_error(), "{status}");
    wait_for_command(&mock, "RPUSH", "request-log").await;

    // The request logger runs outside the gate, so the 503 below is logged too, but not pushed.
    slot.client.set_heartbeat_ok_for_tests(false);
    let (status, _, _) = call(&app, "POST", "/v1/chat/completions", &[], r#"{"model":"nope","messages":[]}"#).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(mock.count("RPUSH", Some("request-log")), 1, "no second log while Home is unhealthy");
}

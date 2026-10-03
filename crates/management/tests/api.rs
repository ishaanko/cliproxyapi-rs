//! Focused HTTP tests of the management router (tower `oneshot`): access rules, the config tree,
//! credential files, usage feeds and OAuth session endpoints. Credential tests substitute an
//! in-memory registry for the conductor.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use cpa_auth::{Auth, FileTokenStore, OAuthSessions};
use cpa_management::{AuthRegistry, ManagementState, oauth_redirect_router, router};
use cpa_runtime::conductor::Manager;
use cpa_runtime::usage::{TokenUsage, UsageFailure, UsageRecord, UsageTracker};
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::watch;
use tower::ServiceExt;

/// Serializes tests that touch the process-wide usage queue and usage-statistics flag.
static USAGE_GLOBALS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const LOCAL: &str = "127.0.0.1:5000";
const REMOTE: &str = "203.0.113.5:5000";

const BASE_CONFIG: &str = "\
# Test configuration
config-version: 8

# Management API settings
management:
  allow-remote: false
  secret-key: test-secret

access:
  # client keys
  api-keys:
    - k1

observability:
  logs:
    logging-to-file: true
  usage:
    usage-statistics-enabled: true
";

#[derive(Default)]
struct FakeRegistry {
    auths: Mutex<BTreeMap<String, Auth>>,
}

#[async_trait::async_trait]
impl AuthRegistry for FakeRegistry {
    fn list(&self) -> Vec<Auth> {
        self.auths.lock().values().cloned().collect()
    }

    fn get(&self, id: &str) -> Option<Auth> {
        self.auths.lock().get(id).cloned()
    }

    async fn update(&self, auth: Auth) -> Result<Auth, String> {
        self.auths.lock().insert(auth.id.clone(), auth.clone());
        Ok(auth)
    }

    async fn remove(&self, id: &str) {
        self.auths.lock().remove(id);
    }

    async fn force_refresh_auth(&self, id: &str) -> Result<Auth, String> {
        self.get(id).ok_or_else(|| format!("auth not found: {id}"))
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    app: Router,
    registry: Arc<FakeRegistry>,
    usage: Arc<UsageTracker>,
    sessions: Arc<OAuthSessions>,
    config_path: PathBuf,
    auth_dir: PathBuf,
    log_dir: PathBuf,
}

fn harness_with(config: &str, env_secret: Option<&str>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.yaml");
    let auth_dir = dir.path().join("auths");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&auth_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::write(&config_path, config).unwrap();
    // Loading hashes the plaintext secret in the file, like the real startup.
    let cfg = cpa_config::load_config(&config_path).unwrap();
    let (tx, rx) = watch::channel(Arc::new(cfg));

    let registry = Arc::new(FakeRegistry::default());
    let store = Arc::new(FileTokenStore::with_dir(&auth_dir));
    let sessions = Arc::new(OAuthSessions::default());
    let login = cpa_auth::Manager::new(store.clone()).with_sessions(sessions.clone());
    let usage = Arc::new(UsageTracker::new());

    let reload_path = config_path.clone();
    let hook: cpa_management::ReloadHook = Arc::new(move || {
        let (tx, path) = (tx.clone(), reload_path.clone());
        Box::pin(async move {
            if let Ok(cfg) = cpa_config::load_config(&path) {
                tx.send_replace(Arc::new(cfg));
            }
        })
    });
    let state = ManagementState::new(
        &config_path,
        rx,
        Arc::new(Manager::default()),
        store,
        sessions.clone(),
        login,
        usage.clone(),
        &log_dir,
    )
    .with_env_secret(env_secret.map(str::to_string))
    .with_registry(registry.clone())
    .with_reload_hook(hook);
    let app = router(state.clone()).merge(oauth_redirect_router(state));
    Harness {
        _dir: dir,
        app,
        registry,
        usage,
        sessions,
        config_path,
        auth_dir,
        log_dir,
    }
}

fn harness() -> Harness {
    harness_with(BASE_CONFIG, None)
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not json ({e}): {}", String::from_utf8_lossy(&self.body)))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    peer: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut req = req.body(Body::from(body)).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes().to_vec(),
    }
}

fn bearer() -> [(&'static str, &'static str); 1] {
    [("authorization", "Bearer test-secret")]
}

async fn get(app: &Router, uri: &str) -> Reply {
    send(app, Method::GET, uri, LOCAL, &bearer(), Vec::new()).await
}

async fn with_json(app: &Router, method: Method, uri: &str, body: Value) -> Reply {
    let headers = [
        ("authorization", "Bearer test-secret"),
        ("content-type", "application/json"),
    ];
    send(
        app,
        method,
        uri,
        LOCAL,
        &headers,
        serde_json::to_vec(&body).unwrap(),
    )
    .await
}

fn record(model: &str, failed: bool) -> UsageRecord {
    UsageRecord {
        timestamp: chrono::Utc::now(),
        latency_ms: 5,
        ttft_ms: 0,
        source: "a@b.c".into(),
        auth_index: "idx".into(),
        auth_type: "oauth".into(),
        provider: "codex".into(),
        executor_type: "codex".into(),
        model: model.into(),
        alias: String::new(),
        endpoint: "POST /v1/responses".into(),
        api_key: "sk-1".into(),
        request_id: "req".into(),
        failed,
        stream: false,
        fail: UsageFailure::default(),
        extra: Default::default(),
        tokens: TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            total_tokens: 2,
            ..Default::default()
        },
    }
}

// ---- access rules ----

#[tokio::test]
async fn management_is_a_bare_404_until_a_secret_exists() {
    let h = harness_with("config-version: 8\n", None);
    let r = get(&h.app, "/v8/management/credentials").await;
    assert_eq!(r.status, 404);
    assert!(r.body.is_empty());
}

#[tokio::test]
async fn key_rules_for_local_and_remote_clients() {
    let h = harness();
    let uri = "/v8/management/credentials";

    let r = send(&h.app, Method::GET, uri, LOCAL, &[], Vec::new()).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (401, json!("missing management key"))
    );
    assert!(
        r.headers.contains_key("x-cpa-version"),
        "X-CPA headers are set on failures too"
    );

    let r = send(
        &h.app,
        Method::GET,
        uri,
        LOCAL,
        &[("authorization", "Bearer nope")],
        Vec::new(),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (401, json!("invalid management key"))
    );

    // Bearer, X-Management-Key and a bare Authorization value all carry the key.
    for headers in [
        vec![("authorization", "Bearer test-secret")],
        vec![("x-management-key", "test-secret")],
        vec![("authorization", "test-secret")],
    ] {
        let r = send(&h.app, Method::GET, uri, LOCAL, &headers, Vec::new()).await;
        assert_eq!(r.status, 200, "{headers:?}");
    }

    // Remote access is off by default and refused before any key is looked at.
    let r = send(&h.app, Method::GET, uri, REMOTE, &bearer(), Vec::new()).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (403, json!("remote management disabled"))
    );
}

#[tokio::test]
async fn remote_access_follows_allow_remote_and_the_env_secret() {
    let allowed = BASE_CONFIG.replace("allow-remote: false", "allow-remote: true");
    let h = harness_with(&allowed, None);
    let r = send(
        &h.app,
        Method::GET,
        "/v8/management/credentials",
        REMOTE,
        &bearer(),
        Vec::new(),
    )
    .await;
    assert_eq!(r.status, 200);

    // MANAGEMENT_PASSWORD works in plaintext and forces remote access on.
    let h = harness_with(BASE_CONFIG, Some("env-pass"));
    let r = send(
        &h.app,
        Method::GET,
        "/v8/management/credentials",
        REMOTE,
        &[("authorization", "Bearer env-pass")],
        Vec::new(),
    )
    .await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn five_failures_ban_the_ip_even_for_the_right_key() {
    let h = harness();
    let uri = "/v8/management/credentials";
    for _ in 0..5 {
        let r = send(
            &h.app,
            Method::GET,
            uri,
            LOCAL,
            &[("authorization", "Bearer wrong")],
            Vec::new(),
        )
        .await;
        assert_eq!(r.status, 401);
    }
    let r = send(&h.app, Method::GET, uri, LOCAL, &bearer(), Vec::new()).await;
    assert_eq!(r.status, 403);
    let msg = r.json()["error"].as_str().unwrap().to_string();
    assert!(
        msg.starts_with("IP banned due to too many failed attempts. Try again in 30m"),
        "{msg}"
    );
    // Another IP is unaffected.
    let r = send(&h.app, Method::GET, uri, "[::1]:1", &[], Vec::new()).await;
    assert_eq!(r.status, 401);
}

#[tokio::test]
async fn cors_preflight_is_204_and_oauth_callback_needs_no_key() {
    let h = harness();
    let r = send(
        &h.app,
        Method::OPTIONS,
        "/v8/management/config",
        REMOTE,
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(r.status, 204);
    assert_eq!(r.headers["access-control-allow-origin"], "*");

    // No key, remote peer: the callback endpoint only needs a pending state.
    let r = send(
        &h.app,
        Method::GET,
        "/v8/management/oauth/callback?state=nope&code=c",
        REMOTE,
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (404, json!("unknown or expired state"))
    );
}

// ---- config tree ----

#[tokio::test]
async fn config_tree_round_trip_keeps_comments_and_validates() {
    let h = harness();
    let app = &h.app;

    let r = get(app, "/v8/management/config/access/api-keys").await;
    assert_eq!((r.status.as_u16(), r.json()), (200, json!(["k1"])));
    assert_eq!(r.headers["cache-control"], "no-store");
    assert_eq!(
        get(app, "/v8/management/config/access/missing")
            .await
            .status,
        404
    );

    // api-keys CRUD: add, remove, delete the whole list.
    let r = with_json(
        app,
        Method::PUT,
        "/v8/management/config/access/api-keys",
        json!(["k1", "k2", "k3"]),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (200, json!({"status": "ok", "config-version": 8}))
    );
    assert_eq!(
        get(app, "/v8/management/config/access/api-keys")
            .await
            .json(),
        json!(["k1", "k2", "k3"])
    );
    with_json(
        app,
        Method::PUT,
        "/v8/management/config/access/api-keys",
        json!(["k1", "k3"]),
    )
    .await;
    assert_eq!(
        get(app, "/v8/management/config/access/api-keys")
            .await
            .json(),
        json!(["k1", "k3"])
    );

    let on_disk = std::fs::read_to_string(&h.config_path).unwrap();
    assert!(
        on_disk.contains("# Test configuration") && on_disk.contains("# client keys"),
        "{on_disk}"
    );
    assert!(on_disk.contains("# Management API settings"), "{on_disk}");

    // The live config follows the write (reload hook), so the new key is what the file holds.
    let r = send(
        app,
        Method::DELETE,
        "/v8/management/config/access/api-keys",
        LOCAL,
        &bearer(),
        Vec::new(),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        get(app, "/v8/management/config/access/api-keys")
            .await
            .status,
        404
    );
    assert_eq!(
        get(app, "/v8/management/config/access").await.status,
        404,
        "emptied parent is pruned"
    );

    // PATCH merges maps; PUT replaces; errors map to the documented codes.
    with_json(
        app,
        Method::PATCH,
        "/v8/management/config/observability/logs",
        json!({"debug": true}),
    )
    .await;
    let logs = get(app, "/v8/management/config/observability/logs")
        .await
        .json();
    assert_eq!(
        (logs["debug"].clone(), logs["logging-to-file"].clone()),
        (json!(true), json!(true))
    );

    let r = send(
        app,
        Method::PUT,
        "/v8/management/config/access",
        LOCAL,
        &bearer(),
        b"{not json".to_vec(),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("invalid_json"))
    );
    let r = with_json(app, Method::PUT, "/v8/management/config", json!(["x"])).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("config_must_be_object"))
    );
    let r = with_json(
        app,
        Method::PUT,
        "/v8/management/config/management/secret-key/x",
        json!(1),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("invalid_path"))
    );
    let r = send(
        app,
        Method::DELETE,
        "/v8/management/config/",
        LOCAL,
        &bearer(),
        Vec::new(),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("cannot_delete_config"))
    );
    let r = with_json(
        app,
        Method::PUT,
        "/v8/management/config/credentials/concurrency",
        json!({"lifecycle-config-revision": 9}),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("read_only_field"))
    );
    let r = with_json(
        app,
        Method::PUT,
        "/v8/management/config/bogus-section",
        json!({"a": 1}),
    )
    .await;
    assert_eq!(
        r.status,
        400,
        "unknown v8 sections are rejected: {}",
        r.text()
    );
}

#[tokio::test]
async fn config_yaml_reads_the_normalized_document_and_put_replaces_it() {
    let h = harness();
    let app = &h.app;
    let r = get(app, "/v8/management/config.yaml").await;
    assert_eq!(r.status, 200);
    assert!(
        r.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/yaml")
    );
    let yaml = r.text();
    assert!(yaml.contains("config-version: 8") && yaml.contains("# client keys"));

    let edited = yaml.replace("- k1", "- k1\n        - from-yaml");
    let r = send(
        app,
        Method::PUT,
        "/v8/management/config.yaml",
        LOCAL,
        &[
            ("authorization", "Bearer test-secret"),
            ("content-type", "application/yaml"),
        ],
        edited.into_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        get(app, "/v8/management/config/access/api-keys")
            .await
            .json(),
        json!(["k1", "from-yaml"])
    );
    assert!(
        std::fs::read_to_string(&h.config_path)
            .unwrap()
            .contains("# client keys")
    );

    // Invalid documents are rejected before the file is touched.
    let before = std::fs::read_to_string(&h.config_path).unwrap();
    let r = send(
        app,
        Method::PUT,
        "/v8/management/config.yaml",
        LOCAL,
        &bearer(),
        b"config-version: 8\nunknown-root: 1\n".to_vec(),
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(std::fs::read_to_string(&h.config_path).unwrap(), before);
}

// ---- credentials ----

fn multipart(files: &[(&str, &str, &str)]) -> (String, Vec<u8>) {
    let boundary = "XBOUNDARY";
    let mut body = String::new();
    for (field, filename, content) in files {
        body.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: application/json\r\n\r\n{content}\r\n"));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    (
        format!("multipart/form-data; boundary={boundary}"),
        body.into_bytes(),
    )
}

async fn upload(h: &Harness, files: &[(&str, &str, &str)]) -> Reply {
    let (ct, body) = multipart(files);
    send(
        &h.app,
        Method::POST,
        "/v8/management/credentials",
        LOCAL,
        &[
            ("authorization", "Bearer test-secret"),
            ("content-type", &ct),
        ],
        body,
    )
    .await
}

#[tokio::test]
async fn credential_upload_list_download_and_delete_against_a_temp_auth_dir() {
    let h = harness();
    let claude = r#"{"type":"claude","email":"a@example.com","access_token":"at","refresh_token":"rt","note":"mine"}"#;

    let r = upload(&h, &[("file", "claude-a@example.com.json", claude)]).await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (200, json!({"status": "ok"}))
    );
    let path = h.auth_dir.join("claude-a@example.com.json");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), claude);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(
        h.registry.get("claude-a@example.com.json").is_some(),
        "registered with the conductor"
    );

    // Listing: file-backed entry with identity, counters and operator fields.
    let list = get(&h.app, "/v8/management/credentials").await.json();
    let files = list["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    let f = &files[0];
    assert_eq!(f["name"], "claude-a@example.com.json");
    assert_eq!(
        (
            f["provider"].clone(),
            f["source"].clone(),
            f["email"].clone()
        ),
        (json!("claude"), json!("file"), json!("a@example.com"))
    );
    assert_eq!(
        (f["note"].clone(), f["disabled"].clone(), f["size"].clone()),
        (json!("mine"), json!(false), json!(claude.len()))
    );
    assert!(f["auth_index"].as_str().is_some_and(|s| s.len() == 16));
    assert!(f["cooldowns"].as_array().is_some_and(Vec::is_empty));
    assert_eq!(f["recent_requests"].as_array().unwrap().len(), 20);
    assert!(list["observed_at"].is_string());

    // Pagination envelope.
    let page = get(&h.app, "/v8/management/credentials?page=1&page_size=1")
        .await
        .json();
    assert_eq!(
        (
            page["total"].clone(),
            page["page"].clone(),
            page["has_more"].clone()
        ),
        (json!(1), json!(1), json!(false))
    );
    assert_eq!(
        get(&h.app, "/v8/management/credentials?page=0")
            .await
            .status,
        400
    );

    // Download.
    let r = get(
        &h.app,
        "/v8/management/credentials/download?name=claude-a@example.com.json",
    )
    .await;
    assert_eq!(r.text(), claude);
    assert!(
        r.headers["content-disposition"]
            .to_str()
            .unwrap()
            .contains("claude-a@example.com.json")
    );
    assert_eq!(
        get(&h.app, "/v8/management/credentials/download?name=../x.json")
            .await
            .status,
        400
    );
    assert_eq!(
        get(&h.app, "/v8/management/credentials/download?name=x.txt")
            .await
            .status,
        400
    );
    assert_eq!(
        get(
            &h.app,
            "/v8/management/credentials/download?name=missing.json"
        )
        .await
        .status,
        404
    );

    // Raw JSON upload with ?name=.
    let r = send(
        &h.app,
        Method::POST,
        "/v8/management/credentials?name=codex-b.json",
        LOCAL,
        &bearer(),
        br#"{"type":"codex","email":"b@example.com","access_token":"x"}"#.to_vec(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(h.registry.list().len(), 2);

    // Invalid payloads: nothing is written.
    let r = send(
        &h.app,
        Method::POST,
        "/v8/management/credentials?name=bad.json",
        LOCAL,
        &bearer(),
        b"not json".to_vec(),
    )
    .await;
    assert_eq!(r.status, 500);
    assert!(!h.auth_dir.join("bad.json").exists());
    let r = upload(&h, &[("file", "notes.txt", "{}")]).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("file must be .json"))
    );

    // Several files: partial success is a 207 listing the failures.
    let r = upload(
        &h,
        &[
            ("a", "ok1.json", r#"{"type":"kimi","access_token":"1"}"#),
            ("b", "ok2.txt", "{}"),
        ],
    )
    .await;
    let body = r.json();
    assert_eq!(
        (
            r.status.as_u16(),
            body["status"].clone(),
            body["uploaded"].clone()
        ),
        (207, json!("partial"), json!(1))
    );
    assert_eq!(body["failed"][0]["name"], "ok2.txt");

    // Delete: one name, then several with one missing, then all.
    let r = send(
        &h.app,
        Method::DELETE,
        "/v8/management/credentials?name=claude-a@example.com.json",
        LOCAL,
        &bearer(),
        Vec::new(),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (200, json!({"status": "ok"}))
    );
    assert!(!path.exists() && h.registry.get("claude-a@example.com.json").is_none());
    let r = send(
        &h.app,
        Method::DELETE,
        "/v8/management/credentials",
        LOCAL,
        &bearer(),
        br#"{"names":["codex-b.json","ghost.json"]}"#.to_vec(),
    )
    .await;
    let body = r.json();
    assert_eq!(
        (
            r.status.as_u16(),
            body["status"].clone(),
            body["deleted"].clone()
        ),
        (207, json!("partial"), json!(1))
    );
    assert_eq!(body["failed"][0]["error"], "auth file not found");
    let r = send(
        &h.app,
        Method::DELETE,
        "/v8/management/credentials?all=true",
        LOCAL,
        &bearer(),
        Vec::new(),
    )
    .await;
    assert_eq!(r.json()["deleted"], 1);
    assert!(std::fs::read_dir(&h.auth_dir).unwrap().next().is_none());
    assert_eq!(
        send(
            &h.app,
            Method::DELETE,
            "/v8/management/credentials",
            LOCAL,
            &bearer(),
            Vec::new()
        )
        .await
        .status,
        400
    );
}

#[tokio::test]
async fn credential_status_and_field_patches_update_the_registry() {
    let h = harness();
    upload(
        &h,
        &[(
            "file",
            "codex-c.json",
            r#"{"type":"codex","email":"c@example.com","access_token":"x"}"#,
        )],
    )
    .await;

    let r = with_json(
        &h.app,
        Method::PATCH,
        "/v8/management/credentials/status",
        json!({"name": "codex-c.json", "disabled": true}),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (200, json!({"status": "ok", "disabled": true}))
    );
    let a = h.registry.get("codex-c.json").unwrap();
    assert!(a.disabled && a.metadata["disabled"] == json!(true));
    let r = with_json(
        &h.app,
        Method::PATCH,
        "/v8/management/credentials/status",
        json!({"name": "ghost.json", "disabled": true}),
    )
    .await;
    assert_eq!(r.status, 404);
    let r = with_json(
        &h.app,
        Method::PATCH,
        "/v8/management/credentials/status",
        json!({"name": "codex-c.json"}),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("disabled is required"))
    );

    let r = with_json(&h.app, Method::PATCH, "/v8/management/credentials/fields", json!({"name": "codex-c.json", "note": "n", "priority": 3, "weight": 5, "headers": {"X-A": "1"}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let a = h.registry.get("codex-c.json").unwrap();
    assert_eq!(a.metadata["note"], "n");
    assert_eq!(
        (a.attr("priority"), a.attr("weight"), a.attr("header:X-A")),
        ("3".into(), "5".into(), "1".into())
    );
    let r = with_json(
        &h.app,
        Method::PATCH,
        "/v8/management/credentials/fields",
        json!({"name": "codex-c.json", "weight": "5"}),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("weight must be an integer"))
    );
    let r = with_json(
        &h.app,
        Method::PATCH,
        "/v8/management/credentials/fields",
        json!({"name": "codex-c.json"}),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("no fields to update"))
    );
}

// ---- usage ----

#[tokio::test]
async fn usage_requests_feed_pages_forward_and_honors_the_statistics_gate() {
    let _globals = USAGE_GLOBALS.lock().await;
    let h = harness();
    for i in 0..5 {
        h.usage.record(record(&format!("m{i}"), i == 2));
    }
    let seqs = |r: &Reply| {
        r.json()["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };

    let r = get(&h.app, "/v8/management/observability/requests?limit=2").await;
    assert_eq!(
        (
            seqs(&r),
            r.json()["has_more"].clone(),
            r.json()["seq"].clone(),
            r.json()["capacity"].clone()
        ),
        (vec![4, 5], json!(false), json!(5), json!(1000))
    );
    assert_eq!(r.headers["cache-control"], "no-store");

    let r = get(
        &h.app,
        "/v8/management/observability/requests?after=0&limit=2",
    )
    .await;
    assert_eq!(
        (seqs(&r), r.json()["has_more"].clone()),
        (vec![1, 2], json!(true))
    );
    let r = get(
        &h.app,
        "/v8/management/observability/requests?after=4&limit=2",
    )
    .await;
    assert_eq!(
        (seqs(&r), r.json()["has_more"].clone()),
        (vec![5], json!(false))
    );

    for bad in ["-1", "1.5", "abc"] {
        let r = get(
            &h.app,
            &format!("/v8/management/observability/requests?after={bad}"),
        )
        .await;
        assert_eq!(
            (r.status.as_u16(), r.json()["error"].clone()),
            (400, json!("invalid_after")),
            "after={bad}"
        );
    }
    // Non-numeric limit falls back to the default of 100.
    assert_eq!(
        seqs(&get(&h.app, "/v8/management/observability/requests?limit=abc").await).len(),
        5
    );

    let e = &get(&h.app, "/v8/management/observability/requests?limit=1")
        .await
        .json()["events"][0];
    assert_eq!(e["model"], "m4");
    assert_eq!(
        e["tokens"],
        json!({"input_tokens": 1, "output_tokens": 1, "reasoning_tokens": 0, "cached_tokens": 0, "total_tokens": 2})
    );
    assert!(e["timestamp"].as_str().unwrap().ends_with('Z') && e["fail"]["status_code"] == 0);

    let s = get(&h.app, "/v8/management/observability/usage/summary")
        .await
        .json();
    assert_eq!(
        (
            s["totals"]["requests"].clone(),
            s["totals"]["failed"].clone()
        ),
        (json!(5), json!(1))
    );
    assert_eq!(s["hourly"].as_array().unwrap().len(), 24);
    assert_eq!(s["credentials"][0]["auth_index"], "idx");

    // Switching the flag off through the config API closes both endpoints.
    with_json(
        &h.app,
        Method::PUT,
        "/v8/management/config/observability/usage/usage-statistics-enabled",
        json!(false),
    )
    .await;
    for path in ["requests", "usage/summary"] {
        let r = get(&h.app, &format!("/v8/management/observability/{path}")).await;
        assert_eq!(
            (r.status.as_u16(), r.json()["error"].clone()),
            (404, json!("usage_statistics_disabled")),
            "{path}"
        );
    }
    // With statistics off nothing is queued.
    assert_eq!(
        get(&h.app, "/v8/management/observability/usage/queue")
            .await
            .json(),
        json!([])
    );
}

// ---- logs ----

#[tokio::test]
async fn logs_tail_then_resume_from_the_cursor_and_clear() {
    let h = harness();
    std::fs::write(
        h.log_dir.join("main.log"),
        "2026-01-01 10:00:00 one\n2026-01-01 10:00:01 two\n2026-01-01 10:00:02 three\n",
    )
    .unwrap();
    std::fs::write(h.log_dir.join("main.log.1"), "2026-01-01 09:00:00 older\n").unwrap();

    let r = get(&h.app, "/v8/management/observability/logs?limit=2")
        .await
        .json();
    assert_eq!(
        r["lines"],
        json!(["2026-01-01 10:00:01 two", "2026-01-01 10:00:02 three"])
    );
    let cursor = r["next-cursor"].as_str().unwrap().to_string();
    assert!(!cursor.is_empty());

    let r = get(
        &h.app,
        &format!("/v8/management/observability/logs?cursor={cursor}"),
    )
    .await
    .json();
    assert_eq!(
        (r["lines"].clone(), r["next-cursor"].clone()),
        (json!([]), json!(cursor))
    );

    let mut main = std::fs::OpenOptions::new()
        .append(true)
        .open(h.log_dir.join("main.log"))
        .unwrap();
    std::io::Write::write_all(&mut main, b"2026-01-01 10:00:03 four\n").unwrap();
    let r = get(
        &h.app,
        &format!("/v8/management/observability/logs?cursor={cursor}"),
    )
    .await
    .json();
    assert_eq!(r["lines"], json!(["2026-01-01 10:00:03 four"]));

    // A garbage cursor resets to the tail.
    let r = get(
        &h.app,
        "/v8/management/observability/logs?cursor=garbage&limit=1",
    )
    .await
    .json();
    assert_eq!(
        (r["cursor-reset"].clone(), r["lines"].clone()),
        (json!(true), json!(["2026-01-01 10:00:03 four"]))
    );
    assert_eq!(
        get(&h.app, "/v8/management/observability/logs?limit=0")
            .await
            .status,
        400
    );

    let r = send(
        &h.app,
        Method::DELETE,
        "/v8/management/observability/logs",
        LOCAL,
        &bearer(),
        Vec::new(),
    )
    .await;
    assert_eq!(r.json()["removed"], 1);
    assert_eq!(
        std::fs::metadata(h.log_dir.join("main.log")).unwrap().len(),
        0
    );
    assert!(!h.log_dir.join("main.log.1").exists());
}

#[tokio::test]
async fn request_logs_are_found_by_short_id_and_error_logs_listed() {
    let h = harness();
    std::fs::write(
        h.log_dir.join("v1-chat-2026-01-02T030405-abcd1234.log"),
        "request body",
    )
    .unwrap();
    std::fs::write(
        h.log_dir
            .join("error-v1-chat-2026-01-02T030405-ffff0000.log"),
        "boom",
    )
    .unwrap();

    let r = get(
        &h.app,
        "/v8/management/observability/logs/requests/xxxxabcd1234",
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.text()),
        (200, "request body".to_string())
    );
    assert_eq!(
        get(
            &h.app,
            "/v8/management/observability/logs/requests/unknown1"
        )
        .await
        .status,
        404
    );

    let r = get(&h.app, "/v8/management/observability/logs/errors")
        .await
        .json();
    assert_eq!(
        r["files"][0]["name"],
        "error-v1-chat-2026-01-02T030405-ffff0000.log"
    );
    let r = get(
        &h.app,
        "/v8/management/observability/logs/errors/error-v1-chat-2026-01-02T030405-ffff0000.log",
    )
    .await;
    assert_eq!(r.text(), "boom");
    assert_eq!(
        get(&h.app, "/v8/management/observability/logs/errors/notes.log")
            .await
            .status,
        404
    );
}

// ---- oauth sessions ----

#[tokio::test]
async fn oauth_status_cancel_and_callback_follow_the_session_registry() {
    let h = harness();
    let r = get(&h.app, "/v8/management/oauth/auth-url").await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("provider is required"))
    );
    let r = get(&h.app, "/v8/management/oauth/auth-url?provider=nope").await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (404, json!("provider_not_found"))
    );

    h.sessions.register("state-1", "codex");
    assert_eq!(
        get(&h.app, "/v8/management/oauth/status?state=state-1")
            .await
            .json(),
        json!({"status": "wait"})
    );
    assert_eq!(
        get(&h.app, "/v8/management/oauth/status?state=nope")
            .await
            .json()["error"],
        "unknown or expired state"
    );
    assert_eq!(
        get(&h.app, "/v8/management/oauth/status?state=bad%2Fstate")
            .await
            .status,
        400
    );

    // A pasted redirect URL reaches the session through the callback file.
    let r = with_json(&h.app, Method::POST, "/v8/management/oauth/callback", json!({"state": "state-1", "redirect_url": "http://localhost:1455/auth/callback?code=abc&state=state-1"})).await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (200, json!({"status": "ok"}))
    );
    assert!(h.auth_dir.join(".oauth-codex-state-1.oauth").exists());

    // Provider redirects land on the main-port routes without a key and always show the page.
    h.sessions.register("state-2", "anthropic");
    let r = send(
        &h.app,
        Method::GET,
        "/anthropic/callback?state=state-2&code=zzz",
        REMOTE,
        &[],
        Vec::new(),
    )
    .await;
    assert!(r.status == 200 && r.text().contains("Authentication successful"));
    assert!(h.auth_dir.join(".oauth-anthropic-state-2.oauth").exists());
    let r = send(
        &h.app,
        Method::GET,
        "/callback?state=state-2",
        REMOTE,
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("code or error is required"))
    );
    let r = send(
        &h.app,
        Method::GET,
        "/devin/callback?state=unknown&code=c",
        REMOTE,
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("invalid or expired OAuth callback"))
    );

    let r = send(
        &h.app,
        Method::DELETE,
        "/v8/management/oauth/session?state=state-1",
        LOCAL,
        &bearer(),
        Vec::new(),
    )
    .await;
    assert_eq!(r.json(), json!({"status": "ok", "cancelled": true}));
    assert_eq!(
        send(
            &h.app,
            Method::DELETE,
            "/v8/management/oauth/session",
            LOCAL,
            &bearer(),
            Vec::new()
        )
        .await
        .status,
        400
    );
}

#[tokio::test]
async fn non_ascii_keys_authenticate_and_missing_peer_info_fails_closed() {
    let h = harness_with(BASE_CONFIG, Some("pässwörd"));
    let mut req = Request::builder()
        .uri("/v8/management/credentials")
        .header(
            "authorization",
            axum::http::HeaderValue::from_bytes("Bearer pässwörd".as_bytes()).unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo("127.0.0.1:1".parse::<SocketAddr>().unwrap()));
    assert_eq!(h.app.clone().oneshot(req).await.unwrap().status(), 200);

    // Without connection info the ban list and the localhost rule cannot work: refuse.
    let req = Request::builder()
        .uri("/v8/management/credentials")
        .header("authorization", "Bearer anything")
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.app.clone().oneshot(req).await.unwrap().status(), 500);
}

// ---- v0 tree ----

#[tokio::test]
async fn v0_settings_and_key_lists_persist_through_the_legacy_layout() {
    let h = harness();
    let app = &h.app;
    let r = with_json(
        app,
        Method::PUT,
        "/v0/management/request-retry",
        json!({"value": 4}),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (200, json!({"status": "ok"}))
    );
    assert_eq!(
        get(app, "/v0/management/request-retry").await.json(),
        json!({"request-retry": 4})
    );
    let r = with_json(
        app,
        Method::PUT,
        "/v0/management/request-retry",
        json!({"value": "x"}),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (400, json!({"error": "invalid body"}))
    );

    // Keys written through v0 land in the file and survive a reload; comments stay.
    let r = with_json(
        app,
        Method::PATCH,
        "/v0/management/api-keys",
        json!({"old": "k1", "new": "k2"}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        get(app, "/v0/management/api-keys").await.json(),
        json!({"api-keys": ["k2"]})
    );
    let text = std::fs::read_to_string(&h.config_path).unwrap();
    assert!(text.contains("# client keys"), "{text}");

    // gin.H answers are written in key order.
    let r = send(
        app,
        Method::DELETE,
        "/v0/management/oauth-session?state=nope",
        LOCAL,
        &bearer(),
        Vec::new(),
    )
    .await;
    let keys: Vec<String> = r.json().as_object().unwrap().keys().cloned().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[tokio::test]
async fn v0_unrouted_paths_pass_the_gate_before_the_404() {
    let h = harness();
    let uri = "/v0/management/no-such-thing";
    let r = send(&h.app, Method::GET, uri, LOCAL, &[], Vec::new()).await;
    assert_eq!(
        r.status, 401,
        "an unauthenticated caller learns nothing about routes"
    );
    let r = get(&h.app, uri).await;
    assert!(r.status == 404 && r.body.is_empty() && r.headers.contains_key("x-cpa-version"));
    // v8 misses are plain 404s without management headers.
    let r = get(&h.app, "/v8/management/no-such-thing").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn usage_queue_pops_each_event_once() {
    let _globals = USAGE_GLOBALS.lock().await;
    let h = harness();
    // The queue is process-wide; this is the only test in this binary that enables it.
    cpa_runtime::usage_queue::install(&h.usage);
    cpa_home::queue::set_enabled(true);
    for model in ["m1", "m2", "m3"] {
        h.usage.record(record(model, false));
    }
    let r = get(&h.app, "/v0/management/usage-queue?count=2").await;
    let models: Vec<String> = r
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["model"].as_str().unwrap().into())
        .collect();
    assert_eq!(models, ["m1", "m2"]);
    let r = get(&h.app, "/v8/management/observability/usage/queue").await;
    assert_eq!(r.json().as_array().unwrap()[0]["model"], "m3");
    assert_eq!(
        get(&h.app, "/v0/management/usage-queue").await.json(),
        json!([])
    );
    let r = get(&h.app, "/v0/management/usage-queue?count=0").await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (400, json!({"error": "count must be a positive integer"}))
    );
    cpa_home::queue::set_enabled(false);
}

//! Throwaway management server for driving ui/ against the Rust port:
//!
//! ```text
//! cd ui && bun install && bun run build
//! cargo run -p cpa-management --example dev_server -- /tmp/cpa-rust-dev --demo
//! bun dev/e2e.ts http://127.0.0.1:18328 dev-secret
//! ```
//!
//! Serves `ui/dist` at `/` and `/management.html`, `/healthz`, an empty `/v1/models`, and the
//! management API (key `dev-secret`) over an auth directory in the work dir. Credentials come
//! from the files in `<work dir>/auth` (no conductor): the registry here lists them straight from
//! disk and writes edits back. `--demo` records synthetic usage so the charts have data.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::Path as UrlPath;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use cpa_auth::{Auth, FileTokenStore, OAuthSessions, SaveOptions, Store};
use cpa_management::{AuthRegistry, ManagementState, oauth_redirect_router, router};
use cpa_runtime::conductor::Manager;
use cpa_runtime::usage::{TokenUsage, UsageFailure, UsageRecord, UsageTracker};

/// Credentials straight from the auth directory.
struct DirRegistry(Arc<FileTokenStore>);

#[async_trait::async_trait]
impl AuthRegistry for DirRegistry {
    fn list(&self) -> Vec<Auth> {
        self.0.list().unwrap_or_default()
    }

    fn get(&self, id: &str) -> Option<Auth> {
        self.list().into_iter().find(|a| a.id == id)
    }

    async fn update(&self, mut auth: Auth) -> Result<Auth, String> {
        self.0
            .save(
                &mut auth,
                SaveOptions {
                    creation_intent: true,
                },
            )
            .map_err(|e| e.to_string())?;
        Ok(auth)
    }

    async fn remove(&self, _id: &str) {}
}

const CONFIG: &str = "\
config-version: 8
server:
  port: 18328
management:
  allow-remote: false
  secret-key: dev-secret
access:
  api-keys:
    - sk-dev-client-aaaa1111
observability:
  logs:
    logging-to-file: true
  usage:
    usage-statistics-enabled: true
";

fn demo_usage(usage: &UsageTracker) {
    let models = ["gpt-5", "claude-sonnet-4-5", "gemini-3-pro-preview"];
    let now = chrono::Utc::now();
    for i in 0..300i64 {
        let failed = i % 23 == 0;
        usage.record(UsageRecord {
            timestamp: now - chrono::Duration::minutes(i * 4),
            latency_ms: 300 + (i * 37) % 4000,
            ttft_ms: 120 + (i * 13) % 600,
            source: "demo@example.com".into(),
            auth_index: format!("{:016x}", 0x2272fbe37d906486u64 + (i % 3) as u64),
            auth_type: "oauth".into(),
            provider: ["codex", "claude", "antigravity"][(i % 3) as usize].into(),
            executor_type: "demo".into(),
            model: models[(i % 3) as usize].into(),
            alias: String::new(),
            endpoint: "POST /v1/chat/completions".into(),
            api_key: "sk-dev-client-aaaa1111".into(),
            request_id: format!("{i:08x}"),
            failed,
            stream: i % 2 == 0,
            fail: if failed {
                UsageFailure {
                    status_code: 429,
                    body: "rate_limit_exceeded".into(),
                }
            } else {
                UsageFailure::default()
            },
            tokens: if failed {
                TokenUsage::default()
            } else {
                TokenUsage {
                    input_tokens: 800 + i,
                    output_tokens: 90,
                    total_tokens: 890 + i,
                    ..Default::default()
                }
            },
        });
    }
}

async fn static_file(dist: PathBuf, rel: String) -> Response {
    let rel = if rel.is_empty() {
        "index.html".to_string()
    } else {
        rel
    };
    if rel.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(dist.join(&rel)).await {
        Ok(bytes) => {
            let mime = match rel.rsplit('.').next().unwrap_or("") {
                "html" => "text/html; charset=utf-8",
                "js" => "text/javascript",
                "css" => "text/css",
                "svg" => "image/svg+xml",
                "woff2" => "font/woff2",
                "json" => "application/json",
                _ => "application/octet-stream",
            };
            ([(header::CONTENT_TYPE, mime)], bytes).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let work = PathBuf::from(
        args.iter()
            .find(|a| !a.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| "/tmp/cpa-rust-dev".into()),
    );
    let (auth_dir, log_dir, config_path) = (
        work.join("auth"),
        work.join("logs"),
        work.join("config.yaml"),
    );
    std::fs::create_dir_all(&auth_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    if !config_path.exists() {
        std::fs::write(
            &config_path,
            format!("{CONFIG}oauth:\n  auth-dir: {}\n", auth_dir.display()),
        )
        .unwrap();
    }
    let cfg = cpa_config::load_config(&config_path).unwrap();
    let watcher = Arc::new(
        cpa_config::watcher::ConfigWatcher::start(&config_path, Arc::new(cfg), None)
            .await
            .unwrap(),
    );

    let store = Arc::new(FileTokenStore::with_dir(&auth_dir));
    let sessions = Arc::new(OAuthSessions::default());
    let login = cpa_auth::Manager::new(store.clone()).with_sessions(sessions.clone());
    let usage = Arc::new(UsageTracker::new());
    if args.iter().any(|a| a == "--demo") {
        demo_usage(&usage);
    }
    let reload = watcher.clone();
    let state = ManagementState::new(
        &config_path,
        watcher.subscribe(),
        Arc::new(Manager::default()),
        store.clone(),
        sessions,
        login,
        usage,
        &log_dir,
    )
    .with_registry(Arc::new(DirRegistry(store)))
    .with_reload_hook(Arc::new(move || {
        let reload = reload.clone();
        Box::pin(async move {
            reload.reload_now().await;
        })
    }));

    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist");
    let (d1, d2) = (dist.clone(), dist);
    let app = Router::new()
        .route(
            "/healthz",
            get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        )
        .route(
            "/v1/models",
            get(|| async { axum::Json(serde_json::json!({"object": "list", "data": []})) }),
        )
        .route(
            "/management.html",
            get({
                let d = d1.clone();
                move || static_file(d.clone(), String::new())
            }),
        )
        .route("/", get(move || static_file(d1.clone(), String::new())))
        .route(
            "/{*path}",
            get(move |UrlPath(p): UrlPath<String>| static_file(d2.clone(), p)),
        )
        .merge(router(state.clone()))
        .merge(oauth_redirect_router(state));

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(18328);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .unwrap();
    println!(
        "management dev server on http://127.0.0.1:{port} (key dev-secret), work dir {}",
        work.display()
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

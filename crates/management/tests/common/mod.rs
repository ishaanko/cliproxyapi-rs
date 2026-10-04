//! Shared harness of the config API tests: a management router over a temp config file, with the
//! secret supplied through the environment so the file under test stays exactly as written.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use cpa_auth::{FileTokenStore, OAuthSessions};
use cpa_management::{ManagementState, router};
use cpa_runtime::conductor::Manager;
use cpa_runtime::usage::UsageTracker;
use http_body_util::BodyExt;
use tokio::sync::watch;
use tower::ServiceExt;

pub struct Harness {
    pub dir: tempfile::TempDir,
    pub app: Router,
    pub path: PathBuf,
    /// The conductor behind the management registry (credentials registered here are "live").
    pub manager: Arc<Manager>,
}

/// A router over `raw` written to `config.yaml`. The file is loaded like startup does, but the
/// `/v8` and `/v0` requests are served from the file itself.
pub fn harness(raw: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let auth_dir = dir.path().join("auths");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&auth_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::write(&path, raw).unwrap();
    let cfg = cpa_config::load_config(&path).unwrap();
    let (tx, rx) = watch::channel(Arc::new(cfg));
    let store = Arc::new(FileTokenStore::with_dir(&auth_dir));
    let sessions = Arc::new(OAuthSessions::default());
    let login = cpa_auth::Manager::new(store.clone()).with_sessions(sessions.clone());
    let manager = Arc::new(Manager::default());
    let reload_path = path.clone();
    let hook: cpa_management::ReloadHook = Arc::new(move || {
        let (tx, path) = (tx.clone(), reload_path.clone());
        Box::pin(async move {
            if let Ok(cfg) = cpa_config::load_config(&path) {
                tx.send_replace(Arc::new(cfg));
            }
        })
    });
    let state = ManagementState::new(
        &path,
        rx,
        manager.clone(),
        store,
        sessions,
        login,
        Arc::new(UsageTracker::new()),
        &log_dir,
    )
    .with_env_secret(Some("test-secret".to_string()))
    .with_reload_hook(hook);
    Harness {
        dir,
        app: router(state),
        path,
        manager,
    }
}

impl Harness {
    /// One authenticated local request; returns the status and the body text.
    pub async fn call(&self, method: Method, uri: &str, body: &str) -> (StatusCode, String) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer test-secret")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:5000".parse::<SocketAddr>().unwrap()));
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (parts.status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `call` that asserts the status and returns the body.
    pub async fn expect(&self, method: Method, uri: &str, body: &str, want: u16) -> String {
        let (status, text) = self.call(method.clone(), uri, body).await;
        assert_eq!(status.as_u16(), want, "{method} {uri}: {text}");
        text
    }

    pub fn read(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap()
    }

    pub fn load(&self) -> cpa_config::Config {
        cpa_config::load_config(&self.path).unwrap()
    }
}

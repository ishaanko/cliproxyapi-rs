//! Shared harness: a real `Manager` with a scripted fake executor behind the real router.
#![allow(dead_code, clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use cpa_auth::{Auth, FileTokenStore, OAuthSessions};
use cpa_config::Config;
use cpa_core::registry::{ModelInfo, global_registry};
use cpa_runtime::conductor::Manager;
use cpa_runtime::executor::{ExecError, Executor, Options, Request as ExecRequest, Response as ExecResponse, StreamResult};
use cpa_runtime::usage::UsageTracker;
use cpa_server::{AppState, build_router};
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;

pub type Chunk = Result<&'static str, ExecError>;

/// What the fake upstream does for a request whose payload contains `marker`.
#[derive(Clone)]
pub enum Script {
    Body(&'static str),
    Fail(ExecError),
    Stream(Vec<Chunk>),
    Slow(Duration, &'static str),
}

pub struct FakeExecutor {
    provider: String,
    pub scripts: Mutex<Vec<(&'static str, Script)>>,
    pub default_body: &'static str,
    pub seen: Mutex<Vec<(String, String)>>,
    /// `Options::codex_multi_agent_v2_tools_prepared` of each execute / execute_stream call.
    pub prepared: Mutex<Vec<bool>>,
}

impl FakeExecutor {
    fn pick(&self, req: &ExecRequest) -> Script {
        let payload = String::from_utf8_lossy(&req.payload).into_owned();
        self.seen.lock().push((req.model.clone(), payload.clone()));
        for (marker, script) in self.scripts.lock().iter() {
            if payload.contains(marker) {
                return script.clone();
            }
        }
        Script::Body(self.default_body)
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    fn identifier(&self) -> &str {
        &self.provider
    }

    async fn execute(&self, _auth: &Auth, req: ExecRequest, opts: Options) -> Result<ExecResponse, ExecError> {
        self.prepared.lock().push(opts.codex_multi_agent_v2_tools_prepared);
        match self.pick(&req) {
            Script::Body(b) => Ok(ExecResponse { payload: Bytes::from_static(b.as_bytes()), ..Default::default() }),
            Script::Slow(d, b) => {
                tokio::time::sleep(d).await;
                Ok(ExecResponse { payload: Bytes::from_static(b.as_bytes()), ..Default::default() })
            }
            Script::Fail(e) => Err(e),
            Script::Stream(_) => Err(ExecError::new(500, "stream script on non-stream call")),
        }
    }

    async fn execute_stream(&self, _auth: &Auth, req: ExecRequest, opts: Options) -> Result<StreamResult, ExecError> {
        self.prepared.lock().push(opts.codex_multi_agent_v2_tools_prepared);
        match self.pick(&req) {
            Script::Fail(e) => Err(e),
            Script::Body(b) => stream_of(vec![Ok(b)]),
            Script::Slow(_, b) => stream_of(vec![Ok(b)]),
            Script::Stream(items) => stream_of(items),
        }
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        Ok(auth.clone())
    }

    async fn count_tokens(&self, _auth: &Auth, req: ExecRequest, _opts: Options) -> Result<ExecResponse, ExecError> {
        match self.pick(&req) {
            Script::Fail(e) => Err(e),
            Script::Body(b) | Script::Slow(_, b) => Ok(ExecResponse { payload: Bytes::from_static(b.as_bytes()), ..Default::default() }),
            Script::Stream(_) => Err(ExecError::new(500, "stream script on count call")),
        }
    }
}

fn stream_of(items: Vec<Chunk>) -> Result<StreamResult, ExecError> {
    let (tx, rx) = mpsc::channel(items.len().max(1) + 1);
    for item in items {
        let _ = tx.try_send(item.map(|s| Bytes::from_static(s.as_bytes())));
    }
    Ok(StreamResult::new(Default::default(), rx))
}

pub struct Harness {
    pub router: Router,
    pub exec: Arc<FakeExecutor>,
    /// Model registered for the fake credential.
    pub model: String,
    pub cfg_tx: watch::Sender<Arc<Config>>,
}

pub async fn harness(name: &str, edit: impl FnOnce(&mut Config)) -> Harness {
    let provider = format!("fake-{name}");
    let model = format!("model-{name}");
    let mut cfg = Config {
        api_keys: vec!["k1".into()],
        ..Config::default()
    };
    edit(&mut cfg);
    let (cfg_tx, cfg_rx) = watch::channel(Arc::new(cfg));

    let exec = Arc::new(FakeExecutor {
        provider: provider.clone(),
        scripts: Mutex::new(Vec::new()),
        default_body: r#"{"id":"c1","object":"chat.completion","model":"x","choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}]}"#,
        seen: Mutex::new(Vec::new()),
        prepared: Mutex::new(Vec::new()),
    });
    let manager = Arc::new(Manager::default());
    manager.register_executor(exec.clone());
    let auth_id = format!("auth-{name}");
    global_registry().register_client(
        &auth_id,
        &provider,
        &[ModelInfo { id: model.clone(), object: "model".into(), owned_by: "fake".into(), ..Default::default() }],
    );
    manager.update(Auth::new(&auth_id, &provider)).await.expect("register auth");

    let dir = tempfile::tempdir().expect("tempdir").keep();
    let state = AppState::new(
        cfg_rx,
        manager,
        Arc::new(FileTokenStore::with_dir(&dir)),
        Arc::new(OAuthSessions::default()),
        Arc::new(UsageTracker::default()),
    );
    Harness { router: build_router(state), exec, model, cfg_tx }
}

impl Harness {
    pub fn script(&self, marker: &'static str, script: Script) {
        self.exec.scripts.lock().push((marker, script));
    }

    pub async fn call(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut req = Request::builder().method(method).uri(path).header("x-api-key", "k1");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = self
            .router
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).expect("request"))
            .await
            .expect("infallible");
        let (parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.expect("body");
        (parts.status, parts.headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn chat(&self, extra: &str) -> String {
        format!(r#"{{"model":"{}","messages":[{{"role":"user","content":"{extra}"}}]}}"#, self.model)
    }

    pub fn chat_stream(&self, extra: &str) -> String {
        format!(r#"{{"model":"{}","stream":true,"messages":[{{"role":"user","content":"{extra}"}}]}}"#, self.model)
    }
}


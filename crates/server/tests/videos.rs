//! Video endpoint behavior over the real router with a capturing `xai` executor: credential and
//! model pinning across create and retrieve, and the content download going through the pinned
//! credential's proxy. Ports of the Go `openai_videos_handlers_test.go` cases of the same names.

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
use cpa_server::handlers::videos::VIDEO_AUTH_BINDINGS;
use cpa_server::{AppState, build_router};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower::ServiceExt;

const DEFAULT_MODEL: &str = "grok-imagine-video";
const MODEL_15: &str = "grok-imagine-video-1.5";
const PREVIEW_ALIAS: &str = "grok-imagine-video-1.5-preview";

/// What the capturing executor saw per call: `(auth id, routed model, payload model)`.
type Calls = Arc<Mutex<Vec<(String, String, String)>>>;

/// `videoAuthCaptureExecutor`: answers every call with a completed video.
struct CaptureExecutor {
    request_id: String,
    content_url: String,
    /// Time every call takes, to outlast the non-stream keep-alive interval.
    delay: Duration,
    calls: Calls,
}

#[async_trait]
impl Executor for CaptureExecutor {
    fn identifier(&self) -> &str {
        "xai"
    }

    async fn execute(&self, auth: &Auth, req: ExecRequest, _opts: Options) -> Result<ExecResponse, ExecError> {
        tokio::time::sleep(self.delay).await;
        let payload: Value = serde_json::from_slice(&req.payload).unwrap_or(Value::Null);
        let payload_model = payload["model"].as_str().unwrap_or("").trim().to_string();
        self.calls.lock().push((auth.id.clone(), req.model.clone(), payload_model));
        let request_id = payload["request_id"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&self.request_id);
        let content_url = if self.content_url.is_empty() { "https://vidgen.x.ai/video.mp4" } else { &self.content_url };
        let body = json!({"request_id": request_id, "status": "completed", "progress": 100, "video": {"url": content_url, "duration": 4}});
        Ok(ExecResponse { payload: Bytes::from(body.to_string()), ..Default::default() })
    }

    async fn execute_stream(&self, _auth: &Auth, _req: ExecRequest, _opts: Options) -> Result<StreamResult, ExecError> {
        Err(ExecError::new(500, "ExecuteStream not implemented"))
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        Ok(auth.clone())
    }

    async fn count_tokens(&self, _auth: &Auth, _req: ExecRequest, _opts: Options) -> Result<ExecResponse, ExecError> {
        Err(ExecError::new(500, "CountTokens not implemented"))
    }
}

struct Server {
    router: Router,
    calls: Calls,
    auth_ids: Vec<String>,
}

impl Drop for Server {
    fn drop(&mut self) {
        for id in &self.auth_ids {
            global_registry().unregister_client(id);
        }
    }
}

/// A server with one `xai` credential per `(auth id, proxy url, models)` entry.
async fn server(request_id: &str, content_url: &str, global_proxy: &str, auths: &[(&str, &str, &[&str])]) -> Server {
    server_with(request_id, content_url, Duration::ZERO, |cfg| cfg.proxy_url = global_proxy.into(), auths).await
}

async fn server_with(request_id: &str, content_url: &str, delay: Duration, edit: impl FnOnce(&mut Config), auths: &[(&str, &str, &[&str])]) -> Server {
    let calls = Calls::default();
    let manager = Arc::new(Manager::default());
    manager.register_executor(Arc::new(CaptureExecutor { request_id: request_id.into(), content_url: content_url.into(), delay, calls: calls.clone() }));
    for (id, proxy, models) in auths {
        let models: Vec<ModelInfo> = models.iter().map(|m| ModelInfo { id: (*m).into(), object: "model".into(), owned_by: "xai".into(), ..Default::default() }).collect();
        global_registry().register_client(id, "xai", &models);
        let mut auth = Auth::new(*id, "xai");
        auth.proxy_url = (*proxy).into();
        manager.update(auth).await.expect("register auth");
    }
    let mut cfg = Config { api_keys: vec!["k1".into()], ..Config::default() };
    edit(&mut cfg);
    let (tx, rx) = watch::channel(Arc::new(cfg));
    // The watch sender must outlive the state, so it is leaked for the test process.
    std::mem::forget(tx);
    let dir = tempfile::tempdir().expect("tempdir").keep();
    let state = AppState::new(rx, manager, Arc::new(FileTokenStore::with_dir(&dir)), Arc::new(OAuthSessions::default()), Arc::new(UsageTracker::default()));
    Server { router: build_router(state), calls, auth_ids: auths.iter().map(|(id, ..)| (*id).to_string()).collect() }
}

impl Server {
    async fn call(&self, method: &str, path: &str, body: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("x-api-key", "k1")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        let resp = self.router.clone().oneshot(req).await.expect("infallible");
        let (parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.expect("body");
        (parts.status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `(auth ids, routed models, payload models)` of the executor calls so far.
    fn seen(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let calls = self.calls.lock();
        (calls.iter().map(|c| c.0.clone()).collect(), calls.iter().map(|c| c.1.clone()).collect(), calls.iter().map(|c| c.2.clone()).collect())
    }
}

/// Minimal HTTP/1.1 server: every request gets `reply(request head)` as `(status, content type,
/// body)`. Returns its URL.
async fn http_server(reply: impl Fn(&str) -> (u16, &'static str, &'static str) + Send + Sync + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let reply = Arc::new(reply);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let reply = Arc::clone(&reply);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let mut chunk = [0u8; 2048];
                    match stream.read(&mut chunk).await {
                        Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                        _ => return,
                    }
                }
                let (status, content_type, body) = reply(&String::from_utf8_lossy(&buf));
                let head = format!("HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    url
}

#[tokio::test]
async fn videos_content_uses_selected_auth_proxy_for_download() {
    let upstream = http_server(|_| (200, "video/mp4", "video-bytes")).await;
    let proxy_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits = Arc::clone(&proxy_hits);
    let global_proxy = http_server(move |_| {
        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (502, "text/plain", "unexpected proxy")
    })
    .await;
    let (video_id, auth_id) = ("video-content-selected", "video-content-selected-auth");
    let srv = server(video_id, &format!("{upstream}/video.mp4"), &global_proxy, &[(auth_id, "direct", &[DEFAULT_MODEL])]).await;

    let (status, body) = srv.call("GET", &format!("/openai/v1/videos/{video_id}/content"), "").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "video-bytes"));
    assert_eq!(srv.seen().0, [auth_id]);
    assert_eq!(VIDEO_AUTH_BINDINGS.get(video_id).as_deref(), Some(auth_id));
    assert_eq!(proxy_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn videos_content_falls_back_to_global_proxy() {
    let global_proxy = http_server(|head| {
        assert!(head.starts_with("GET http://video-host.invalid/video.mp4 "), "{head}");
        (200, "video/mp4", "via-proxy")
    })
    .await;
    let video_id = "video-content-global-proxy";
    let srv = server(video_id, "http://video-host.invalid/video.mp4", &global_proxy, &[("video-content-global-auth", "", &[DEFAULT_MODEL])]).await;

    let (status, body) = srv.call("GET", &format!("/openai/v1/videos/{video_id}/content"), "").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "via-proxy"));
}

/// The keep-alive covers only the poll: its newline commits 200 and the download body then
/// streams after it (Go: `stopKeepAlive` right after the poll, then `io.Copy`).
#[tokio::test]
async fn videos_content_streams_the_download_after_a_keepalive_poll() {
    let upstream = http_server(|_| (200, "video/mp4", "video-bytes")).await;
    let video_id = "video-content-keepalive";
    let srv = server_with(
        video_id,
        &format!("{upstream}/video.mp4"),
        Duration::from_millis(1500),
        |cfg| cfg.nonstream_keepalive_interval = 1,
        &[("video-content-keepalive-auth", "direct", &[DEFAULT_MODEL])],
    )
    .await;

    let (status, body) = srv.call("GET", &format!("/openai/v1/videos/{video_id}/content"), "").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "\nvideo-bytes"));
}

#[tokio::test]
async fn videos_create_preview_alias_uses_preview_auth_with_ga_payload() {
    let auth_id = "video-openai-preview-auth";
    let srv = server("video-openai-preview-alias", "", "", &[(auth_id, "", &[PREVIEW_ALIAS])]).await;

    let (status, body) = srv.call("POST", "/openai/v1/videos", &format!(r#"{{"model":"{PREVIEW_ALIAS}","prompt":"make a video"}}"#)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let created: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(created["model"], MODEL_15);
    let video_id = created["id"].as_str().expect("id").to_string();

    let (status, body) = srv.call("GET", &format!("/openai/v1/videos/{video_id}"), "").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (auth_ids, models, payload_models) = srv.seen();
    assert_eq!(auth_ids, [auth_id, auth_id]);
    assert_eq!(models, [PREVIEW_ALIAS, PREVIEW_ALIAS]);
    assert_eq!(payload_models[0], MODEL_15);
    let binding = VIDEO_AUTH_BINDINGS.get_binding(&video_id).expect("binding stored");
    assert_eq!((binding.auth_id.as_str(), binding.model.as_str()), (auth_id, PREVIEW_ALIAS));
}

#[tokio::test]
async fn xai_videos_native_retrieve_uses_canonical_bound_model() {
    let srv = server(
        "video-xai-1.5-bound",
        "",
        "",
        &[("video-xai-1.5-default-auth", "", &[DEFAULT_MODEL]), ("video-xai-1.5-auth", "", &[MODEL_15])],
    )
    .await;

    let (status, body) = srv.call("POST", "/v1/videos/generations", &format!(r#"{{"model":"{MODEL_15}","prompt":"make a video"}}"#)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let video_id = serde_json::from_str::<Value>(&body).expect("json")["request_id"].as_str().expect("request_id").to_string();
    assert_eq!(video_id, "video-xai-1.5-bound");

    let (status, body) = srv.call("GET", &format!("/v1/videos/{video_id}"), "").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (auth_ids, models, payload_models) = srv.seen();
    assert_eq!(auth_ids, ["video-xai-1.5-auth", "video-xai-1.5-auth"]);
    assert_eq!(models, [MODEL_15, MODEL_15]);
    assert_eq!(payload_models[0], MODEL_15);
    let binding = VIDEO_AUTH_BINDINGS.get_binding(&video_id).expect("binding stored");
    assert_eq!((binding.auth_id.as_str(), binding.model.as_str()), ("video-xai-1.5-auth", MODEL_15));
}

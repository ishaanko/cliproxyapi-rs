//! Redis protocol on the API port (ports of `internal/api/redis_queue_protocol_integration_test.go`
//! and `protocol_multiplexer_test.go`): management gating, AUTH, SUBSCRIBE, LPOP/RPOP and an idle
//! connection not blocking HTTP.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use axum::serve::ListenerExt;
use cpa_auth::{FileTokenStore, OAuthSessions};
use cpa_home::queue;
use cpa_management::ManagementState;
use cpa_runtime::conductor::Manager;
use cpa_runtime::usage::UsageTracker;
use cpa_server::mux;
use cpa_server::redis_protocol::RedisProtocol;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

const PASSWORD: &str = "test-management-password";

/// The queue is process-wide, so tests of this binary take turns.
static QUEUE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Server {
    addr: SocketAddr,
    _guard: tokio::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}

async fn start(secret: Option<&str>, home: bool) -> Server {
    let guard = QUEUE_LOCK.lock().await;
    queue::set_enabled(false);
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.yaml");
    let yaml = match secret {
        Some(s) => format!("remote-management:\n  secret-key: {s}\n  allow-remote: true\n"),
        None => "port: 8317\n".to_string(),
    };
    std::fs::write(&config_path, yaml).unwrap();
    let mut cfg = cpa_config::load_config(&config_path).unwrap();
    cfg.home.enabled = home;
    let has_secret = !cfg.remote_management.secret_key.is_empty();
    let (_tx, rx) = watch::channel(Arc::new(cfg));
    std::mem::forget(_tx);
    let store = Arc::new(FileTokenStore::with_dir(dir.path().join("auths")));
    let sessions = Arc::new(OAuthSessions::default());
    let login = cpa_auth::Manager::new(store.clone()).with_sessions(sessions.clone());
    let state = ManagementState::new(
        &config_path,
        rx.clone(),
        Arc::new(Manager::default()),
        store,
        sessions,
        login,
        Arc::new(UsageTracker::new()),
        dir.path().join("logs"),
    )
    .with_env_secret(None);
    queue::set_enabled(has_secret || home);
    let redis = Arc::new(RedisProtocol { config: rx, management: state, routes_enabled: Arc::new(AtomicBool::new(has_secret)) });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mux = mux::start(listener, None, Some(redis)).unwrap().tap_io(|_| {});
    let app = Router::new().route("/", get(|| async { "ok" }));
    tokio::spawn(async move {
        let _ = axum::serve(mux, app.into_make_service_with_connect_info::<SocketAddr>()).await;
    });
    Server { addr, _guard: guard, _dir: dir }
}

struct Conn(BufReader<TcpStream>);

impl Conn {
    async fn open(addr: SocketAddr) -> Self {
        Conn(BufReader::new(TcpStream::connect(addr).await.unwrap()))
    }

    async fn send(&mut self, args: &[&str]) {
        let mut out = format!("*{}\r\n", args.len());
        for a in args {
            out.push_str(&format!("${}\r\n{a}\r\n", a.len()));
        }
        self.0.get_mut().write_all(out.as_bytes()).await.unwrap();
    }

    async fn line(&mut self) -> String {
        let mut s = String::new();
        tokio::time::timeout(Duration::from_secs(5), self.0.read_line(&mut s)).await.unwrap().unwrap();
        s.trim_end().to_string()
    }

    /// A bulk string; `None` for the nil bulk.
    async fn bulk(&mut self) -> Option<String> {
        let head = self.line().await;
        let n: i64 = head.strip_prefix('$').unwrap_or_else(|| panic!("not a bulk: {head}")).parse().unwrap();
        if n < 0 {
            return None;
        }
        let mut buf = vec![0u8; n as usize + 2];
        self.0.read_exact(&mut buf).await.unwrap();
        buf.truncate(n as usize);
        Some(String::from_utf8(buf).unwrap())
    }

    async fn bulk_array(&mut self) -> Vec<String> {
        let head = self.line().await;
        let n: usize = head.strip_prefix('*').unwrap().parse().unwrap();
        let mut out = vec![];
        for _ in 0..n {
            out.push(self.bulk().await.unwrap());
        }
        out
    }

    async fn auth(&mut self) {
        self.send(&["AUTH", PASSWORD]).await;
        assert_eq!(self.line().await, "+OK");
    }

    async fn expect_closed(&mut self) {
        let mut b = [0u8; 1];
        let r = tokio::time::timeout(Duration::from_secs(2), self.0.read(&mut b)).await.expect("timed out, not closed");
        assert!(matches!(r, Ok(0) | Err(_)), "expected close, got {r:?}");
    }
}

#[tokio::test]
async fn management_disabled_rejects_connection() {
    let s = start(None, false).await;
    let mut c = Conn::open(s.addr).await;
    c.send(&["PING"]).await;
    c.expect_closed().await;
}

#[tokio::test]
async fn home_enabled_disables_connection() {
    let s = start(Some(PASSWORD), true).await;
    let mut c = Conn::open(s.addr).await;
    c.send(&["PING"]).await;
    assert_eq!(c.line().await, "-ERR redis usage output disabled in home mode");
    c.expect_closed().await;
}

#[tokio::test]
async fn subscribe_usage_sends_support_refresh_then_records() {
    let s = start(Some(PASSWORD), false).await;
    let mut c = Conn::open(s.addr).await;
    c.auth().await;
    c.send(&["SUBSCRIBE", "usage"]).await;
    assert_eq!(c.bulk_array_head().await, ("subscribe".to_string(), "usage".to_string(), 1));
    assert_eq!(c.bulk_array().await, ["message", "usage", r#"{"support_refresh":true}"#]);
    queue::enqueue(br#"{"id":1}"#);
    assert_eq!(c.bulk_array().await, ["message", "usage", r#"{"id":1}"#]);
}

#[tokio::test]
async fn subscribe_errors_receives_error_events() {
    let s = start(Some(PASSWORD), false).await;
    let mut c = Conn::open(s.addr).await;
    c.auth().await;
    c.send(&["SUBSCRIBE", "errors"]).await;
    assert_eq!(c.bulk_array_head().await, ("subscribe".to_string(), "errors".to_string(), 1));
    queue::enqueue_error(br#"{"auth_index":"auth-1","status_code":401}"#);
    assert_eq!(c.bulk_array().await, ["message", "errors", r#"{"auth_index":"auth-1","status_code":401}"#]);
}

#[tokio::test]
async fn auth_and_pop_contracts() {
    let s = start(Some(PASSWORD), false).await;
    let mut c = Conn::open(s.addr).await;
    c.auth().await;
    assert!(queue::enabled());
    for p in ["a", "b", "c"] {
        queue::enqueue(p.as_bytes());
    }
    c.send(&["RPOP", "usage"]).await;
    assert_eq!(c.bulk().await.as_deref(), Some("a"));
    c.send(&["LPOP", "usage"]).await;
    assert_eq!(c.bulk().await.as_deref(), Some("b"));
    c.send(&["RPOP", "usage", "10"]).await;
    assert_eq!(c.bulk_array().await, ["c"]);
    c.send(&["LPOP", "usage"]).await;
    assert_eq!(c.bulk().await, None);
    c.send(&["RPOP", "usage", "2"]).await;
    assert!(c.bulk_array().await.is_empty());
    c.send(&["RPOP", "errors", "2"]).await;
    assert_eq!(c.line().await, "-ERR unsupported channel 'errors'");
}

#[tokio::test]
async fn idle_connection_does_not_block_http() {
    let s = start(None, false).await;
    let _idle = TcpStream::connect(s.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut http = TcpStream::connect(s.addr).await.unwrap();
    http.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut body = String::new();
    tokio::time::timeout(Duration::from_secs(3), http.read_to_string(&mut body)).await.expect("http blocked").unwrap();
    assert!(body.starts_with("HTTP/1.1 200"), "{body}");
}

impl Conn {
    /// `["subscribe", channel, count]` acknowledgement.
    async fn bulk_array_head(&mut self) -> (String, String, i64) {
        assert_eq!(self.line().await, "*3");
        let kind = self.bulk().await.unwrap();
        let channel = self.bulk().await.unwrap();
        let count = self.line().await.strip_prefix(':').unwrap().parse().unwrap();
        (kind, channel, count)
    }
}

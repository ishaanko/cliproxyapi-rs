#![allow(dead_code)]
//! Test harness: a mock management API (axum) and a driver that feeds an `App` the same messages
//! the event loop would, then renders it into a ratatui `TestBackend`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_tui::keys::Key;
use cpa_tui::{App, LogHook, Msg};
use parking_lot::Mutex;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use unicode_width::UnicodeWidthStr;

/// One request the mock received.
#[derive(Debug, Clone)]
pub struct Call {
    pub method: String,
    /// Path and query.
    pub path: String,
    pub auth: String,
    pub body: String,
}

pub struct Mock {
    pub addr: SocketAddr,
    pub calls: Arc<Mutex<Vec<Call>>>,
    pub state: Arc<Mutex<MockData>>,
}

/// Mutable fixtures behind the mock.
pub struct MockData {
    pub secret: String,
    pub config: Value,
    pub api_keys: Vec<String>,
    pub auth_files: Vec<Value>,
    pub log_lines: Vec<String>,
    pub oauth_start: Value,
}

impl MockData {
    fn fixture() -> Self {
        MockData {
            secret: "mgmt-secret".into(),
            config: json!({
                "port": 8317, "host": "127.0.0.1", "debug": false, "proxy-url": "",
                "request-retry": 3, "max-retry-interval": 30, "force-model-prefix": "",
                "logging-to-file": true, "logs-max-total-size-mb": 0, "error-logs-max-files": 10,
                "usage-statistics-enabled": true, "request-log": false,
                "quota-exceeded": {"switch-project": true, "switch-preview-model": false},
                "routing": {"strategy": "round-robin"}, "ws-auth": false
            }),
            api_keys: vec!["sk-test-key-0001-abcdefghij".into(), "short".into()],
            auth_files: vec![
                json!({"name": "claude-test@example.com.json", "channel": "claude", "email": "test@example.com",
                       "disabled": false, "status": "active", "auth_type": "oauth", "priority": 5}),
                json!({"name": "codex-a-very-long-credential-file-name.json", "channel": "codex",
                       "email": "someone-with-a-long-address@example.org", "disabled": true}),
            ],
            log_lines: vec![
                "[2026-10-02 10:00:00] [--------] [info ] [main.rs:1] server started".into(),
                "[2026-10-02 10:00:01] [--------] [debug] [main.rs:2] debug detail".into(),
                "[2026-10-02 10:00:02] [--------] [warn ] [main.rs:3] slow upstream".into(),
                "[2026-10-02 10:00:03] [--------] [error] [main.rs:4] upstream failed".into(),
            ],
            oauth_start: json!({"url": "https://auth.example.com/authorize?client_id=abc&state=st-1", "state": "st-1"}),
        }
    }
}

async fn handle(State(m): State<(Arc<Mutex<Vec<Call>>>, Arc<Mutex<MockData>>)>, req: Request) -> Response {
    let (calls, data) = m;
    let method = req.method().to_string();
    let path = req.uri().path_and_query().map(|p| p.to_string()).unwrap_or_default();
    let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = String::from_utf8_lossy(&to_bytes(req.into_body(), 1 << 20).await.unwrap_or_default()).into_owned();
    calls.lock().push(Call { method: method.clone(), path: path.clone(), auth: auth.clone(), body: body.clone() });

    let mut d = data.lock();
    if auth != format!("Bearer {}", d.secret) {
        return (StatusCode::UNAUTHORIZED, json!({"error": "invalid management key"}).to_string()).into_response();
    }
    let route = path.split('?').next().unwrap_or("").trim_start_matches("/v0/management/").to_string();
    let ok = || (StatusCode::OK, json!({"status": "ok"}).to_string()).into_response();
    let json_ok = |v: Value| (StatusCode::OK, v.to_string()).into_response();
    match (method.as_str(), route.as_str()) {
        ("GET", "config") => json_ok(d.config.clone()),
        ("GET", "auth-files") => json_ok(json!({"files": d.auth_files})),
        ("GET", "api-keys") => json_ok(json!({"api-keys": d.api_keys})),
        ("GET", "claude-api-key") => json_ok(json!({"claude-api-key": [
            {"api-key": "sk-ant-abcdefghijklmnop", "base-url": "https://api.anthropic.com", "prefix": "team"}]})),
        ("GET", "openai-compatibility") => json_ok(json!({"openai-compatibility": [
            {"name": "openrouter", "base-url": "https://openrouter.ai/api/v1"}]})),
        ("GET", r) if r.ends_with("-api-key") => json_ok(json!({})),
        ("GET", "logs") => json_ok(json!({"lines": d.log_lines, "latest-timestamp": 100})),
        ("GET", "get-auth-status") => json_ok(json!({"status": "wait"})),
        ("GET", r) if r.ends_with("-auth-url") => json_ok(d.oauth_start.clone()),
        ("PATCH", "api-keys") => {
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            match v.get("new").and_then(Value::as_str) {
                Some(new) => d.api_keys.push(new.to_string()),
                None => return (StatusCode::BAD_REQUEST, json!({"error": "missing fields"}).to_string()).into_response(),
            }
            ok()
        }
        ("DELETE", "api-keys") => {
            if let Some(i) = path.split("index=").nth(1).and_then(|s| s.parse::<usize>().ok())
                && i < d.api_keys.len()
            {
                d.api_keys.remove(i);
            }
            ok()
        }
        ("PUT", r) if !r.contains('/') && r != "config.yaml" => {
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            if let Some(val) = v.get("value") {
                d.config[r] = val.clone();
            }
            ok()
        }
        _ => ok(),
    }
}

impl Mock {
    pub async fn start() -> Mock {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(MockData::fixture()));
        let app = Router::new().fallback(handle).with_state((calls.clone(), state.clone()));
        // Shared CI hosts can briefly run out of ephemeral ports; retry the bind.
        let mut attempt = 0;
        let listener = loop {
            match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                Ok(l) => break l,
                Err(e) if attempt < 40 => {
                    attempt += 1;
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => panic!("bind mock: {e}"),
            }
        };
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Mock { addr, calls, state }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn calls_matching(&self, method: &str, prefix: &str) -> Vec<Call> {
        self.calls.lock().iter().filter(|c| c.method == method && c.path.starts_with(prefix)).cloned().collect()
    }
}

pub struct Harness {
    pub app: App,
    rx: UnboundedReceiver<Msg>,
    pub quit: bool,
    pub w: u16,
    pub h: u16,
    base_url: String,
}

impl Harness {
    /// Client mode (password gate) against `base_url`.
    pub fn client_mode(base_url: &str, secret: &str) -> Harness {
        Self::build(base_url, secret, None)
    }

    /// Standalone mode: no gate, logs from a hook.
    pub fn standalone(base_url: &str, secret: &str, hook: LogHook) -> Harness {
        Self::build(base_url, secret, Some(hook))
    }

    fn build(base_url: &str, secret: &str, hook: Option<LogHook>) -> Harness {
        let (tx, rx) = unbounded_channel();
        let app = App::new(base_url, secret, hook, tx);
        Harness { app, rx, quit: false, w: 100, h: 30, base_url: base_url.to_string() }
    }

    /// Sends the initial resize and the first fetches, then settles.
    pub async fn start(&mut self) {
        self.feed(Msg::Resize(self.w, self.h));
        self.app.init();
        self.settle().await;
    }

    pub fn feed(&mut self, msg: Msg) {
        if self.app.update(msg) {
            self.quit = true;
        }
    }

    /// Applies queued results until the channel has been quiet for a moment.
    pub async fn settle(&mut self) {
        while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(250), self.rx.recv()).await {
            self.feed(msg);
        }
    }

    /// Applies only the messages already queued, without waiting for in-flight requests.
    pub fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.feed(msg);
        }
    }

    pub fn key(&mut self, name: &str) {
        self.feed(Msg::Key(Key::named(name)));
    }

    pub fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(&c.to_string());
        }
    }

    pub async fn press(&mut self, name: &str) {
        self.key(name);
        self.settle().await;
    }

    pub fn screen(&mut self) -> String {
        let mut terminal = Terminal::new(TestBackend::new(self.w, self.h)).expect("test terminal");
        terminal.draw(|f| self.app.draw(f)).expect("draw");
        // The mock's port differs per run; keep snapshots stable.
        buffer_text(terminal.backend().buffer()).replace(self.base_url.trim_start_matches("http://"), "127.0.0.1:PORT")
    }

    /// Like `screen` but the buffer itself, for style assertions.
    pub fn buffer(&mut self) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(self.w, self.h)).expect("test terminal");
        terminal.draw(|f| self.app.draw(f)).expect("draw");
        terminal.backend().buffer().clone()
    }
}

/// Plain text of a buffer, one trimmed line per row; wide glyphs count once.
pub fn buffer_text(buf: &Buffer) -> String {
    let area = buf.area;
    let mut rows = Vec::new();
    for y in 0..area.height {
        let mut row = String::new();
        let mut skip = 0;
        for x in 0..area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let sym = buf[(x, y)].symbol();
            skip = UnicodeWidthStr::width(sym).saturating_sub(1);
            row.push_str(sym);
        }
        rows.push(row.trim_end().to_string());
    }
    while rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    rows.join("\n") + "\n"
}

/// Compares `actual` with `tests/snapshots/<name>.txt`; `UPDATE_SNAPSHOTS=1` rewrites it.
pub fn assert_snapshot(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots").join(format!("{name}.txt"));
    if std::env::var("UPDATE_SNAPSHOTS").is_ok() {
        std::fs::create_dir_all(path.parent().expect("snapshot dir")).expect("create snapshot dir");
        std::fs::write(&path, actual).expect("write snapshot");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {path:?}; run with UPDATE_SNAPSHOTS=1\n--- actual ---\n{actual}"));
    assert_eq!(expected, actual, "snapshot {name} differs (UPDATE_SNAPSHOTS=1 to accept)");
}

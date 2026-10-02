//! Local OAuth redirect receiver (`oauth_server.go` of claude/codex/devin and the antigravity
//! inline server). One tiny GET-only HTTP/1.1 server per login; the first result is delivered
//! through a channel.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{AuthErrorKind, AuthFlowError};

const CLAUDE_SUCCESS_HTML: &str = include_str!("assets/claude_success.html");
const CLAUDE_SETUP_NOTICE: &str = include_str!("assets/claude_setup_notice.html");
const CODEX_SUCCESS_HTML: &str = include_str!("assets/codex_success.html");
const CODEX_SETUP_NOTICE: &str = include_str!("assets/codex_setup_notice.html");

/// Which provider's redirect contract the server speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// `GET /callback` then 302 `/success`; default port 54545.
    Claude,
    /// `GET /auth/callback` then 302 `/success`; default port 1455.
    Codex,
    /// `GET /oauth-callback`; default port 51121; values trimmed, tiny inline pages.
    Antigravity,
    /// `GET /callback` bound to 127.0.0.1 (port 0 = ephemeral); HTML success/failure pages.
    Devin,
}

impl Flavor {
    pub fn default_port(self) -> u16 {
        match self {
            Flavor::Claude => 54545,
            Flavor::Codex => 1455,
            Flavor::Antigravity => 51121,
            Flavor::Devin => 0,
        }
    }

    fn callback_path(self) -> &'static str {
        match self {
            Flavor::Claude | Flavor::Devin => "/callback",
            Flavor::Codex => "/auth/callback",
            Flavor::Antigravity => "/oauth-callback",
        }
    }
}

/// Result of one redirect: either `code` + `state`, or `error`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OAuthResult {
    pub code: String,
    pub state: String,
    pub error: String,
}

/// A running callback server. Dropping it stops listening.
pub struct CallbackServer {
    port: u16,
    results: mpsc::Receiver<OAuthResult>,
    task: JoinHandle<()>,
}

impl CallbackServer {
    /// Binds the provider's port (all interfaces, like Go's `:port`; Devin binds loopback) and
    /// starts serving. Fails with `PortInUse` when the port is taken.
    pub async fn start(flavor: Flavor, port: u16) -> Result<CallbackServer, AuthFlowError> {
        let port = if port == 0 && flavor != Flavor::Devin {
            flavor.default_port()
        } else {
            port
        };
        let listener = bind(flavor, port).await.map_err(|e| {
            if e.kind() == io::ErrorKind::AddrInUse {
                AuthFlowError::authentication(
                    AuthErrorKind::PortInUse,
                    format!("port {port} is already in use"),
                )
            } else {
                AuthFlowError::authentication(AuthErrorKind::ServerStartFailed, e)
            }
        })?;
        let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
        let (tx, rx) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(10),
                        serve_connection(stream, flavor, tx),
                    )
                    .await;
                });
            }
        });
        Ok(CallbackServer {
            port: bound,
            results: rx,
            task,
        })
    }

    /// Actual listening port (differs from the request when 0 was asked for).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Waits for the first redirect result.
    pub async fn wait(&mut self, timeout: Duration) -> Result<OAuthResult, AuthFlowError> {
        match tokio::time::timeout(timeout, self.results.recv()).await {
            Ok(Some(r)) => Ok(r),
            Ok(None) => Err(AuthFlowError::other("OAuth callback server stopped")),
            Err(_) => Err(AuthFlowError::authentication(
                AuthErrorKind::CallbackTimeout,
                "timeout waiting for OAuth callback",
            )),
        }
    }

    /// Next redirect result with no deadline (callers race it against their own timers).
    pub async fn next_result(&mut self) -> Option<OAuthResult> {
        self.results.recv().await
    }

    /// Non-blocking check used to prefer a ready browser result over a prompt.
    pub fn try_result(&mut self) -> Option<OAuthResult> {
        self.results.try_recv().ok()
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Web UI helper: listens on `0.0.0.0:<port>` and 302-redirects every request (query preserved) to
/// `target_base`, so a browser hitting the provider's fixed redirect port lands on the main server's
/// callback route. Dropping it stops listening.
pub struct CallbackForwarder {
    port: u16,
    task: JoinHandle<()>,
}

impl CallbackForwarder {
    pub async fn start(port: u16, target_base: &str) -> Result<CallbackForwarder, AuthFlowError> {
        let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))
            .await
            .map_err(|e| {
                AuthFlowError::Config(format!("failed to listen on 0.0.0.0:{port}: {e}"))
            })?;
        let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
        let target = target_base.to_string();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let target = target.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(5), async {
                        let Some((_, request_target)) = read_request_line(&mut stream).await else { return };
                        let location = forward_location(&target, &request_target);
                        let head = format!(
                            "HTTP/1.1 302 Found\r\nCache-Control: no-store\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    })
                    .await;
                });
            }
        });
        Ok(CallbackForwarder { port: bound, task })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

impl Drop for CallbackForwarder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `target_base` plus the incoming raw query, joined with `?` or `&`.
fn forward_location(target_base: &str, request_target: &str) -> String {
    match request_target.split_once('?') {
        Some((_, raw)) if !raw.is_empty() => {
            let sep = if target_base.contains('?') { '&' } else { '?' };
            format!("{target_base}{sep}{raw}")
        }
        _ => target_base.to_string(),
    }
}

async fn bind(flavor: Flavor, port: u16) -> io::Result<TcpListener> {
    if flavor == Flavor::Devin {
        return TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await;
    }
    match TcpListener::bind(SocketAddr::from(([0u16; 8], port))).await {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Err(e),
        Err(_) => TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await,
    }
}

struct Response {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Response {
    fn text(status: u16, body: impl Into<String>) -> Self {
        let mut body: String = body.into();
        body.push('\n');
        Response {
            status,
            headers: vec![("Content-Type", "text/plain; charset=utf-8".into())],
            body,
        }
    }

    fn html(status: u16, body: String) -> Self {
        Response {
            status,
            headers: vec![("Content-Type", "text/html; charset=utf-8".into())],
            body,
        }
    }

    fn redirect(location: &str) -> Self {
        Response {
            status: 302,
            headers: vec![
                ("Content-Type", "text/html; charset=utf-8".into()),
                ("Location", location.to_string()),
            ],
            body: format!("<a href=\"{location}\">Found</a>.\n\n"),
        }
    }

    fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            302 => "Found",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            _ => "Status",
        }
    }
}

async fn serve_connection(mut stream: TcpStream, flavor: Flavor, tx: mpsc::Sender<OAuthResult>) {
    let Some((method, target)) = read_request_line(&mut stream).await else {
        return;
    };
    let response = route(flavor, &method, &target, &tx);
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, response.reason());
    for (k, v) in &response.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    ));
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(response.body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Reads up to the end of the request headers and returns `(method, request-target)`.
async fn read_request_line(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buf.len() < 16 * 1024 {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.contains(&b'\n') && n < chunk.len() {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    Some((parts.next()?.to_string(), parts.next()?.to_string()))
}

fn route(flavor: Flavor, method: &str, target: &str, tx: &mpsc::Sender<OAuthResult>) -> Response {
    let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else {
        return Response::text(400, "Bad Request");
    };
    let q = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    };
    let path = url.path();

    if path == flavor.callback_path() {
        return match flavor {
            Flavor::Claude | Flavor::Codex => {
                browser_callback(method, &q("code"), &q("state"), &q("error"), tx)
            }
            Flavor::Antigravity => antigravity_callback(&q, tx),
            Flavor::Devin => devin_callback(&q, tx),
        };
    }
    if path == "/success" && matches!(flavor, Flavor::Claude | Flavor::Codex) {
        let setup_required = q("setup_required") == "true";
        let platform = q("platform_url");
        return Response::html(200, success_page(flavor, setup_required, &platform));
    }
    Response::text(404, "404 page not found")
}

fn send(tx: &mpsc::Sender<OAuthResult>, result: OAuthResult) {
    if tx.try_send(result).is_err() {
        tracing::warn!("OAuth result channel is full, result dropped");
    }
}

/// Claude and Codex share the same handler.
fn browser_callback(
    method: &str,
    code: &str,
    state: &str,
    error: &str,
    tx: &mpsc::Sender<OAuthResult>,
) -> Response {
    if method != "GET" {
        return Response::text(405, "Method not allowed");
    }
    if !error.is_empty() {
        tracing::error!("OAuth error received: {error}");
        send(
            tx,
            OAuthResult {
                error: error.to_string(),
                ..Default::default()
            },
        );
        return Response::text(400, format!("OAuth error: {error}"));
    }
    if code.is_empty() {
        tracing::error!("No authorization code received");
        send(
            tx,
            OAuthResult {
                error: "no_code".into(),
                ..Default::default()
            },
        );
        return Response::text(400, "No authorization code received");
    }
    if state.is_empty() {
        tracing::error!("No state parameter received");
        send(
            tx,
            OAuthResult {
                error: "no_state".into(),
                ..Default::default()
            },
        );
        return Response::text(400, "No state parameter received");
    }
    send(
        tx,
        OAuthResult {
            code: code.to_string(),
            state: state.to_string(),
            error: String::new(),
        },
    );
    Response::redirect("/success")
}

fn antigravity_callback(q: &dyn Fn(&str) -> String, tx: &mpsc::Sender<OAuthResult>) -> Response {
    let result = OAuthResult {
        code: q("code").trim().to_string(),
        error: q("error").trim().to_string(),
        state: q("state").trim().to_string(),
    };
    let ok = !result.code.is_empty() && result.error.is_empty();
    send(tx, result);
    if ok {
        Response::html(
            200,
            "<h1>Login successful</h1><p>You can close this window.</p>".into(),
        )
    } else {
        Response::html(
            200,
            "<h1>Login failed</h1><p>Please check the CLI output.</p>".into(),
        )
    }
}

fn devin_callback(q: &dyn Fn(&str) -> String, tx: &mpsc::Sender<OAuthResult>) -> Response {
    let code = q("code").trim().to_string();
    let state = q("state").trim().to_string();
    let err = q("error").trim().to_string();
    let desc = q("error_description").trim().to_string();
    if !err.is_empty() || code.is_empty() {
        let mut msg = err.clone();
        if !desc.is_empty() {
            msg = format!("{err}: {desc}");
        }
        if msg.is_empty() {
            msg = "missing authorization code".into();
        }
        send(
            tx,
            OAuthResult {
                error: msg.clone(),
                ..Default::default()
            },
        );
        return Response::html(400, DEVIN_FAILURE_HTML.replace("%s", &html_escape(&msg)));
    }
    send(
        tx,
        OAuthResult {
            code,
            state,
            error: String::new(),
        },
    );
    Response::html(200, DEVIN_SUCCESS_HTML.to_string())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&#34;")
        .replace('\'', "&#39;")
}

/// Success page with `{{PLATFORM_URL}}` / `{{SETUP_NOTICE}}` substituted.
fn success_page(flavor: Flavor, setup_required: bool, platform_url: &str) -> String {
    let (page, notice, default_url) = match flavor {
        Flavor::Codex => (
            CODEX_SUCCESS_HTML,
            CODEX_SETUP_NOTICE,
            "https://platform.openai.com",
        ),
        _ => (
            CLAUDE_SUCCESS_HTML,
            CLAUDE_SETUP_NOTICE,
            "https://console.anthropic.com/",
        ),
    };
    let platform = if platform_url.is_empty() {
        default_url.to_string()
    } else {
        html_escape(platform_url)
    };
    let html = page.replace("{{PLATFORM_URL}}", &platform);
    if setup_required {
        html.replacen(
            "{{SETUP_NOTICE}}",
            &notice.replace("{{PLATFORM_URL}}", &platform),
            1,
        )
    } else {
        html.replacen("{{SETUP_NOTICE}}", "", 1)
    }
}

const DEVIN_SUCCESS_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>Authentication Successful - Devin</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #0f172a; color: #f8fafc; }
        .card { background: #1e293b; padding: 2.5rem; border-radius: 12px; box-shadow: 0 8px 30px rgba(0,0,0,0.4); text-align: center; max-width: 420px; }
        h2 { margin-top: 0; color: #38bdf8; }
        p { color: #94a3b8; font-size: 15px; }
    </style>
</head>
<body>
    <div class="card">
        <h2>Authentication Complete</h2>
        <p>You have successfully logged in to Devin via CLIProxyAPI.</p>
        <p>You may safely close this window and return to your terminal.</p>
    </div>
</body>
</html>"#;

const DEVIN_FAILURE_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>Authentication Failed - Devin</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #0f172a; color: #f8fafc; }
        .card { background: #1e293b; padding: 2.5rem; border-radius: 12px; box-shadow: 0 8px 30px rgba(0,0,0,0.4); text-align: center; max-width: 420px; }
        h2 { margin-top: 0; color: #f87171; }
        p { color: #94a3b8; font-size: 15px; }
    </style>
</head>
<body>
    <div class="card">
        <h2>Authentication Failed</h2>
        <p>Devin authentication encountered an error: %s</p>
        <p>Please check your terminal and try again.</p>
    </div>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[test]
    fn forward_location_preserves_query() {
        assert_eq!(
            forward_location(
                "http://127.0.0.1:8317/codex/callback",
                "/auth/callback?code=a&state=b"
            ),
            "http://127.0.0.1:8317/codex/callback?code=a&state=b"
        );
        assert_eq!(
            forward_location("http://h/cb?x=1", "/p?code=a"),
            "http://h/cb?x=1&code=a"
        );
        assert_eq!(forward_location("http://h/cb", "/p"), "http://h/cb");
    }

    #[tokio::test]
    async fn forwarder_redirects_with_query() {
        let port = free_port();
        let fwd = CallbackForwarder::start(port, "http://127.0.0.1:9/anthropic/callback")
            .await
            .unwrap();
        assert_eq!(fwd.port(), port);
        let resp = get(port, "/callback?code=c&state=s").await;
        assert!(resp.starts_with("HTTP/1.1 302"), "{resp}");
        assert!(resp.contains("Location: http://127.0.0.1:9/anthropic/callback?code=c&state=s"));
        assert!(resp.contains("Cache-Control: no-store"));
    }

    #[tokio::test]
    async fn devin_flavor_delivers_result_on_loopback() {
        let mut server = CallbackServer::start(Flavor::Devin, 0)
            .await
            .unwrap_or_else(|_| unreachable!());
        let port = server.port();
        // Devin flavor serves /callback on loopback with an ephemeral port.
        let resp = get(port, "/callback?code=abc&state=xyz").await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        let r = server.wait(Duration::from_secs(2)).await.unwrap();
        assert_eq!(
            (r.code.as_str(), r.state.as_str(), r.error.as_str()),
            ("abc", "xyz", "")
        );
    }

    #[tokio::test]
    async fn codex_flavor_errors_and_success_page() {
        // Port 0 means "default port" for non-Devin flavors; use a dedicated high port instead.
        let port = free_port();
        let mut server = CallbackServer::start(Flavor::Codex, port).await.unwrap();
        let resp = get(port, "/auth/callback?code=c1&state=s1").await;
        assert!(resp.starts_with("HTTP/1.1 302"), "{resp}");
        assert!(resp.contains("Location: /success"));
        let r = server.wait(Duration::from_secs(2)).await.unwrap();
        assert_eq!(r.code, "c1");

        let resp = get(port, "/success?setup_required=true").await;
        assert!(resp.contains("Authentication Successful - Codex"), "{resp}");
        assert!(resp.contains("https://platform.openai.com"));

        let resp = get(port, "/auth/callback?code=c2").await;
        assert!(resp.starts_with("HTTP/1.1 400") && resp.contains("No state parameter received"));
        assert_eq!(
            server.wait(Duration::from_secs(2)).await.unwrap().error,
            "no_state"
        );
    }

    #[tokio::test]
    async fn port_in_use_is_reported() {
        let port = free_port();
        let _first = CallbackServer::start(Flavor::Claude, port).await.unwrap();
        match CallbackServer::start(Flavor::Claude, port).await {
            Err(AuthFlowError::Authentication {
                kind: AuthErrorKind::PortInUse,
                ..
            }) => {}
            other => panic!("expected PortInUse, got {:?}", other.err()),
        }
    }

    #[tokio::test]
    async fn wait_times_out_with_callback_timeout() {
        let mut server = CallbackServer::start(Flavor::Devin, 0).await.unwrap();
        match server.wait(Duration::from_millis(50)).await {
            Err(AuthFlowError::Authentication {
                kind: AuthErrorKind::CallbackTimeout,
                ..
            }) => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
}

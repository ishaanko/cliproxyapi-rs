//! Optional profiling endpoint (Go: sdk/cliproxy/pprof_server.go plus net/http/pprof).
//!
//! `pprof.enable` serves `/debug/pprof/` on `pprof.addr` (default `127.0.0.1:8316`); the server
//! starts, restarts on an address change and stops as the config is reloaded.
//!
//! What this build serves: the index, `cmdline`, `symbol` (always `num_symbols: 1`, no address
//! resolution) and `profile` (a gzip-compressed CPU profile in the pprof protobuf format, sampled
//! at 100 Hz with pprof-rs). Go-runtime profiles (`heap`, `allocs`, `goroutine`, `block`, `mutex`,
//! `threadcreate`, `trace`) have no Rust counterpart and answer 501. Unlike Go there is no
//! read-header timeout, and a server that does not drain within 5 seconds is aborted rather than
//! left serving its open connections.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Method, Response, StatusCode, Uri, header};
use cpa_config::Config;
use cpa_config::DEFAULT_PPROF_ADDR;
use cpa_discovery::Ctx;
use parking_lot::Mutex;
use tokio::task::JoinHandle;

use super::discovery::Signal;

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// One listening server instance.
pub struct RunningServer {
    shutdown: Signal,
    done: Signal,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl RunningServer {
    /// A server that never started serving (its shutdown completes immediately).
    #[cfg(test)]
    fn idle() -> Arc<Self> {
        let done = Signal::new();
        done.fire();
        Arc::new(Self { shutdown: Signal::new(), done, task: Mutex::new(None) })
    }

    /// Go `http.Server.Shutdown` with a 5 second budget. Errors when the budget (or `ctx`) runs out.
    async fn stop(&self, ctx: &Ctx) -> Result<(), String> {
        self.shutdown.fire();
        let finished = tokio::select! {
            _ = self.done.wait() => Ok(()),
            _ = tokio::time::sleep(STOP_TIMEOUT) => Err("context deadline exceeded".to_string()),
            err = ctx.done() => Err(err.to_string()),
        };
        if finished.is_err()
            && let Some(task) = self.task.lock().take()
        {
            task.abort();
        }
        finished
    }
}

struct PprofState {
    server: Option<Arc<RunningServer>>,
    addr: String,
    enabled: bool,
    owner: u64,
}

/// Starts, restarts and stops the profiling server to follow `pprof.*` config.
#[derive(Clone)]
pub struct PprofServer {
    state: Arc<Mutex<PprofState>>,
}

impl Default for PprofServer {
    fn default() -> Self {
        Self::new()
    }
}

impl PprofServer {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(PprofState { server: None, addr: String::new(), enabled: false, owner: 0 })) }
    }

    /// Reconciles the server with `cfg`. Returns false when `ctx` ended or a stop failed.
    pub async fn apply(&self, ctx: &Ctx, cfg: &Config) -> bool {
        if ctx.err().is_some() {
            return false;
        }
        let addr = cfg.pprof.addr.trim();
        let addr = if addr.is_empty() { DEFAULT_PPROF_ADDR } else { addr }.to_string();
        let enabled = cfg.pprof.enable;

        let (owner, current_server, current_addr) = {
            let mut st = self.state.lock();
            st.owner += 1;
            let owner = st.owner;
            let current_server = st.server.clone();
            let current_addr = std::mem::replace(&mut st.addr, addr.clone());
            st.enabled = enabled;
            if !enabled {
                st.server = None;
            } else if current_server.is_some() && current_addr == addr {
                drop(st);
                return ctx.err().is_none();
            } else {
                st.server = None;
            }
            (owner, current_server, current_addr)
        };

        if !enabled {
            if let Some(server) = current_server
                && self.stop_server(ctx, &server, &current_addr, "disabled").await.is_err()
            {
                return false;
            }
            return ctx.err().is_none();
        }

        if let Some(server) = current_server
            && self.stop_server(ctx, &server, &current_addr, "restarted").await.is_err()
        {
            return false;
        }
        if ctx.err().is_some() {
            return false;
        }

        let started = self.start_server(&addr, owner);
        if ctx.err().is_some() {
            if let Some(started) = started {
                let this = self.clone();
                tokio::spawn(async move {
                    let _ = this.stop_owned_server(&Ctx::background(), &started, &addr, "canceled", owner).await;
                });
            }
            return false;
        }
        true
    }

    /// Stops the server for good; later applies may start a new one.
    pub async fn shutdown(&self, ctx: &Ctx) -> Result<(), String> {
        let (server, addr) = {
            let mut st = self.state.lock();
            let server = st.server.take();
            st.owner += 1;
            st.enabled = false;
            (server, st.addr.clone())
        };
        match server {
            Some(server) => self.stop_server(ctx, &server, &addr, "shutdown").await,
            None => Ok(()),
        }
    }

    /// Registers a new server for `addr` if this apply still owns the state, then serves it in the
    /// background. A bind or serve failure is logged and clears the registration.
    fn start_server(&self, addr: &str, owner: u64) -> Option<Arc<RunningServer>> {
        let server = Arc::new(RunningServer {
            shutdown: Signal::new(),
            done: Signal::new(),
            task: Mutex::new(None),
        });
        {
            let mut st = self.state.lock();
            if !st.enabled || st.addr != addr || st.owner != owner || st.server.is_some() {
                return None;
            }
            st.server = Some(server.clone());
        }
        tracing::info!("pprof server starting on {addr}");
        let this = self.clone();
        let task_server = server.clone();
        let addr = addr.to_string();
        let handle = tokio::spawn(async move {
            if let Err(err) = serve(&addr, task_server.shutdown.clone()).await {
                tracing::error!("pprof server failed on {addr}: {err}");
                this.clear_failed_server(&task_server);
            }
            task_server.done.fire();
        });
        *server.task.lock() = Some(handle);
        Some(server)
    }

    /// Removes a failed server even if a same-address apply took over ownership while it was
    /// still binding.
    fn clear_failed_server(&self, server: &Arc<RunningServer>) {
        let mut st = self.state.lock();
        if st.server.as_ref().is_some_and(|s| Arc::ptr_eq(s, server)) {
            st.server = None;
        }
    }

    /// Stops `server` only if it is still the registered one and `owner` still owns it.
    async fn stop_owned_server(&self, ctx: &Ctx, server: &Arc<RunningServer>, addr: &str, reason: &str, owner: u64) -> Result<(), String> {
        {
            let mut st = self.state.lock();
            if !st.server.as_ref().is_some_and(|s| Arc::ptr_eq(s, server)) || st.owner != owner {
                return Ok(());
            }
            st.server = None;
        }
        self.stop_server(ctx, server, addr, reason).await
    }

    async fn stop_server(&self, ctx: &Ctx, server: &Arc<RunningServer>, addr: &str, reason: &str) -> Result<(), String> {
        match server.stop(ctx).await {
            Ok(()) => {
                tracing::info!("pprof server stopped on {addr} ({reason})");
                Ok(())
            }
            Err(err) => {
                tracing::error!("pprof server stop failed on {addr}: {err}");
                Err(err)
            }
        }
    }
}

impl Drop for PprofState {
    fn drop(&mut self) {
        if let Some(task) = self.server.take().and_then(|s| s.task.lock().take()) {
            task.abort();
        }
    }
}

/// Resolves a Go-style listen address (`host:port`, `:port`) to a bound listener.
async fn bind(addr: &str) -> std::io::Result<tokio::net::TcpListener> {
    if let Some(port) = addr.strip_prefix(':') {
        // Go's ":6060" listens on every interface, dual stack when available.
        if let Ok(listener) = tokio::net::TcpListener::bind(format!("[::]:{port}")).await {
            return Ok(listener);
        }
        return tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await;
    }
    tokio::net::TcpListener::bind(addr).await
}

async fn serve(addr: &str, shutdown: Signal) -> std::io::Result<()> {
    let listener = bind(addr).await?;
    let app = router();
    axum::serve(listener, app).with_graceful_shutdown(async move { shutdown.wait().await }).await
}

// ---------------------------------------------------------------------------------------------
// HTTP handlers (Go: net/http/pprof)
// ---------------------------------------------------------------------------------------------

/// Profiles Go registers under `/debug/pprof/<name>` that this build cannot produce.
const GO_RUNTIME_PROFILES: &[&str] = &["allocs", "block", "goroutine", "heap", "mutex", "threadcreate"];

fn router() -> Router {
    Router::new().fallback(handle)
}

async fn handle(method: Method, uri: Uri) -> Response<Body> {
    let path = uri.path();
    let query = parse_query(uri.query().unwrap_or(""));
    match path {
        "/debug/pprof" => redirect("/debug/pprof/", &method),
        "/debug/pprof/" => index(),
        "/debug/pprof/cmdline" => cmdline(),
        "/debug/pprof/profile" => cpu_profile(&query).await,
        "/debug/pprof/symbol" => symbol(),
        "/debug/pprof/trace" => serve_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not enable tracing: execution traces are not supported by this build"),
        _ => match path.strip_prefix("/debug/pprof/") {
            Some(name) if GO_RUNTIME_PROFILES.contains(&name) => {
                serve_error(StatusCode::NOT_IMPLEMENTED, &format!("profile {name:?} is not supported by this build"))
            }
            Some(_) => serve_error(StatusCode::NOT_FOUND, "Unknown profile"),
            None => text_response(StatusCode::NOT_FOUND, "404 page not found\n"),
        },
    }
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(raw.as_bytes()).into_owned().collect()
}

fn text_response(status: StatusCode, body: &str) -> Response<Body> {
    let mut resp = Response::new(Body::from(body.to_string()));
    *resp.status_mut() = status;
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    resp.headers_mut().insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    resp
}

/// Go `serveError`.
fn serve_error(status: StatusCode, text: &str) -> Response<Body> {
    let mut resp = text_response(status, &format!("{text}\n"));
    resp.headers_mut().insert("X-Go-Pprof", HeaderValue::from_static("1"));
    resp
}

/// Go `http.Redirect` with a permanent status (the ServeMux slash redirect).
fn redirect(to: &str, method: &Method) -> Response<Body> {
    let body = if method == Method::GET || method == Method::HEAD {
        format!("<a href=\"{to}\">Moved Permanently</a>.\n\n")
    } else {
        String::new()
    };
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = StatusCode::MOVED_PERMANENTLY;
    resp.headers_mut().insert(header::LOCATION, HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/")));
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    resp
}

fn cmdline() -> Response<Body> {
    let args: Vec<String> = std::env::args().collect();
    let mut resp = text_response(StatusCode::OK, &args.join("\0"));
    resp.headers_mut().insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    resp
}

fn symbol() -> Response<Body> {
    text_response(StatusCode::OK, "num_symbols: 1\n")
}

fn index() -> Response<Body> {
    let profiles = [
        ("cmdline", "The command line invocation of the current program"),
        (
            "profile",
            "CPU profile. You can specify the duration in the seconds GET parameter. After you get the profile file, use the go tool pprof command to investigate the profile.",
        ),
        (
            "symbol",
            "Maps given program counters to function names. Counters can be specified in a GET raw query or POST body, multiple counters are separated by '+'.",
        ),
    ];
    let mut b = String::from(
        "<html>\n<head>\n<title>/debug/pprof/</title>\n<style>\n.profile-name{\n\tdisplay:inline-block;\n\twidth:6rem;\n}\n</style>\n</head>\n<body>\n/debug/pprof/\n<br>\n<p>Set debug=1 as a query parameter to export in legacy text format</p>\n<br>\nTypes of profiles available:\n<table>\n<thead><td>Count</td><td>Profile</td></thead>\n",
    );
    for (name, _) in &profiles {
        b.push_str(&format!("<tr><td>0</td><td><a href='{name}?debug=1'>{name}</a></td></tr>\n"));
    }
    b.push_str("</table>\n<br>\n<p>\nProfile Descriptions:\n<ul>\n");
    for (name, desc) in &profiles {
        b.push_str(&format!("<li><div class=profile-name>{name}: </div> {desc}</li>\n"));
    }
    b.push_str("</ul>\n</p>\n</body>\n</html>");
    let mut resp = Response::new(Body::from(b));
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    resp.headers_mut().insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    resp
}

/// Only one CPU profile at a time (Go: "cpu profiling already in use").
static CPU_PROFILING: AtomicBool = AtomicBool::new(false);

/// Clears the in-use flag and tells the sampling thread to stop early when the request is dropped.
struct ProfileSession {
    cancel: Arc<AtomicBool>,
}

impl Drop for ProfileSession {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

async fn cpu_profile(query: &HashMap<String, String>) -> Response<Body> {
    let sec = match query.get("seconds").and_then(|s| s.parse::<i64>().ok()) {
        Some(sec) if sec > 0 => sec,
        _ => 30,
    };
    if CPU_PROFILING.swap(true, Ordering::SeqCst) {
        return serve_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not enable CPU profiling: cpu profiling already in use");
    }
    let session = ProfileSession { cancel: Arc::new(AtomicBool::new(false)) };
    let cancel = session.cancel.clone();
    let result = tokio::task::spawn_blocking(move || {
        let outcome = sample_cpu(Duration::from_secs(sec as u64), &cancel);
        CPU_PROFILING.store(false, Ordering::SeqCst);
        outcome
    })
    .await;
    drop(session);
    match result {
        Ok(Ok(bytes)) => {
            let mut resp = Response::new(Body::from(bytes));
            let headers = resp.headers_mut();
            headers.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            headers.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"profile\""));
            resp
        }
        Ok(Err(err)) => serve_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Could not enable CPU profiling: {err}")),
        Err(err) => serve_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Could not enable CPU profiling: {err}")),
    }
}

/// Samples the process for `duration` (or until `cancel`) and returns a gzip-compressed pprof
/// protobuf, which is what `go tool pprof` expects from `/debug/pprof/profile`.
fn sample_cpu(duration: Duration, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    use std::io::Write;

    use pprof::protos::Message;

    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(100)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .map_err(|e| e.to_string())?;
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let report = guard.report().build().map_err(|e| e.to_string())?;
    let profile = report.pprof().map_err(|e| e.to_string())?;
    let raw = profile.write_to_bytes().map_err(|e| e.to_string())?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&raw).map_err(|e| e.to_string())?;
    gz.finish().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(pprof: &PprofServer, server: &Arc<RunningServer>) {
        pprof.state.lock().server = Some(server.clone());
    }

    fn is_current(pprof: &PprofServer, server: &Arc<RunningServer>) -> bool {
        pprof.state.lock().server.as_ref().is_some_and(|s| Arc::ptr_eq(s, server))
    }

    #[tokio::test]
    async fn stop_owned_server_keeps_replacement() {
        let pprof = PprofServer::new();
        let old = RunningServer::idle();
        let replacement = RunningServer::idle();
        registered(&pprof, &replacement);
        pprof.stop_owned_server(&Ctx::background(), &old, "old", "canceled", 1).await.unwrap();
        assert!(is_current(&pprof, &replacement), "stopping a stale server removed the replacement");
    }

    #[tokio::test]
    async fn same_server_owner_transfer_keeps_current_server() {
        let pprof = PprofServer::new();
        let server = RunningServer::idle();
        {
            let mut st = pprof.state.lock();
            st.server = Some(server.clone());
            st.addr = "127.0.0.1:6060".into();
            st.enabled = true;
            st.owner = 1;
        }
        let mut cfg = Config::default();
        cfg.pprof.enable = true;
        cfg.pprof.addr = "127.0.0.1:6060".into();
        assert!(pprof.apply(&Ctx::background(), &cfg).await, "same-address apply must transfer ownership");
        assert_ne!(pprof.state.lock().owner, 1, "apply did not transfer ownership");
        pprof.stop_owned_server(&Ctx::background(), &server, "127.0.0.1:6060", "canceled", 1).await.unwrap();
        assert!(is_current(&pprof, &server), "stale owner stopped the current server");
    }

    #[tokio::test]
    async fn serve_failure_clears_transferred_owner() {
        let pprof = PprofServer::new();
        let server = RunningServer::idle();
        {
            let mut st = pprof.state.lock();
            st.server = Some(server.clone());
            st.owner = 2;
        }
        pprof.clear_failed_server(&server);
        assert!(pprof.state.lock().server.is_none());
    }

    /// Full lifecycle on a loopback port: enable serves, an address change restarts, disable stops.
    #[tokio::test]
    async fn serves_index_cmdline_and_unsupported_profiles() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn get(addr: &str, path: &str) -> String {
            let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
            conn.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let mut out = String::new();
            conn.read_to_string(&mut out).await.unwrap();
            out
        }

        // Reserve a free port, then hand it to the server.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap().to_string();
        drop(probe);

        let pprof = PprofServer::new();
        let mut cfg = Config::default();
        cfg.pprof.enable = true;
        cfg.pprof.addr = addr.clone();
        assert!(pprof.apply(&Ctx::background(), &cfg).await);
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let index = get(&addr, "/debug/pprof/").await;
        assert!(index.starts_with("HTTP/1.1 200"), "{index}");
        assert!(index.contains("Types of profiles available:"));
        assert!(get(&addr, "/debug/pprof").await.starts_with("HTTP/1.1 301"));
        assert!(get(&addr, "/debug/pprof/cmdline").await.contains("200 OK"));
        assert!(get(&addr, "/debug/pprof/symbol").await.ends_with("num_symbols: 1\n"));
        assert!(get(&addr, "/debug/pprof/heap").await.starts_with("HTTP/1.1 501"));
        let unknown = get(&addr, "/debug/pprof/nope").await;
        assert!(unknown.starts_with("HTTP/1.1 404") && unknown.ends_with("Unknown profile\n"), "{unknown}");
        assert!(get(&addr, "/elsewhere").await.ends_with("404 page not found\n"));

        cfg.pprof.enable = false;
        assert!(pprof.apply(&Ctx::background(), &cfg).await);
        assert!(tokio::net::TcpStream::connect(&addr).await.is_err(), "server still listening after disable");
        pprof.shutdown(&Ctx::background()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cpu_profile_is_gzip_protobuf() {
        let resp = cpu_profile(&HashMap::from([("seconds".to_string(), "1".to_string())])).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap();
        assert_eq!(&body[..2], &[0x1f, 0x8b], "profile is not gzip");
    }
}

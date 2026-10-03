//! Closed-loop HTTP/1.1 load generator: `conc` keep-alive connections, each sending the next
//! request as soon as the previous response is fully read.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::{self, SendRequest};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use memchr::memmem;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::WebSocketStream;

/// One request template, sent repeatedly.
#[derive(Clone)]
pub struct Target {
    pub addr: SocketAddr,
    pub method: Method,
    pub path: String,
    pub body: Bytes,
    /// Websocket target: the body is sent as one text message per request and the response ends
    /// at the first message containing `response.completed`.
    pub ws: bool,
}

impl Target {
    fn request(&self) -> Result<Request<Full<Bytes>>> {
        Request::builder()
            .method(self.method.clone())
            .uri(&self.path)
            .header("host", self.addr.to_string())
            .header("authorization", "Bearer bench-client-key")
            .header("content-type", "application/json")
            .body(Full::new(self.body.clone()))
            .context("build request")
    }
}

pub fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

async fn connect(addr: SocketAddr) -> Result<SendRequest<Full<Bytes>>> {
    let sock = TcpStream::connect(addr).await?;
    sock.set_nodelay(true)?;
    let (sender, conn) = http1::handshake(TokioIo::new(sock)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

/// Opens a websocket to the target (Nagle off) with the bench credentials.
async fn ws_connect(t: &Target) -> Result<WebSocketStream<TcpStream>> {
    let mut req = format!("ws://{}{}", t.addr, t.path).into_client_request()?;
    req.headers_mut().insert("authorization", "Bearer bench-client-key".parse()?);
    let sock = TcpStream::connect(t.addr).await?;
    sock.set_nodelay(true)?;
    let (ws, _) = tokio_tungstenite::client_async(req, sock).await?;
    Ok(ws)
}

/// One websocket request: sends the body and reads until the completion event. Returns the
/// concatenated text frames.
async fn ws_exchange(ws: &mut WebSocketStream<TcpStream>, t: &Target) -> Result<Vec<u8>> {
    let text = String::from_utf8_lossy(&t.body).into_owned();
    ws.send(Message::text(text)).await?;
    let done = memmem::Finder::new(b"response.completed");
    let mut all = Vec::new();
    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Text(m) => {
                let end = done.find(m.as_bytes()).is_some();
                all.extend_from_slice(m.as_bytes());
                if end {
                    return Ok(all);
                }
                if memmem::find(m.as_bytes(), b"\"type\":\"error\"").is_some() {
                    bail!("ws error event: {}", String::from_utf8_lossy(&all));
                }
            }
            Message::Close(_) => bail!("ws closed mid-response"),
            _ => {}
        }
    }
    bail!("ws ended mid-response")
}

/// Idle keep-alive connections per destination, shared by every phase of a run. The load
/// generator never opens a connection per request, and it reuses connections across cells and
/// scenarios too: the host has only ~4000 ephemeral ports, and every closed connection leaves a
/// TIME-WAIT socket for 60 s. A full run therefore opens about `max concurrency` connections per
/// server (plus the server's own upstream connections), not one per cell.
#[derive(Default)]
struct Pool {
    idle: Mutex<Vec<SendRequest<Full<Bytes>>>>,
}

impl Pool {
    fn take(&self) -> Option<SendRequest<Full<Bytes>>> {
        let mut idle = self.idle.lock().ok()?;
        while let Some(s) = idle.pop() {
            if !s.is_closed() {
                return Some(s);
            }
        }
        None
    }

    fn put(&self, s: SendRequest<Full<Bytes>>) {
        if let (false, Ok(mut idle)) = (s.is_closed(), self.idle.lock()) {
            idle.push(s);
        }
    }
}

static POOLS: OnceLock<Mutex<HashMap<SocketAddr, Arc<Pool>>>> = OnceLock::new();

fn pool_for(addr: SocketAddr) -> Arc<Pool> {
    let map = POOLS.get_or_init(Default::default);
    match map.lock() {
        Ok(mut m) => m.entry(addr).or_default().clone(),
        Err(_) => Arc::default(),
    }
}

/// Drops every idle pooled connection (call when a server under test goes away).
pub fn clear_pools() {
    if let Some(Ok(mut m)) = POOLS.get().map(Mutex::lock) {
        m.clear();
    }
}

/// Sends the request once and returns status and body (used for pre-flight checks and mock
/// control). Uses a pooled connection.
pub async fn once(t: &Target) -> Result<(u16, Bytes)> {
    if t.ws {
        let mut ws = ws_connect(t).await?;
        let out = ws_exchange(&mut ws, t).await?;
        let _ = ws.close(None).await;
        return Ok((200, Bytes::from(out)));
    }
    let pool = pool_for(t.addr);
    let mut sender = match pool.take() {
        Some(s) => s,
        None => connect(t.addr).await?,
    };
    let resp = sender.send_request(t.request()?).await?;
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await?.to_bytes();
    pool.put(sender);
    Ok((status, body))
}

/// Flags shared between the orchestrator and the workers.
pub struct Shared {
    /// Samples are recorded only for requests that start and finish while this is set.
    pub measure: AtomicBool,
    pub stop: AtomicBool,
    /// Fixed-count mode (`Running::start_counted`): requests still to be started.
    pub budget: AtomicI64,
    pub limited: bool,
}

#[derive(Default)]
pub struct WorkerOut {
    pub lat_us: Vec<u32>,
    pub errors: u64,
    /// Stream timing (only collected with `timing`): first body byte, first marked delta.
    pub ttfb_us: Vec<u32>,
    pub ttft_us: Vec<u32>,
    /// Age of each marked delta when the client read it.
    pub chunk_us: Vec<u32>,
}

/// Finds every `@@<16 digit unix micros>` marker in `buf` and pushes its age at `now`.
fn scan_markers(buf: &[u8], now: u64, out: &mut Vec<u32>) {
    let mut from = 0;
    while let Some(pos) = memmem::find(&buf[from..], b"@@") {
        let start = from + pos + 2;
        from = start;
        let Some(digits) = buf.get(start..start + 16) else { break };
        if !digits.iter().all(u8::is_ascii_digit) {
            continue;
        }
        let ts = digits.iter().fold(0u64, |acc, d| acc * 10 + u64::from(d - b'0'));
        out.push(now.saturating_sub(ts).min(u64::from(u32::MAX)) as u32);
    }
}

/// One request/response exchange. Returns the status and, with `timing`, fills the stream samples.
async fn exchange(
    sender: &mut SendRequest<Full<Bytes>>,
    t: &Target,
    timing: bool,
    t0: Instant,
    sample: &mut WorkerOut,
) -> Result<u16> {
    let resp = sender.send_request(t.request()?).await?;
    let status = resp.status().as_u16();
    let mut body = resp.into_body();
    let mut first = true;
    let mut first_marker = true;
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        let Some(data) = frame.data_ref() else { continue };
        if !timing || data.is_empty() {
            continue;
        }
        let now = Instant::now();
        if first {
            first = false;
            sample.ttfb_us.push((now - t0).as_micros() as u32);
        }
        let before = sample.chunk_us.len();
        scan_markers(data, now_us(), &mut sample.chunk_us);
        if first_marker && sample.chunk_us.len() > before {
            first_marker = false;
            sample.ttft_us.push((now - t0).as_micros() as u32);
        }
    }
    Ok(status)
}

/// Websocket flavour of `worker`: one connection per worker, one `response.create` per request.
async fn ws_worker(t: Arc<Target>, sh: Arc<Shared>) -> WorkerOut {
    let mut out = WorkerOut::default();
    let mut conn = None;
    while !sh.stop.load(Ordering::Relaxed) {
        if sh.limited && sh.budget.fetch_sub(1, Ordering::Relaxed) <= 0 {
            break;
        }
        if conn.is_none() {
            match ws_connect(&t).await {
                Ok(c) => conn = Some(c),
                Err(_) => {
                    if sh.measure.load(Ordering::Relaxed) {
                        out.errors += 1;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
            }
        }
        let Some(ws) = conn.as_mut() else { continue };
        let started_measuring = sh.measure.load(Ordering::Relaxed);
        let t0 = Instant::now();
        let result = ws_exchange(ws, &t).await;
        let elapsed = t0.elapsed();
        if !(started_measuring && sh.measure.load(Ordering::Relaxed)) {
            continue;
        }
        match result {
            Ok(_) => out.lat_us.push(elapsed.as_micros().min(u128::from(u32::MAX)) as u32),
            Err(e) => {
                if std::env::var_os("CPA_BENCH_DEBUG").is_some() {
                    eprintln!("ws request error: {e}");
                }
                out.errors += 1;
                conn = None;
            }
        }
    }
    out
}

async fn worker(t: Arc<Target>, sh: Arc<Shared>, timing: bool) -> WorkerOut {
    if t.ws {
        return ws_worker(t, sh).await;
    }
    let mut out = WorkerOut::default();
    let pool = pool_for(t.addr);
    let mut sender: Option<SendRequest<Full<Bytes>>> = pool.take();
    while !sh.stop.load(Ordering::Relaxed) {
        if sh.limited && sh.budget.fetch_sub(1, Ordering::Relaxed) <= 0 {
            break;
        }
        if sender.as_ref().is_none_or(SendRequest::is_closed) {
            match connect(t.addr).await {
                Ok(s) => sender = Some(s),
                Err(_) => {
                    if sh.measure.load(Ordering::Relaxed) {
                        out.errors += 1;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
            }
        }
        let Some(s) = sender.as_mut() else { continue };
        let started_measuring = sh.measure.load(Ordering::Relaxed);
        let t0 = Instant::now();
        let mut sample = WorkerOut::default();
        let result = exchange(s, &t, timing, t0, &mut sample).await;
        let elapsed = t0.elapsed();
        if !(started_measuring && sh.measure.load(Ordering::Relaxed)) {
            continue;
        }
        match result {
            Ok(200) => {
                out.lat_us.push(elapsed.as_micros().min(u128::from(u32::MAX)) as u32);
                out.ttfb_us.append(&mut sample.ttfb_us);
                out.ttft_us.append(&mut sample.ttft_us);
                out.chunk_us.append(&mut sample.chunk_us);
            }
            other => {
                if std::env::var_os("CPA_BENCH_DEBUG").is_some() {
                    eprintln!("request error: {other:?}");
                }
                out.errors += 1;
                sender = None;
            }
        }
    }
    if let Some(s) = sender {
        pool.put(s);
    }
    out
}

/// Handles to a running load: workers keep going until `finish`.
pub struct Running {
    shared: Arc<Shared>,
    tasks: Vec<tokio::task::JoinHandle<WorkerOut>>,
}

impl Running {
    pub fn start(t: &Target, conc: usize, timing: bool) -> Self {
        Self::spawn(t, conc, timing, None)
    }

    /// Fixed request count instead of a time window: every request counts and workers stop on
    /// their own; use `join` to wait for them. Gives load-insensitive per-request metrics.
    pub fn start_counted(t: &Target, conc: usize, timing: bool, requests: u64) -> Self {
        Self::spawn(t, conc, timing, Some(requests))
    }

    fn spawn(t: &Target, conc: usize, timing: bool, limit: Option<u64>) -> Self {
        let shared = Arc::new(Shared {
            measure: AtomicBool::new(limit.is_some()),
            stop: AtomicBool::new(false),
            budget: AtomicI64::new(limit.map_or(i64::MAX, |n| n as i64)),
            limited: limit.is_some(),
        });
        let t = Arc::new(t.clone());
        let tasks = (0..conc).map(|_| tokio::spawn(worker(t.clone(), shared.clone(), timing))).collect();
        Running { shared, tasks }
    }

    pub fn set_measuring(&self, on: bool) {
        self.shared.measure.store(on, Ordering::SeqCst);
    }

    /// Stops the workers and merges their samples.
    pub async fn finish(self) -> Result<WorkerOut> {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.join().await
    }

    /// Waits for the workers to end (after `stop` or an exhausted budget) and merges their samples.
    pub async fn join(self) -> Result<WorkerOut> {
        let mut all = WorkerOut::default();
        for task in self.tasks {
            match tokio::time::timeout(Duration::from_secs(60), task).await {
                Ok(Ok(mut w)) => {
                    all.lat_us.append(&mut w.lat_us);
                    all.ttfb_us.append(&mut w.ttfb_us);
                    all.ttft_us.append(&mut w.ttft_us);
                    all.chunk_us.append(&mut w.chunk_us);
                    all.errors += w.errors;
                }
                _ => bail!("load worker did not finish"),
            }
        }
        Ok(all)
    }
}

/// Nearest-rank percentile of an unsorted sample (0 when empty).
pub fn percentile(v: &mut [u32], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let idx = ((p * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1;
    u64::from(v[idx])
}

/// Serialized percentile triple in microseconds.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Pcts {
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
}

pub fn pcts(v: &mut [u32]) -> Pcts {
    Pcts { p50: percentile(v, 0.50), p90: percentile(v, 0.90), p99: percentile(v, 0.99) }
}

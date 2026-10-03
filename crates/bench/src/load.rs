//! Closed-loop HTTP/1.1 load generator: `conc` keep-alive connections, each sending the next
//! request as soon as the previous response is fully read.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::{self, SendRequest};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use memchr::memmem;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;

/// One request template, sent repeatedly.
#[derive(Clone)]
pub struct Target {
    pub addr: SocketAddr,
    pub method: Method,
    pub path: String,
    pub body: Bytes,
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

/// Sends the request once and returns status and body (used for pre-flight checks).
pub async fn once(t: &Target) -> Result<(u16, Bytes)> {
    let mut sender = connect(t.addr).await?;
    let resp = sender.send_request(t.request()?).await?;
    let status = resp.status().as_u16();
    Ok((status, resp.into_body().collect().await?.to_bytes()))
}

/// Flags shared between the orchestrator and the workers.
#[derive(Default)]
pub struct Shared {
    /// Samples are recorded only for requests that start and finish while this is set.
    pub measure: AtomicBool,
    pub stop: AtomicBool,
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

async fn worker(t: Arc<Target>, sh: Arc<Shared>, timing: bool) -> WorkerOut {
    let mut out = WorkerOut::default();
    let mut sender: Option<SendRequest<Full<Bytes>>> = None;
    while !sh.stop.load(Ordering::Relaxed) {
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
            _ => {
                out.errors += 1;
                sender = None;
            }
        }
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
        let shared = Arc::new(Shared::default());
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

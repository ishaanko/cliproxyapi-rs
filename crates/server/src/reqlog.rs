//! Request log files (Go: api/middleware request_logging + response_writer,
//! logging/request_logger*).
//!
//! With `request-log: true` every request writes one file; with `false` only "actionable error"
//! requests (status >= 400 other than 499, or recorded upstream errors) produce an `error-*.log`.
//! Layout and file naming follow Go. The upstream sections (`API REQUEST`, executor-side
//! `API RESPONSE n` blocks, `API WEBSOCKET TIMELINE`) come from the executors' capture
//! ([`cpa_runtime::apilog::ApiLog`], handed over through `Options.api_log`); the handler-level
//! response text (`API_RESPONSE`) and `API ERROR RESPONSE` entries are recorded here.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method};
use axum::middleware::Next;
use axum::response::Response;
use chrono::{DateTime, Local, SecondsFormat};
use cpa_config::Config;
use cpa_core::util::{mask_sensitive_header_value, mask_sensitive_query};
use cpa_runtime::apilog::{ApiLog as ExecLog, ApiLogHandle as ExecLogHandle};
use parking_lot::Mutex;
use tokio::sync::{Notify, watch};

use crate::bodytee::TeeBody;
use crate::req::RequestId;
use crate::state::AppState;

const MAX_ERROR_ONLY_CAPTURED_REQUEST_BODY: u64 = 1 << 20;
const MAX_DECODED_BODY: usize = 32 << 20;
const MAX_RESPONSE_CAPTURE: usize = 128 << 20;

/// Per-request log context shared between the middleware and handlers (the gin keys
/// `API_RESPONSE`, `API_RESPONSE_ERROR`, `WEBSOCKET_TIMELINE_*` of the Go code).
#[derive(Default)]
pub struct ApiLog {
    pub(crate) data: Mutex<ApiLogData>,
    /// Upstream capture written by the executors serving this request.
    exec: Arc<ExecLog>,
    pub(crate) ws_done: Notify,
    /// Set when `request-log` is off: Go only records `API_RESPONSE_ERROR` entries then.
    errors_muted: AtomicBool,
}

#[derive(Default)]
pub struct ApiLogData {
    /// `(status, text)` per recorded upstream error (`API_RESPONSE_ERROR`).
    pub errors: Vec<(u16, String)>,
    /// Appended upstream/handler response text (`API_RESPONSE`).
    pub api_response: Vec<u8>,
    pub ws_timeline: String,
}

/// The executors' capture as the renderer needs it (Go: `API_REQUEST`, the executor part of
/// `API_RESPONSE`, `API_WEBSOCKET_TIMELINE`, `API_RESPONSE_TIMESTAMP`).
#[derive(Default)]
pub struct ExecView {
    pub request: Vec<u8>,
    pub response: Vec<u8>,
    pub ws_timeline: Vec<u8>,
    pub response_timestamp: Option<DateTime<Local>>,
}

impl ApiLog {
    /// Per-request switch mirroring the `request-log` gate in `LoggingAPIResponseError`.
    pub fn set_error_logging(&self, enabled: bool) {
        self.errors_muted.store(!enabled, Ordering::Relaxed);
    }

    /// `LoggingAPIResponseError`.
    pub fn record_error(&self, status: u16, text: &str) {
        if self.errors_muted.load(Ordering::Relaxed) {
            return;
        }
        self.data.lock().errors.push((status, text.to_string()));
    }

    /// Handle for `Options.api_log`: the executors record upstream attempts into this request's log.
    pub fn exec_handle(&self) -> ExecLogHandle {
        ExecLogHandle::new(self.exec.clone())
    }

    /// `appendAPIResponse`: newline-separated accumulation with a first-write timestamp.
    pub fn append_api_response(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.exec.mark_response_timestamp();
        let mut d = self.data.lock();
        if !d.api_response.is_empty() && d.api_response.last() != Some(&b'\n') {
            d.api_response.push(b'\n');
        }
        d.api_response.extend_from_slice(data);
    }

    /// The error branch of the handler's cancel function (`GetContextWithCancel`): the cause is
    /// appended to `API_RESPONSE` unless the log already holds handler-level response text.
    pub fn note_cancel(&self, text: &str) {
        let held = !self.data.lock().api_response.trim_ascii().is_empty();
        if !held {
            self.append_api_response(text.as_bytes());
        }
    }

    /// `c.Set("API_RESPONSE", body)` as done by `WriteModelListResponse`: replaces the response
    /// text without touching the timestamp.
    pub fn set_api_response(&self, data: &[u8]) {
        self.data.lock().api_response = data.to_vec();
    }

    /// `markAPIResponseTimestamp`.
    pub fn mark_response_timestamp(&self) {
        self.exec.mark_response_timestamp();
    }

    /// Snapshot of the executors' capture for rendering. A forced (error-only) log has no
    /// recorded attempts, so the deferred requests stand in for `API REQUEST`.
    fn exec_view(&self, force: bool) -> ExecView {
        let mut request = self.exec.api_request();
        if force && request.is_empty() {
            request = self.exec.deferred_api_requests().concat();
        }
        ExecView {
            request,
            response: self.exec.api_response(),
            ws_timeline: self.exec.websocket_timeline(),
            response_timestamp: self.exec.response_timestamp(),
        }
    }

    /// Appends one websocket timeline event (`Timestamp` / `Event: websocket.<type>` / payload).
    pub fn ws_timeline_append(&self, event_type: &str, payload: &[u8]) {
        let trimmed = payload.trim_ascii();
        if trimmed.is_empty() {
            return;
        }
        let mut d = self.data.lock();
        if !d.ws_timeline.is_empty() {
            d.ws_timeline.push('\n');
        }
        d.ws_timeline.push_str(&format!(
            "Timestamp: {}\nEvent: websocket.{event_type}\n{}\n",
            Local::now().to_rfc3339_opts(SecondsFormat::Nanos, false),
            String::from_utf8_lossy(trimmed)
        ));
    }

    /// Signals the end of a websocket session so the deferred request log can be written.
    pub fn ws_finished(&self) {
        self.ws_done.notify_one();
    }

    fn has_actionable_error(&self, status: u16) -> bool {
        let d = self.data.lock();
        let cancel = |s: u16, t: &str| s == 499 || t.to_lowercase().contains("context canceled") || t.to_lowercase().contains("client closed request");
        if d.errors.iter().any(|(s, t)| !cancel(*s, t)) {
            return true;
        }
        status != 499 && status >= 400
    }
}

impl std::fmt::Debug for ApiLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiLog").finish_non_exhaustive()
    }
}

/// Extension carrying the shared [`ApiLog`].
#[derive(Clone, Default)]
pub struct ApiLogHandle(pub Arc<ApiLog>);

/// File request logger.
pub struct RequestLogger {
    config: watch::Receiver<Arc<Config>>,
    config_dir: Option<PathBuf>,
    counter: AtomicU64,
}

struct RequestInfo {
    url: String,
    method: String,
    headers: HeaderMap,
    body: Vec<u8>,
    request_id: String,
    timestamp: DateTime<Local>,
}

impl RequestLogger {
    pub fn new(config: watch::Receiver<Arc<Config>>, config_dir: Option<PathBuf>) -> Self {
        RequestLogger {
            config,
            config_dir,
            counter: AtomicU64::new(0),
        }
    }

    fn cfg(&self) -> Arc<Config> {
        self.config.borrow().clone()
    }

    pub fn enabled(&self) -> bool {
        let cfg = self.cfg();
        cfg.request_log && !cfg.commercial_mode
    }

    /// `homeEnabled` (`SetHomeEnabled(cfg.Home.Enabled)`): full request logs go to Home, not files.
    pub fn home_enabled(&self) -> bool {
        self.cfg().home.enabled
    }

    fn logs_dir(&self) -> PathBuf {
        let dir = crate::logging::resolve_log_directory(&self.cfg());
        match &self.config_dir {
            Some(base) if !dir.is_absolute() && !base.as_os_str().is_empty() => base.join(dir),
            _ => dir,
        }
    }

    /// `generateFilename`.
    fn filename(&self, url: &str, request_id: &str) -> String {
        let path = url.split('?').next().unwrap_or("");
        let path = path.strip_prefix('/').unwrap_or(path);
        let sanitized = sanitize_for_filename(path);
        let timestamp = Local::now().format("%Y-%m-%dT%H%M%S");
        let id_part = if request_id.is_empty() {
            (self.counter.fetch_add(1, Ordering::Relaxed) + 1).to_string()
        } else {
            crate::logging::short_request_id(request_id)
        };
        format!("{sanitized}-{timestamp}-{id_part}.log")
    }

    fn write_log(&self, filename: &str, content: &[u8], force: bool) {
        let dir = self.logs_dir();
        if let Err(e) = fs::create_dir_all(&dir) {
            tracing::warn!("failed to create logs directory: {e}");
            return;
        }
        let name = if force { format!("error-{filename}") } else { filename.to_string() };
        match create_unique_log_file(&dir, &name) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(content) {
                    tracing::warn!("failed to write log file: {e}");
                }
            }
            Err(e) => {
                tracing::warn!("failed to create log file: {e}");
                return;
            }
        }
        if force {
            self.cleanup_old_error_logs(&dir);
        }
    }

    /// `cleanupOldErrorLogs`: keep the newest `error-logs-max-files` error logs.
    fn cleanup_old_error_logs(&self, dir: &Path) {
        let max = self.cfg().error_logs_max_files;
        if max <= 0 {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else { return };
        let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.starts_with("error-") && name.ends_with(".log") && e.path().is_file()
            })
            .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
            .collect();
        if files.len() <= max as usize {
            return;
        }
        files.sort_by_key(|f| std::cmp::Reverse(f.1));
        for (path, _) in files.into_iter().skip(max as usize) {
            if let Err(e) = fs::remove_file(&path) {
                tracing::warn!("failed to remove old error log: {}: {e}", path.display());
            }
        }
    }
}

/// `sanitizeForFilename`.
fn sanitize_for_filename(path: &str) -> String {
    let mut s = String::with_capacity(path.len());
    for c in path.chars() {
        if c == '/' || c == ':' || matches!(c, '<' | '>' | '"' | '|' | '?' | '*') || c.is_whitespace() {
            s.push('-');
        } else {
            s.push(c);
        }
    }
    let mut collapsed = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_dash {
                collapsed.push(c);
            }
            prev_dash = true;
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }
    let trimmed = collapsed.trim_matches('-');
    if trimmed.is_empty() { "root".to_string() } else { trimmed.to_string() }
}

/// `createUniqueLogFile`: O_EXCL create, falling back to `<prefix>_<seq>-<id>.log`.
fn create_unique_log_file(dir: &Path, filename: &str) -> std::io::Result<fs::File> {
    let open = |name: &str| fs::OpenOptions::new().write(true).create_new(true).open(dir.join(name));
    match open(filename) {
        Ok(f) => return Ok(f),
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        Err(_) => {}
    }
    let (base, ext) = match filename.rfind('.') {
        Some(i) => (&filename[..i], &filename[i..]),
        None => (filename, ""),
    };
    let (prefix, id_part) = match base.rfind('-') {
        Some(i) if i > 0 => (&base[..i], &base[i + 1..]),
        _ => (base, ""),
    };
    for seq in 1..=1000 {
        match open(&format!("{prefix}_{seq}-{id_part}{ext}")) {
            Ok(f) => return Ok(f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!("too many conflicting log files for {filename}")))
}

pub(crate) fn canonical_header_name(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn sorted_headers(headers: &HeaderMap, skip: &[&str]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (canonical_header_name(k.as_str()), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        // Go keeps `Host` out of the header map, and `Content-Length`/`Transfer-Encoding` on
        // responses are added by net/http after the handler, so they never reach the log.
        .filter(|(k, _)| !skip.contains(&k.as_str()))
        .collect();
    out.sort();
    out
}

fn trailing_newlines(payload: &[u8]) -> usize {
    payload.iter().rev().take_while(|b| **b == b'\n').count()
}

/// `writeSectionSpacing`: pad so the section ends with three newlines.
fn section_spacing(out: &mut Vec<u8>, trailing: usize) {
    for _ in trailing..3 {
        out.push(b'\n');
    }
}

fn write_request_info(out: &mut Vec<u8>, info: &RequestInfo, downstream: &str, upstream: &str, include_body: bool) {
    let version = option_env!("CPA_VERSION").unwrap_or("dev");
    out.extend_from_slice(b"=== REQUEST INFO ===\n");
    out.extend_from_slice(format!("Version: {version}\nURL: {}\nMethod: {}\n", info.url, info.method).as_bytes());
    if !downstream.trim().is_empty() {
        out.extend_from_slice(format!("Downstream Transport: {downstream}\n").as_bytes());
    }
    if !upstream.trim().is_empty() {
        out.extend_from_slice(format!("Upstream Transport: {upstream}\n").as_bytes());
    }
    out.extend_from_slice(
        format!("Timestamp: {}\n", info.timestamp.to_rfc3339_opts(SecondsFormat::Nanos, false)).as_bytes(),
    );
    section_spacing(out, 1);

    out.extend_from_slice(b"=== HEADERS ===\n");
    for (key, value) in sorted_headers(&info.headers, &["Host"]) {
        out.extend_from_slice(format!("{key}: {}\n", mask_sensitive_header_value(&key, &value)).as_bytes());
    }
    section_spacing(out, 1);

    if !include_body {
        return;
    }
    out.extend_from_slice(b"=== REQUEST BODY ===\n");
    out.extend_from_slice(&info.body);
    let trailing = if info.body.is_empty() { 1 } else { trailing_newlines(&info.body) };
    section_spacing(out, trailing);
}

fn write_api_section(out: &mut Vec<u8>, header: &str, prefix: &str, payload: &[u8], timestamp: Option<DateTime<Local>>) {
    if payload.is_empty() {
        return;
    }
    if payload.starts_with(prefix.as_bytes()) {
        out.extend_from_slice(payload);
    } else {
        out.extend_from_slice(header.as_bytes());
        if let Some(ts) = timestamp {
            out.extend_from_slice(format!("Timestamp: {}\n", ts.to_rfc3339_opts(SecondsFormat::Nanos, false)).as_bytes());
        }
        out.extend_from_slice(payload);
    }
    section_spacing(out, trailing_newlines(payload));
}

fn write_response_section(out: &mut Vec<u8>, status: Option<u16>, headers: &HeaderMap, body: &[u8], trailing_newline: bool) {
    out.extend_from_slice(b"=== RESPONSE ===\n");
    if let Some(status) = status {
        out.extend_from_slice(format!("Status: {status}\n").as_bytes());
    }
    for (key, value) in sorted_headers(headers, &["Content-Length", "Transfer-Encoding"]) {
        out.extend_from_slice(format!("{key}: {value}\n").as_bytes());
    }
    if !(body.starts_with(b"\r\n") || body.starts_with(b"\n")) {
        out.push(b'\n');
    }
    out.extend_from_slice(body);
    if trailing_newline {
        out.push(b'\n');
    }
}

/// Everything known about a finished exchange.
struct Exchange {
    info: RequestInfo,
    status: u16,
    response_headers: HeaderMap,
    response_body: Vec<u8>,
    streaming: bool,
    /// When the first body chunk was written (Go: `firstChunkTimestamp`).
    first_chunk: Option<DateTime<Local>>,
}

/// `writePreformattedAPISectionWithSource`: the handler's `API_RESPONSE` payload gets the
/// section header, the executor capture is already formatted (`=== API RESPONSE n ===` blocks)
/// and follows it as is.
fn write_preformatted_section(out: &mut Vec<u8>, header: &str, prefix: &str, payload: &[u8], captured: &[u8], timestamp: Option<DateTime<Local>>) {
    if captured.is_empty() {
        write_api_section(out, header, prefix, payload, timestamp);
        return;
    }
    write_api_section(out, header, prefix, payload, timestamp);
    out.extend_from_slice(captured);
    section_spacing(out, trailing_newlines(captured));
}

fn has_payload(payload: &[u8]) -> bool {
    !payload.trim_ascii().is_empty()
}

/// `writeNonStreamingLog`, or with `stream_layout` the `FileStreamingLogWriter` final log
/// (used when the logger is enabled and the response streamed): no error sections, the API
/// response timestamp is the first chunk's.
fn render(exchange: &Exchange, api: &ApiLogData, exec: &ExecView, websocket: bool, stream_layout: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let has_http = has_payload(&exec.request) || has_payload(&api.api_response) || has_payload(&exec.response);
    let upstream = match (has_http, has_payload(&exec.ws_timeline)) {
        (true, true) => "websocket+http",
        (false, true) => "websocket",
        (true, false) => "http",
        (false, false) => "",
    };
    let is_ws_transcript = !stream_layout && !api.ws_timeline.trim().is_empty();
    let downstream = if !stream_layout && (is_ws_transcript || websocket) { "websocket" } else { "http" };
    write_request_info(&mut out, &exchange.info, downstream, upstream, !is_ws_transcript);
    if is_ws_transcript {
        write_api_section(&mut out, "=== WEBSOCKET TIMELINE ===\n", "=== WEBSOCKET TIMELINE", api.ws_timeline.as_bytes(), None);
    }
    write_api_section(&mut out, "=== API WEBSOCKET TIMELINE ===\n", "=== API WEBSOCKET TIMELINE", &exec.ws_timeline, None);
    write_api_section(&mut out, "=== API REQUEST ===\n", "=== API REQUEST", &exec.request, None);
    if !stream_layout {
        for (status, text) in &api.errors {
            out.extend_from_slice(b"=== API ERROR RESPONSE ===\n");
            out.extend_from_slice(format!("HTTP Status: {status}\n").as_bytes());
            out.extend_from_slice(text.as_bytes());
            section_spacing(&mut out, if text.is_empty() { 1 } else { trailing_newlines(text.as_bytes()) });
        }
    }
    let ts = if stream_layout { exchange.first_chunk } else { exec.response_timestamp };
    write_preformatted_section(&mut out, "=== API RESPONSE ===\n", "=== API RESPONSE", &api.api_response, &exec.response, ts);
    if is_ws_transcript {
        return out;
    }
    write_response_section(
        &mut out,
        Some(exchange.status),
        &exchange.response_headers,
        &exchange.response_body,
        // Go's streamed logs end with an extra newline too (verified against the reference).
        true,
    );
    out
}

fn is_responses_websocket_upgrade(req: &Request) -> bool {
    let path = req.uri().path();
    (path == "/v1/responses" || path == "/backend-api/codex/responses")
        && req
            .headers()
            .get("upgrade")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"))
}

/// `decodeCapturedRequestBodyForLog`: zstd bodies are decoded for the log only.
fn decode_body_for_log(raw: &[u8], encoding: &str) -> Vec<u8> {
    let encoding = encoding.trim();
    if raw.is_empty() || encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
        return raw.to_vec();
    }
    let mut body = raw.to_vec();
    for part in encoding.split(',').rev() {
        match part.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => {}
            "zstd" => {
                use std::io::Read;
                let Ok(decoder) = zstd::stream::read::Decoder::new(&body[..]) else {
                    return raw.to_vec();
                };
                let mut decoded = Vec::new();
                let mut limited = decoder.take(MAX_DECODED_BODY as u64 + 1);
                if limited.read_to_end(&mut decoded).is_err() {
                    return raw.to_vec();
                }
                if decoded.len() > MAX_DECODED_BODY {
                    decoded.truncate(MAX_DECODED_BODY);
                    if !decoded.ends_with(b"\n") {
                        decoded.push(b'\n');
                    }
                    decoded.extend_from_slice(b"[DECOMPRESSED REQUEST BODY TRUNCATED]");
                    return decoded;
                }
                body = decoded;
            }
            _ => return raw.to_vec(),
        }
    }
    body
}

#[derive(Default)]
struct Captured {
    body: Vec<u8>,
    first_chunk: Option<DateTime<Local>>,
}

/// Inner layer for handler routes: Go's `WriteErrorResponse` appends every error body it writes
/// to `API_RESPONSE`, so the log gets an `API RESPONSE` section. Auth, 404, 405 and `c.JSON`
/// validation errors stay out of it (this layer is a `route_layer` under auth).
pub async fn capture_handler_errors(req: Request, next: Next) -> Response {
    let api_log = req.extensions().get::<ApiLogHandle>().map(|h| h.0.clone());
    let resp = next.run(req).await;
    let Some(api_log) = api_log else { return resp };
    // `WriteErrorResponse` sets a bare `application/json`; `c.JSON` errors carry a charset and
    // never reach `API_RESPONSE`.
    let from_error_writer = resp.headers().get("content-type").is_some_and(|v| v.as_bytes() == b"application/json");
    if resp.status().as_u16() < 400 || !from_error_writer {
        return resp;
    }
    let (parts, body) = resp.into_parts();
    match axum::body::to_bytes(body, MAX_RESPONSE_CAPTURE).await {
        Ok(bytes) => {
            api_log.append_api_response(&bytes);
            Response::from_parts(parts, Body::from(bytes))
        }
        Err(_) => Response::from_parts(parts, Body::empty()),
    }
}

/// `RequestLoggingMiddleware`.
pub async fn request_log(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let Some(logger) = st.request_logger.clone() else {
        return next.run(req).await;
    };
    // Plain GETs are not logged (only the Responses websocket upgrade is).
    let ws_upgrade = is_responses_websocket_upgrade(&req);
    if req.method() == Method::GET && !ws_upgrade {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    if path.starts_with("/v0/management") || path.starts_with("/v8/management") || path.starts_with("/management") {
        return next.run(req).await;
    }

    let enabled = logger.enabled();
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let content_length: Option<u64> = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());
    let capture_body = enabled
        || (!content_type.starts_with("multipart/form-data")
            && content_length.is_some_and(|n| n > 0 && n <= MAX_ERROR_ONLY_CAPTURED_REQUEST_BODY));

    let masked_query = mask_sensitive_query(req.uri().query().unwrap_or(""));
    let url = if masked_query.is_empty() { path.clone() } else { format!("{path}?{masked_query}") };
    let request_id = req.extensions().get::<RequestId>().map(|r| r.0.clone()).unwrap_or_default();
    let api_log = req
        .extensions()
        .get::<ApiLogHandle>()
        .map(|h| h.0.clone())
        .unwrap_or_default();
    let encoding = req
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let (parts, body) = req.into_parts();
    let (req, logged_body) = if capture_body {
        match axum::body::to_bytes(body, usize::MAX).await {
            Ok(bytes) => {
                let logged = decode_body_for_log(&bytes, &encoding);
                (Request::from_parts(parts.clone(), Body::from(bytes)), logged)
            }
            Err(_) => return next.run(Request::from_parts(parts, Body::empty())).await,
        }
    } else {
        (Request::from_parts(parts.clone(), body), Vec::new())
    };
    let info = RequestInfo {
        url,
        method: parts.method.to_string(),
        headers: parts.headers.clone(),
        body: logged_body,
        request_id,
        timestamp: Local::now(),
    };

    let resp = next.run(req).await;
    let (resp_parts, resp_body) = resp.into_parts();
    let status = resp_parts.status.as_u16();
    let response_headers = resp_parts.headers.clone();

    let ct = response_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let streaming = if ct.contains("text/event-stream") {
        true
    } else if ct.trim().is_empty() {
        let b = &info.body;
        let has = |needle: &[u8]| b.windows(needle.len()).any(|w| w == needle);
        !b.is_empty() && (has(br#""stream": true"#) || has(br#""stream":true"#))
    } else {
        false
    };

    // Buffer the response body only when it can end up in a file.
    let buffer_all = enabled;
    let buffer_errors = !enabled && status >= 400 && status != 499;
    let captured = Arc::new(Mutex::new(Captured::default()));
    let on_chunk: Option<crate::bodytee::OnChunk> = if buffer_all || buffer_errors {
        let captured = captured.clone();
        Some(Box::new(move |chunk| {
            let mut c = captured.lock();
            if c.first_chunk.is_none() {
                c.first_chunk = Some(Local::now());
            }
            if c.body.len() < MAX_RESPONSE_CAPTURE {
                c.body.extend_from_slice(chunk);
            }
        }))
    } else {
        None
    };

    // Home mode with request-log on: the log goes to Home. A streaming exchange that starts
    // without a healthy Home client gets no log (`LogStreamingRequest` returns a no-op writer).
    let to_home = enabled && logger.home_enabled();
    let skip_home = to_home && streaming && crate::reqlog_home::ready_client().is_none();

    let on_done = {
        let captured = captured.clone();
        let logger = logger.clone();
        Box::new(move || {
            let (response_body, first_chunk) = {
                let mut c = captured.lock();
                (std::mem::take(&mut c.body), c.first_chunk)
            };
            let exchange = Exchange { info, status, response_headers, response_body, streaming, first_chunk };
            let task = async move {
                if ws_upgrade && status == 101 {
                    api_log.ws_done.notified().await;
                }
                if to_home {
                    if !skip_home {
                        forward_to_home(&exchange, &api_log, ws_upgrade).await;
                    }
                    return;
                }
                // Rendering and the file write are blocking fs work.
                let _ = tokio::task::spawn_blocking(move || finalize(&logger, exchange, &api_log, enabled, ws_upgrade)).await;
            };
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(task);
                }
                Err(_) => {
                    tracing::debug!("request log skipped: no runtime");
                }
            }
        })
    };
    Response::from_parts(resp_parts, TeeBody::wrap(resp_body, on_chunk, on_done))
}

/// Home variant of the log write: render the log text and `RPUSH request-log` it.
async fn forward_to_home(exchange: &Exchange, api_log: &ApiLog, websocket: bool) {
    // Health is checked again now: the log may have outlived the Home connection.
    if crate::reqlog_home::ready_client().is_none() {
        return;
    }
    let exec = api_log.exec_view(false);
    let content = {
        let data = api_log.data.lock();
        render(exchange, &data, &exec, websocket, exchange.streaming)
    };
    let text = String::from_utf8_lossy(&content);
    if let Err(e) = crate::reqlog_home::forward_request_log(&exchange.info.headers, &exchange.info.request_id, &text).await {
        tracing::debug!("failed to forward request log to home: {e}");
    }
}

fn finalize(logger: &RequestLogger, exchange: Exchange, api_log: &ApiLog, enabled: bool, websocket: bool) {
    let actionable = api_log.has_actionable_error(exchange.status);
    let force = !enabled && actionable;
    if !enabled && !force {
        return;
    }
    let exec = api_log.exec_view(force);
    let content = {
        let data = api_log.data.lock();
        render(&exchange, &data, &exec, websocket, enabled && exchange.streaming)
    };
    let filename = logger.filename(&exchange.info.url, &exchange.info.request_id);
    logger.write_log(&filename, &content, force && !enabled);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(body: &str, status: u16, streaming: bool) -> Exchange {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer sk-1234567890abcdef".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let mut rh = HeaderMap::new();
        rh.insert("content-type", "application/json".parse().unwrap());
        Exchange {
            info: RequestInfo {
                url: "/v1/chat/completions?key=abcdefghijkl".into(),
                method: "POST".into(),
                headers,
                body: br#"{"model":"m"}"#.to_vec(),
                request_id: "01a0fb51-2286-7912-bedc-74541b480008".into(),
                timestamp: Local::now(),
            },
            status,
            response_headers: rh,
            response_body: body.as_bytes().to_vec(),
            streaming,
            first_chunk: None,
        }
    }

    #[test]
    fn file_names_are_sanitized_and_unique() {
        assert_eq!(sanitize_for_filename("v1/chat/completions"), "v1-chat-completions");
        assert_eq!(sanitize_for_filename("v1beta/models/gemini:generateContent"), "v1beta-models-gemini-generateContent");
        assert_eq!(sanitize_for_filename("//"), "root");
        let dir = tempfile::tempdir().unwrap();
        drop(create_unique_log_file(dir.path(), "a-2026-01-01T000000-abcd1234.log").unwrap());
        drop(create_unique_log_file(dir.path(), "a-2026-01-01T000000-abcd1234.log").unwrap());
        assert!(dir.path().join("a-2026-01-01T000000_1-abcd1234.log").exists());
    }

    #[test]
    fn non_streaming_layout() {
        let ex = exchange(r#"{"ok":true}"#, 200, false);
        let text = String::from_utf8(render(&ex, &ApiLogData::default(), &ExecView::default(), false, ex.streaming)).unwrap();
        assert!(text.starts_with("=== REQUEST INFO ===\nVersion: dev\nURL: /v1/chat/completions?key=abcdefghijkl\nMethod: POST\nDownstream Transport: http\nTimestamp: "), "{text}");
        assert!(text.contains("\n\n\n=== HEADERS ===\nAuthorization: Bearer sk-1...cdef\nContent-Type: application/json\n\n\n=== REQUEST BODY ===\n{\"model\":\"m\"}\n\n\n=== RESPONSE ===\nStatus: 200\nContent-Type: application/json\n\n{\"ok\":true}\n"), "{text}");
    }

    #[test]
    fn streaming_layout_ends_with_an_extra_newline_like_go() {
        let ex = exchange("data: x\n\n", 200, true);
        let text = String::from_utf8(render(&ex, &ApiLogData::default(), &ExecView::default(), false, ex.streaming)).unwrap();
        assert!(text.ends_with("=== RESPONSE ===\nStatus: 200\nContent-Type: application/json\n\ndata: x\n\n\n"), "{text}");
    }

    #[test]
    fn error_sections_precede_the_response() {
        let ex = exchange("{}", 502, false);
        let mut api = ApiLogData::default();
        api.errors.push((502, "upstream exploded".into()));
        api.api_response = b"{\"error\":1}".to_vec();
        let text = String::from_utf8(render(&ex, &api, &ExecView::default(), false, false)).unwrap();
        let e = text.find("=== API ERROR RESPONSE ===\nHTTP Status: 502\nupstream exploded\n\n\n").unwrap();
        let r = text.find("=== API RESPONSE ===\n").unwrap();
        let resp = text.find("=== RESPONSE ===\n").unwrap();
        assert!(e < r && r < resp);
        assert!(text.contains("Upstream Transport: http"));
    }

    #[test]
    fn websocket_transcripts_skip_body_and_response_sections() {
        let ex = exchange("", 101, false);
        let api = ApiLog::default();
        api.ws_timeline_append("request", b"{\"type\":\"response.create\"}");
        let text = String::from_utf8(render(&ex, &api.data.lock(), &ExecView::default(), true, false)).unwrap();
        assert!(text.contains("Downstream Transport: websocket"));
        assert!(text.contains("=== WEBSOCKET TIMELINE ===\nTimestamp: "));
        assert!(!text.contains("=== REQUEST BODY ==="));
        assert!(!text.contains("=== RESPONSE ==="));
    }

    #[test]
    fn actionable_errors() {
        let api = ApiLog::default();
        assert!(!api.has_actionable_error(200));
        assert!(!api.has_actionable_error(499));
        assert!(api.has_actionable_error(500));
        api.record_error(499, "context canceled");
        assert!(!api.has_actionable_error(200));
        api.record_error(500, "boom");
        assert!(api.has_actionable_error(200));
    }

    #[test]
    fn zstd_request_bodies_are_decoded_for_the_log() {
        let compressed = zstd::stream::encode_all(&b"{\"a\":1}"[..], 1).unwrap();
        assert_eq!(decode_body_for_log(&compressed, "zstd"), b"{\"a\":1}");
        assert_eq!(decode_body_for_log(b"raw", "gzip"), b"raw");
    }
}

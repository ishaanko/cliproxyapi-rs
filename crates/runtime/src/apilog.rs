//! Upstream request/response capture for request logging (Go: helps/logging_helpers.go).
//!
//! Go stores the capture in the gin context; here one [`ApiLog`] per inbound request is shared
//! (`Arc`) by the executors serving it and read back by the HTTP layer, which writes it to the
//! request log. Text layout matches Go (`=== API REQUEST n ===` / `=== API RESPONSE n ===`
//! blocks, masked headers). Capture is memory-backed only (Go's file-body sources are not
//! ported). Every record also emits a `tracing` debug event on target `cpa::upstream` with
//! secrets masked.

use chrono::{DateTime, Local, Timelike};
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::util::{hide_api_key, mask_sensitive_header_value};
use http::HeaderMap;
use parking_lot::Mutex;

/// Cap on request bodies kept for deferred request logging (32 MiB).
pub const MAX_DEFERRED_API_REQUEST_BODY_BYTES: usize = 32 << 20;
/// Cap of the captured upstream response of one attempt and of the websocket timeline, like the
/// handler-level response capture; bytes past it are dropped.
pub const MAX_RESPONSE_CAPTURE: usize = 128 << 20;

/// The outbound upstream request details for logging.
#[derive(Debug, Clone, Default)]
pub struct UpstreamRequestLog {
    pub url: String,
    pub method: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub provider: String,
    pub auth_id: String,
    pub auth_label: String,
    /// `api_key`, `oauth` or another label.
    pub auth_type: String,
    /// API key (masked on output) or account for non-OAuth types.
    pub auth_value: String,
}

impl UpstreamRequestLog {
    /// Request details with the auth identity Go derives from `auth.ID`, `auth.Label` and
    /// `auth.AccountInfo()`; the single place executors build their request-log entries.
    pub fn from_auth(provider: &str, auth: Option<&Auth>, method: &str, url: &str, headers: &HeaderMap, body: &[u8]) -> Self {
        let mut info = UpstreamRequestLog {
            url: url.to_string(),
            method: method.to_string(),
            headers: headers.clone(),
            body: body.to_vec(),
            provider: provider.to_string(),
            ..Default::default()
        };
        if let Some(auth) = auth {
            info.auth_id = auth.id.clone();
            info.auth_label = auth.label.clone();
            let (kind, value) = auth.account_info();
            info.auth_type = kind.to_string();
            info.auth_value = value;
        }
        info
    }
}

fn request_log_capture_enabled(cfg: &Config) -> bool {
    cfg.request_log && !cfg.commercial_mode
}

/// Go `time.RFC3339Nano` in local time: trailing fractional zeros trimmed, `Z` for UTC.
fn rfc3339_nano(t: DateTime<Local>) -> String {
    let mut out = t.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = t.nanosecond().min(999_999_999);
    if nanos != 0 {
        out.push('.');
        out.push_str(format!("{nanos:09}").trim_end_matches('0'));
    }
    let offset = t.offset().local_minus_utc();
    if offset == 0 {
        out.push('Z');
    } else {
        let (sign, abs) = if offset < 0 { ('-', -offset) } else { ('+', offset) };
        out.push_str(&format!("{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60));
    }
    out
}

fn now_stamp() -> String {
    rfc3339_nano(Local::now())
}

/// `Content-Type` style canonical form of a header name (Go: `textproto.CanonicalMIMEHeaderKey`).
fn canonical_header_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = true;
    for c in name.chars() {
        out.push(if upper { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() });
        upper = c == '-';
    }
    out
}

/// Header block sorted by name with sensitive values masked, or `<none>`.
pub fn write_headers(out: &mut String, headers: &HeaderMap) {
    write_headers_as(out, headers, canonical_header_name);
}

/// Like [`write_headers`] with `name_of` choosing the printed name (and so the sort key).
fn write_headers_as(out: &mut String, headers: &HeaderMap, name_of: impl Fn(&str) -> String) {
    if headers.is_empty() {
        out.push_str("<none>\n");
        return;
    }
    let mut entries: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (name_of(k.as_str()), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    // Stable: values of one header keep their wire order.
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, value) in entries {
        out.push_str(&format!("{key}: {}\n", mask_sensitive_header_value(&key, &value)));
    }
}

/// `provider=..., auth_id=..., label=..., type=...` summary line (API keys masked).
pub fn format_auth_info(info: &UpstreamRequestLog) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (label, value) in [("provider", &info.provider), ("auth_id", &info.auth_id), ("label", &info.auth_label)] {
        let v = value.trim();
        if !v.is_empty() {
            parts.push(format!("{label}={v}"));
        }
    }
    let auth_type = info.auth_type.trim().to_lowercase();
    let auth_value = info.auth_value.trim();
    match auth_type.as_str() {
        "api_key" if !auth_value.is_empty() => parts.push(format!("type=api_key value={}", hide_api_key(auth_value))),
        "api_key" => parts.push("type=api_key".into()),
        "oauth" => parts.push("type=oauth".into()),
        "" => {}
        other if !auth_value.is_empty() => parts.push(format!("type={other} value={auth_value}")),
        other => parts.push(format!("type={other}")),
    }
    parts.join(", ")
}

fn api_request_header(index: usize, info: &UpstreamRequestLog, stamp: &str) -> String {
    let mut b = format!("=== API REQUEST {index} ===\nTimestamp: {stamp}\n");
    if info.url.is_empty() {
        b.push_str("Upstream URL: <unknown>\n");
    } else {
        b.push_str(&format!("Upstream URL: {}\n", info.url));
    }
    if !info.method.is_empty() {
        b.push_str(&format!("HTTP Method: {}\n", info.method));
    }
    let auth = format_auth_info(info);
    if !auth.is_empty() {
        b.push_str(&format!("Auth: {auth}\n"));
    }
    b.push_str("\nHeaders:\n");
    write_headers(&mut b, &info.headers);
    b.push_str("\nBody:\n");
    b
}

#[derive(Default)]
struct Attempt {
    index: usize,
    request: String,
    response: Vec<u8>,
    response_intro_written: bool,
    status_written: bool,
    headers_written: bool,
    body_started: bool,
    body_has_content: bool,
    prev_was_sse_event: bool,
    error_written: bool,
    trailing_newlines: usize,
}

/// A request captured while request logging is off, rendered on demand (for example when the
/// downstream request later fails and is force-logged).
struct DeferredRequest {
    index: usize,
    info: UpstreamRequestLog,
    captured_at: DateTime<Local>,
    body_empty: bool,
    capture_length: usize,
    body_truncated: bool,
}

impl DeferredRequest {
    fn render(&self) -> Vec<u8> {
        let mut b = api_request_header(self.index, &self.info, &rfc3339_nano(self.captured_at));
        if self.body_empty {
            b.push_str("<empty>");
        } else {
            b.push_str(&String::from_utf8_lossy(&self.info.body));
            if self.body_truncated {
                b.push_str(&format!("\n[API REQUEST BODY TRUNCATED: captured first {} bytes]", self.capture_length));
            }
        }
        b.push_str("\n\n");
        b.into_bytes()
    }
}

#[derive(Default)]
struct LogState {
    attempts: Vec<Attempt>,
    websocket_timeline: Vec<u8>,
    deferred: Vec<DeferredRequest>,
    deferred_bytes: usize,
    response_timestamp: Option<DateTime<Local>>,
    response_headers: HeaderMap,
    credits_used: bool,
}

/// Final client response status of one inbound request (Go: the logging response-status
/// holder). Zero until the handler finished; usage records share it so the queue can fold the
/// final status into `failed` when a record is dispatched after the response.
#[derive(Clone, Default, Debug)]
pub struct ResponseStatus(std::sync::Arc<std::sync::atomic::AtomicU16>);

impl ResponseStatus {
    /// Stores the status; zero is ignored like Go's `SetResponseStatus`.
    pub fn set(&self, status: u16) {
        if status > 0 {
            self.0.store(status, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The stored status, 0 while unknown.
    pub fn get(&self) -> u16 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Capture of the upstream attempts of one inbound request.
#[derive(Default)]
pub struct ApiLog {
    state: Mutex<LogState>,
    response_status: ResponseStatus,
}

impl std::fmt::Debug for ApiLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiLog").finish_non_exhaustive()
    }
}

/// What `Options` carries: the request's [`ApiLog`], or nothing for calls made outside an
/// inbound request (Go: a context without the gin value, where every recorder is a no-op).
#[derive(Clone, Default, Debug)]
pub struct ApiLogHandle(Option<std::sync::Arc<ApiLog>>);

impl ApiLogHandle {
    pub fn new(log: std::sync::Arc<ApiLog>) -> Self {
        ApiLogHandle(Some(log))
    }

    pub fn get(&self) -> Option<&ApiLog> {
        self.0.as_deref()
    }

    /// The request's final-status holder, for usage records of its attempts.
    pub fn response_status(&self) -> Option<ResponseStatus> {
        self.get().map(|l| l.response_status.clone())
    }

    pub fn record_api_request(&self, cfg: &Config, info: UpstreamRequestLog) {
        if let Some(l) = self.get() {
            l.record_api_request(cfg, info);
        }
    }

    pub fn record_api_response_metadata(&self, cfg: &Config, status: u16, headers: &HeaderMap) {
        if let Some(l) = self.get() {
            l.record_api_response_metadata(cfg, status, headers);
        }
    }

    pub fn record_api_response_error(&self, cfg: &Config, err: &str) {
        if let Some(l) = self.get() {
            l.record_api_response_error(cfg, err);
        }
    }

    pub fn append_api_response_chunk(&self, cfg: &Config, chunk: &[u8]) {
        if let Some(l) = self.get() {
            l.append_api_response_chunk(cfg, chunk);
        }
    }

    pub fn record_api_websocket_request(&self, cfg: &Config, info: &UpstreamRequestLog) {
        if let Some(l) = self.get() {
            l.record_api_websocket_request(cfg, info);
        }
    }

    pub fn record_api_websocket_handshake(&self, cfg: &Config, status: u16, headers: &HeaderMap) {
        if let Some(l) = self.get() {
            l.record_api_websocket_handshake(cfg, status, headers);
        }
    }

    pub fn record_api_websocket_upgrade_rejection(
        &self,
        cfg: &Config,
        info: UpstreamRequestLog,
        status: u16,
        headers: &HeaderMap,
        body: &[u8],
    ) {
        if let Some(l) = self.get() {
            l.record_api_websocket_upgrade_rejection(cfg, info, status, headers, body);
        }
    }

    pub fn append_api_websocket_response(&self, cfg: &Config, payload: &[u8]) {
        if let Some(l) = self.get() {
            l.append_api_websocket_response(cfg, payload);
        }
    }

    pub fn record_api_websocket_error(&self, cfg: &Config, stage: &str, err: &str) {
        if let Some(l) = self.get() {
            l.record_api_websocket_error(cfg, stage, err);
        }
    }

    pub fn merge_response_headers(&self, extra: &HeaderMap) {
        if let Some(l) = self.get() {
            l.merge_response_headers(extra);
        }
    }

    /// Starts a fresh upstream attempt for response headers (Go: `WithFreshResponseHeadersHolder`).
    pub fn reset_response_headers(&self) {
        if let Some(l) = self.get() {
            l.state.lock().response_headers = HeaderMap::new();
        }
    }

    /// Upstream response headers recorded since the last reset (Go: `GetResponseHeaders`).
    pub fn response_headers(&self) -> HeaderMap {
        self.get().map(ApiLog::response_headers).unwrap_or_default()
    }

    pub fn mark_credits_used(&self) {
        if let Some(l) = self.get() {
            l.mark_credits_used();
        }
    }

    pub fn credits_used(&self) -> bool {
        self.get().is_some_and(ApiLog::credits_used)
    }
}

impl ApiLog {
    /// Records the final client response status once the handler finished (Go: the cancel
    /// function of `GetContextWithCancel` calling `SetResponseStatus`).
    pub fn set_response_status(&self, status: u16) {
        self.response_status.set(status);
    }

    pub fn new() -> Self {
        Self::default()
    }

    // ---- HTTP attempts

    /// Records an outbound upstream request as a new attempt. With request logging off the
    /// request is kept only in deferred form; commercial mode records nothing.
    pub fn record_api_request(&self, cfg: &Config, info: UpstreamRequestLog) {
        if cfg.commercial_mode {
            return;
        }
        tracing::debug!(
            target: "cpa::upstream",
            "upstream request {} {} {}",
            info.method,
            info.url,
            format_auth_info(&info)
        );
        let mut s = self.state.lock();
        if !cfg.request_log {
            s.defer(info);
            return;
        }
        let index = s.attempts.len() + 1;
        let mut text = api_request_header(index, &info, &now_stamp());
        if info.body.is_empty() {
            text.push_str("<empty>");
        } else {
            text.push_str(&String::from_utf8_lossy(&info.body));
        }
        text.push_str("\n\n");
        s.attempts.push(Attempt { index, request: text, ..Default::default() });
    }

    /// Records response status and headers on the latest attempt.
    pub fn record_api_response_metadata(&self, cfg: &Config, status: u16, headers: &HeaderMap) {
        tracing::debug!(target: "cpa::upstream", "upstream response status={status}");
        let mut s = self.state.lock();
        s.response_headers = headers.clone();
        if !request_log_capture_enabled(cfg) {
            return;
        }
        let attempt = s.ensure_attempt();
        let mut parts: Vec<Vec<u8>> = Vec::new();
        if status > 0 && !attempt.status_written {
            parts.push(format!("Status: {status}\n").into_bytes());
            attempt.status_written = true;
        }
        if !attempt.headers_written {
            let mut b = String::from("Headers:\n");
            // Go's `http.Response.Header` never holds `Transfer-Encoding` (the transport parses
            // it into a field), so a chunked body leaves no trace in the log.
            let mut logged = headers.clone();
            logged.remove(http::header::TRANSFER_ENCODING);
            write_headers(&mut b, &logged);
            parts.push(b.into_bytes());
            parts.push(b"\n".to_vec());
            attempt.headers_written = true;
        }
        s.write_parts(parts);
    }

    /// Records an error entry on the latest attempt when no HTTP response is available.
    pub fn record_api_response_error(&self, cfg: &Config, err: &str) {
        if !request_log_capture_enabled(cfg) {
            return;
        }
        let mut s = self.state.lock();
        let attempt = s.ensure_attempt();
        let mut parts = Vec::new();
        if attempt.body_started && !attempt.body_has_content {
            attempt.body_started = false;
        }
        if attempt.error_written {
            parts.push(b"\n".to_vec());
        }
        parts.push(format!("Error: {err}\n").into_bytes());
        attempt.error_written = true;
        s.write_parts(parts);
    }

    /// Appends an upstream response chunk (SSE line or body) to the latest attempt.
    pub fn append_api_response_chunk(&self, cfg: &Config, chunk: &[u8]) {
        if !request_log_capture_enabled(cfg) {
            return;
        }
        let data = trim_space(chunk);
        if data.is_empty() {
            return;
        }
        let mut s = self.state.lock();
        let attempt = s.ensure_attempt();
        let mut parts: Vec<Vec<u8>> = Vec::new();
        if !attempt.headers_written {
            let mut b = String::from("Headers:\n");
            write_headers(&mut b, &HeaderMap::new());
            parts.push(b.into_bytes());
            parts.push(b"\n".to_vec());
            attempt.headers_written = true;
        }
        if !attempt.body_started {
            parts.push(b"Body:\n".to_vec());
            attempt.body_started = true;
        }
        let is_event = data.starts_with(b"event:");
        let is_data = data.starts_with(b"data:");
        if attempt.body_has_content {
            let sep: &[u8] = if attempt.prev_was_sse_event && is_data { b"\n" } else { b"\n\n" };
            parts.push(sep.to_vec());
        }
        parts.push(data.to_vec());
        attempt.body_has_content = true;
        attempt.prev_was_sse_event = is_event;
        s.write_parts(parts);
    }

    // ---- websocket timeline

    /// Records an upstream websocket request event.
    pub fn record_api_websocket_request(&self, cfg: &Config, info: &UpstreamRequestLog) {
        if !request_log_capture_enabled(cfg) {
            return;
        }
        let mut b = format!("Timestamp: {}\nEvent: api.websocket.request\n", now_stamp());
        if !info.url.is_empty() {
            b.push_str(&format!("Upstream URL: {}\n", info.url));
        }
        let auth = format_auth_info(info);
        if !auth.is_empty() {
            b.push_str(&format!("Auth: {auth}\n"));
        }
        b.push_str("Headers:\n");
        // The Codex handshake carries `session_id` as a raw (non-canonical) map key.
        write_headers_as(&mut b, &info.headers, |name| {
            if name == "session_id" { name.to_string() } else { canonical_header_name(name) }
        });
        b.push_str("\nBody:\n");
        if info.body.is_empty() {
            b.push_str("<empty>");
        } else {
            b.push_str(&String::from_utf8_lossy(&info.body));
        }
        b.push('\n');
        self.state.lock().append_timeline(b.as_bytes());
    }

    /// Records the upstream websocket handshake response.
    pub fn record_api_websocket_handshake(&self, cfg: &Config, status: u16, headers: &HeaderMap) {
        let mut s = self.state.lock();
        s.response_headers = headers.clone();
        if !request_log_capture_enabled(cfg) {
            return;
        }
        let mut b = format!("Timestamp: {}\nEvent: api.websocket.handshake\n", now_stamp());
        if status > 0 {
            b.push_str(&format!("Status: {status}\n"));
        }
        b.push_str("Headers:\n");
        write_headers(&mut b, headers);
        b.push('\n');
        s.append_timeline(b.as_bytes());
    }

    /// Records a rejected websocket upgrade as an HTTP attempt.
    pub fn record_api_websocket_upgrade_rejection(
        &self,
        cfg: &Config,
        info: UpstreamRequestLog,
        status: u16,
        headers: &HeaderMap,
        body: &[u8],
    ) {
        self.state.lock().response_headers = headers.clone();
        if !request_log_capture_enabled(cfg) {
            return;
        }
        self.record_api_request(cfg, info);
        self.record_api_response_metadata(cfg, status, headers);
        self.append_api_response_chunk(cfg, body);
    }

    /// Records an upstream websocket response frame.
    pub fn append_api_websocket_response(&self, cfg: &Config, payload: &[u8]) {
        if !request_log_capture_enabled(cfg) {
            return;
        }
        let data = trim_space(payload);
        if data.is_empty() {
            return;
        }
        let mut s = self.state.lock();
        s.mark_response_timestamp();
        let mut b = format!("Timestamp: {}\nEvent: api.websocket.response\n", now_stamp()).into_bytes();
        b.extend_from_slice(data);
        b.push(b'\n');
        s.append_timeline(&b);
    }

    /// Records an upstream websocket error event.
    pub fn record_api_websocket_error(&self, cfg: &Config, stage: &str, err: &str) {
        if !request_log_capture_enabled(cfg) {
            return;
        }
        let mut s = self.state.lock();
        s.mark_response_timestamp();
        let mut b = format!("Timestamp: {}\nEvent: api.websocket.error\n", now_stamp());
        if !stage.trim().is_empty() {
            b.push_str(&format!("Stage: {}\n", stage.trim()));
        }
        b.push_str(&format!("Error: {err}\n"));
        s.append_timeline(b.as_bytes());
    }

    /// Merges extra response headers (for example quota headers parsed from a websocket frame)
    /// into the headers exposed to usage sinks.
    pub fn merge_response_headers(&self, extra: &HeaderMap) {
        let mut s = self.state.lock();
        for (k, v) in extra {
            s.response_headers.insert(k.clone(), v.clone());
        }
    }

    // ---- readers

    /// All attempts' request logs concatenated.
    pub fn api_request(&self) -> Vec<u8> {
        self.state.lock().attempts.iter().flat_map(|a| a.request.bytes()).collect()
    }

    /// All attempts' response logs concatenated, newline-terminated when non-empty.
    pub fn api_response(&self) -> Vec<u8> {
        let s = self.state.lock();
        let mut out: Vec<u8> = s.attempts.iter().flat_map(|a| a.response.iter().copied()).collect();
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out
    }

    /// The websocket event timeline.
    pub fn websocket_timeline(&self) -> Vec<u8> {
        self.state.lock().websocket_timeline.clone()
    }

    /// Requests captured while request logging was off, rendered as request-log blocks.
    pub fn deferred_api_requests(&self) -> Vec<Vec<u8>> {
        self.state.lock().deferred.iter().map(DeferredRequest::render).collect()
    }

    /// Upstream response headers of the latest attempt (for usage sinks).
    pub fn response_headers(&self) -> HeaderMap {
        self.state.lock().response_headers.clone()
    }

    /// Timestamp of the first upstream (or handler) response event, when one was recorded.
    pub fn response_timestamp(&self) -> Option<DateTime<Local>> {
        self.state.lock().response_timestamp
    }

    /// `API_RESPONSE_TIMESTAMP` set-once marker, shared with the handler layer.
    pub fn mark_response_timestamp(&self) {
        self.state.lock().mark_response_timestamp();
    }

    // ---- credits flag

    /// Flags the request as having used AI credits for billing.
    pub fn mark_credits_used(&self) {
        self.state.lock().credits_used = true;
    }

    pub fn credits_used(&self) -> bool {
        self.state.lock().credits_used
    }
}

impl LogState {
    fn defer(&mut self, info: UpstreamRequestLog) {
        let index = self.deferred.len() + 1;
        let remaining = MAX_DEFERRED_API_REQUEST_BODY_BYTES.saturating_sub(self.deferred_bytes);
        let capture_length = info.body.len().min(remaining);
        let mut captured = info;
        let body_empty = captured.body.is_empty();
        let body_truncated = capture_length < captured.body.len();
        captured.body.truncate(capture_length);
        self.deferred_bytes += capture_length;
        self.deferred.push(DeferredRequest {
            index,
            info: captured,
            captured_at: Local::now(),
            body_empty,
            capture_length,
            body_truncated,
        });
    }

    /// Latest attempt, creating a placeholder `<missing>` attempt when none exists.
    fn ensure_attempt(&mut self) -> &mut Attempt {
        if self.attempts.is_empty() {
            self.attempts.push(Attempt {
                index: 1,
                request: "=== API REQUEST 1 ===\n<missing>\n\n".into(),
                ..Default::default()
            });
        }
        let last = self.attempts.len() - 1;
        &mut self.attempts[last]
    }

    fn mark_response_timestamp(&mut self) {
        if self.response_timestamp.is_none() {
            self.response_timestamp = Some(Local::now());
        }
    }

    fn append_timeline(&mut self, chunk: &[u8]) {
        let data = trim_space(chunk);
        if data.is_empty() {
            return;
        }
        let existing = &mut self.websocket_timeline;
        if existing.len() >= MAX_RESPONSE_CAPTURE {
            return;
        }
        if !existing.is_empty() {
            if !existing.ends_with(b"\n") {
                existing.push(b'\n');
            }
            existing.push(b'\n');
        }
        let room = MAX_RESPONSE_CAPTURE.saturating_sub(existing.len());
        existing.extend_from_slice(&data[..data.len().min(room)]);
    }

    /// Writes response text to the latest attempt, emitting the `=== API RESPONSE n ===` intro
    /// first (padded so consecutive attempts are separated by a blank line).
    fn write_parts(&mut self, parts: Vec<Vec<u8>>) {
        let last = self.attempts.len().saturating_sub(1);
        if self.attempts.is_empty() {
            return;
        }
        if !self.attempts[last].response_intro_written {
            let previous_trailing = self.attempts[..last]
                .iter()
                .rev()
                .find(|a| a.response_intro_written)
                .map(|a| a.trailing_newlines);
            let attempt = &mut self.attempts[last];
            if let Some(trailing) = previous_trailing
                && trailing < 2
            {
                write_attempt_response(attempt, "\n".repeat(2 - trailing).as_bytes());
            }
            let index = attempt.index;
            write_attempt_response(attempt, format!("=== API RESPONSE {index} ===\n").as_bytes());
            write_attempt_response(attempt, format!("Timestamp: {}\n", now_stamp()).as_bytes());
            write_attempt_response(attempt, b"\n");
            attempt.response_intro_written = true;
        }
        for part in parts {
            write_attempt_response(&mut self.attempts[last], &part);
        }
    }
}

fn write_attempt_response(attempt: &mut Attempt, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    let mut trailing = payload.iter().rev().take_while(|b| **b == b'\n').count();
    if trailing == payload.len() {
        trailing += attempt.trailing_newlines;
    }
    attempt.trailing_newlines = trailing;
    let room = MAX_RESPONSE_CAPTURE.saturating_sub(attempt.response.len());
    attempt.response.extend_from_slice(&payload[..payload.len().min(room)]);
}

/// `strings.TrimSpace` over bytes.
fn trim_space(b: &[u8]) -> &[u8] {
    match std::str::from_utf8(b) {
        Ok(s) => s.trim().as_bytes(),
        Err(_) => b.trim_ascii(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(request_log: bool) -> Config {
        Config { request_log, ..Config::default() }
    }

    fn info(body: &str) -> UpstreamRequestLog {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer sk-secret-token-1234".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        UpstreamRequestLog {
            url: "https://api.example/v1".into(),
            method: "POST".into(),
            headers,
            body: body.as_bytes().to_vec(),
            provider: "claude".into(),
            auth_id: "a1".into(),
            auth_type: "api_key".into(),
            auth_value: "sk-secret-token-1234".into(),
            ..Default::default()
        }
    }

    #[test]
    fn captures_attempts_with_masked_secrets() {
        let log = ApiLog::new();
        let cfg = cfg(true);
        log.record_api_request(&cfg, info("{\"a\":1}"));
        log.record_api_response_metadata(&cfg, 200, &HeaderMap::new());
        log.append_api_response_chunk(&cfg, b"event: message_start");
        log.append_api_response_chunk(&cfg, b"data: {\"x\":1}");
        log.append_api_response_chunk(&cfg, b"data: {\"y\":2}");
        log.record_api_request(&cfg, info(""));
        log.record_api_response_error(&cfg, "boom");
        let req = String::from_utf8(log.api_request()).unwrap();
        assert!(req.starts_with("=== API REQUEST 1 ===\nTimestamp: "));
        assert!(req.contains("Upstream URL: https://api.example/v1\nHTTP Method: POST\nAuth: provider=claude, auth_id=a1, type=api_key value="));
        assert!(!req.contains("sk-secret-token-1234"));
        assert!(req.contains("Authorization: Bearer ") && req.contains("Content-Type: application/json\n"));
        assert!(req.contains("Body:\n{\"a\":1}\n\n=== API REQUEST 2 ===") && req.ends_with("Body:\n<empty>\n\n"));
        let resp = String::from_utf8(log.api_response()).unwrap();
        assert!(resp.starts_with("=== API RESPONSE 1 ===\nTimestamp: "));
        assert!(resp.contains("Status: 200\nHeaders:\n<none>\n\nBody:\nevent: message_start\ndata: {\"x\":1}\n\ndata: {\"y\":2}"));
        // The second attempt's intro is separated from the first by a blank line.
        assert!(resp.contains("}\n\n=== API RESPONSE 2 ===\n"));
        assert!(resp.ends_with("Error: boom\n"));
    }

    #[test]
    fn disabled_logging_defers_or_drops() {
        let log = ApiLog::new();
        log.record_api_request(&cfg(false), info("body"));
        assert!(log.api_request().is_empty());
        let deferred = String::from_utf8(log.deferred_api_requests().remove(0)).unwrap();
        assert!(deferred.starts_with("=== API REQUEST 1 ===") && deferred.ends_with("Body:\nbody\n\n"));
        let commercial = Config { request_log: true, commercial_mode: true, ..Config::default() };
        let log = ApiLog::new();
        log.record_api_request(&commercial, info("body"));
        assert!(log.api_request().is_empty() && log.deferred_api_requests().is_empty());
    }

    #[test]
    fn rfc3339_nano_trims_zeros() {
        use chrono::TimeZone;
        let t = Local.timestamp_nanos(1_700_000_000_120_000_000);
        let s = rfc3339_nano(t);
        assert!(s.contains(".12") && !s.contains(".120"));
    }

    #[test]
    fn websocket_timeline_and_urls() {
        let log = ApiLog::new();
        let cfg = cfg(true);
        log.record_api_websocket_handshake(&cfg, 101, &HeaderMap::new());
        log.append_api_websocket_response(&cfg, b"  {\"type\":\"x\"}  ");
        log.record_api_websocket_error(&cfg, "read", "closed");
        let timeline = String::from_utf8(log.websocket_timeline()).unwrap();
        assert!(timeline.contains("Event: api.websocket.handshake\nStatus: 101\nHeaders:\n<none>\n\nTimestamp"));
        assert!(timeline.contains("Event: api.websocket.response\n{\"type\":\"x\"}\n\nTimestamp"));
        assert!(timeline.ends_with("Stage: read\nError: closed"));
    }
}

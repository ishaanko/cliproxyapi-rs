//! Log endpoints (Go: `logs.go`): incremental `main.log` reads with opaque cursors, clearing,
//! error-log listing and request-log download. Reads are line oriented and only ever return
//! complete lines (up to the last newline).

use std::fs::{self, File};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use chrono::{Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::http::{ApiError, ApiResult, ok_json, query_trim};
use crate::state::ManagementState;

const DEFAULT_LOG_FILE: &str = "main.log";
const LOG_LINE_MAX: usize = 8 * 1024 * 1024;
const CURSOR_VERSION: i64 = 1;
const FINGERPRINT_MAX: i64 = 4 * 1024;

type IoResult<T> = std::io::Result<T>;

fn not_found(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

fn invalid_input(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg.to_string())
}

// ---- file naming ----

/// `main.log.<N>` (numeric rotation) or `main-<local timestamp>[.n].log[.gz]`; the order puts
/// larger values first in age (older), timestamps as `i64::MAX - unix`.
fn rotation_order(name: &str) -> Option<i64> {
    if let Some(n) = name
        .strip_prefix("main.log.")
        .and_then(|s| s.parse::<i64>().ok())
    {
        return Some(n);
    }
    let rest = name.strip_prefix("main-")?;
    let rest = rest.strip_suffix(".gz").unwrap_or(rest);
    let rest = rest.strip_suffix(".log")?;
    let rest = rest.split('.').next().unwrap_or("");
    let naive = NaiveDateTime::parse_from_str(rest, "%Y-%m-%dT%H-%M-%S").ok()?;
    let unix = Local.from_local_datetime(&naive).earliest()?.timestamp();
    Some(i64::MAX - unix)
}

fn is_rotated_log_file(name: &str) -> bool {
    rotation_order(name).is_some()
}

fn is_allowed_cursor_file(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return false;
    }
    name == DEFAULT_LOG_FILE || is_rotated_log_file(name)
}

fn safe_log_file_path(log_dir: &Path, name: &str) -> IoResult<PathBuf> {
    if !is_allowed_cursor_file(name) {
        return Err(invalid_input("invalid log file"));
    }
    Ok(crate::state::abs_path(log_dir).join(name))
}

/// Log files oldest first, ending with `main.log`.
fn collect_log_files(dir: &Path) -> IoResult<Vec<PathBuf>> {
    let mut cands: Vec<(i64, String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == DEFAULT_LOG_FILE {
            cands.push((0, name, entry.path()));
        } else if let Some(order) = rotation_order(&name) {
            cands.push((order, name, entry.path()));
        }
    }
    cands.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(cands.into_iter().rev().map(|(_, _, p)| p).collect())
}

// ---- parameters ----

fn parse_cutoff(raw: &str) -> i64 {
    raw.trim()
        .parse::<i64>()
        .ok()
        .filter(|t| *t > 0)
        .unwrap_or(0)
}

/// `0` means unlimited.
fn parse_limit(raw: &str) -> Result<usize, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(0);
    }
    let n: i64 = raw.parse().map_err(|_| "must be a positive integer")?;
    if n <= 0 {
        return Err("must be greater than zero");
    }
    Ok(n as usize)
}

/// Unix time of a log line's `YYYY-MM-DD HH:MM:SS` prefix (local time, optional leading `[`).
fn parse_timestamp(line: &str) -> i64 {
    let line = line.strip_prefix('[').unwrap_or(line);
    let Some(candidate) = line.get(..19) else {
        return 0;
    };
    NaiveDateTime::parse_from_str(candidate, "%Y-%m-%d %H:%M:%S")
        .ok()
        .and_then(|n| Local.from_local_datetime(&n).earliest())
        .map_or(0, |t| t.timestamp())
}

// ---- cursor ----

#[derive(Serialize, Deserialize, Default, Clone)]
struct LogCursor {
    #[serde(rename = "v", default)]
    version: i64,
    #[serde(default)]
    file: String,
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    size: i64,
    #[serde(rename = "modTime", default)]
    mod_time: i64,
    #[serde(rename = "modTimeUnixNano", default, skip_serializing_if = "is_zero")]
    mod_time_unix_nano: i64,
    #[serde(rename = "latestTimestamp", default)]
    latest_timestamp: i64,
    #[serde(default)]
    fingerprint: String,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

impl LogCursor {
    fn mod_time_nanos(&self) -> i64 {
        if self.mod_time_unix_nano > 0 {
            self.mod_time_unix_nano
        } else {
            self.mod_time * 1_000_000_000
        }
    }

    fn fingerprint_boundary(&self) -> i64 {
        if self.offset == 0 && self.size > 0 {
            self.size
        } else {
            self.offset
        }
    }
}

fn decode_cursor(raw: &str) -> Option<LogCursor> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    let data = URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| URL_SAFE.decode(value))
        .ok()?;
    let cursor: LogCursor = serde_json::from_slice(&data).ok()?;
    let valid = cursor.version == CURSOR_VERSION
        && is_allowed_cursor_file(&cursor.file)
        && cursor.offset >= 0
        && cursor.size >= 0
        && cursor.mod_time >= 0
        && cursor.latest_timestamp >= 0
        && !cursor.fingerprint.trim().is_empty();
    valid.then_some(cursor)
}

fn mod_nanos(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

fn read_range(file: &mut File, start: i64, len: i64, sink: &mut impl FnMut(&[u8])) -> IoResult<()> {
    if len <= 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(start as u64))?;
    let mut remaining = len as u64;
    let mut buf = vec![0u8; 32 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        sink(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(())
}

/// Hash of the first and last 4 KiB before `boundary`, which detects truncated or replaced files.
fn file_fingerprint(path: &Path, boundary: i64) -> IoResult<String> {
    if boundary < 0 {
        return Err(invalid_input("invalid fingerprint boundary"));
    }
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(invalid_input("invalid log file"));
    }
    if boundary > meta.len() as i64 {
        return Err(invalid_input("invalid fingerprint boundary"));
    }
    let mut hash = Sha256::new();
    hash.update(format!("log-cursor-v1:{boundary}:").as_bytes());
    let first_len = boundary.min(FINGERPRINT_MAX);
    read_range(&mut file, 0, first_len, &mut |b| hash.update(b))?;
    let tail_len = boundary.min(FINGERPRINT_MAX);
    let tail_start = boundary - tail_len;
    hash.update(format!(":{tail_start}:").as_bytes());
    read_range(&mut file, tail_start, tail_len, &mut |b| hash.update(b))?;
    Ok(URL_SAFE_NO_PAD.encode(&hash.finalize()[..12]))
}

fn new_cursor(path: &Path, offset: i64, latest: i64) -> IoResult<String> {
    let meta = fs::metadata(path)?;
    if meta.is_dir() {
        return Err(invalid_input("invalid log file"));
    }
    let size = meta.len() as i64;
    if offset < 0 || offset > size {
        return Err(invalid_input("invalid cursor offset"));
    }
    let probe = LogCursor {
        offset,
        size,
        ..Default::default()
    };
    let fingerprint = file_fingerprint(path, probe.fingerprint_boundary())?;
    let nanos = mod_nanos(&meta);
    let cursor = LogCursor {
        version: CURSOR_VERSION,
        file: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        offset,
        size,
        mod_time: nanos.div_euclid(1_000_000_000),
        mod_time_unix_nano: nanos,
        latest_timestamp: latest,
        fingerprint,
    };
    let raw = serde_json::to_vec(&cursor).map_err(|e| invalid_input(&e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(raw))
}

// ---- reading ----

#[derive(Default)]
struct CompleteRead {
    lines: Vec<String>,
    end_offset: i64,
    latest: i64,
    hit_limit: bool,
}

#[derive(Default)]
struct ReadResult {
    lines: Vec<String>,
    latest: i64,
    next_cursor: String,
}

/// Complete lines in `[offset, max_offset)` (`max_offset < 0`: end of file), at most `limit`
/// (`0`: unlimited). A trailing partial line is left for the next read.
fn read_complete_lines(
    path: &Path,
    offset: i64,
    max_offset: i64,
    limit: usize,
) -> IoResult<CompleteRead> {
    if offset < 0 {
        return Err(invalid_input("invalid log offset"));
    }
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(invalid_input("invalid log file"));
    }
    let size = meta.len() as i64;
    let max_offset = if max_offset < 0 || max_offset > size {
        size
    } else {
        max_offset
    };
    if offset > max_offset {
        return Err(invalid_input("invalid log offset"));
    }
    file.seek(SeekFrom::Start(offset as u64))?;
    let mut reader =
        std::io::BufReader::with_capacity(32 * 1024, file.take((max_offset - offset) as u64));
    let mut result = CompleteRead {
        end_offset: offset,
        ..Default::default()
    };
    let mut current = offset;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if line.len() > LOG_LINE_MAX {
            return Err(invalid_input(&format!(
                "log line exceeds {LOG_LINE_MAX} bytes"
            )));
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        current += n as i64;
        let text = String::from_utf8_lossy(&line[..line.len() - 1])
            .trim_end_matches('\r')
            .to_string();
        result.latest = result.latest.max(parse_timestamp(&text));
        result.lines.push(text);
        result.end_offset = current;
        if limit > 0 && result.lines.len() >= limit {
            result.hit_limit = true;
            break;
        }
    }
    Ok(result)
}

/// Offset just past the last newline (0 when there is none).
fn complete_boundary(path: &Path) -> IoResult<i64> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(invalid_input("invalid log file"));
    }
    let mut pos = meta.len() as i64;
    let mut buf = vec![0u8; 32 * 1024];
    while pos > 0 {
        let chunk = (buf.len() as i64).min(pos);
        pos -= chunk;
        file.seek(SeekFrom::Start(pos as u64))?;
        file.read_exact(&mut buf[..chunk as usize])?;
        if let Some(idx) = buf[..chunk as usize].iter().rposition(|b| *b == b'\n') {
            return Ok(pos + idx as i64 + 1);
        }
    }
    Ok(0)
}

/// Start offset such that at most `limit` complete lines precede `boundary`.
fn tail_start_offset(path: &Path, boundary: i64, limit: usize) -> IoResult<i64> {
    if limit == 0 {
        return Ok(0);
    }
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 32 * 1024];
    let mut pos = boundary;
    let mut breaks = 0usize;
    while pos > 0 {
        let chunk = (buf.len() as i64).min(pos);
        pos -= chunk;
        file.seek(SeekFrom::Start(pos as u64))?;
        file.read_exact(&mut buf[..chunk as usize])?;
        let mut data = &buf[..chunk as usize];
        while let Some(idx) = data.iter().rposition(|b| *b == b'\n') {
            breaks += 1;
            if breaks > limit {
                return Ok(pos + idx as i64 + 1);
            }
            data = &data[..idx];
        }
    }
    Ok(0)
}

fn read_tail_lines(path: &Path, limit: usize) -> IoResult<CompleteRead> {
    let boundary = complete_boundary(path)?;
    if boundary == 0 {
        return Ok(CompleteRead::default());
    }
    let start = tail_start_offset(path, boundary, limit)?;
    read_complete_lines(path, start, boundary, limit)
}

/// Cursor at the end of the newest log file that still exists.
fn cursor_for_latest(files: &[PathBuf], latest: i64) -> IoResult<String> {
    for path in files.iter().rev() {
        let boundary = match complete_boundary(path) {
            Ok(b) => b,
            Err(e) if not_found(&e) => continue,
            Err(e) => return Err(e),
        };
        match new_cursor(path, boundary, latest) {
            Ok(c) => return Ok(c),
            Err(e) if not_found(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(String::new())
}

/// The last `limit` complete lines across files (newest files first, prepended).
fn tail_files(files: &[PathBuf], limit: usize, fallback_latest: i64) -> IoResult<ReadResult> {
    let mut result = ReadResult {
        latest: fallback_latest,
        ..Default::default()
    };
    for path in files.iter().rev() {
        let remaining = if limit > 0 {
            let r = limit.saturating_sub(result.lines.len());
            if r == 0 {
                break;
            }
            r
        } else {
            0
        };
        let read = match read_tail_lines(path, remaining) {
            Ok(r) => r,
            Err(e) if not_found(&e) => continue,
            Err(e) => return Err(e),
        };
        if read.lines.is_empty() {
            continue;
        }
        let mut lines = read.lines;
        lines.append(&mut result.lines);
        result.lines = lines;
        result.latest = result.latest.max(read.latest);
    }
    result.next_cursor = cursor_for_latest(files, result.latest)?;
    Ok(result)
}

fn file_matches_cursor(path: &Path, cursor: &LogCursor) -> IoResult<(bool, bool)> {
    let meta = fs::metadata(path)?;
    if meta.is_dir() {
        return Err(invalid_input("invalid log file"));
    }
    let size = meta.len() as i64;
    if size < cursor.offset {
        return Ok((false, true));
    }
    let boundary = cursor.fingerprint_boundary();
    if size < boundary {
        return Ok((false, true));
    }
    Ok((
        file_fingerprint(path, boundary)? == cursor.fingerprint,
        false,
    ))
}

fn changed_after_cursor(path: &Path, cursor: &LogCursor) -> bool {
    fs::metadata(path)
        .is_ok_and(|m| !m.is_dir() && m.len() > 0 && mod_nanos(&m) > cursor.mod_time_nanos())
}

fn base_name(p: &Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

fn is_empty_main_cursor(cursor: &LogCursor) -> bool {
    cursor.file == DEFAULT_LOG_FILE && cursor.offset == 0 && cursor.size == 0
}

fn should_defer_empty_main_to_rotated(files: &[PathBuf], cursor: &LogCursor) -> bool {
    is_empty_main_cursor(cursor)
        && files
            .iter()
            .filter(|f| base_name(f) != DEFAULT_LOG_FILE)
            .any(|f| changed_after_cursor(f, cursor))
}

fn should_reset_ambiguous_empty_main(
    files: &[PathBuf],
    main_index: usize,
    cursor: &LogCursor,
) -> bool {
    if !is_empty_main_cursor(cursor) {
        return false;
    }
    let Ok(info) = fs::metadata(&files[main_index]) else {
        return false;
    };
    if info.is_dir()
        || (info.len() as i64 == cursor.size && mod_nanos(&info) == cursor.mod_time_nanos())
    {
        return false;
    }
    files.iter().enumerate().any(|(i, f)| {
        i != main_index
            && base_name(f) != DEFAULT_LOG_FILE
            && fs::metadata(f).is_ok_and(|m| !m.is_dir() && m.len() > 0)
            && !changed_after_cursor(f, cursor)
    })
}

/// Index of the log file the cursor points into, following rotation. `None` resets the cursor.
fn locate_cursor_file(files: &[PathBuf], cursor: &LogCursor) -> IoResult<Option<usize>> {
    let mut defer_empty_main = false;
    if let Some(index) = files.iter().position(|f| base_name(f) == cursor.file) {
        match file_matches_cursor(&files[index], cursor) {
            Err(e) if not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
            Ok((true, false)) => {
                if should_defer_empty_main_to_rotated(files, cursor) {
                    defer_empty_main = true;
                } else if should_reset_ambiguous_empty_main(files, index, cursor) {
                    return Ok(None);
                } else {
                    return Ok(Some(index));
                }
            }
            Ok(_) => {}
        }
    }
    if cursor.file != DEFAULT_LOG_FILE
        || (cursor.offset == 0 && cursor.size == 0 && !defer_empty_main)
    {
        return Ok(None);
    }
    let rotated = |i: usize| base_name(&files[i]) != DEFAULT_LOG_FILE;
    let check = |i: usize| -> IoResult<bool> {
        match file_matches_cursor(&files[i], cursor) {
            Err(e) if not_found(&e) => Ok(false),
            Err(e) => Err(e),
            Ok((matches, truncated)) => Ok(matches && !truncated),
        }
    };
    if cursor.offset == 0 && cursor.size == 0 {
        for i in (0..files.len()).filter(|i| rotated(*i)) {
            if changed_after_cursor(&files[i], cursor) && check(i)? {
                return Ok(Some(i));
            }
        }
        return Ok(None);
    }
    for i in (0..files.len()).rev().filter(|i| rotated(*i)) {
        if check(i)? {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// Reads complete lines after the cursor through later files. The bool is "cursor reset".
fn read_from_cursor(
    log_dir: &Path,
    files: &[PathBuf],
    raw: &str,
    limit: usize,
) -> IoResult<(ReadResult, bool)> {
    let Some(cursor) = decode_cursor(raw) else {
        return Ok((ReadResult::default(), true));
    };
    let mut result = ReadResult {
        latest: cursor.latest_timestamp,
        next_cursor: raw.to_string(),
        ..Default::default()
    };
    if safe_log_file_path(log_dir, &cursor.file).is_err() {
        return Ok((result, true));
    }
    let Some(start) = locate_cursor_file(files, &cursor)? else {
        return Ok((result, true));
    };
    let mut cursor_path = files[start].clone();
    let mut cursor_offset = cursor.offset;
    let mut advanced = false;
    for (i, path) in files.iter().enumerate().skip(start) {
        let remaining = if limit > 0 {
            let r = limit.saturating_sub(result.lines.len());
            if r == 0 {
                break;
            }
            r
        } else {
            0
        };
        let offset = if i == start { cursor.offset } else { 0 };
        let read = match read_complete_lines(path, offset, -1, remaining) {
            Ok(r) => r,
            Err(e) if not_found(&e) => return Ok((result, true)),
            Err(e) => return Err(e),
        };
        if !read.lines.is_empty() {
            result.latest = result.latest.max(read.latest);
            result.lines.extend(read.lines);
            cursor_path = path.clone();
            cursor_offset = read.end_offset;
            advanced = true;
        }
        if read.hit_limit {
            break;
        }
    }
    if !advanced {
        return Ok((result, false));
    }
    match new_cursor(&cursor_path, cursor_offset, result.latest) {
        Ok(c) => result.next_cursor = c,
        Err(e) if not_found(&e) => return Ok((result, true)),
        Err(e) => return Err(e),
    }
    Ok((result, false))
}

/// Legacy timestamp scan: lines newer than `cutoff`, continuation lines following their line's
/// decision; keeps the last `limit` lines.
struct Accumulator {
    cutoff: i64,
    limit: usize,
    lines: std::collections::VecDeque<String>,
    total: usize,
    latest: i64,
    include: bool,
}

impl Accumulator {
    fn consume_file(&mut self, path: &Path) -> IoResult<()> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if not_found(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut reader = std::io::BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.len() > LOG_LINE_MAX {
                return Err(invalid_input("bufio.Scanner: token too long"));
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            self.add_line(&String::from_utf8_lossy(&line));
        }
        Ok(())
    }

    fn add_line(&mut self, raw: &str) {
        let line = raw.trim_end_matches('\r');
        self.total += 1;
        let ts = parse_timestamp(line);
        self.latest = self.latest.max(ts);
        if ts > 0 {
            self.include = self.cutoff == 0 || ts > self.cutoff;
            if self.cutoff == 0 || self.include {
                self.push(line);
            }
            return;
        }
        if self.cutoff == 0 || self.include {
            self.push(line);
        }
    }

    fn push(&mut self, line: &str) {
        self.lines.push_back(line.to_string());
        if self.limit > 0 && self.lines.len() > self.limit {
            self.lines.pop_front();
        }
    }
}

// ---- handlers ----

fn logs_response(
    lines: Vec<String>,
    line_count: usize,
    latest: i64,
    next_cursor: String,
    reset: bool,
) -> Response {
    let mut payload = json!({"lines": lines, "line-count": line_count, "latest-timestamp": latest, "next-cursor": next_cursor});
    if reset {
        payload["cursor-reset"] = true.into();
    }
    ok_json(&payload)
}

fn require_log_setup(st: &ManagementState) -> ApiResult<PathBuf> {
    if !st.cfg().logging_to_file {
        return Err(ApiError::bad_request("logging to file disabled"));
    }
    log_dir(st)
}

fn log_dir(st: &ManagementState) -> ApiResult<PathBuf> {
    if st.log_dir.as_os_str().is_empty() {
        return Err(ApiError::new(500, "log directory not configured"));
    }
    Ok(st.log_dir.clone())
}

fn read_error(e: std::io::Error, what: &str) -> ApiError {
    ApiError::new(500, format!("failed to {what}: {e}"))
}

/// `GET /observability/logs?limit=&after=&cursor=`.
pub(crate) async fn get_logs(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let dir = require_log_setup(&st)?;
    let raw_cursor = query_trim(req.uri(), "cursor");
    let limit_raw = query_trim(req.uri(), "limit");
    let cutoff = parse_cutoff(&query_trim(req.uri(), "after"));
    tokio::task::spawn_blocking(move || get_logs_blocking(&dir, &raw_cursor, &limit_raw, cutoff))
        .await
        .map_err(|e| ApiError::new(500, e.to_string()))?
}

fn get_logs_blocking(dir: &Path, raw_cursor: &str, limit_raw: &str, cutoff: i64) -> ApiResult {
    let files = match collect_log_files(dir) {
        Ok(f) => f,
        Err(e) if not_found(&e) => {
            let mut latest = cutoff;
            if !raw_cursor.is_empty()
                && let Some(c) = decode_cursor(raw_cursor)
            {
                latest = latest.max(c.latest_timestamp);
            }
            return Ok(logs_response(
                Vec::new(),
                0,
                latest,
                String::new(),
                !raw_cursor.is_empty(),
            ));
        }
        Err(e) => return Err(read_error(e, "list log files")),
    };
    let limit =
        parse_limit(limit_raw).map_err(|e| ApiError::bad_request(format!("invalid limit: {e}")))?;
    let read_err = |e| read_error(e, "read log files");

    if !raw_cursor.is_empty() {
        let (result, reset) = read_from_cursor(dir, &files, raw_cursor, limit).map_err(read_err)?;
        if reset {
            let tail = tail_files(&files, limit, result.latest).map_err(read_err)?;
            let n = tail.lines.len();
            return Ok(logs_response(
                tail.lines,
                n,
                tail.latest,
                tail.next_cursor,
                true,
            ));
        }
        let n = result.lines.len();
        return Ok(logs_response(
            result.lines,
            n,
            result.latest,
            result.next_cursor,
            false,
        ));
    }
    if cutoff == 0 && limit > 0 {
        let tail = tail_files(&files, limit, 0).map_err(read_err)?;
        let n = tail.lines.len();
        return Ok(logs_response(
            tail.lines,
            n,
            tail.latest,
            tail.next_cursor,
            false,
        ));
    }

    let mut acc = Accumulator {
        cutoff,
        limit,
        lines: Default::default(),
        total: 0,
        latest: 0,
        include: false,
    };
    for f in &files {
        acc.consume_file(f)
            .map_err(|e| read_error(e, "read log file"))?;
    }
    let latest = if acc.latest == 0 || acc.latest < cutoff {
        cutoff
    } else {
        acc.latest
    };
    let next_cursor =
        cursor_for_latest(&files, latest).map_err(|e| read_error(e, "prepare log cursor"))?;
    Ok(logs_response(
        acc.lines.into_iter().collect(),
        acc.total,
        latest,
        next_cursor,
        false,
    ))
}

/// `DELETE /observability/logs`: truncates `main.log` and removes rotated files.
pub(crate) async fn delete_logs(State(st): State<ManagementState>) -> ApiResult {
    let dir = require_log_setup(&st)?;
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if not_found(&e) => return Err(ApiError::new(404, "log directory not found")),
        Err(e) => return Err(read_error(e, "list log directory")),
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name == DEFAULT_LOG_FILE {
            match File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_len(0))
            {
                Err(e) if !not_found(&e) => {
                    return Err(ApiError::new(
                        500,
                        format!("failed to truncate log file: {e}"),
                    ));
                }
                _ => {}
            }
        } else if is_rotated_log_file(&name) {
            match fs::remove_file(&path) {
                Err(e) if !not_found(&e) => {
                    return Err(ApiError::new(500, format!("failed to remove {name}: {e}")));
                }
                _ => removed += 1,
            }
        }
    }
    Ok(ok_json(
        &json!({"success": true, "message": "Logs cleared successfully", "removed": removed}),
    ))
}

/// `GET /observability/logs/errors`: `error-*.log` files, newest first (empty while request
/// logging is on, since then every request has a full log).
pub(crate) async fn error_logs(State(st): State<ManagementState>) -> ApiResult {
    if st.cfg().request_log {
        return Ok(ok_json(&json!({"files": []})));
    }
    let dir = log_dir(&st)?;
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if not_found(&e) => return Ok(ok_json(&json!({"files": []}))),
        Err(e) => return Err(read_error(e, "list request error logs")),
    };
    let mut files: Vec<(i64, serde_json::Value)> = Vec::new();
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("error-") || !name.ends_with(".log") {
            continue;
        }
        let meta = entry
            .metadata()
            .map_err(|e| ApiError::new(500, format!("failed to read log info for {name}: {e}")))?;
        let modified = mod_nanos(&meta).div_euclid(1_000_000_000);
        files.push((
            modified,
            json!({"name": name, "size": meta.len(), "modified": modified}),
        ));
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let files: Vec<_> = files.into_iter().map(|(_, v)| v).collect();
    Ok(ok_json(&json!({"files": files})))
}

fn attachment(path: &Path, name: &str) -> ApiResult {
    let data =
        fs::read(path).map_err(|e| ApiError::new(500, format!("failed to read log file: {e}")))?;
    let mut resp = Response::new(axum::body::Body::from(data));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        name.replace('"', "\\\"")
    )) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

/// Resolves `name` inside `dir`, rejecting escapes, missing files and directories.
fn resolve_log_file(dir: &Path, name: &str) -> ApiResult<PathBuf> {
    let dir_abs = crate::state::abs_path(dir);
    let full = cpa_auth::util::clean_path(&dir_abs.join(name));
    if !full.starts_with(&dir_abs) || full == dir_abs {
        return Err(ApiError::bad_request("invalid log file path"));
    }
    match fs::metadata(&full) {
        Ok(m) if m.is_dir() => Err(ApiError::bad_request("invalid log file")),
        Ok(_) => Ok(full),
        Err(e) if not_found(&e) => Err(ApiError::new(404, "log file not found")),
        Err(e) => Err(read_error(e, "read log file")),
    }
}

/// `GET /observability/logs/errors/{name}`.
pub(crate) async fn download_error_log(
    State(st): State<ManagementState>,
    UrlPath(name): UrlPath<String>,
) -> ApiResult {
    let dir = log_dir(&st)?;
    let name = name.trim().to_string();
    if name.is_empty() || name.contains(['/', '\\']) {
        return Err(ApiError::bad_request("invalid log file name"));
    }
    if !name.starts_with("error-") || !name.ends_with(".log") {
        return Err(ApiError::new(404, "log file not found"));
    }
    let full = resolve_log_file(&dir, &name)?;
    attachment(&full, &name)
}

/// `ShortRequestID`: the last 8 characters of a request id.
fn short_request_id(id: &str) -> &str {
    let id = id.trim();
    if id.len() > 8 {
        id.get(id.len() - 8..).unwrap_or(id)
    } else {
        id
    }
}

struct LogMeta {
    prefix: String,
    time: Option<NaiveDateTime>,
    seq: i64,
}

/// Splits `<prefix>-<timestamp>[_<seq>]-<id>.log` (timestamp `YYYY-MM-DDTHHMMSS`).
fn parse_log_metadata(filename: &str) -> LogMeta {
    let base = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    let Some(last_hyphen) = base.rfind('-').filter(|i| *i > 0) else {
        return LogMeta {
            prefix: base.to_string(),
            time: None,
            seq: 0,
        };
    };
    let before_id = &base[..last_hyphen];
    let (mut seq, mut before_seq) = (0, before_id);
    if let Some(us) = before_id.rfind('_')
        && let Ok(parsed) = before_id[us + 1..].parse::<i64>()
    {
        seq = parsed;
        before_seq = &before_id[..us];
    }
    const TS_LEN: usize = 17;
    if before_seq.len() >= TS_LEN
        && let Some(ts) = before_seq.get(before_seq.len() - TS_LEN..)
        && let Ok(t) = NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H%M%S")
    {
        let mut prefix = before_seq;
        if before_seq.len() > TS_LEN && before_seq.as_bytes()[before_seq.len() - TS_LEN - 1] == b'-'
        {
            prefix = &before_seq[..before_seq.len() - TS_LEN - 1];
        }
        return LogMeta {
            prefix: prefix.to_string(),
            time: Some(t),
            seq,
        };
    }
    LogMeta {
        prefix: before_seq.to_string(),
        time: None,
        seq,
    }
}

/// Newer by modification time; ties by embedded timestamp and collision sequence, then name.
fn log_file_is_newer(
    cand: &str,
    cand_mod: std::time::SystemTime,
    cur: &str,
    cur_mod: std::time::SystemTime,
) -> bool {
    if cand_mod > cur_mod {
        return true;
    }
    if cand_mod < cur_mod {
        return false;
    }
    let (c, p) = (parse_log_metadata(cand), parse_log_metadata(cur));
    if let (Some(ct), Some(pt)) = (c.time, p.time) {
        if ct != pt {
            return ct > pt;
        }
        if c.prefix == p.prefix && c.seq != p.seq {
            return c.seq > p.seq;
        }
    }
    cand > cur
}

/// `GET /observability/logs/requests/{id}`: the newest log file named `*-<short id>.log`.
pub(crate) async fn request_log_by_id(
    State(st): State<ManagementState>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult {
    let dir = log_dir(&st)?;
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("missing request ID"));
    }
    if id.contains(['/', '\\']) {
        return Err(ApiError::bad_request("invalid request ID"));
    }
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if not_found(&e) => return Err(ApiError::new(404, "log directory not found")),
        Err(e) => return Err(read_error(e, "list log directory")),
    };
    let suffix = format!("-{}.log", short_request_id(&id));
    let mut matched: Option<(String, std::time::SystemTime)> = None;
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(&suffix) {
            continue;
        }
        match entry.metadata().and_then(|m| m.modified()) {
            Err(_) => {
                if matched.is_none() {
                    matched = Some((name, UNIX_EPOCH));
                }
            }
            Ok(modified) => {
                let newer = matched
                    .as_ref()
                    .is_none_or(|(cur, cur_mod)| log_file_is_newer(&name, modified, cur, *cur_mod));
                if newer {
                    matched = Some((name, modified));
                }
            }
        }
    }
    let Some((name, _)) = matched else {
        return Err(ApiError::new(
            404,
            "log file not found for the given request ID",
        ));
    };
    let full = resolve_log_file(&dir, &name)?;
    attachment(&full, &name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        fs::write(dir.join(name), text).unwrap();
    }

    fn files(dir: &Path) -> Vec<PathBuf> {
        collect_log_files(dir).unwrap()
    }

    #[test]
    fn rotation_names_order_oldest_first_and_main_last() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "main.log", "c\n");
        write(d.path(), "main.log.2", "a\n");
        write(d.path(), "main.log.1", "b\n");
        write(d.path(), "main-2026-01-02T03-04-05.log.gz", "t\n");
        write(d.path(), "error-x.log", "e\n");
        let names: Vec<String> = files(d.path())
            .iter()
            .map(|p| base_name(p).to_string())
            .collect();
        assert_eq!(
            names,
            [
                "main-2026-01-02T03-04-05.log.gz",
                "main.log.2",
                "main.log.1",
                "main.log"
            ]
        );
        assert_eq!(base_name(files(d.path()).last().unwrap()), "main.log");
        assert!(is_rotated_log_file("main-2026-01-02T03-04-05.1.log"));
        assert!(!is_rotated_log_file("error-1.log"));
    }

    #[test]
    fn tail_returns_last_complete_lines_and_cursor_resumes_without_replay() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "main.log",
            "2026-01-01 10:00:00 one\n2026-01-01 10:00:01 two\n2026-01-01 10:00:02 three\npartial",
        );
        let fs_ = files(d.path());
        let tail = tail_files(&fs_, 2, 0).unwrap();
        assert_eq!(
            tail.lines,
            vec!["2026-01-01 10:00:01 two", "2026-01-01 10:00:02 three"]
        );
        assert!(!tail.next_cursor.is_empty());

        // Nothing new: same cursor, no lines.
        let (r, reset) = read_from_cursor(d.path(), &fs_, &tail.next_cursor, 0).unwrap();
        assert!(!reset && r.lines.is_empty() && r.next_cursor == tail.next_cursor);

        // The partial line completes and a new one arrives.
        write(
            d.path(),
            "main.log",
            "2026-01-01 10:00:00 one\n2026-01-01 10:00:01 two\n2026-01-01 10:00:02 three\npartial done\nfour\n",
        );
        let (r, reset) = read_from_cursor(d.path(), &fs_, &tail.next_cursor, 0).unwrap();
        assert!(!reset);
        assert_eq!(r.lines, vec!["partial done", "four"]);
    }

    #[test]
    fn cursor_resets_when_the_file_was_replaced() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "main.log", "aaaa\nbbbb\n");
        let fs_ = files(d.path());
        let tail = tail_files(&fs_, 10, 0).unwrap();
        write(d.path(), "main.log", "zzzz\nyyyy\nxxxx\n");
        let (_, reset) = read_from_cursor(d.path(), &fs_, &tail.next_cursor, 0).unwrap();
        assert!(reset);
        let (_, reset) = read_from_cursor(d.path(), &fs_, "not-a-cursor", 0).unwrap();
        assert!(reset);
    }

    #[test]
    fn legacy_scan_keeps_continuation_lines_of_included_entries() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "main.log",
            "2000-01-01 00:00:00 old\n  cont old\n2999-01-01 00:00:00 new\n  cont new\n",
        );
        let mut acc = Accumulator {
            cutoff: 1_700_000_000,
            limit: 0,
            lines: Default::default(),
            total: 0,
            latest: 0,
            include: false,
        };
        acc.consume_file(&d.path().join("main.log")).unwrap();
        assert_eq!(
            acc.lines.iter().cloned().collect::<Vec<_>>(),
            vec!["2999-01-01 00:00:00 new", "  cont new"]
        );
        assert_eq!(acc.total, 4);
    }

    #[test]
    fn request_log_ties_prefer_embedded_timestamp_then_sequence() {
        let t = UNIX_EPOCH;
        assert!(log_file_is_newer(
            "v1-chat-2026-01-02T030405-abcd1234.log",
            t,
            "v1-chat-2026-01-02T030404-abcd1234.log",
            t
        ));
        assert!(log_file_is_newer(
            "v1-chat-2026-01-02T030405_2-abcd1234.log",
            t,
            "v1-chat-2026-01-02T030405_1-abcd1234.log",
            t
        ));
        assert_eq!(short_request_id("0123456789abcdef"), "89abcdef");
        assert_eq!(short_request_id("abc"), "abc");
    }
}

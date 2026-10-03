//! Application logging (Go: internal/logging global_logger / log_dir_cleaner / requestid):
//! tracing subscriber with the Go line format, stdout or a size-rotated `main.log`, and the
//! background size cleaner.
//!
//! Line format: `[YYYY-MM-DD HH:MM:SS] [<reqid8|-------->] [<level:5>] [<file>:<line>] message[ k=v...]`.

use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cpa_config::Config;
use parking_lot::Mutex;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;

use crate::sse_validate::go_quote;

tokio::task_local! {
    /// Request id of the request being served; shown in the second log column.
    pub static REQUEST_ID: String;
}

/// `ShortRequestID`: last 8 characters.
pub fn short_request_id(request_id: &str) -> String {
    let id = request_id.trim();
    if id.len() > 8 {
        id[id.len() - 8..].to_string()
    } else {
        id.to_string()
    }
}

/// Applies Go `json.Marshal`'s default HTML escaping to compact JSON text: `<`, `>`, `&` and
/// U+2028/9 can only occur inside strings, so a plain replace is safe.
pub(crate) fn go_json_html_escape(json: String) -> String {
    if !json.contains(['<', '>', '&', '\u{2028}', '\u{2029}']) {
        return json;
    }
    json.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// `GenerateRequestID`: UUIDv7.
pub fn generate_request_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

const FIELD_ORDER: &[&str] = &[
    "provider",
    "model",
    "plugin_id",
    "plugin_name",
    "source_id",
    "version",
    "active_version",
    "retired_version",
    "overwritten",
    "mode",
    "budget",
    "level",
    "original_mode",
    "original_value",
    "min",
    "max",
    "clamped_to",
    "error",
    "credential",
    "auth_id",
    "connection",
    "proxy_scheme",
    "remote_transport",
    "media_session_id",
    "call_id",
    "peer",
    "state",
    "reason",
];

const QUOTED_FIELDS: &[&str] = &[
    "credential",
    "auth_id",
    "connection",
    "proxy_scheme",
    "remote_transport",
    "media_session_id",
    "call_id",
    "peer",
    "state",
    "reason",
];

#[derive(Default)]
struct FieldCollector {
    message: String,
    request_id: Option<String>,
    fields: Vec<(String, String, bool)>,
}

impl FieldCollector {
    fn push(&mut self, field: &Field, value: String, is_str: bool) {
        match field.name() {
            "message" => self.message = value,
            "request_id" => self.request_id = Some(value),
            name => self.fields.push((name.to_string(), value, is_str)),
        }
    }
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_string(), true);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.push(field, format!("{value:?}"), false);
    }
}

/// One formatted application log line plus the facts the Home forwarder needs.
pub(crate) struct LogLine {
    /// The Go `LogFormatter` line, newline terminated.
    pub text: String,
    /// logrus level name (`warning`, not `warn`).
    pub level: &'static str,
    pub time: chrono::DateTime<chrono::Local>,
    /// Full (unshortened) request id from the event field or the task-local, if any.
    pub request_id: Option<String>,
}

/// The Go `LogFormatter.Format` for one tracing event.
pub(crate) fn render_line(event: &Event<'_>) -> LogLine {
    let mut fields = FieldCollector::default();
    event.record(&mut fields);

    let request_id = fields
        .request_id
        .filter(|id| !id.is_empty())
        .or_else(|| REQUEST_ID.try_with(|id| id.clone()).ok().filter(|id| !id.is_empty()));
    let req = request_id.as_deref().map(short_request_id).unwrap_or_else(|| "--------".into());

    let meta = event.metadata();
    let (level, level_name) = match *meta.level() {
        Level::ERROR => ("error", "error"),
        Level::WARN => ("warn", "warning"),
        Level::INFO => ("info", "info"),
        Level::DEBUG => ("debug", "debug"),
        Level::TRACE => ("trace", "trace"),
    };
    let message = fields.message.trim_end_matches(['\r', '\n']);

    let mut extra = String::new();
    for key in FIELD_ORDER {
        if let Some((_, value, is_str)) = fields.fields.iter().find(|(k, _, _)| k == key) {
            let rendered = if *is_str && QUOTED_FIELDS.contains(key) {
                go_quote(value.as_bytes())
            } else {
                value.clone()
            };
            let _ = write!(extra, " {key}={rendered}");
        }
    }

    let time = chrono::Local::now();
    let ts = time.format("%Y-%m-%d %H:%M:%S");
    let text = match (meta.file(), meta.line()) {
        (Some(file), Some(line)) => {
            let base = Path::new(file).file_name().and_then(|n| n.to_str()).unwrap_or(file);
            format!("[{ts}] [{req}] [{level:<5}] [{base}:{line}] {message}{extra}\n")
        }
        _ => format!("[{ts}] [{req}] [{level:<5}] {message}{extra}\n"),
    };
    LogLine { text, level: level_name, time, request_id }
}

/// The Go `LogFormatter`.
pub struct GoLogFormat;

impl<S, N> FormatEvent<S, N> for GoLogFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, _ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
        writer.write_str(&render_line(event).text)
    }
}

// ------------------------------------------------------------------ output

/// `<dir>/main.log` rotated at 10 MiB into `main-<timestamp>.log` (lumberjack naming); no
/// backup limit and no compression.
pub struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max_size: u64,
}

impl RotatingFile {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let mut rf = RotatingFile {
            path,
            file: None,
            size: 0,
            max_size: 10 * 1024 * 1024,
        };
        rf.open_existing()?;
        Ok(rf)
    }

    fn open_existing(&mut self) -> io::Result<()> {
        let file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.size = file.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(file);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let stem = self.path.file_stem().and_then(|s| s.to_str()).unwrap_or("main");
        let ext = self.path.extension().and_then(|s| s.to_str()).unwrap_or("log");
        let stamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S%.3f");
        let backup = dir.join(format!("{stem}-{stamp}.{ext}"));
        if self.path.exists() {
            fs::rename(&self.path, backup)?;
        }
        self.open_existing()
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.file.is_none() {
            self.open_existing()?;
        }
        if self.size + buf.len() as u64 > self.max_size && self.size > 0 {
            self.rotate()?;
        }
        let n = match self.file.as_mut() {
            Some(f) => f.write(buf)?,
            None => return Err(io::Error::other("log file closed")),
        };
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

enum Output {
    Stdout,
    File(RotatingFile),
}

/// Receives every formatted log line (the TUI's log hook in standalone mode).
pub type LogTap = Arc<dyn Fn(&str) + Send + Sync>;

/// Swappable log destination shared with the subscriber.
#[derive(Clone)]
pub struct SwitchWriter {
    inner: Arc<Mutex<Output>>,
    tap: Arc<Mutex<Option<LogTap>>>,
}

/// Writer for one log event. The tap sees the whole event once, when the guard is dropped.
pub struct SwitchGuard {
    inner: Arc<Mutex<Output>>,
    tap: Arc<Mutex<Option<LogTap>>>,
    event: Vec<u8>,
}

impl Write for SwitchGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.tap.lock().is_some() {
            self.event.extend_from_slice(buf);
        }
        match &mut *self.inner.lock() {
            Output::Stdout => io::stdout().write(buf),
            Output::File(f) => f.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut *self.inner.lock() {
            Output::Stdout => io::stdout().flush(),
            Output::File(f) => f.flush(),
        }
    }
}

impl Drop for SwitchGuard {
    fn drop(&mut self) {
        if self.event.is_empty() {
            return;
        }
        let tap = self.tap.lock().clone();
        if let Some(tap) = tap {
            tap(&String::from_utf8_lossy(&self.event));
        }
    }
}

impl<'a> MakeWriter<'a> for SwitchWriter {
    type Writer = SwitchGuard;

    fn make_writer(&'a self) -> Self::Writer {
        SwitchGuard {
            inner: self.inner.clone(),
            tap: self.tap.clone(),
            event: Vec::new(),
        }
    }
}

/// Handle returned by [`init`]; `apply_config` reconfigures level, destination and the cleaner.
pub struct LogControl {
    writer: SwitchWriter,
    level: reload::Handle<LevelFilter, tracing_subscriber::Registry>,
    cleaner: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Installs the global subscriber (stdout, info level). Safe to call once per process.
pub fn init() -> Arc<LogControl> {
    let writer = SwitchWriter {
        inner: Arc::new(Mutex::new(Output::Stdout)),
        tap: Arc::new(Mutex::new(None)),
    };
    let (filter, level) = reload::Layer::new(LevelFilter::INFO);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .event_format(GoLogFormat)
        .with_writer(writer.clone());
    // The Home forwarder layer follows the level filter, like the logrus hook follows the level.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(crate::home_app_log::HomeAppLogLayer::global())
        .with(fmt_layer)
        .try_init();
    Arc::new(LogControl {
        writer,
        level,
        cleaner: Mutex::new(None),
    })
}

/// `ResolveLogDirectory`: `<WRITABLE_PATH>/logs`, else `logs` when writable, else
/// `<auth-dir>/logs`.
pub fn resolve_log_directory(cfg: &Config) -> PathBuf {
    let base = cpa_core::util::writable_path();
    if !base.is_empty() {
        return Path::new(&base).join("logs");
    }
    let log_dir = PathBuf::from("logs");
    if !is_dir_writable(&log_dir) {
        match cpa_config::resolve_auth_dir(&cfg.auth_dir) {
            Ok(auth_dir) if !auth_dir.as_os_str().is_empty() => return auth_dir.join("logs"),
            Ok(_) => {}
            Err(e) => tracing::warn!("Failed to resolve auth-dir {:?} for log directory: {e}", cfg.auth_dir),
        }
    }
    log_dir
}

fn is_dir_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(".perm_test");
    match File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl LogControl {
    /// Standalone TUI: mirrors every log event to `tap` (once per event).
    pub fn attach_tui(&self, tap: LogTap) {
        *self.writer.tap.lock() = Some(tap);
    }

    /// Undoes [`attach_tui`](Self::attach_tui) once the TUI has released the terminal.
    pub fn detach_tui(&self) {
        *self.writer.tap.lock() = None;
    }

    /// `ConfigureLogOutput` + `util.SetLogLevel`.
    pub fn apply_config(&self, cfg: &Config) -> io::Result<()> {
        let _ = self.level.modify(|f| *f = if cfg.debug { LevelFilter::DEBUG } else { LevelFilter::INFO });
        let log_dir = resolve_log_directory(cfg);
        let mut protected = None;
        {
            let mut out = self.writer.inner.lock();
            if cfg.logging_to_file {
                fs::create_dir_all(&log_dir)
                    .map_err(|e| io::Error::new(e.kind(), format!("logging: failed to create log directory: {e}")))?;
                let path = log_dir.join("main.log");
                *out = Output::File(RotatingFile::open(path.clone())?);
                protected = Some(path);
            } else {
                *out = Output::Stdout;
            }
        }
        if let Some(handle) = self.cleaner.lock().take() {
            handle.abort();
        }
        if cfg.logs_max_total_size_mb > 0
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let max_bytes = (cfg.logs_max_total_size_mb as u64) * 1024 * 1024;
            let task = handle.spawn(run_log_dir_cleaner(log_dir, max_bytes, protected));
            *self.cleaner.lock() = Some(task);
        }
        Ok(())
    }
}

async fn run_log_dir_cleaner(dir: PathBuf, max_bytes: u64, protected: Option<PathBuf>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    loop {
        ticker.tick().await;
        match enforce_log_dir_size_limit(&dir, max_bytes, protected.as_deref()) {
            Ok(n) if n > 0 => tracing::debug!("logging: removed {n} old log file(s) to enforce log directory size limit"),
            Ok(_) => {}
            Err(e) => tracing::warn!("logging: failed to enforce log directory size limit: {e}"),
        }
    }
}

fn is_log_file_name(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    !lower.is_empty() && (lower.ends_with(".log") || lower.ends_with(".log.gz"))
}

/// `enforceLogDirSizeLimit`: deletes the oldest `*.log` / `*.log.gz` files (never the protected
/// one) until the total size fits.
pub fn enforce_log_dir_size_limit(dir: &Path, max_bytes: u64, protected: Option<&Path>) -> io::Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_log_file_name(&name) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        total += meta.len();
        files.push((entry.path(), meta.len(), meta.modified().unwrap_or(std::time::UNIX_EPOCH)));
    }
    if total <= max_bytes {
        return Ok(0);
    }
    files.sort_by_key(|f| f.2);
    let mut deleted = 0;
    for (path, size, _) in files {
        if total <= max_bytes {
            break;
        }
        if protected.is_some_and(|p| p == path) {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// `time.Duration.String()` for the access log latency column.
pub fn go_duration_string(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0s".into();
    }
    let frac = |value: u128, unit: u128, digits: usize| -> String {
        let whole = value / unit;
        let rem = value % unit;
        if rem == 0 {
            return whole.to_string();
        }
        let mut s = format!("{rem:0digits$}");
        while s.ends_with('0') {
            s.pop();
        }
        format!("{whole}.{s}")
    };
    if nanos < 1_000 {
        return format!("{nanos}ns");
    }
    if nanos < 1_000_000 {
        return format!("{}µs", frac(nanos, 1_000, 3));
    }
    if nanos < 1_000_000_000 {
        return format!("{}ms", frac(nanos, 1_000_000, 6));
    }
    let total_secs = nanos / 1_000_000_000;
    let sub = nanos % 1_000_000_000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    let sec_text = frac(secs * 1_000_000_000 + sub, 1_000_000_000, 9);
    let mut out = String::new();
    if hours > 0 {
        let _ = write!(out, "{hours}h");
    }
    if hours > 0 || mins > 0 {
        let _ = write!(out, "{mins}m");
    }
    let _ = write!(out, "{sec_text}s");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids() {
        assert_eq!(short_request_id("01a0fb51-2286-7912-bedc-74541b480008"), "1b480008");
        assert_eq!(short_request_id("abc"), "abc");
    }

    #[test]
    fn duration_strings_match_go() {
        assert_eq!(go_duration_string(Duration::from_millis(0)), "0s");
        assert_eq!(go_duration_string(Duration::from_millis(5)), "5ms");
        assert_eq!(go_duration_string(Duration::from_millis(1500)), "1.5s");
        assert_eq!(go_duration_string(Duration::from_secs(62)), "1m2s");
        assert_eq!(go_duration_string(Duration::from_secs(3600)), "1h0m0s");
        assert_eq!(go_duration_string(Duration::from_micros(1500)), "1.5ms");
    }

    #[test]
    fn cleaner_removes_oldest_but_never_protected() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main.log");
        fs::write(&main, vec![b'x'; 100]).unwrap();
        let old = dir.path().join("old.log");
        fs::write(&old, vec![b'x'; 100]).unwrap();
        // make `old` older than `main`
        let file = File::options().write(true).open(&old).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(10)).unwrap();
        let removed = enforce_log_dir_size_limit(dir.path(), 150, Some(&main)).unwrap();
        assert_eq!(removed, 1);
        assert!(main.exists() && !old.exists());
        // main.log alone exceeds the limit but is protected
        assert_eq!(enforce_log_dir_size_limit(dir.path(), 10, Some(&main)).unwrap(), 0);
    }

    #[test]
    fn rotating_file_rolls_over_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let mut rf = RotatingFile::open(dir.path().join("main.log")).unwrap();
        rf.max_size = 10;
        rf.write_all(b"123456").unwrap();
        rf.write_all(b"789012").unwrap();
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert_eq!(fs::read(dir.path().join("main.log")).unwrap(), b"789012");
    }
}

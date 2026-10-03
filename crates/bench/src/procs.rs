//! Process management: pinned child processes, server config and `/proc` sampling.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};

pub const MOCK_PORT: u16 = 19090;
pub const SERVER_PORT: u16 = 18317;
pub const CLIENT_KEY: &str = "bench-client-key";

/// Clock ticks per second used by `/proc/<pid>/stat` (USER_HZ is 100 on Linux).
const CLK_TCK: f64 = 100.0;

/// A child process, killed on drop.
pub struct Proc {
    child: Child,
    pub pid: u32,
}

impl Proc {
    /// Spawns `program args...` pinned to `cpus` with `taskset -c` (when given). taskset execs the
    /// program, so the pid is the program's.
    pub fn spawn(program: &Path, args: &[&str], cpus: Option<&str>, dir: &Path, envs: &[(&str, &str)]) -> Result<Self> {
        // The child runs in `dir`, so a relative program path must be resolved first.
        let program = &program.canonicalize().with_context(|| format!("resolve {}", program.display()))?;
        let mut cmd = match cpus {
            Some(c) => {
                let mut cmd = Command::new("taskset");
                cmd.arg("-c").arg(c).arg(program);
                cmd
            }
            None => Command::new(program),
        };
        let child = cmd
            .args(args)
            .current_dir(dir)
            .envs(envs.iter().copied())
            .stdin(Stdio::null())
            // Both servers log every request at info level to stdout; discard it identically.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {}", program.display()))?;
        let pid = child.id().context("child pid")?;
        Ok(Proc { child, pid })
    }

    pub fn exited(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    pub async fn stop(mut self) {
        let _ = self.child.kill().await;
    }
}

/// Server config shared byte-for-byte by both implementations (legacy flat layout, accepted by
/// both). Request logging, usage statistics, file logging and debug are off.
pub fn server_config(auth_dir: &Path) -> String {
    let base = |f: &str| format!("http://127.0.0.1:{MOCK_PORT}/{f}");
    let keys = |f: &str, label: &str| {
        (1..=2)
            .map(|i| format!("  - api-key: \"sk-{label}-{i}\"\n    base-url: \"{}\"\n", base(f)))
            .collect::<String>()
    };
    format!(
        r#"host: "127.0.0.1"
port: {SERVER_PORT}
auth-dir: "{auth}"
api-keys:
  - "{CLIENT_KEY}"
remote-management:
  allow-remote: false
  disable-control-panel: true
debug: false
logging-to-file: false
request-log: false
usage-statistics-enabled: false
request-retry: 2
claude-api-key:
{claude}codex-api-key:
{codex}gemini-api-key:
{gemini}openai-compatibility:
  - name: "mockcompat"
    base-url: "{compat}"
    api-key-entries:
      - api-key: "sk-compat-1"
      - api-key: "sk-compat-2"
    models:
      - name: "mock-gpt-4o"
        alias: "compat-gpt-4o"
"#,
        auth = auth_dir.display(),
        claude = keys("anthropic", "claude"),
        codex = keys("codex", "codex"),
        gemini = keys("gemini", "gemini"),
        compat = base("compat"),
    )
}

/// Starts a server binary and waits until `/v1/models` answers 200 with a non-empty list.
/// Returns the process and the time from spawn to healthy.
pub async fn start_server(bin: &Path, dir: &Path, cpus: Option<&str>) -> Result<(Proc, Duration)> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir.join("auth"))?;
    // Absolute paths: the server runs with the work dir as its cwd.
    let dir = &dir.canonicalize()?;
    let auth = dir.join("auth");
    let cfg = dir.join("config.yaml");
    std::fs::write(&cfg, server_config(&auth))?;
    let dir_s = dir.to_string_lossy().into_owned();
    let t0 = Instant::now();
    let mut proc = Proc::spawn(
        bin,
        &["--config", &cfg.to_string_lossy(), "--local-model"],
        cpus,
        dir,
        &[("WRITABLE_PATH", &dir_s), ("TZ", "UTC")],
    )?;
    let target = crate::load::Target {
        addr: ([127, 0, 0, 1], SERVER_PORT).into(),
        method: hyper::Method::GET,
        path: "/v1/models".into(),
        body: Default::default(),
    };
    loop {
        if proc.exited()? {
            bail!("server {} exited early", bin.display());
        }
        let last = match crate::load::once(&target).await {
            Ok((200, body)) if String::from_utf8_lossy(&body).contains("\"id\"") => return Ok((proc, t0.elapsed())),
            Ok((status, _)) => format!("status {status}"),
            Err(e) => e.to_string(),
        };
        if t0.elapsed() > Duration::from_secs(60) {
            bail!("server {} not healthy after 60s ({last})", bin.display());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn status_kb(pid: u32, key: &str) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = s.lines().find(|l| l.starts_with(key))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Current resident set size in kB.
pub fn rss_kb(pid: u32) -> Option<u64> {
    status_kb(pid, "VmRSS:")
}

/// Peak resident set size (high-water mark) in kB since the last `reset_peak`.
pub fn peak_kb(pid: u32) -> Option<u64> {
    status_kb(pid, "VmHWM:")
}

/// Resets the kernel's RSS high-water mark to the current RSS.
pub fn reset_peak(pid: u32) {
    let _ = std::fs::write(format!("/proc/{pid}/clear_refs"), "5");
}

/// Total user+system CPU time of the process in seconds.
pub fn cpu_seconds(pid: u32) -> Option<f64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name; utime and stime are fields 14 and 15.
    let rest = s.rsplit_once(')')?.1;
    let mut f = rest.split_whitespace();
    let utime: f64 = f.nth(11)?.parse().ok()?;
    let stime: f64 = f.next()?.parse().ok()?;
    Some((utime + stime) / CLK_TCK)
}

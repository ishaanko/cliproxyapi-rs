//! Process management: pinned child processes, server config and `/proc` sampling.

use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};

use crate::meter::Meter;

/// Fallback ports; `run` and `quick` pick free ones at startup (`free_ports`).
const DEFAULT_MOCK_PORT: u16 = 19090;
const DEFAULT_SERVER_PORT: u16 = 18317;

static PORTS: OnceLock<(u16, u16)> = OnceLock::new();

/// Overrides the (server, mock) ports for this process.
pub fn set_ports(server: u16, mock: u16) {
    let _ = PORTS.set((server, mock));
}

pub fn server_port() -> u16 {
    PORTS.get().map_or(DEFAULT_SERVER_PORT, |p| p.0)
}

pub fn mock_port() -> u16 {
    PORTS.get().map_or(DEFAULT_MOCK_PORT, |p| p.1)
}

/// Two currently free loopback ports (bound and released; a small race window remains).
pub fn free_ports() -> Result<(u16, u16)> {
    let a = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let b = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok((a.local_addr()?.port(), b.local_addr()?.port()))
}
pub const CLIENT_KEY: &str = "bench-client-key";

/// Clock ticks per second used by `/proc/<pid>/stat` (USER_HZ is 100 on Linux).
const CLK_TCK: f64 = 100.0;

/// A child process, killed on drop.
pub struct Proc {
    child: Child,
    pub pid: u32,
    /// Perf/proc/allocator counters, attached before the program starts (see `spawn_gated`).
    pub meter: Option<Meter>,
    /// Write end of the start gate of a gated spawn.
    gate: Option<ChildStdin>,
}

impl Proc {
    /// Spawns `program args...` pinned to `cpus` with `taskset -c` (when given). taskset execs the
    /// program, so the pid is the program's.
    pub fn spawn(program: &Path, args: &[&str], cpus: Option<&str>, dir: &Path, envs: &[(&str, &str)]) -> Result<Self> {
        Self::spawn_inner(program, args, cpus, dir, envs, false)
    }

    /// Like `spawn`, but the program does not start until `release`: the child is a shell blocked
    /// on `read`, so a perf counter attached to its pid (with `inherit`) sees every thread the
    /// program will ever create, no matter how the scheduler orders parent and child.
    pub fn spawn_gated(program: &Path, args: &[&str], cpus: Option<&str>, dir: &Path, envs: &[(&str, &str)]) -> Result<Self> {
        Self::spawn_inner(program, args, cpus, dir, envs, true)
    }

    fn spawn_inner(program: &Path, args: &[&str], cpus: Option<&str>, dir: &Path, envs: &[(&str, &str)], gated: bool) -> Result<Self> {
        // The child runs in `dir`, so a relative program path must be resolved first.
        let program = &program.canonicalize().with_context(|| format!("resolve {}", program.display()))?;
        let mut cmd = if gated {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", GATE_SCRIPT, "sh"]);
            if let Some(c) = cpus {
                cmd.args(["taskset", "-c", c]);
            }
            cmd.arg(program);
            cmd
        } else if let Some(c) = cpus {
            let mut cmd = Command::new("taskset");
            cmd.arg("-c").arg(c).arg(program);
            cmd
        } else {
            Command::new(program)
        };
        let mut child = cmd
            .args(args)
            .current_dir(dir)
            .envs(envs.iter().copied())
            .stdin(if gated { Stdio::piped() } else { Stdio::null() })
            // Both servers log every request at info level to stdout; discard it identically.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {}", program.display()))?;
        let pid = child.id().context("child pid")?;
        let gate = child.stdin.take();
        Ok(Proc { child, pid, meter: None, gate })
    }

    /// Lets a gated program start (no-op for ungated spawns).
    pub async fn release(&mut self) -> Result<()> {
        if let Some(mut g) = self.gate.take() {
            g.write_all(b"\n").await?;
            g.flush().await?;
        }
        Ok(())
    }

    pub fn exited(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    pub async fn stop(mut self) {
        // Pooled connections to a dead server are useless (and would count as errors).
        crate::load::clear_pools();
        let _ = self.child.kill().await;
    }
}

/// Shell that waits for the release byte, then replaces itself with the program (keeping the pid).
const GATE_SCRIPT: &str = "read _; exec \"$@\" </dev/null";

/// Server config shared byte-for-byte by both implementations (legacy flat layout, accepted by
/// both). Request logging, usage statistics, file logging and debug are off.
pub fn server_config(auth_dir: &Path) -> String {
    let (server_port, mock_port) = (server_port(), mock_port());
    let base = |f: &str| format!("http://127.0.0.1:{mock_port}/{f}");
    let keys = |f: &str, label: &str| {
        (1..=2)
            .map(|i| format!("  - api-key: \"sk-{label}-{i}\"\n    base-url: \"{}\"\n", base(f)))
            .collect::<String>()
    };
    format!(
        r#"host: "127.0.0.1"
port: {server_port}
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
    // A Rust server built with the `alloc-stats` hook serves allocator totals on this abstract
    // socket; other builds ignore the variable.
    let sock = format!("cpa-alloc-{}-{}", std::process::id(), crate::load::now_us());
    let envs = [("WRITABLE_PATH", dir_s.as_str()), ("TZ", "UTC"), ("CPA_ALLOC_STATS_SOCK", sock.as_str())];
    let mut proc = Proc::spawn_gated(bin, &["--config", &cfg.to_string_lossy(), "--local-model"], cpus, dir, &envs)?;
    proc.meter = Some(Meter::attach(proc.pid, Some(sock.clone())));
    proc.release().await?;
    let target = crate::load::Target {
        addr: ([127, 0, 0, 1], server_port()).into(),
        method: hyper::Method::GET,
        path: "/v1/models".into(),
        body: Default::default(),
        ws: false,
    };
    loop {
        if proc.exited()? {
            bail!("server {} exited early", bin.display());
        }
        // A short per-probe timeout: a SYN sent before the listener exists can hang for a 1 s TCP
        // retransmit, which would show up as a 1 s startup.
        let last = match tokio::time::timeout(Duration::from_millis(10), crate::load::once(&target)).await {
            Ok(Ok((200, body))) if String::from_utf8_lossy(&body).contains("\"id\"") => return Ok((proc, t0.elapsed())),
            Ok(Ok((status, _))) => format!("status {status}"),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "probe timed out".into(),
        };
        if t0.elapsed() > Duration::from_secs(60) {
            bail!("server {} not healthy after 60s ({last})", bin.display());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
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

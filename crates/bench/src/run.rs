//! Orchestrator: runs every scenario against Go, Rust and direct-to-mock, several times, and
//! writes the raw samples to a JSON file that `report` turns into markdown.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::load::{self, Pcts, Running, Target};
use crate::meter::Meter;
use crate::procs::{self, Proc, mock_port, server_port};
use crate::scenarios::{self, Scenario, Shape};

#[derive(Args, Clone)]
pub struct RunArgs {
    #[arg(long, default_value = "target/bench-bin/cli-proxy-api-go")]
    pub go_bin: PathBuf,
    #[arg(long, default_value = "target/bench-bin/cliproxy")]
    pub rust_bin: PathBuf,
    /// Source checkout of the Go reference (only used to record its commit).
    #[arg(long, default_value = "/home/ishaan/box/cliproxyapirust/tmp/CLIProxyAPI")]
    pub go_src: PathBuf,
    #[arg(long, default_value = "bench/results/raw.json")]
    pub out: PathBuf,
    #[arg(long, default_value = "target/bench-work")]
    pub work_dir: PathBuf,
    #[arg(long, default_value_t = 5)]
    pub runs: usize,
    #[arg(long, value_delimiter = ',', default_values_t = [1usize, 16, 64, 256, 1024])]
    pub conc: Vec<usize>,
    /// Warmup seconds before each measured window.
    #[arg(long, default_value_t = 1.5)]
    pub warmup: f64,
    /// Measured seconds per throughput cell.
    #[arg(long, default_value_t = 5.0)]
    pub measure: f64,
    /// Servers to run (`go`, `rust`); `direct` (mock baseline) always runs.
    #[arg(long, value_delimiter = ',', default_values_t = ["go".to_string(), "rust".to_string()])]
    pub servers: Vec<String>,
    /// CPUs for the server under test, the mock and (via the wrapper script) the load generator.
    #[arg(long, default_value = "0-7")]
    pub server_cpus: String,
    #[arg(long, default_value = "8-11")]
    pub mock_cpus: String,
    /// Only run scenarios whose id contains one of these substrings.
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,
    #[arg(long)]
    pub skip_stream: bool,
    #[arg(long)]
    pub skip_large: bool,
    /// Skip the additional scenarios (long/native/Gemini/websocket/agent/slow-upstream).
    #[arg(long)]
    pub skip_extra: bool,
    #[arg(long, default_value_t = 2_000_000)]
    pub large_bytes: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cell {
    /// `tput`, `stream` or `large`.
    pub kind: String,
    pub scenario: String,
    pub conc: usize,
    pub requests: u64,
    pub errors: u64,
    pub rps: f64,
    /// Full request latency in microseconds.
    pub lat: Pcts,
    /// Server CPU microseconds per request (identical to ms per 1k requests).
    pub cpu_ms_per_1k: Option<f64>,
    /// User-space instructions per request (perf_event_open; `None` without a PMU).
    pub instr_per_req: Option<f64>,
    pub ctxsw_per_req: Option<f64>,
    pub rss_kb: Option<u64>,
    pub peak_kb: Option<u64>,
    /// Idle RSS of the server (3 s after startup), copied into every cell so `peak_kb - idle_kb`
    /// over the concurrency gives the memory per in-flight request.
    pub idle_kb: Option<u64>,
    /// Stream timing (microseconds): first body byte, first marked delta, age of each delta.
    pub ttfb: Option<Pcts>,
    pub ttft: Option<Pcts>,
    pub chunk: Option<Pcts>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Record {
    pub run: usize,
    /// `go`, `rust` or `direct`.
    pub server: String,
    pub loadavg: f64,
    pub startup_ms: Option<f64>,
    pub idle_rss_kb: Option<u64>,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Meta {
    pub date: String,
    pub cpu_model: String,
    pub logical_cpus: usize,
    pub mem_total_mb: u64,
    pub kernel: String,
    pub rustc: String,
    pub go_version: String,
    pub rust_commit: String,
    pub go_commit: String,
    pub go_bin_bytes: u64,
    pub rust_bin_bytes: u64,
    pub rust_stripped_bytes: u64,
    pub server_cpus: String,
    pub mock_cpus: String,
    pub load_cpus: String,
    pub runs: usize,
    pub warmup_s: f64,
    pub measure_s: f64,
    pub conc: Vec<usize>,
    pub large_bytes: usize,
    pub scenarios: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Results {
    pub meta: Meta,
    pub records: Vec<Record>,
}

fn cmd_out(program: &str, args: &[&str], dir: Option<&Path>) -> String {
    let mut c = std::process::Command::new(program);
    c.args(args);
    if let Some(d) = dir {
        c.current_dir(d);
    }
    c.output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

fn file_bytes(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn loadavg() -> f64 {
    std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| s.split_whitespace().next()?.parse().ok()).unwrap_or(0.0)
}

fn meta(a: &RunArgs, scenarios: &[&Scenario]) -> Meta {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu_model = cpuinfo.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split_once(':')).map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    let mem_kb: u64 = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("MemTotal"))?.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0);
    let go = std::env::var("GO_TOOLCHAIN").unwrap_or_else(|_| "/home/ishaan/box/cliproxyapirust/tmp/go-toolchain".into());
    let stripped = a.rust_bin.with_file_name("cliproxy-stripped");
    Meta {
        date: cmd_out("date", &["-u", "+%Y-%m-%d %H:%M UTC"], None),
        cpu_model,
        logical_cpus: cpuinfo.lines().filter(|l| l.starts_with("processor")).count(),
        mem_total_mb: mem_kb / 1024,
        kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string(),
        rustc: cmd_out("rustc", &["--version"], None),
        go_version: cmd_out(&format!("{go}/bin/go"), &["version"], None),
        rust_commit: cmd_out("git", &["rev-parse", "--short", "HEAD"], None),
        go_commit: cmd_out("git", &["rev-parse", "--short", "HEAD"], Some(&a.go_src)),
        go_bin_bytes: file_bytes(&a.go_bin),
        rust_bin_bytes: file_bytes(&a.rust_bin),
        rust_stripped_bytes: file_bytes(&stripped),
        server_cpus: a.server_cpus.clone(),
        mock_cpus: a.mock_cpus.clone(),
        load_cpus: std::env::var("CPA_BENCH_LOAD_CPUS").unwrap_or_else(|_| "unpinned".into()),
        runs: a.runs,
        warmup_s: a.warmup,
        measure_s: a.measure,
        conc: a.conc.clone(),
        large_bytes: a.large_bytes,
        scenarios: scenarios.iter().map(|s| (s.id.to_string(), s.description.to_string())).collect(),
    }
}

fn mock_target() -> Target {
    Target { addr: ([127, 0, 0, 1], mock_port()).into(), method: hyper::Method::GET, path: String::new(), body: Default::default(), ws: false }
}

/// Retunes the mock's stream shape.
pub async fn set_shape(sh: Shape) -> Result<()> {
    let Shape { first_ms, first_max_ms, gap_us, chunks } = sh;
    let t = Target { path: format!("/__ctl?first_ms={first_ms}&first_max_ms={first_max_ms}&gap_us={gap_us}&chunks={chunks}"), ..mock_target() };
    load::once(&t).await?;
    Ok(())
}

/// What a request targets: a server under test, or the mock directly.
#[derive(Clone, Copy, PartialEq)]
pub enum Side {
    Server,
    Direct,
}

pub fn target_for(sc: &Scenario, side: Side) -> Option<Target> {
    let (port, path) = match side {
        Side::Server => (server_port(), sc.path.clone()),
        Side::Direct => (mock_port(), sc.direct_path.clone()?),
    };
    Some(Target { addr: ([127, 0, 0, 1], port).into(), method: sc.method.clone(), path, body: sc.body.clone(), ws: sc.ws })
}

/// Fails unless the target answers 200 with the expected content.
pub async fn preflight(sc: &Scenario, t: &Target, who: &str, side: Side) -> Result<()> {
    let (status, body) = load::once(t).await?;
    let text = String::from_utf8_lossy(&body);
    if status != 200 || (side == Side::Server && !text.contains(sc.expect)) {
        let head: String = text.chars().take(300).collect();
        bail!("preflight {} on {who}: status {status}, body: {head}", sc.id);
    }
    Ok(())
}

struct CellSpec<'a> {
    kind: &'a str,
    id: &'a str,
    target: &'a Target,
    conc: usize,
    warmup: f64,
    measure: f64,
    timing: bool,
    pid: Option<u32>,
    meter: Option<&'a mut Meter>,
}

async fn run_cell(mut s: CellSpec<'_>) -> Result<Cell> {
    let running = Running::start(s.target, s.conc, s.timing);
    tokio::time::sleep(Duration::from_secs_f64(s.warmup)).await;
    if let Some(pid) = s.pid {
        procs::reset_peak(pid);
    }
    let m0 = s.meter.as_deref_mut().map(Meter::sample);
    running.set_measuring(true);
    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_secs_f64(s.measure)).await;
    running.set_measuring(false);
    let elapsed = t0.elapsed().as_secs_f64();
    let m1 = s.meter.as_deref_mut().map(Meter::sample);
    let rss_kb = s.pid.and_then(procs::rss_kb);
    let peak_kb = s.pid.and_then(procs::peak_kb);
    let mut out = running.finish().await?;
    let requests = out.lat_us.len() as u64;
    let per = m0.zip(m1).map(|(a, b)| b.since(&a, requests)).unwrap_or_default();
    let mut cell = Cell {
        kind: s.kind.into(),
        scenario: s.id.into(),
        conc: s.conc,
        requests,
        errors: out.errors,
        rps: requests as f64 / elapsed,
        lat: load::pcts(&mut out.lat_us),
        cpu_ms_per_1k: per.cpu_us,
        instr_per_req: per.instr,
        ctxsw_per_req: per.ctxsw,
        rss_kb,
        peak_kb,
        ..Default::default()
    };
    if s.timing {
        cell.ttfb = Some(load::pcts(&mut out.ttfb_us));
        cell.ttft = Some(load::pcts(&mut out.ttft_us));
        cell.chunk = Some(load::pcts(&mut out.chunk_us));
    }
    eprintln!(
        "    {:<24} c={:<4} {:>9.0} req/s  p50 {:>8}us p99 {:>8}us  err {}",
        s.id, s.conc, cell.rps, cell.lat.p50, cell.lat.p99, cell.errors
    );
    Ok(cell)
}

pub async fn run(a: RunArgs) -> Result<()> {
    let mut standard = scenarios::standard();
    let mut large = if a.skip_large { vec![] } else { scenarios::large(a.large_bytes) };
    let keep = |s: &Scenario| a.only.is_empty() || a.only.iter().any(|o| s.id.contains(o.as_str()));
    let mut extra = if a.skip_extra { vec![] } else { scenarios::extras() };
    standard.retain(keep);
    large.retain(keep);
    extra.retain(keep);
    let all: Vec<&Scenario> = standard.iter().chain(&large).chain(&extra).collect();
    for l in large.iter().chain(&extra).filter(|s| s.id.starts_with("agent") || s.kind == "large") {
        eprintln!("{}: {} bytes", l.id, l.body.len());
    }
    let mut results = Results { meta: meta(&a, &all), records: vec![] };
    std::fs::create_dir_all(a.out.parent().unwrap_or(Path::new(".")))?;

    // Free ports, so a run never collides with another benchmark process on the machine.
    let (sp, mp) = procs::free_ports()?;
    procs::set_ports(sp, mp);
    let exe = std::env::current_exe()?;
    let mock = Proc::spawn(&exe, &["mock", "--port", &mock_port().to_string()], Some(&a.mock_cpus), Path::new("."), &[])?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    set_shape(Shape::FAST).await?;

    for run in 0..a.runs {
        let mut order: Vec<&str> = a.servers.iter().map(String::as_str).collect();
        // Alternate which server goes first so drift (thermal, neighbours) hits both equally.
        if run % 2 == 1 {
            order.reverse();
        }
        order.push("direct");
        for who in order {
            eprintln!("== run {}/{} server {who}", run + 1, a.runs);
            let rec = run_one(&a, who, run, &standard, &large, &extra).await?;
            results.records.push(rec);
            std::fs::write(&a.out, serde_json::to_vec_pretty(&results)?)?;
        }
    }
    mock.stop().await;
    eprintln!("wrote {}", a.out.display());
    Ok(())
}

async fn run_one(a: &RunArgs, who: &str, run: usize, standard: &[Scenario], large: &[Scenario], extra: &[Scenario]) -> Result<Record> {
    let mut rec = Record { run, server: who.into(), loadavg: loadavg(), ..Default::default() };
    let (side, mut proc) = if who == "direct" {
        (Side::Direct, None)
    } else {
        let bin = if who == "go" { &a.go_bin } else { &a.rust_bin };
        let dir = a.work_dir.join(format!("{who}-{run}"));
        let (proc, startup) = procs::start_server(bin, &dir, Some(&a.server_cpus)).await?;
        rec.startup_ms = Some(startup.as_secs_f64() * 1e3);
        tokio::time::sleep(Duration::from_secs(3)).await;
        rec.idle_rss_kb = procs::rss_kb(proc.pid);
        (Side::Server, Some(proc))
    };
    let pid = proc.as_ref().map(|p| p.pid);

    set_shape(Shape::FAST).await?;
    // Warm every route (connection pools, caches, GC) and verify the responses.
    for sc in standard.iter().chain(large).chain(extra) {
        let Some(t) = target_for(sc, side) else { continue };
        set_shape(sc.shape).await?;
        preflight(sc, &t, who, side).await?;
        if sc.kind != "large" {
            let r = Running::start(&t, 16, false);
            tokio::time::sleep(Duration::from_secs(2)).await;
            r.finish().await?;
        }
    }

    // Preflights left the last scenario's shape behind.
    set_shape(Shape::FAST).await?;
    for sc in standard {
        let Some(t) = target_for(sc, side) else { continue };
        for &conc in &a.conc {
            let spec = CellSpec { kind: "tput", id: sc.id, target: &t, conc, warmup: a.warmup, measure: a.measure, timing: false, pid, meter: proc.as_mut().and_then(|p| p.meter.as_mut()) };
            rec.cells.push(run_cell(spec).await?);
        }
    }

    if !a.skip_stream {
        // Upstream with think time and paced deltas, so added proxy latency is visible.
        set_shape(Shape::PACED).await?;
        for sc in standard.iter().filter(|s| s.stream) {
            let Some(t) = target_for(sc, side) else { continue };
            for (conc, measure) in [(1usize, 8.0), (16, 6.0)] {
                let spec = CellSpec { kind: "stream", id: sc.id, target: &t, conc, warmup: 1.0, measure, timing: true, pid, meter: proc.as_mut().and_then(|p| p.meter.as_mut()) };
                rec.cells.push(run_cell(spec).await?);
            }
        }
        set_shape(Shape::FAST).await?;
    }

    for sc in large {
        let Some(t) = target_for(sc, side) else { continue };
        for conc in [1usize, 8] {
            let spec = CellSpec { kind: "large", id: sc.id, target: &t, conc, warmup: 1.0, measure: 6.0, timing: false, pid, meter: proc.as_mut().and_then(|p| p.meter.as_mut()) };
            rec.cells.push(run_cell(spec).await?);
        }
    }

    for sc in extra {
        let Some(t) = target_for(sc, side) else { continue };
        set_shape(sc.shape).await?;
        // Slow upstreams need a warmup longer than their think time.
        let warmup = if sc.shape.first_max_ms > 0 { a.warmup.max(3.0) } else { a.warmup };
        for &conc in sc.conc {
            let spec = CellSpec { kind: "extra", id: sc.id, target: &t, conc, warmup, measure: sc.measure_s, timing: false, pid, meter: proc.as_mut().and_then(|p| p.meter.as_mut()) };
            rec.cells.push(run_cell(spec).await?);
        }
    }
    set_shape(Shape::FAST).await?;

    for c in &mut rec.cells {
        c.idle_kb = rec.idle_rss_kb;
    }
    if let Some(p) = proc {
        p.stop().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(rec)
}

#[derive(Args, Clone)]
pub struct LoadArgs {
    /// Scenario id (standard or large).
    #[arg(long)]
    pub scenario: String,
    #[arg(long, default_value_t = 64)]
    pub conc: usize,
    #[arg(long, default_value_t = 10.0)]
    pub secs: f64,
    #[arg(long, default_value_t = 1.5)]
    pub warmup: f64,
    /// Pid of the server, for CPU per request.
    #[arg(long)]
    pub pid: Option<u32>,
    #[arg(long, default_value_t = 2_000_000)]
    pub large_bytes: usize,
}

/// Runs one throughput cell against a server and mock that are already up (profiling aid).
pub async fn load_only(a: LoadArgs) -> Result<()> {
    let sc = scenarios::standard()
        .into_iter()
        .chain(scenarios::large(a.large_bytes))
        .find(|s| s.id == a.scenario)
        .ok_or_else(|| anyhow::anyhow!("unknown scenario {}", a.scenario))?;
    let t = target_for(&sc, Side::Server).ok_or_else(|| anyhow::anyhow!("no server target"))?;
    preflight(&sc, &t, "server", Side::Server).await?;
    let cell = run_cell(CellSpec { kind: "tput", id: sc.id, target: &t, conc: a.conc, warmup: a.warmup, measure: a.secs, timing: false, pid: a.pid }).await?;
    if let Some(c) = cell.cpu_ms_per_1k {
        eprintln!("    cpu ms/1k req: {c:.0}");
    }
    Ok(())
}

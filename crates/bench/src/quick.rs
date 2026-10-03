//! `cpa-bench quick`: one scenario, a fixed request count, deterministic metrics in well under a
//! minute. Meant for the inner optimization loop (see bench/README.md): CPU time, instructions,
//! allocations and context switches per request barely move with machine load; wall-clock columns
//! (req/s, latency) are only trustworthy under `tools/exclusive.sh`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::load::{self, Running};
use crate::meter::{PerRequest, Sample};
use crate::procs::{self, Proc};
use crate::run::{Side, preflight, set_shape, target_for};
use crate::scenarios::{self, Scenario};

#[derive(Args, Clone)]
pub struct QuickArgs {
    /// Scenario id (`--list` shows them).
    pub scenario: Option<String>,
    #[arg(long)]
    pub list: bool,
    /// Run every scenario and finish with one markdown comparison table.
    #[arg(long)]
    pub all: bool,
    /// Concurrent connections; default depends on the scenario (64, or 1024 for slow upstreams).
    #[arg(long)]
    pub conc: Option<usize>,
    /// Requests per measured window; default depends on the scenario (see `--list`).
    #[arg(long)]
    pub requests: Option<u64>,
    /// Measured windows on the same server instance; the table shows the median of each metric.
    #[arg(long, default_value_t = 3)]
    pub reps: usize,
    /// Servers to run: `go`, `rust` (comma separated).
    #[arg(long, value_delimiter = ',', default_values_t = ["go".to_string(), "rust".to_string()])]
    pub servers: Vec<String>,
    #[arg(long, default_value = "target/bench-bin/cli-proxy-api-go")]
    pub go_bin: PathBuf,
    #[arg(long, default_value = "target/bench-bin/cliproxy")]
    pub rust_bin: PathBuf,
    /// Rust build with the `alloc-stats` hook; when it exists, one extra pass reports allocations.
    #[arg(long, default_value = "target/bench-bin/cliproxy-alloc")]
    pub rust_alloc_bin: PathBuf,
    #[arg(long, default_value = "target/bench-work")]
    pub work_dir: PathBuf,
    #[arg(long, default_value = "0-7")]
    pub server_cpus: String,
    #[arg(long, default_value = "8-11")]
    pub mock_cpus: String,
    #[arg(long, default_value_t = 2_000_000)]
    pub large_bytes: usize,
}

/// One measured window.
#[derive(Clone, Copy, Debug, Default)]
struct Window {
    rps: f64,
    p50_us: u64,
    p99_us: u64,
    errors: u64,
    per: PerRequest,
    /// RSS at the end of the window and the high-water mark within it (kB).
    rss1_kb: u64,
    peak_kb: u64,
}

/// Per-server result: median over windows.
#[derive(Clone, Debug, Default)]
struct Summary {
    who: String,
    /// RSS a second after startup, before any request.
    idle_kb: u64,
    w: Window,
    instr_available: bool,
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

fn med_opt(ws: &[Window], f: impl Fn(&Window) -> Option<f64>) -> Option<f64> {
    median(ws.iter().filter_map(f).collect())
}

fn summarize(who: &str, idle_kb: u64, ws: &[Window], instr_available: bool) -> Summary {
    let m = |f: fn(&Window) -> f64| median(ws.iter().map(f).collect()).unwrap_or(0.0);
    Summary {
        who: who.into(),
        idle_kb,
        instr_available,
        w: Window {
            rps: m(|w| w.rps),
            p50_us: m(|w| w.p50_us as f64) as u64,
            p99_us: m(|w| w.p99_us as f64) as u64,
            errors: ws.iter().map(|w| w.errors).sum(),
            per: PerRequest {
                cpu_us: med_opt(ws, |w| w.per.cpu_us),
                task_us: med_opt(ws, |w| w.per.task_us),
                instr: med_opt(ws, |w| w.per.instr),
                ctxsw: med_opt(ws, |w| w.per.ctxsw),
                allocs: med_opt(ws, |w| w.per.allocs),
                alloc_bytes: med_opt(ws, |w| w.per.alloc_bytes),
            },
            rss1_kb: m(|w| w.rss1_kb as f64) as u64,
            peak_kb: m(|w| w.peak_kb as f64) as u64,
        },
    }
}

/// Default measured request count: enough to dwarf connection setup and 10 ms CPU ticks without
/// letting the slow scenarios run for minutes.
fn default_requests(sc: &Scenario, conc: usize) -> u64 {
    sc.quick_requests.max(conc as u64 * sc.quick_waves)
}

async fn window(proc: &mut Proc, t: &load::Target, conc: usize, requests: u64) -> Result<Window> {
    let pid = proc.pid;
    procs::reset_peak(pid);
    let m0: Option<Sample> = proc.meter.as_mut().map(|m| m.sample());
    let t0 = Instant::now();
    let mut out = Running::start_counted(t, conc, false, requests).join().await?;
    let elapsed = t0.elapsed().as_secs_f64();
    let m1 = proc.meter.as_mut().map(|m| m.sample());
    if std::env::var_os("CPA_BENCH_DEBUG").is_some() {
        eprintln!("samples {m0:?} -> {m1:?}");
    }
    let rss1_kb = procs::rss_kb(pid).unwrap_or(0);
    let peak_kb = procs::peak_kb(pid).unwrap_or(0);
    let done = out.lat_us.len() as u64;
    let p = load::pcts(&mut out.lat_us);
    Ok(Window {
        rps: done as f64 / elapsed,
        p50_us: p.p50,
        p99_us: p.p99,
        errors: out.errors,
        per: m0.zip(m1).map(|(a, b)| b.since(&a, done)).unwrap_or_default(),
        rss1_kb,
        peak_kb,
    })
}

/// Starts one server, warms it up, runs `reps` fixed-count windows and stops it.
async fn measure_server(a: &QuickArgs, sc: &Scenario, who: &str, bin: &Path, conc: usize, requests: u64) -> Result<Summary> {
    let dir = a.work_dir.join(format!("quick-{}-{who}", std::process::id()));
    let (mut proc, _) = procs::start_server(bin, &dir, pin(&a.server_cpus)).await?;
    // Idle RSS before any request: the baseline for memory per in-flight request.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let idle_kb = procs::rss_kb(proc.pid).unwrap_or(0);
    set_shape(sc.shape).await?;
    let t = target_for(sc, Side::Server).context("scenario has no server route")?;
    preflight(sc, &t, who, Side::Server).await?;
    // Warm pools, caches and the allocator, then let background work (GC, trimming) settle.
    let warm = (requests / 4).max(conc as u64).max(200);
    Running::start_counted(&t, conc, false, warm).join().await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let instr = proc.meter.as_ref().is_some_and(|m| m.has_instructions());
    let mut ws = vec![];
    for _ in 0..a.reps {
        ws.push(window(&mut proc, &t, conc, requests).await?);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    proc.stop().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(summarize(who, idle_kb, &ws, instr))
}

/// An empty CPU list means unpinned.
fn pin(cpus: &str) -> Option<&str> {
    (!cpus.is_empty()).then_some(cpus)
}

fn fmt_opt(v: Option<f64>, f: impl Fn(f64) -> String) -> String {
    v.map(f).unwrap_or_else(|| "-".into())
}

fn print_table(sc: &Scenario, a: &QuickArgs, conc: usize, requests: u64, rows: &[Summary]) {
    println!(
        "scenario {}  conc {}  requests {} x {} windows (median)  body {:.1} KB  shape {:?}",
        sc.id, conc, requests, a.reps, sc.body.len() as f64 / 1024.0, sc.shape
    );
    println!(
        "{:<11} {:>9} {:>9} {:>9} {:>11} {:>14} {:>9} {:>10} {:>10} {:>8} {:>8} {:>8} {:>10} {:>5}",
        "server", "req/s*", "p50 ms*", "p99 ms*", "cpu us/req", "instr/req", "ctxsw/req", "allocs/req", "alloc KB/r", "idle MB", "peak MB", "end MB", "KB/inflt", "errs"
    );
    for r in rows {
        let w = &r.w;
        // Peak RSS growth over idle, per in-flight request.
        let per_inflight = w.peak_kb.saturating_sub(r.idle_kb) as f64 / conc as f64;
        println!(
            "{:<11} {:>9.0} {:>9.2} {:>9.2} {:>11} {:>14} {:>9} {:>10} {:>10} {:>8.1} {:>8.1} {:>8.1} {:>10.1} {:>5}",
            r.who,
            w.rps,
            w.p50_us as f64 / 1e3,
            w.p99_us as f64 / 1e3,
            fmt_opt(w.per.task_us.or(w.per.cpu_us), |v| format!("{v:.1}")),
            fmt_opt(w.per.instr, |v| format!("{v:.0}")),
            fmt_opt(w.per.ctxsw, |v| format!("{v:.2}")),
            fmt_opt(w.per.allocs, |v| format!("{v:.1}")),
            fmt_opt(w.per.alloc_bytes, |v| format!("{:.1}", v / 1024.0)),
            r.idle_kb as f64 / 1024.0,
            w.peak_kb as f64 / 1024.0,
            w.rss1_kb as f64 / 1024.0,
            per_inflight,
            w.errors,
        );
    }
    if let [go, rust, ..] = rows.iter().filter(|r| r.who == "go" || r.who == "rust").collect::<Vec<_>>().as_slice() {
        let ratio = |r: Option<f64>, g: Option<f64>| match (r, g) {
            (Some(r), Some(g)) if g > 0.0 => format!("{:.2}x", r / g),
            _ => "-".into(),
        };
        let (g, r) = (&go.w, &rust.w);
        println!(
            "{:<11} {:>9} {:>9} {:>9} {:>11} {:>14} {:>9}",
            "rust/go",
            ratio(Some(r.rps), Some(g.rps)),
            ratio(Some(r.p50_us as f64), Some(g.p50_us as f64)),
            ratio(Some(r.p99_us as f64), Some(g.p99_us as f64)),
            ratio(r.per.task_us.or(r.per.cpu_us), g.per.task_us.or(g.per.cpu_us)),
            ratio(r.per.instr, g.per.instr),
            ratio(r.per.ctxsw, g.per.ctxsw),
        );
    }
    println!("* wall-clock columns are only meaningful under tools/exclusive.sh; cpu/instr/allocs/ctxsw are load-insensitive.");
    if rows.iter().any(|r| !r.instr_available) {
        println!("note: hardware instruction counting is unavailable here (no PMU or permission); instr/req shows '-'.");
    }
}

pub async fn quick(a: QuickArgs) -> Result<()> {
    let all = scenarios::all(a.large_bytes);
    if a.list || (a.scenario.is_none() && !a.all) {
        for s in &all {
            println!("{:<30} conc {:>4} min {:>5} req  {}", s.id, s.quick_conc, s.quick_requests, s.description);
        }
        return Ok(());
    }
    let picked: Vec<&Scenario> = if a.all {
        all.iter().collect()
    } else {
        let id = a.scenario.clone().unwrap_or_default();
        let Some(sc) = all.iter().find(|s| s.id == id) else { bail!("unknown scenario {id} (see --list)") };
        vec![sc]
    };

    // Free ports and a per-process work dir, so concurrent quick runs do not collide.
    let (sp, mp) = procs::free_ports()?;
    procs::set_ports(sp, mp);
    let exe = std::env::current_exe()?;
    let mock = Proc::spawn(&exe, &["mock", "--port", &procs::mock_port().to_string()], pin(&a.mock_cpus), Path::new("."), &[])?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut summary = vec![];
    for sc in picked {
        let conc = a.conc.unwrap_or(sc.quick_conc);
        let requests = a.requests.unwrap_or_else(|| default_requests(sc, conc));
        let rows = match run_servers(&a, sc, conc, requests).await {
            Ok(r) => r,
            Err(e) => {
                mock.stop().await;
                return Err(e);
            }
        };
        print_table(sc, &a, conc, requests, &rows);
        summary.push((sc.id, conc, rows));
    }
    mock.stop().await;
    if a.all {
        print_markdown(&summary);
    }
    Ok(())
}

/// One markdown table over every scenario (`--all`): Go vs Rust deterministic metrics.
fn print_markdown(all: &[(&str, usize, Vec<Summary>)]) {
    println!("\n| Scenario | Conc | Instr/req Go (k) | Rust (k) | Rust/Go | CPU us/req Go | Rust | Rust/Go | Rust allocs/req | Rust alloc KB/req | Peak RSS MB Go | Rust | KB/in-flight Go | Rust |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for (id, conc, rows) in all {
        let find = |who: &str| rows.iter().find(|r| r.who == who);
        let (go, rust) = (find("go"), find("rust"));
        let cpu = |r: Option<&Summary>| r.and_then(|r| r.w.per.task_us.or(r.w.per.cpu_us));
        let instr = |r: Option<&Summary>| r.and_then(|r| r.w.per.instr);
        let ratio = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) if b > 0.0 => format!("{:.2}x", a / b),
            _ => "-".into(),
        };
        let f = |v: Option<f64>, scale: f64, prec: usize| v.map_or("-".into(), |v| format!("{:.prec$}", v / scale));
        let peak = |r: Option<&Summary>| r.map_or("-".into(), |r| format!("{:.0}", r.w.peak_kb as f64 / 1024.0));
        let per_flight = |r: Option<&Summary>| r.map_or("-".into(), |r| format!("{:.0}", r.w.peak_kb.saturating_sub(r.idle_kb) as f64 / *conc as f64));
        println!(
            "| `{id}` | {conc} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            f(instr(go), 1e3, 0),
            f(instr(rust), 1e3, 0),
            ratio(instr(rust), instr(go)),
            f(cpu(go), 1.0, 0),
            f(cpu(rust), 1.0, 0),
            ratio(cpu(rust), cpu(go)),
            f(rust.and_then(|r| r.w.per.allocs), 1.0, 0),
            f(rust.and_then(|r| r.w.per.alloc_bytes), 1024.0, 0),
            peak(go),
            peak(rust),
            per_flight(go),
            per_flight(rust),
        );
    }
}

async fn run_servers(a: &QuickArgs, sc: &Scenario, conc: usize, requests: u64) -> Result<Vec<Summary>> {
    let mut rows = vec![];
    for who in &a.servers {
        let bin = if who == "go" { &a.go_bin } else { &a.rust_bin };
        eprintln!("== {who}");
        let mut s = measure_server(a, sc, who, bin, conc, requests).await?;
        if who == "rust" && a.rust_alloc_bin.exists() {
            // The counting allocator perturbs timing, so allocations come from a separate pass.
            eprintln!("== rust (alloc-stats build)");
            let alloc = measure_server_alloc(a, sc, conc, requests).await?;
            s.w.per.allocs = alloc.0;
            s.w.per.alloc_bytes = alloc.1;
        }
        rows.push(s);
    }
    Ok(rows)
}

/// Allocations and bytes per request from the `alloc-stats` build (median of its windows).
async fn measure_server_alloc(a: &QuickArgs, sc: &Scenario, conc: usize, requests: u64) -> Result<(Option<f64>, Option<f64>)> {
    let mut b = a.clone();
    b.reps = b.reps.min(2);
    let s = measure_server(&b, sc, "rust-alloc", &a.rust_alloc_bin, conc, requests).await?;
    Ok((s.w.per.allocs, s.w.per.alloc_bytes))
}

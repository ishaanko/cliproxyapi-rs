//! Turns the raw samples of `run` into `bench/results.md`: medians over runs, spread, and the
//! Rust/Go ratio for every cell.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::run::{Cell, Record, Results};

/// Median and half-range of a metric across runs.
#[derive(Clone, Copy)]
struct Agg {
    median: f64,
    /// (max - min) / 2 as a fraction of the median.
    spread: f64,
}

fn aggregate(mut v: Vec<f64>) -> Option<Agg> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let median = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
    let spread = if median > 0.0 { (v[n - 1] - v[0]) / 2.0 / median } else { 0.0 };
    Some(Agg { median, spread })
}

struct Data<'a>(&'a [Record]);

impl Data<'_> {
    /// Metric `f` of every run's cell for (server, kind, scenario, conc).
    fn agg(&self, server: &str, kind: &str, scenario: &str, conc: usize, f: impl Fn(&Cell) -> Option<f64>) -> Option<Agg> {
        let vals = self
            .0
            .iter()
            .filter(|r| r.server == server)
            .flat_map(|r| r.cells.iter())
            .filter(|c| c.kind == kind && c.scenario == scenario && c.conc == conc)
            .filter_map(&f)
            .collect();
        aggregate(vals)
    }

    fn rec_agg(&self, server: &str, f: impl Fn(&Record) -> Option<f64>) -> Option<Agg> {
        aggregate(self.0.iter().filter(|r| r.server == server).filter_map(f).collect())
    }

    /// Distinct (scenario, conc) pairs of a kind, in first-seen order.
    fn keys(&self, kind: &str) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = vec![];
        for c in self.0.iter().flat_map(|r| r.cells.iter()).filter(|c| c.kind == kind) {
            let k = (c.scenario.clone(), c.conc);
            if !out.contains(&k) {
                out.push(k);
            }
        }
        out
    }

    fn errors(&self, server: &str) -> u64 {
        self.0.iter().filter(|r| r.server == server).flat_map(|r| r.cells.iter()).map(|c| c.errors).sum()
    }
}

fn thousands(n: f64) -> String {
    let s = format!("{:.0}", n.round());
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn with_spread(a: Option<Agg>, fmt: impl Fn(f64) -> String) -> String {
    match a {
        Some(a) => format!("{} ±{:.0}%", fmt(a.median), a.spread * 100.0),
        None => "-".into(),
    }
}

fn plain(a: Option<Agg>, fmt: impl Fn(f64) -> String) -> String {
    a.map(|a| fmt(a.median)).unwrap_or_else(|| "-".into())
}

fn ratio(rust: Option<Agg>, go: Option<Agg>) -> String {
    match (rust, go) {
        (Some(r), Some(g)) if g.median > 0.0 => format!("{:.2}x", r.median / g.median),
        _ => "-".into(),
    }
}

fn ms(us: f64) -> String {
    format!("{:.2}", us / 1000.0)
}

fn mb(kb: f64) -> String {
    format!("{:.0}", kb / 1024.0)
}

fn table(out: &mut String, header: &[&str], rows: Vec<Vec<String>>) {
    let _ = writeln!(out, "| {} |", header.join(" | "));
    let _ = writeln!(out, "|{}|", header.iter().map(|_| "---").collect::<Vec<_>>().join("|"));
    for r in rows {
        let _ = writeln!(out, "| {} |", r.join(" | "));
    }
    out.push('\n');
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / 1048576.0)
}

fn machine(out: &mut String, r: &Results) {
    let m = &r.meta;
    let loadavg = |s: &str| r.records.iter().filter(|x| x.server == s).map(|x| x.loadavg).fold(0.0, f64::max);
    let _ = writeln!(out, "## Machine and setup\n");
    let _ = writeln!(out, "- Date: {}", m.date);
    let _ = writeln!(out, "- CPU: {} ({} logical CPUs), {} MB RAM, kernel {}", m.cpu_model, m.logical_cpus, m.mem_total_mb, m.kernel);
    let _ = writeln!(out, "- Pinning (taskset): server `{}`, mock upstream `{}`, load generator `{}`", m.server_cpus, m.mock_cpus, m.load_cpus);
    let _ = writeln!(out, "- Go: {} at {} (`-trimpath -ldflags \"-s -w\"`)", m.go_version, m.go_commit);
    let _ = writeln!(out, "- Rust: {} at {} (`cargo build --release`)", m.rustc, m.rust_commit);
    let _ = writeln!(
        out,
        "- {} runs per cell, {}s warmup + {}s measured per throughput cell, concurrency {:?}; cells show the median over runs with ±(half the min-max range)",
        m.runs, m.warmup_s, m.measure_s, m.conc
    );
    let _ = writeln!(
        out,
        "- Peak 1-minute load average sampled at the start of a run: go {:.1}, rust {:.1} (other processes on the machine add noise)",
        loadavg("go"),
        loadavg("rust")
    );
    let d = Data(&r.records);
    let _ = writeln!(out, "- Request errors during measured windows: go {}, rust {}, direct {}\n", d.errors("go"), d.errors("rust"), d.errors("direct"));
    let _ = writeln!(out, "Ratio is always Rust / Go: above 1 is better for req/s, below 1 is better for latency, memory and CPU.\n");
    let _ = writeln!(out, "Scenarios:\n");
    for (id, desc) in &m.scenarios {
        let _ = writeln!(out, "- `{id}`: {desc}");
    }
    out.push('\n');
}

fn throughput(out: &mut String, d: &Data) {
    let _ = writeln!(out, "## Throughput\n");
    let _ = writeln!(out, "Closed loop, zero-latency upstream. `direct` is the same request sent straight to the mock: the ceiling the proxies compete against.\n");
    let mut rows = vec![];
    for (sc, c) in d.keys("tput") {
        let g = |s: &str| d.agg(s, "tput", &sc, c, |x| Some(x.rps));
        let (go, rust, direct) = (g("go"), g("rust"), g("direct"));
        rows.push(vec![
            format!("`{sc}`"),
            c.to_string(),
            with_spread(go, thousands),
            with_spread(rust, thousands),
            ratio(rust, go),
            plain(direct, thousands),
        ]);
    }
    table(out, &["Scenario", "Conc", "Go req/s", "Rust req/s", "Rust/Go", "Direct req/s"], rows);
}

fn latency(out: &mut String, d: &Data) {
    let _ = writeln!(out, "## Latency (ms)\n");
    let mut rows = vec![];
    for (sc, c) in d.keys("tput") {
        let mut row = vec![format!("`{sc}`"), c.to_string()];
        for p in ["p50", "p90", "p99"] {
            let f = |x: &Cell| Some(match p {
                "p50" => x.lat.p50,
                "p90" => x.lat.p90,
                _ => x.lat.p99,
            } as f64);
            let (go, rust) = (d.agg("go", "tput", &sc, c, f), d.agg("rust", "tput", &sc, c, f));
            row.extend([plain(go, ms), plain(rust, ms), ratio(rust, go)]);
        }
        rows.push(row);
    }
    table(
        out,
        &["Scenario", "Conc", "p50 Go", "p50 Rust", "x", "p90 Go", "p90 Rust", "x", "p99 Go", "p99 Rust", "x"],
        rows,
    );
}

fn streaming(out: &mut String, d: &Data) {
    let keys = d.keys("stream");
    if keys.is_empty() {
        return;
    }
    let _ = writeln!(out, "## Streaming overhead (ms)\n");
    let _ = writeln!(
        out,
        "Upstream waits 20 ms, then sends 40 text deltas 5 ms apart. Each delta carries its send time, so the client measures how old a chunk is when it arrives. `direct` is the same request sent straight to the mock; `+` columns are the proxy's added time over it. TTFB is the first body byte, first token the first text delta.\n"
    );
    let mut rows = vec![];
    for (sc, c) in keys {
        let f = |pick: fn(&Cell) -> Option<u64>| move |x: &Cell| pick(x).map(|v| v as f64);
        let ttfb = f(|x| x.ttfb.map(|p| p.p50));
        let ttft = f(|x| x.ttft.map(|p| p.p50));
        let c50 = f(|x| x.chunk.map(|p| p.p50));
        let c99 = f(|x| x.chunk.map(|p| p.p99));
        let get = |s: &str, g: &dyn Fn(&Cell) -> Option<f64>| d.agg(s, "stream", &sc, c, g);
        let added = |s: &str, g: &dyn Fn(&Cell) -> Option<f64>| match (get(s, g), get("direct", g)) {
            (Some(a), Some(b)) => format!("+{}", ms(a.median - b.median)),
            _ => "-".into(),
        };
        rows.push(vec![
            format!("`{sc}`"),
            c.to_string(),
            plain(get("direct", &ttfb), ms),
            added("go", &ttfb),
            added("rust", &ttfb),
            plain(get("direct", &ttft), ms),
            added("go", &ttft),
            added("rust", &ttft),
            plain(get("direct", &c50), ms),
            added("go", &c50),
            added("rust", &c50),
            plain(get("direct", &c99), ms),
            added("go", &c99),
            added("rust", &c99),
        ]);
    }
    table(
        out,
        &[
            "Scenario", "Conc", "TTFB direct", "+Go", "+Rust", "First token direct", "+Go", "+Rust", "Chunk age p50 direct", "+Go", "+Rust",
            "p99 direct", "+Go", "+Rust",
        ],
        rows,
    );
}

fn resources(out: &mut String, r: &Results, d: &Data) {
    let m = &r.meta;
    let _ = writeln!(out, "## Resource usage\n");
    let startup = |s| d.rec_agg(s, |x| x.startup_ms);
    let idle = |s| d.rec_agg(s, |x| x.idle_rss_kb.map(|v| v as f64));
    let (gs, rs) = (startup("go"), startup("rust"));
    let (gi, ri) = (idle("go"), idle("rust"));
    table(
        out,
        &["", "Go", "Rust", "Rust/Go"],
        vec![
            vec!["Startup to healthy (ms)".into(), with_spread(gs, |v| format!("{v:.0}")), with_spread(rs, |v| format!("{v:.0}")), ratio(rs, gs)],
            vec!["Idle RSS (MB, 3 s after start)".into(), with_spread(gi, mb), with_spread(ri, mb), ratio(ri, gi)],
            vec![
                "Binary size (stripped)".into(),
                mib(m.go_bin_bytes),
                format!("{} ({} unstripped)", mib(m.rust_stripped_bytes), mib(m.rust_bin_bytes)),
                format!("{:.2}x", m.rust_stripped_bytes as f64 / m.go_bin_bytes.max(1) as f64),
            ],
        ],
    );
    let _ = writeln!(
        out,
        "Under load (steady RSS is read at the end of the measured window, peak is the kernel high-water mark reset at the start of that window, CPU is user+system time of the server process per 1,000 requests):\n"
    );
    let mut rows = vec![];
    for (sc, c) in d.keys("tput").into_iter().filter(|(_, c)| [16, 64, 256].contains(c)) {
        let g = |s: &str, f: fn(&Cell) -> Option<f64>| d.agg(s, "tput", &sc, c, f);
        let rss = |x: &Cell| x.rss_kb.map(|v| v as f64);
        let peak = |x: &Cell| x.peak_kb.map(|v| v as f64);
        let cpu = |x: &Cell| x.cpu_ms_per_1k;
        let (gc, rc) = (g("go", cpu), g("rust", cpu));
        rows.push(vec![
            format!("`{sc}`"),
            c.to_string(),
            plain(g("go", rss), mb),
            plain(g("rust", rss), mb),
            plain(g("go", peak), mb),
            plain(g("rust", peak), mb),
            plain(gc, |v| format!("{v:.0}")),
            plain(rc, |v| format!("{v:.0}")),
            ratio(rc, gc),
        ]);
    }
    table(
        out,
        &["Scenario", "Conc", "Steady RSS Go (MB)", "Rust", "Peak RSS Go (MB)", "Rust", "CPU ms/1k req Go", "Rust", "Rust/Go"],
        rows,
    );
}

fn large(out: &mut String, d: &Data) {
    let keys = d.keys("large");
    if keys.is_empty() {
        return;
    }
    let _ = writeln!(out, "## Large requests\n");
    let _ = writeln!(out, "End to end for a ~2 MB agentic conversation (24 tools, tool calls with large results), non-stream, zero-latency upstream. Latency is full request time.\n");
    let mut rows = vec![];
    for (sc, c) in keys {
        let g = |s: &str, f: fn(&Cell) -> Option<f64>| d.agg(s, "large", &sc, c, f);
        let p50 = |x: &Cell| Some(x.lat.p50 as f64);
        let p99 = |x: &Cell| Some(x.lat.p99 as f64);
        let rps = |x: &Cell| Some(x.rps);
        let peak = |x: &Cell| x.peak_kb.map(|v| v as f64);
        let cpu = |x: &Cell| x.cpu_ms_per_1k.map(|v| v / 1000.0);
        let (go, rust) = (g("go", p50), g("rust", p50));
        rows.push(vec![
            format!("`{sc}`"),
            c.to_string(),
            plain(g("direct", p50), ms),
            plain(go, ms),
            plain(rust, ms),
            ratio(rust, go),
            plain(g("go", p99), ms),
            plain(g("rust", p99), ms),
            plain(g("go", rps), |v| format!("{v:.0}")),
            plain(g("rust", rps), |v| format!("{v:.0}")),
            plain(g("go", peak), mb),
            plain(g("rust", peak), mb),
            plain(g("go", cpu), |v| format!("{v:.1}")),
            plain(g("rust", cpu), |v| format!("{v:.1}")),
        ]);
    }
    table(
        out,
        &[
            "Scenario", "Conc", "p50 direct", "p50 Go", "p50 Rust", "x", "p99 Go", "p99 Rust", "req/s Go", "req/s Rust", "Peak RSS Go (MB)", "Rust",
            "CPU ms/req Go", "Rust",
        ],
        rows,
    );
}

/// Short table for the README: throughput and p50/p99 at one concurrency, plus idle memory.
fn summary(out: &mut String, r: &Results, d: &Data) {
    const C: usize = 64;
    let _ = writeln!(out, "## Summary (concurrency {C})\n");
    let mut rows = vec![];
    for (sc, c) in d.keys("tput").into_iter().filter(|(_, c)| *c == C) {
        let g = |s: &str, f: fn(&Cell) -> Option<f64>| d.agg(s, "tput", &sc, c, f);
        let rps = |x: &Cell| Some(x.rps);
        let p50 = |x: &Cell| Some(x.lat.p50 as f64);
        let p99 = |x: &Cell| Some(x.lat.p99 as f64);
        let cpu = |x: &Cell| x.cpu_ms_per_1k;
        rows.push(vec![
            format!("`{sc}`"),
            plain(g("go", rps), thousands),
            plain(g("rust", rps), thousands),
            ratio(g("rust", rps), g("go", rps)),
            format!("{} / {}", plain(g("go", p50), ms), plain(g("rust", p50), ms)),
            format!("{} / {}", plain(g("go", p99), ms), plain(g("rust", p99), ms)),
            ratio(g("rust", cpu), g("go", cpu)),
        ]);
    }
    table(out, &["Scenario", "Go req/s", "Rust req/s", "Rust/Go", "p50 ms Go / Rust", "p99 ms Go / Rust", "CPU per request Rust/Go"], rows);
    let idle = |s| d.rec_agg(s, |x| x.idle_rss_kb.map(|v| v as f64));
    let startup = |s| d.rec_agg(s, |x| x.startup_ms);
    let _ = writeln!(
        out,
        "Idle RSS {} MB (Go) vs {} MB (Rust), startup {} ms vs {} ms, binary {} vs {}.\n",
        plain(idle("go"), mb),
        plain(idle("rust"), mb),
        plain(startup("go"), |v| format!("{v:.0}")),
        plain(startup("rust"), |v| format!("{v:.0}")),
        mib(r.meta.go_bin_bytes),
        mib(r.meta.rust_stripped_bytes),
    );
}

pub fn report(input: &Path, out_path: &Path) -> Result<()> {
    let raw = std::fs::read(input).with_context(|| format!("read {}", input.display()))?;
    let r: Results = serde_json::from_slice(&raw)?;
    let d = Data(&r.records);
    let mut out = String::from("# Benchmark results: Go vs Rust\n\nGenerated by `cpa-bench report` from `bench/results/raw.json`. Methodology is in [bench/README.md](README.md).\n\n");
    summary(&mut out, &r, &d);
    machine(&mut out, &r);
    throughput(&mut out, &d);
    latency(&mut out, &d);
    streaming(&mut out, &d);
    resources(&mut out, &r, &d);
    large(&mut out, &d);
    std::fs::write(out_path, out)?;
    eprintln!("wrote {}", out_path.display());
    Ok(())
}

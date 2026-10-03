//! `cpa-e2e`: end-to-end differential harness for CLIProxyAPI-compatible servers.
//!
//! `record` runs every scenario against a server binary and writes goldens; `check` re-runs
//! them, diffs against the goldens and writes `report.md`. Goldens are recorded from the Go
//! reference, so a passing `check` means a server behaves like the reference.

mod client;
mod config;
mod golden;
mod mock;
mod normalize;
mod resp;
mod runner;
mod scenario;
mod scenarios;
mod server;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use config::Layout;
use golden::Outcome;
use runner::RunOpts;
use scenario::Scenario;

const DEFAULT_MOCK_PORT: u16 = 38921;

#[derive(Parser)]
#[command(name = "cpa-e2e", about = "End-to-end differential harness for CLIProxyAPI-compatible servers")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    Legacy,
    V8,
}

#[derive(Args, Clone)]
struct Common {
    /// Server binary under test; it must accept `--config <path> --local-model`.
    #[arg(long)]
    server: PathBuf,
    /// Only run scenarios whose id contains this substring.
    #[arg(long)]
    filter: Option<String>,
    /// Directory holding the golden files.
    #[arg(long, default_value = "conformance/e2e")]
    golden_dir: PathBuf,
    /// Fixed mock upstream port. It is part of credential identity (auth index hashes), so
    /// goldens are only comparable when this stays constant.
    #[arg(long, default_value_t = DEFAULT_MOCK_PORT)]
    mock_port: u16,
    /// Fixed port the server under test listens on (the port shows up in config dumps).
    #[arg(long, default_value_t = 38922)]
    server_port: u16,
    /// Scratch directory (relative to the current directory, normally the repo root where `tmp/`
    /// is gitignored); per-scenario config, auth dir and server log are kept here.
    #[arg(long, default_value = "tmp/e2e-work")]
    work_dir: PathBuf,
    /// Config layout generated for the server.
    #[arg(long, value_enum, default_value = "legacy")]
    config_layout: LayoutArg,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run all scenarios against the server and write goldens.
    Record {
        #[command(flatten)]
        common: Common,
        /// Runs per scenario. Leaves that differ between runs are masked in the golden (and listed);
        /// structural differences make the scenario unstable.
        #[arg(long, default_value_t = 2)]
        runs: usize,
    },
    /// Re-run scenarios, compare with goldens, write the report; non-zero exit on any failure.
    Check {
        #[command(flatten)]
        common: Common,
        /// Do not compare JSON object key order (it is compared by default).
        #[arg(long)]
        ignore_key_order: bool,
        /// Report path (default: <golden-dir>/report.md).
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// List scenario ids.
    List {
        #[arg(long)]
        filter: Option<String>,
    },
    /// Run only the mock upstream (for manual poking).
    Mock {
        #[arg(long, default_value_t = DEFAULT_MOCK_PORT)]
        port: u16,
    },
}

fn select(all: Vec<Scenario>, filter: &Option<String>) -> Vec<Scenario> {
    all.into_iter().filter(|s| filter.as_ref().is_none_or(|f| s.id.contains(f.as_str()))).collect()
}

fn run_opts(c: &Common) -> RunOpts {
    RunOpts {
        server_bin: c.server.clone(),
        mock_port: c.mock_port,
        server_port: c.server_port,
        // Absolute: file credentials hash their path, and the server runs from another cwd.
        work_dir: std::fs::create_dir_all(&c.work_dir).and_then(|_| std::fs::canonicalize(&c.work_dir)).unwrap_or_else(|_| c.work_dir.clone()),
        layout: match c.config_layout {
            LayoutArg::Legacy => Layout::Legacy,
            LayoutArg::V8 => Layout::V8,
        },
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

async fn real_main() -> Result<ExitCode> {
    match Cli::parse().cmd {
        Cmd::List { filter } => {
            for s in select(scenarios::all(DEFAULT_MOCK_PORT), &filter) {
                println!("{}\t{}", s.id, s.desc);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Mock { port } => {
            let handle = mock::start(port).await?;
            eprintln!("mock upstream on 127.0.0.1:{}", handle.port);
            std::future::pending::<()>().await;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Record { common, runs } => record(common, runs).await,
        Cmd::Check { common, ignore_key_order, report } => check(common, !ignore_key_order, report).await,
    }
}

async fn record(common: Common, runs: usize) -> Result<ExitCode> {
    let _mock = mock::start(common.mock_port).await?;
    let opts = run_opts(&common);
    let list = select(scenarios::all(common.mock_port), &common.filter);
    if list.is_empty() {
        bail!("no scenarios match");
    }
    let mut unstable = 0;
    for s in &list {
        let missing = s.missing_plugins(common.mock_port);
        if !missing.is_empty() {
            bail!("scenario {} needs plugin libraries that are not built ({}); run crates/e2e/plugins/build.sh", s.id, missing.join(", "));
        }
        let mut golden = runner::run_scenario(&opts, s).await?;
        let mut error = None;
        for _ in 1..runs.max(1) {
            let again = runner::run_scenario(&opts, s).await?;
            match golden::learn_volatile(&golden, &again) {
                Ok(merged) => golden = merged,
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        match error {
            None => {
                golden::save(&common.golden_dir, &golden)?;
                println!("recorded {}", s.id);
                for p in &golden.volatile {
                    println!("    volatile {p}");
                }
            }
            Some(e) => {
                println!("UNSTABLE {}: {e}", s.id);
                unstable += 1;
            }
        }
    }
    println!("{} scenarios, {} unstable", list.len(), unstable);
    Ok(if unstable == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

async fn check(common: Common, strict_order: bool, report: Option<PathBuf>) -> Result<ExitCode> {
    let _mock = mock::start(common.mock_port).await?;
    let opts = run_opts(&common);
    let list = select(scenarios::all(common.mock_port), &common.filter);
    if list.is_empty() {
        bail!("no scenarios match");
    }
    let mut outcomes: Vec<Outcome> = vec![];
    let mut skipped = 0;
    for s in &list {
        let missing = s.missing_plugins(common.mock_port);
        if !missing.is_empty() {
            println!("SKIP  {} (plugin libraries not built: {}; run crates/e2e/plugins/build.sh)", s.id, missing.join(", "));
            skipped += 1;
            outcomes.push(Outcome { id: s.id.clone(), desc: s.desc.clone(), failures: vec![] });
            continue;
        }
        let failures = match golden::load(&common.golden_dir, &s.id)? {
            None => vec!["no golden recorded".to_string()],
            Some(g) => match runner::run_scenario(&opts, s).await {
                Ok(actual) => {
                    let diffs = golden::diff(&g, &actual, strict_order);
                    if diffs.is_empty() {
                        golden::clear_actual(&common.golden_dir, &s.id);
                    } else {
                        golden::save_actual(&common.golden_dir, &actual)?;
                    }
                    diffs
                }
                Err(e) => vec![format!("run error: {e:#}")],
            },
        };
        if failures.is_empty() {
            println!("PASS  {}", s.id);
        } else {
            println!("FAIL  {}", s.id);
            for d in &failures {
                println!("        {d}");
            }
        }
        outcomes.push(Outcome { id: s.id.clone(), desc: s.desc.clone(), failures });
    }
    // Goldens without a scenario usually mean a renamed or removed scenario.
    if common.filter.is_none() {
        let known: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
        for id in golden::list_ids(&common.golden_dir) {
            if !known.contains(&id.as_str()) {
                println!("FAIL  {id}\n        golden has no scenario");
                outcomes.push(Outcome { id, desc: String::new(), failures: vec!["golden has no scenario".into()] });
            }
        }
    }
    let failed = outcomes.iter().filter(|o| !o.passed()).count();
    let layout = match common.config_layout {
        LayoutArg::Legacy => "legacy",
        LayoutArg::V8 => "v8",
    };
    let md = golden::report(&outcomes, &common.server.to_string_lossy(), &common.golden_dir, layout);
    let path = report.unwrap_or_else(|| common.golden_dir.join("report.md"));
    std::fs::write(&path, md)?;
    println!(
        "\n{} scenarios, {} passed ({} skipped), {} failed (report: {})",
        outcomes.len(),
        outcomes.len() - failed,
        skipped,
        failed,
        path.display()
    );
    Ok(if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

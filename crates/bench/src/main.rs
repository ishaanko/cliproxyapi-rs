//! `cpa-bench`: Go vs Rust performance benchmark (see bench/README.md).

mod load;
mod meter;
mod mock;
mod procs;
mod quick;
mod report;
mod run;
mod scenarios;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(about = "Benchmark the Go and Rust servers under identical configs")]
enum Cmd {
    /// Run the full benchmark and write raw samples to JSON.
    Run(Box<run::RunArgs>),
    /// One scenario, fixed request count, deterministic metrics (CPU, instructions, allocations).
    Quick(Box<quick::QuickArgs>),
    /// Turn raw samples into the markdown tables.
    Report {
        #[arg(long, default_value = "bench/results/raw.json")]
        input: PathBuf,
        #[arg(long, default_value = "bench/results.md")]
        out: PathBuf,
    },
    /// Closed-loop load against an already running server (and mock), for profiling one scenario.
    Load(run::LoadArgs),
    /// Print the shared server config (for starting a server by hand under a profiler).
    Config {
        #[arg(long)]
        auth_dir: PathBuf,
    },
    /// Fast mock upstream (spawned by `run`).
    Mock {
        #[arg(long, default_value_t = 19090)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cmd::parse() {
        Cmd::Run(a) => run::run(*a).await,
        Cmd::Quick(a) => quick::quick(*a).await,
        Cmd::Report { input, out } => report::report(&input, &out),
        Cmd::Load(a) => run::load_only(a).await,
        Cmd::Config { auth_dir } => {
            print!("{}", procs::server_config(&auth_dir));
            Ok(())
        }
        Cmd::Mock { port } => mock::serve(port).await,
    }
}

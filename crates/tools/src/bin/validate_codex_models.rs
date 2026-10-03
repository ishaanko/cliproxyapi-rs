//! Validates a Codex client model catalog file (`--file`).

fn main() {
    std::process::exit(cpa_tools::validate::run(std::env::args().skip(1).collect()));
}

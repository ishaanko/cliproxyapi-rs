//! Fetches the Codex client model catalog with stored credentials into a JSON file.
//!
//! Flags: `--auths-dir`, `--config`, `--output` (default `codex_client_models.json`),
//! `--client-version` (default `0.159.0`), `--pretty`.

#[tokio::main]
async fn main() {
    let code = cpa_tools::codex::run(std::env::args().skip(1).collect()).await;
    std::process::exit(code);
}

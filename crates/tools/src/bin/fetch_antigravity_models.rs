//! Fetches the Antigravity model list with stored credentials into a JSON file.
//!
//! Flags: `--auths-dir`, `--config`, `--output` (default `antigravity_models.json`), `--pretty`.

#[tokio::main]
async fn main() {
    let code = cpa_tools::antigravity::run(std::env::args().skip(1).collect()).await;
    std::process::exit(code);
}

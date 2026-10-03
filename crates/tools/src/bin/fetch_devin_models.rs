//! Fetches the Devin/Codeium model catalog with stored credentials into a JSON file.
//!
//! Flags: `--auths-dir`, `--config`, `--output` (default `devin_models.json`), `--raw`, `--pretty`.

#[tokio::main]
async fn main() {
    let code = cpa_tools::devin::run(std::env::args().skip(1).collect()).await;
    std::process::exit(code);
}

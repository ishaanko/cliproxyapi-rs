//! Sends one GET through a fingerprinted profile and prints the status and headers.
//!
//! `cargo run -p cpa-tlsfp --example probe -- <claude|oauth|chrome> <https-url> [proxy-url]`

use cpa_tlsfp::{ClientConfig, FingerprintClient};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(profile), Some(url)) = (args.first(), args.get(1)) else {
        eprintln!("usage: probe <claude|oauth|chrome> <https-url> [proxy-url]");
        std::process::exit(2);
    };
    let cfg = ClientConfig { proxy: args.get(2).cloned().unwrap_or_default(), ..ClientConfig::default() };
    let client = match profile.as_str() {
        "claude" => FingerprintClient::claude_inference(cfg),
        "oauth" => FingerprintClient::claude_oauth(cfg),
        _ => FingerprintClient::chrome(cfg),
    }
    .expect("build client");
    let req = reqwest::Client::new().get(url).header("user-agent", "probe/1").build().expect("request");
    for round in 1..=2 {
        match client.execute(req.try_clone().expect("clone")).await {
            Ok(resp) => {
                println!("round {round}: {} {:?}", resp.status(), resp.version());
                let body = resp.bytes().await.map(|b| b.len());
                println!("  body bytes: {body:?}");
            }
            Err(e) => println!("round {round}: error: {e}"),
        }
    }
}

//! Loads every plugin in a directory and prints what they registered:
//! `cargo run -p cpa-plugin --example probe -- <plugins-dir> [plugin-id ...]`.

use std::sync::Arc;

use cpa_config::{Config, PluginInstanceConfig};
use cpa_plugin::{CallCtx, Host};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| "plugins".into());
    let only: Vec<String> = args.collect();
    let mut cfg = Config::default();
    cfg.plugins.enabled = true;
    cfg.plugins.dir = dir.clone();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".so") else { continue };
            if !only.is_empty() && !only.iter().any(|o| o == stem) {
                continue;
            }
            cfg.plugins.configs.insert(stem.to_string(), PluginInstanceConfig { enabled: Some(true), priority: 0, ..Default::default() });
        }
    }
    let host = Host::new();
    let ctx = CallCtx::background();
    host.apply_config(&ctx, Some(Arc::new(cfg))).await;
    for info in host.registered_plugins() {
        println!("{} name={} version={} oauth={:?} quota={:?}", info.id, info.metadata.name, info.metadata.version, info.oauth_provider, info.quota_provider);
    }
    host.shutdown_all(&ctx).await;
}

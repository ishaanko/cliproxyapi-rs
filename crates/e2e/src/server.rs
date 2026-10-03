//! Starts and stops the server binary under test.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};

use crate::config::{CLIENT_KEY, ConfigSpec, Layout};

/// Directory holding the plugin libraries built by `crates/e2e/plugins/build.sh`
/// (`CPA_E2E_PLUGIN_DIR`, default `target/e2e-plugins`).
pub fn plugin_source_dir() -> PathBuf {
    std::env::var_os("CPA_E2E_PLUGIN_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("target/e2e-plugins"))
}

/// The plugin libraries `spec` needs that were not built.
pub fn missing_plugins(spec: &ConfigSpec) -> Vec<&'static str> {
    let src = plugin_source_dir();
    spec.plugins.iter().map(|p| p.id).filter(|id| !src.join(format!("{id}.so")).is_file()).collect()
}

/// Copies the scenario's plugin libraries into `<dir>/plugins`.
fn install_plugins(spec: &ConfigSpec, dir: &Path) -> Result<()> {
    if spec.plugins.is_empty() {
        return Ok(());
    }
    let src = plugin_source_dir();
    let dst = dir.join("plugins");
    std::fs::create_dir_all(&dst)?;
    for p in &spec.plugins {
        let from = src.join(format!("{}.so", p.id));
        std::fs::copy(&from, dst.join(format!("{}.so", p.id))).with_context(|| format!("copy plugin {}", from.display()))?;
    }
    Ok(())
}

pub struct ServerProc {
    child: Child,
    pub port: u16,
    pub dir: PathBuf,
}

impl ServerProc {
    /// Writes the config under `dir`, starts the binary and waits until it serves models.
    pub async fn start(bin: &Path, spec: &ConfigSpec, layout: Layout, dir: &Path, port: u16) -> Result<Self> {
        if dir.exists() {
            std::fs::remove_dir_all(dir).with_context(|| format!("clean {}", dir.display()))?;
        }
        let auth_dir = dir.join("auth");
        std::fs::create_dir_all(&auth_dir)?;
        install_plugins(spec, dir)?;
        for (name, content) in &spec.auth_files {
            std::fs::write(auth_dir.join(name), content)?;
        }
        let config_path = dir.join("config.yaml");
        std::fs::write(&config_path, spec.render(layout, port, &auth_dir))?;
        let log = std::fs::File::create(dir.join("server.log"))?;
        let mut cmd = Command::new(bin);
        if spec.plugin_store_mock {
            // The mock's CA is the only trust root the server needs for the store hosts.
            let ca = dir.join("store-mock-ca.pem");
            std::fs::write(&ca, crate::mock::store::CA_PEM)?;
            cmd.env("SSL_CERT_FILE", &ca);
        }
        let child = cmd
            .arg("--config")
            .arg(&config_path)
            // Use embedded model catalogs; never fetch remote model updates.
            .arg("--local-model")
            .current_dir(dir)
            .env("WRITABLE_PATH", dir)
            // The reference formats some timestamps in local time; pin it.
            .env("TZ", "UTC")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {}", bin.display()))?;
        let mut proc = ServerProc { child, port, dir: dir.to_path_buf() };
        proc.wait_ready(&spec.ready_headers).await?;
        Ok(proc)
    }

    /// Ready once `/v1/models` answers 200 with a non-empty list: client keys and config API
    /// keys are applied asynchronously after the listener comes up.
    async fn wait_ready(&mut self, headers: &[(&str, &str)]) -> Result<()> {
        let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(2)).build()?;
        let url = format!("http://127.0.0.1:{}/v1/models", self.port);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!("server exited early ({status}); see {}", self.dir.join("server.log").display());
            }
            let mut req = client.get(&url).bearer_auth(CLIENT_KEY);
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            if let Ok(resp) = req.send().await {
                // Safe mode (template client keys) answers 403 on every proxy route.
                if resp.status() == 403 && resp.headers().contains_key("x-cpa-safe-mode") {
                    return Ok(());
                }
                if resp.status().is_success()
                    && let Ok(v) = resp.json::<serde_json::Value>().await
                    && v["data"].as_array().is_some_and(|a| !a.is_empty())
                {
                    return Ok(());
                }
            }
            if Instant::now() > deadline {
                bail!("server not ready after 30s; see {}", self.dir.join("server.log").display());
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    pub async fn stop(mut self) {
        let _ = self.child.kill().await;
    }
}

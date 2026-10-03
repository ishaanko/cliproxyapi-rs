//! Plugin installation from CLIProxyAPIHome (Go: the Home branch of `cmd/server/main.go` and
//! `internal/homeplugins`): sync the plugins Home assigns to this node, then report what was
//! installed and loaded.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use cpa_config::Config;
use cpa_home::{Client, HomeError};
use cpa_plugin::{CallCtx, Host};
use cpa_pluginstore::home_sync::{PLUGIN_SYNC_SCHEMA_VERSION, PluginSyncRequest, PluginSyncResponse};
use cpa_pluginstore::homeplugins::{
    PluginLoadInspector, PluginRuntime, SyncReport, completed_sync_report, current_platform, installed_versions, sync_resolved_with_report,
    sync_with_report,
};
use cpa_pluginstore::{Context, Error as StoreError};

const SYNC_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_REPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// The plugin host as the runtime the Home sync talks to.
pub struct HostRuntime(pub Arc<Host>);

impl PluginRuntime for HostRuntime {
    fn plugin_busy(&self, id: &str) -> bool {
        self.0.plugin_busy(id)
    }

    fn unload_plugin(&self, id: &str) -> bool {
        let host = self.0.clone();
        let id = id.to_string();
        let run = move || tokio::runtime::Handle::current().block_on(async move { host.unload_plugin(&CallCtx::background(), &id).await });
        match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(run),
            _ => false,
        }
    }
}

impl PluginLoadInspector for HostRuntime {
    fn plugin_registered(&self, id: &str) -> bool {
        self.0.plugin_registered(id)
    }
}

/// Outcome of the startup sync: the report to push, whether Home should hear about it, and the
/// failure that must stop the start (Go: `homePluginStatusReady` / `errHomePlugins`).
pub struct StartupSync {
    pub report: SyncReport,
    pub ready: bool,
    pub error: Option<StoreError>,
}

/// A context that cancels itself after `timeout` (Go: `context.WithTimeout`).
struct Deadline {
    ctx: Context,
    timer: tokio::task::JoinHandle<()>,
}

impl Deadline {
    fn new(timeout: Duration) -> Self {
        let ctx = Context::background();
        let cancel = ctx.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            cancel.cancel();
        });
        Deadline { ctx, timer }
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.timer.abort();
    }
}

/// Installs the plugins Home assigns (falling back to the config's `store` manifests for a Home
/// that predates plugin sync). `sync_cfg` still carries `plugins.store-auth`.
pub async fn startup_sync(client: &Client, sync_cfg: &Config, host: &Arc<Host>) -> StartupSync {
    let platform = current_platform();
    if !sync_cfg.plugins.enabled {
        return StartupSync { report: completed_sync_report(&platform, None), ready: true, error: None };
    }
    let deadline = Deadline::new(SYNC_TIMEOUT);
    let runtime: Arc<dyn PluginRuntime> = Arc::new(HostRuntime(host.clone()));
    let installed = match installed_versions(sync_cfg) {
        Ok(v) => v,
        Err(e) => return StartupSync { report: completed_sync_report(&platform, Some(&e)), ready: true, error: Some(e) },
    };
    let request = PluginSyncRequest {
        schema_version: PLUGIN_SYNC_SCHEMA_VERSION,
        goos: platform.goos.clone(),
        goarch: platform.goarch.clone(),
        installed_versions: installed.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    };
    let fetched = match serde_json::to_vec(&request) {
        Ok(body) => client.get_plugin_sync(&body).await,
        Err(e) => Err(HomeError::Other(e.to_string())),
    };
    let (report, error) = match fetched {
        Ok(raw) => match decode_response(&raw) {
            Ok(mut response) => {
                let expires_at = response.expires_at;
                sync_resolved_with_report(&deadline.ctx, sync_cfg, &mut response.items, expires_at, &installed, Some(runtime)).await
            }
            Err(e) => (completed_sync_report(&platform, Some(&e)), Some(e)),
        },
        Err(HomeError::PluginSyncUnsupported(_)) => sync_with_report(&deadline.ctx, sync_cfg, Some(runtime)).await,
        Err(e) => {
            let e = StoreError::msg(e.to_string());
            (completed_sync_report(&platform, Some(&e)), Some(e))
        }
    };
    StartupSync { report, ready: true, error }
}

fn decode_response(raw: &[u8]) -> Result<PluginSyncResponse, StoreError> {
    let response: PluginSyncResponse =
        serde_json::from_slice(raw).map_err(|e| StoreError::msg(format!("decode home plugin sync response: {e}")))?;
    response.validate(Utc::now())?;
    Ok(response)
}

/// `ReportPluginStatus`: stamps the node id and update time and pushes the report to Home.
pub async fn push_status(client: &Client, node_id: &str, mut report: SyncReport) -> Result<(), String> {
    let node_id = node_id.trim();
    if node_id.is_empty() {
        return Err("home node id is empty".into());
    }
    report.node_id = node_id.to_string();
    report.updated_at = Utc::now();
    let raw = serde_json::to_vec(&report).map_err(|e| format!("marshal home plugin status: {e}"))?;
    match tokio::time::timeout(STATUS_REPORT_TIMEOUT, client.rpush_plugin_status(&raw)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("push home plugin status: {e}")),
        Err(_) => Err("push home plugin status: context deadline exceeded".into()),
    }
}

// ---- runtime: plugin work riding on Home configs ----

use async_trait::async_trait;
use cpa_home::requests::PluginTask;
use cpa_pluginstore::homeplugins::{delete_with_report, mark_load_results};
use cpa_runtime::service::{HomePluginWork, HomePlugins};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

/// Go: `homePluginSyncKey`: changes whenever the plugin section of a Home config does.
fn sync_key(cfg: &Config) -> String {
    if !cfg.home.enabled {
        return String::new();
    }
    let mut hash = Sha256::new();
    hash.update(format!(
        "enabled={}\ndir={}\nauth-revision={}\n",
        cfg.plugins.enabled,
        cfg.plugins.dir.trim(),
        cfg.plugins.auth_revision
    ));
    for (id, item) in &cfg.plugins.configs {
        hash.update(format!("plugin={}\nenabled={}\npriority={}\n", id.trim(), item.enabled.unwrap_or(false), item.priority));
        if !item.raw.is_null()
            && let Ok(raw) = serde_yaml_ng::to_string(&item.raw)
        {
            hash.update(raw.as_bytes());
        }
        hash.update(b"\n");
    }
    hex::encode(hash.finalize())
}

/// The Home overlay's plugin sync (Go: `syncHomePlugins` and the plugin-task handling of
/// `stageHomeOverlayWithClient`).
pub struct ServerHomePlugins {
    host: Arc<Host>,
    /// Key of the last plugin section that was synced and reported.
    synced_key: Arc<Mutex<String>>,
}

impl ServerHomePlugins {
    pub fn new(host: Arc<Host>) -> Arc<Self> {
        Arc::new(ServerHomePlugins { host, synced_key: Arc::new(Mutex::new(String::new())) })
    }
}

struct StatusWork {
    report: SyncReport,
    needs_load_marking: bool,
}

struct TaskWork {
    task: PluginTask,
    report: Option<SyncReport>,
}

struct StagedPlugins {
    host: Arc<Host>,
    cfg: Config,
    status: Vec<StatusWork>,
    next_status: usize,
    tasks: Vec<TaskWork>,
    next_task: usize,
    sync_key: String,
    mark_synced: bool,
    synced_key: Arc<Mutex<String>>,
}

#[async_trait]
impl HomePlugins for ServerHomePlugins {
    async fn stage(&self, client: &Arc<Client>, sync_cfg: &Config, merged: &Config) -> Result<Box<dyn HomePluginWork>, String> {
        let key = sync_key(sync_cfg);
        let mut staged = StagedPlugins {
            host: self.host.clone(),
            cfg: merged.clone(),
            status: Vec::new(),
            next_status: 0,
            tasks: Vec::new(),
            next_task: 0,
            sync_key: key.clone(),
            mark_synced: false,
            synced_key: self.synced_key.clone(),
        };
        let already_synced = !key.is_empty() && *self.synced_key.lock() == key;
        if !already_synced {
            let (report, did_sync, error) = if !sync_cfg.plugins.enabled {
                (completed_sync_report(&current_platform(), None), false, None)
            } else {
                let outcome = startup_sync(client, sync_cfg, &self.host).await;
                (outcome.report, true, outcome.error)
            };
            if let Some(e) = error {
                return Err(format!("sync home plugins: {e}"));
            }
            if !report.task.trim().is_empty() {
                staged.mark_synced = true;
                if !merged.home.node_id.trim().is_empty() {
                    staged.status.push(StatusWork { report, needs_load_marking: did_sync });
                }
            }
        }
        let tasks = client.get_plugin_tasks().await.map_err(|e| format!("stage home plugin tasks: {e}"))?;
        staged.tasks = tasks
            .into_iter()
            .filter(|t| t.operation.trim().eq_ignore_ascii_case("delete"))
            .map(|task| TaskWork { task, report: None })
            .collect();
        Ok(Box::new(staged))
    }
}

#[async_trait]
impl HomePluginWork for StagedPlugins {
    async fn finalize(&mut self, client: &Arc<Client>) -> Result<(), String> {
        let node_id = self.cfg.home.node_id.clone();
        while self.next_status < self.status.len() {
            let work = &mut self.status[self.next_status];
            if work.needs_load_marking {
                let inspector = HostRuntime(self.host.clone());
                if let Some(e) = mark_load_results(&mut work.report, Some(&inspector)) {
                    tracing::warn!("failed to load home plugins: {e}");
                }
                work.needs_load_marking = false;
            }
            push_status(client, &node_id, work.report.clone()).await?;
            self.next_status += 1;
        }
        while self.next_task < self.tasks.len() {
            let work = &mut self.tasks[self.next_task];
            if work.report.is_none() {
                let runtime = HostRuntime(self.host.clone());
                let ctx = Context::background();
                let report = delete_with_report(&ctx, &self.cfg, Some(&runtime), work.task.id, &work.task.plugin_id);
                if !report.ok && !report.error.trim().is_empty() {
                    tracing::warn!("failed to process home plugin delete task {} for {}: {}", work.task.id, work.task.plugin_id, report.error);
                }
                work.report = Some(report);
            }
            if let Some(report) = &work.report {
                push_status(client, &node_id, report.clone()).await?;
            }
            self.next_task += 1;
        }
        if self.mark_synced && !self.sync_key.trim().is_empty() {
            *self.synced_key.lock() = self.sync_key.clone();
            self.mark_synced = false;
        }
        Ok(())
    }
}

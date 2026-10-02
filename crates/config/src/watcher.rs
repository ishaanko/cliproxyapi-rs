//! Config hot reload (port of the config half of `internal/watcher`: `config_reload.go` and the
//! config branch of `events.go`).
//!
//! [`ConfigWatcher`] watches the config file, debounces bursts of filesystem events (150 ms),
//! skips reloads whose content hash is unchanged, reloads with [`load_config`], logs what changed
//! and publishes the new snapshot as an `Arc<Config>` on a `tokio::sync::watch` channel. A config
//! that fails to load is logged and the previous snapshot stays in place.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::diff::build_config_change_details;
use crate::error::{ConfigError, Result};
use crate::load::load_config;
use crate::paths::resolve_auth_dir;
use crate::types::Config;

/// Quiet period after the last filesystem event before the config is reloaded.
pub const CONFIG_RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);

/// Watches a config file and publishes reloaded snapshots. Dropping it stops the watcher.
pub struct ConfigWatcher {
    snapshots: watch::Receiver<Arc<Config>>,
    reload_requests: mpsc::UnboundedSender<oneshot::Sender<bool>>,
    task: JoinHandle<()>,
    // Kept alive for the lifetime of the watcher; events stop when it is dropped.
    _fs_watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    /// Starts watching `path` (which should already have been loaded into `initial`). Must be
    /// called inside a tokio runtime.
    ///
    /// `auth_dir_override` replaces the resolved `auth-dir` on every reload (Go's mirrored auth
    /// dir for store-backed deployments); otherwise `auth-dir` is tilde-expanded.
    pub fn start(
        path: impl Into<PathBuf>,
        initial: Arc<Config>,
        auth_dir_override: Option<PathBuf>,
    ) -> Result<Self> {
        let path = std::path::absolute(path.into()).map_err(|e| ConfigError::io("resolve config path", e))?;
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let file_name = path
            .file_name()
            .map(|n| n.to_os_string())
            .ok_or_else(|| ConfigError::invalid(format!("config path {} has no file name", path.display())))?;

        let (events_tx, events_rx) = mpsc::unbounded_channel::<()>();
        // The parent directory is watched so atomic replace (write temp + rename) is seen too.
        let mut fs_watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) => {
                let relevant = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_))
                    && event.paths.iter().any(|p| p.file_name() == Some(file_name.as_os_str()));
                if relevant {
                    let _ = events_tx.send(());
                }
            }
            Err(err) => tracing::error!("file watcher error: {err}"),
        })
        .map_err(|e| ConfigError::invalid(format!("failed to create config watcher: {e}")))?;
        fs_watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .map_err(|e| ConfigError::invalid(format!("failed to watch config file {}: {e}", path.display())))?;
        tracing::debug!("watching config file: {}", path.display());

        let (snapshots_tx, snapshots) = watch::channel(initial);
        let (reload_requests, reload_rx) = mpsc::unbounded_channel();
        let worker = Worker {
            last_hash: hash_file(&path),
            path,
            auth_dir_override,
            snapshots: snapshots_tx,
        };
        let task = tokio::spawn(worker.run(events_rx, reload_rx));
        Ok(Self { snapshots, reload_requests, task, _fs_watcher: fs_watcher })
    }

    /// A receiver that yields a new `Arc<Config>` after every successful reload.
    pub fn subscribe(&self) -> watch::Receiver<Arc<Config>> {
        self.snapshots.clone()
    }

    /// The latest published snapshot.
    pub fn current(&self) -> Arc<Config> {
        Arc::clone(&self.snapshots.borrow())
    }

    /// Runs the reload path now (the same one filesystem events use, minus the debounce).
    /// Returns whether a new snapshot was published.
    pub async fn reload_now(&self) -> bool {
        let (done_tx, done_rx) = oneshot::channel();
        if self.reload_requests.send(done_tx).is_err() {
            return false;
        }
        done_rx.await.unwrap_or(false)
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Worker {
    path: PathBuf,
    auth_dir_override: Option<PathBuf>,
    last_hash: String,
    snapshots: watch::Sender<Arc<Config>>,
}

impl Worker {
    async fn run(
        mut self,
        mut events: mpsc::UnboundedReceiver<()>,
        mut requests: mpsc::UnboundedReceiver<oneshot::Sender<bool>>,
    ) {
        loop {
            tokio::select! {
                event = events.recv() => {
                    if event.is_none() || !self.debounce(&mut events, &mut requests).await {
                        return;
                    }
                }
                request = requests.recv() => {
                    let Some(done) = request else { return };
                    let published = self.reload_if_changed().await;
                    let _ = done.send(published);
                }
            }
        }
    }

    /// Trailing debounce: waits until the burst of events has settled, then reloads. A manual
    /// reload request preempts the wait. Returns false when a channel was closed.
    async fn debounce(
        &mut self,
        events: &mut mpsc::UnboundedReceiver<()>,
        requests: &mut mpsc::UnboundedReceiver<oneshot::Sender<bool>>,
    ) -> bool {
        loop {
            tokio::select! {
                more = events.recv() => {
                    if more.is_none() {
                        return false;
                    }
                }
                request = requests.recv() => {
                    let Some(done) = request else { return false };
                    let published = self.reload_if_changed().await;
                    let _ = done.send(published);
                    return true;
                }
                () = tokio::time::sleep(CONFIG_RELOAD_DEBOUNCE) => {
                    self.reload_if_changed().await;
                    return true;
                }
            }
        }
    }

    /// Reloads when the file content differs from the last loaded content. Returns whether a new
    /// snapshot was published.
    async fn reload_if_changed(&mut self) -> bool {
        let path = self.path.clone();
        let data = match tokio::fs::read(&path).await {
            Ok(data) => data,
            Err(err) => {
                tracing::error!("failed to read config file for hash check: {err}");
                return false;
            }
        };
        if data.is_empty() {
            tracing::debug!("ignoring empty config file write event");
            return false;
        }
        let new_hash = hash_bytes(&data);
        if !self.last_hash.is_empty() && self.last_hash == new_hash {
            tracing::debug!("config file content unchanged (hash match), skipping reload");
            return false;
        }
        tracing::info!("config file changed, reloading: {}", path.display());
        let loaded = tokio::task::spawn_blocking({
            let path = path.clone();
            move || load_config(&path)
        })
        .await;
        let mut new_config = match loaded {
            Ok(Ok(cfg)) => cfg,
            Ok(Err(err)) => {
                tracing::error!("failed to reload config: {err}");
                return false;
            }
            Err(err) => {
                tracing::error!("config reload task failed: {err}");
                return false;
            }
        };
        match &self.auth_dir_override {
            Some(dir) => new_config.auth_dir = dir.to_string_lossy().into_owned(),
            None => match resolve_auth_dir(&new_config.auth_dir) {
                Ok(dir) => new_config.auth_dir = dir.to_string_lossy().into_owned(),
                Err(err) => tracing::error!("failed to resolve auth directory from config: {err}"),
            },
        }

        let old = Arc::clone(&self.snapshots.borrow());
        let details = build_config_change_details(&old, &new_config);
        if details.is_empty() {
            tracing::debug!("no material config field changes detected");
        } else {
            tracing::info!("config changes detected:");
            for detail in &details {
                tracing::info!("  {detail}");
            }
        }
        tracing::info!("config successfully reloaded");
        self.snapshots.send_replace(Arc::new(new_config));
        // The loader may have rewritten the file (secret hashing, conflicting legacy cleanup), so
        // the hash is taken from disk again.
        self.last_hash = match tokio::fs::read(&path).await {
            Ok(updated) if !updated.is_empty() => hash_bytes(&updated),
            _ => new_hash,
        };
        true
    }
}

fn hash_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// SHA-256 of the file content, or empty when it cannot be read (so the first change reloads).
fn hash_file(path: &Path) -> String {
    std::fs::read(path).map(|d| hash_bytes(&d)).unwrap_or_default()
}

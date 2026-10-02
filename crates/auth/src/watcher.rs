//! Auth-dir watcher: turns filesystem changes of `*.json` credential files into add / update /
//! remove events (internal/watcher, auth-file half).
//!
//! Behavior kept from the Go watcher: non-recursive by default, content-hash gating (rewriting a
//! file with identical bytes emits nothing), removals confirmed after a debounce so an atomic
//! replace (rename over the file) arrives as an update, unreadable / invalid / empty files ignored
//! until they become valid.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::store::{FileTokenStore, id_for, is_auth_json_path};
use crate::types::Auth;

/// A change to a credential file.
#[derive(Debug, Clone)]
pub enum AuthFileEvent {
    Added(Auth),
    Updated(Auth),
    Removed { id: String, path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Emit `Added` for every file present at start.
    pub emit_initial: bool,
    /// Also watch subdirectories (Go does not).
    pub recursive: bool,
    /// How long a missing file must stay missing before `Removed` is emitted.
    pub remove_debounce: Duration,
    /// Delay before re-checking a path after a remove/rename event.
    pub stat_delay: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            emit_initial: true,
            recursive: false,
            remove_debounce: Duration::from_secs(1),
            stat_delay: Duration::from_millis(50),
        }
    }
}

enum Msg {
    /// A path was created or written.
    Touched(PathBuf),
    /// A path was removed or renamed away: re-check after `stat_delay`.
    GoneMaybe(PathBuf),
    /// Debounce elapsed for a missing path: emit `Removed` if it is still gone.
    ConfirmGone(PathBuf),
}

/// Running watcher. Poll [`AuthWatcher::next`] for events; dropping it stops watching.
pub struct AuthWatcher {
    events: mpsc::UnboundedReceiver<AuthFileEvent>,
    _watcher: RecommendedWatcher,
    task: JoinHandle<()>,
}

impl AuthWatcher {
    /// Starts watching the store's base dir.
    pub fn start(
        store: Arc<FileTokenStore>,
        dir: &Path,
        opts: WatchOptions,
    ) -> notify::Result<AuthWatcher> {
        let (msg_tx, msg_rx) = mpsc::unbounded_channel::<Msg>();
        let (event_tx, events) = mpsc::unbounded_channel::<AuthFileEvent>();

        let tx = msg_tx.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                for (i, path) in event.paths.into_iter().enumerate() {
                    if !is_auth_json_path(&path) {
                        continue;
                    }
                    let msg = match event.kind {
                        EventKind::Create(_)
                        | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any) => {
                            Msg::Touched(path)
                        }
                        EventKind::Remove(_)
                        | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                            Msg::GoneMaybe(path)
                        }
                        // `Both` carries [old, new]; every other rename flavor reports the new name.
                        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if i == 0 => {
                            Msg::GoneMaybe(path)
                        }
                        EventKind::Modify(ModifyKind::Name(_)) => Msg::Touched(path),
                        _ => continue,
                    };
                    let _ = tx.send(msg);
                }
            })?;
        let mode = if opts.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher.watch(dir, mode)?;

        let base = dir.to_path_buf();
        let task = tokio::spawn(run(store, base, opts, msg_tx, msg_rx, event_tx));
        Ok(AuthWatcher {
            events,
            _watcher: watcher,
            task,
        })
    }

    /// Next change, or `None` once the watcher has stopped.
    pub async fn next(&mut self) -> Option<AuthFileEvent> {
        self.events.recv().await
    }
}

impl Drop for AuthWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

async fn run(
    store: Arc<FileTokenStore>,
    base: PathBuf,
    opts: WatchOptions,
    msg_tx: mpsc::UnboundedSender<Msg>,
    mut msg_rx: mpsc::UnboundedReceiver<Msg>,
    event_tx: mpsc::UnboundedSender<AuthFileEvent>,
) {
    // path -> (content hash, auth id) of files we have announced.
    let mut known: HashMap<PathBuf, ([u8; 32], String)> = HashMap::new();

    if opts.emit_initial
        && let Ok(entries) = std::fs::read_dir(&base)
    {
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| is_auth_json_path(p))
            .collect();
        paths.sort();
        for path in paths {
            handle_touched(&store, &base, &path, &mut known, &event_tx);
        }
    }

    while let Some(msg) = msg_rx.recv().await {
        match msg {
            Msg::Touched(path) => handle_touched(&store, &base, &path, &mut known, &event_tx),
            Msg::GoneMaybe(path) => {
                let tx = msg_tx.clone();
                let delay = opts.stat_delay;
                let debounce = opts.remove_debounce;
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    if path.exists() {
                        // Atomic replace: the file is back, treat as a write.
                        let _ = tx.send(Msg::Touched(path));
                        return;
                    }
                    tokio::time::sleep(debounce).await;
                    let _ = tx.send(Msg::ConfirmGone(path));
                });
            }
            Msg::ConfirmGone(path) => {
                if path.exists() {
                    handle_touched(&store, &base, &path, &mut known, &event_tx);
                } else if let Some((_, id)) = known.remove(&path) {
                    let _ = event_tx.send(AuthFileEvent::Removed { id, path });
                }
            }
        }
    }
}

/// Reads, hash-gates and announces one file.
fn handle_touched(
    store: &FileTokenStore,
    base: &Path,
    path: &Path,
    known: &mut HashMap<PathBuf, ([u8; 32], String)>,
    tx: &mpsc::UnboundedSender<AuthFileEvent>,
) {
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    if bytes.is_empty() {
        return;
    }
    let digest = hash(&bytes);
    if known.get(path).is_some_and(|(h, _)| *h == digest) {
        return;
    }
    let Ok(Some(auth)) = store.read_auth_file(path, base) else {
        return;
    };
    let id = id_for(path, base);
    let event = if known.contains_key(path) {
        AuthFileEvent::Updated(auth)
    } else {
        AuthFileEvent::Added(auth)
    };
    known.insert(path.to_path_buf(), (digest, id));
    let _ = tx.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fast() -> WatchOptions {
        WatchOptions {
            remove_debounce: Duration::from_millis(150),
            stat_delay: Duration::from_millis(20),
            ..Default::default()
        }
    }

    async fn next(w: &mut AuthWatcher) -> AuthFileEvent {
        tokio::time::timeout(Duration::from_secs(10), w.next())
            .await
            .expect("watcher event timed out")
            .expect("watcher closed")
    }

    #[tokio::test]
    async fn add_update_remove_and_atomic_replace() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("existing.json"),
            r#"{"type":"codex","email":"e@x"}"#,
        )
        .unwrap();
        let store = Arc::new(FileTokenStore::with_dir(dir.path()));
        let mut w = AuthWatcher::start(store, dir.path(), fast()).unwrap();

        // Initial scan.
        match next(&mut w).await {
            AuthFileEvent::Added(a) => assert_eq!(
                (a.id.as_str(), a.provider.as_str()),
                ("existing.json", "codex")
            ),
            other => panic!("expected initial Added, got {other:?}"),
        }

        // New file.
        let path = dir.path().join("claude-a.json");
        fs::write(
            &path,
            r#"{"type":"claude","email":"a@x","access_token":"t1"}"#,
        )
        .unwrap();
        match next(&mut w).await {
            AuthFileEvent::Added(a) => assert_eq!(a.id, "claude-a.json"),
            other => panic!("expected Added, got {other:?}"),
        }

        // Changed content.
        fs::write(
            &path,
            r#"{"type":"claude","email":"a@x","access_token":"t2"}"#,
        )
        .unwrap();
        match next(&mut w).await {
            AuthFileEvent::Updated(a) => assert_eq!(a.metadata["access_token"], "t2"),
            other => panic!("expected Updated, got {other:?}"),
        }

        // Atomic replace (write temp + rename over) is an update, never a removal.
        let tmp = dir.path().join("claude-a.json.tmp");
        fs::write(
            &tmp,
            r#"{"type":"claude","email":"a@x","access_token":"t3"}"#,
        )
        .unwrap();
        fs::rename(&tmp, &path).unwrap();
        match next(&mut w).await {
            AuthFileEvent::Updated(a) => assert_eq!(a.metadata["access_token"], "t3"),
            other => panic!("expected Updated after rename, got {other:?}"),
        }

        // Removal is reported once the debounce elapses.
        fs::remove_file(&path).unwrap();
        match next(&mut w).await {
            AuthFileEvent::Removed { id, .. } => assert_eq!(id, "claude-a.json"),
            other => panic!("expected Removed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_then_valid_file_is_announced_once_valid() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileTokenStore::with_dir(dir.path()));
        let mut w = AuthWatcher::start(store, dir.path(), fast()).unwrap();
        let path = dir.path().join("x.json");
        fs::write(&path, "{not json").unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        fs::write(&path, r#"{"type":"kimi","access_token":"a"}"#).unwrap();
        match next(&mut w).await {
            AuthFileEvent::Added(a) => assert_eq!(a.provider, "kimi"),
            other => panic!("expected Added, got {other:?}"),
        }
    }
}

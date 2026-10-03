//! Incremental auth state (Go: internal/watcher clients.go, dispatcher.go, events.go).
//!
//! [`AuthSync`] remembers the auths it last published (synthesized from config and from auth-dir
//! files) and turns "something changed" into the minimal list of [`AuthUpdate`]s: file events are
//! hash-gated and applied per path, config reloads re-synthesize everything and diff by id.
//! It performs no registration itself; the service applies the updates to the manager and the
//! model registry.
//!
//! Differences from Go: no revision stamping or queue coalescing. Go stamps each update with a
//! per-id revision so out-of-order delivery is detected; here the service computes updates and
//! applies them under one lock (`apply_lock`), so updates reach the manager in the order the
//! state changed. Runtime-only (aistudio websocket) auths bypass this state, and the auth directory
//! used by a reload scan is always the current config's, not the one captured at start.
//!
//! The methods that scan or read files are blocking: call them from `spawn_blocking`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use cpa_auth::Auth;
use cpa_auth::types::{ATTRIBUTE_PATH, ATTRIBUTE_SOURCE};
use cpa_config::{Config, clean_path};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::synth::{SynthesisContext, snapshot_core_auths, synthesize_auth_files};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthUpdateAction {
    Add,
    Modify,
    Delete,
}

/// An incremental change to the set of known auths. `auth` is `None` for deletes.
#[derive(Debug, Clone)]
pub struct AuthUpdate {
    pub action: AuthUpdateAction,
    pub id: String,
    pub auth: Option<Auth>,
}

impl AuthUpdate {
    fn upsert(action: AuthUpdateAction, auth: &Auth) -> Self {
        Self { action, id: auth.id.clone(), auth: Some(auth.clone()) }
    }

    fn delete(id: &str) -> Self {
        Self { action: AuthUpdateAction::Delete, id: id.to_string(), auth: None }
    }
}

/// Go `authEqual`: structural equality ignoring volatile fields (timestamps, runtime state,
/// quota recovery time), so a token refresh or cooldown tick never looks like a config change.
pub fn auth_equal(a: &Auth, b: &Auth) -> bool {
    fn normalized(auth: &Auth) -> Option<Value> {
        let mut clone = auth.clone();
        clone.created_at = None;
        clone.updated_at = None;
        clone.last_refreshed_at = None;
        clone.next_refresh_after = None;
        clone.runtime = None;
        clone.quota.next_recover_at = None;
        serde_json::to_value(&clone).ok()
    }
    a.file_name == b.file_name && a.index == b.index && normalized(a) == normalized(b)
}

fn hash_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn auth_path_key(auth: &Auth) -> String {
    let path = auth.attr(ATTRIBUTE_PATH);
    let path = if path.is_empty() { auth.attr(ATTRIBUTE_SOURCE) } else { path };
    normalize_path(&path)
}

/// Go `normalizeAuthPath`: cleaned path, lowercased without the `\\?\` prefix on Windows.
fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let cleaned = clean_path(trimmed);
    if cfg!(windows) {
        cleaned.strip_prefix(r"\\?\").unwrap_or(&cleaned).to_lowercase()
    } else {
        cleaned
    }
}

/// Tracks published auths and computes updates (Go: the `Watcher` state machine).
pub struct AuthSync {
    config: Arc<Config>,
    auth_dir: String,
    /// Auths as last published, by id.
    current: BTreeMap<String, Auth>,
    /// Normalized file path -> ids synthesized from it.
    file_auths_by_path: HashMap<String, BTreeSet<String>>,
    /// Normalized file path -> content hash of the last processed version.
    file_hashes: HashMap<String, String>,
    /// Whether the last `file_changed` got past the hash check and synthesized without error
    /// (Go then persists the file to the remote store, even when no auth changed).
    last_synced: bool,
}

impl AuthSync {
    pub fn new(config: Arc<Config>, auth_dir: impl Into<String>) -> Self {
        Self {
            config,
            auth_dir: auth_dir.into(),
            current: BTreeMap::new(),
            file_auths_by_path: HashMap::new(),
            file_hashes: HashMap::new(),
            last_synced: false,
        }
    }

    /// True when the last `file_changed` call should be pushed to a remote store.
    pub fn last_synced(&self) -> bool {
        self.last_synced
    }

    /// Go `Watcher.SetConfig` / the config assignment of `reloadConfig`.
    pub fn set_config(&mut self, config: Arc<Config>) {
        self.config = config;
    }

    pub fn set_auth_dir(&mut self, auth_dir: impl Into<String>) {
        self.auth_dir = auth_dir.into();
    }

    fn context(&self) -> SynthesisContext<'_> {
        SynthesisContext { config: &self.config, auth_dir: &self.auth_dir, now: Utc::now() }
    }

    /// Go `reloadClients`: optionally drops auths of providers whose `oauth-excluded-models`
    /// changed (so they are re-added), optionally rescans the auth dir to rebuild the per-file
    /// bookkeeping, then diffs a full snapshot against what was published. `force` re-publishes
    /// every auth as a modify.
    pub fn reload_clients(&mut self, rescan_auth: bool, affected_providers: &[String], force: bool) -> Vec<AuthUpdate> {
        if !affected_providers.is_empty() {
            self.current.retain(|_, auth| {
                let provider = auth.provider.trim().to_lowercase();
                !affected_providers.iter().any(|p| p.trim().eq_ignore_ascii_case(&provider))
            });
        }
        if rescan_auth {
            self.rescan_files();
        }
        let updates = self.refresh_auth_state(force);
        cpa_home::queue::notify_usage_refresh();
        updates
    }

    /// Rebuilds `file_hashes` and `file_auths_by_path` from the auth dir.
    fn rescan_files(&mut self) {
        let mut hashes = HashMap::new();
        let mut by_path: HashMap<String, BTreeSet<String>> = HashMap::new();
        if !self.auth_dir.is_empty() {
            match fs::read_dir(&self.auth_dir) {
                Err(err) => tracing::error!("failed to read auth directory for hash cache: {err}"),
                Ok(entries) => {
                    for entry in entries.filter_map(Result::ok) {
                        if entry.file_type().is_ok_and(|t| t.is_dir()) {
                            continue;
                        }
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if !name.to_lowercase().ends_with(".json") {
                            continue;
                        }
                        let full = Path::new(&self.auth_dir).join(&name);
                        let Ok(data) = fs::read(&full) else { continue };
                        if data.is_empty() {
                            continue;
                        }
                        let key = normalize_path(&full.to_string_lossy());
                        hashes.insert(key.clone(), hash_hex(&data));
                        match synthesize_auth_files(&self.context(), &full.to_string_lossy(), &data) {
                            Ok(auths) => {
                                for auth in auths {
                                    by_path.entry(key.clone()).or_default().insert(auth.id);
                                }
                            }
                            Err(err) => tracing::warn!("skipping auth file {name}: {err}"),
                        }
                    }
                }
            }
        }
        self.file_hashes = hashes;
        self.file_auths_by_path = by_path;
    }

    /// Go `refreshAuthState`: re-synthesizes config and file auths and diffs by id.
    fn refresh_auth_state(&mut self, force: bool) -> Vec<AuthUpdate> {
        let auths = snapshot_core_auths(&self.context());
        self.prepare_auth_updates(auths, force)
    }

    /// Go `prepareAuthUpdatesLocked`: add for new ids, modify when forced or changed, delete for
    /// ids that vanished. The snapshot becomes the published state.
    fn prepare_auth_updates(&mut self, auths: Vec<Auth>, force: bool) -> Vec<AuthUpdate> {
        let mut order: Vec<String> = Vec::with_capacity(auths.len());
        let mut state: BTreeMap<String, Auth> = BTreeMap::new();
        for auth in auths {
            if auth.id.is_empty() {
                continue;
            }
            if !state.contains_key(&auth.id) {
                order.push(auth.id.clone());
            }
            state.insert(auth.id.clone(), auth);
        }
        let mut updates = Vec::with_capacity(state.len());
        for id in &order {
            let auth = &state[id];
            match self.current.get(id) {
                None => updates.push(AuthUpdate::upsert(AuthUpdateAction::Add, auth)),
                Some(existing) if force || !auth_equal(existing, auth) => {
                    updates.push(AuthUpdate::upsert(AuthUpdateAction::Modify, auth));
                }
                Some(_) => {}
            }
        }
        for id in self.current.keys() {
            if !state.contains_key(id) {
                updates.push(AuthUpdate::delete(id));
            }
        }
        self.current = state;
        updates
    }

    /// Go `addOrUpdateClient`: a file was created or written. Unchanged content, empty and
    /// unparsable files produce no updates.
    pub fn file_changed(&mut self, path: &Path) -> Vec<AuthUpdate> {
        self.last_synced = false;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(err) => {
                tracing::error!("failed to read auth file {name}: {err}");
                return Vec::new();
            }
        };
        if data.is_empty() {
            tracing::debug!("ignoring empty auth file: {name}");
            return Vec::new();
        }
        if !matches!(serde_json::from_slice::<Value>(&data), Ok(Value::Object(_))) {
            tracing::error!("failed to parse auth file {name}");
            return Vec::new();
        }
        let key = normalize_path(&path.to_string_lossy());
        let hash = hash_hex(&data);
        if self.file_hashes.get(&key) == Some(&hash) {
            tracing::debug!("auth file unchanged (hash match), skipping reload: {name}");
            return Vec::new();
        }
        self.file_hashes.insert(key.clone(), hash);
        let old_ids = self.file_auths_by_path.get(&key).cloned().unwrap_or_default();

        let generated: Vec<Auth> = match synthesize_auth_files(&self.context(), &path.to_string_lossy(), &data) {
            Ok(auths) => {
                self.last_synced = true;
                auths.into_iter().filter(|a| !a.id.trim().is_empty()).collect()
            }
            Err(err) => {
                tracing::warn!("skipping auth file {name}: {err}");
                Vec::new()
            }
        };
        let new_by_id: BTreeMap<String, Auth> = generated.into_iter().map(|a| (a.id.clone(), a)).collect();
        if new_by_id.is_empty() {
            self.file_auths_by_path.remove(&key);
        } else {
            self.file_auths_by_path.insert(key, new_by_id.keys().cloned().collect());
        }
        let updates = self.per_path_updates(&old_ids, new_by_id);
        cpa_home::queue::notify_usage_refresh();
        updates
    }

    /// Go `removeClient`: a known file was deleted.
    pub fn file_removed(&mut self, path: &Path) -> Vec<AuthUpdate> {
        let key = normalize_path(&path.to_string_lossy());
        let old_ids = self.file_auths_by_path.remove(&key).unwrap_or_default();
        self.file_hashes.remove(&key);
        let updates = self.per_path_updates(&old_ids, BTreeMap::new());
        cpa_home::queue::notify_usage_refresh();
        updates
    }

    /// Go `computePerPathUpdatesLocked`.
    fn per_path_updates(&mut self, old_ids: &BTreeSet<String>, new_by_id: BTreeMap<String, Auth>) -> Vec<AuthUpdate> {
        let mut updates = Vec::with_capacity(old_ids.len() + new_by_id.len());
        for (id, auth) in &new_by_id {
            match self.current.get(id) {
                None => updates.push(AuthUpdate::upsert(AuthUpdateAction::Add, auth)),
                Some(existing) if !auth_equal(existing, auth) => {
                    updates.push(AuthUpdate::upsert(AuthUpdateAction::Modify, auth));
                }
                Some(_) => continue,
            }
            self.current.insert(id.clone(), auth.clone());
        }
        for id in old_ids {
            if !new_by_id.contains_key(id) {
                self.current.remove(id);
                updates.push(AuthUpdate::delete(id));
            }
        }
        updates
    }

    /// Go `dispatchPersistedAuthUpdate` bookkeeping: a credential file was written by a login or
    /// management call. Records it so the file event that follows does not duplicate the work.
    /// Returns false when the auth has no file path.
    pub fn note_persisted(&mut self, auth: &Auth) -> bool {
        let key = auth_path_key(auth);
        if key.is_empty() || auth.id.is_empty() {
            return false;
        }
        self.file_auths_by_path.entry(key).or_default().insert(auth.id.clone());
        self.current.insert(auth.id.clone(), auth.clone());
        true
    }
}

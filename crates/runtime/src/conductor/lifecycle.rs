//! Credential lifecycle: register / update / remove / load and persistence (Go:
//! conductor_lifecycle.go).
//!
//! Every registration bumps a per-id `registration_epoch` (monotonic, also across removal) so
//! stale refreshes and persists from a previous registration are rejected; every mutation bumps
//! `generation`, which orders registry projections and persistence writes.

use std::sync::Arc;

use cpa_auth::credmeta::{normalize_credential_metadata, validate_auth_weight};
use cpa_auth::store::SaveOptions;
use cpa_auth::types::{ATTRIBUTE_API_KEY, Auth, Status};

use super::Manager;
use super::cooldown::{
    clear_cooldown_state_for_auth, clear_unauthorized_model_states, has_unauthorized_auth_failure,
    is_disabled, normalize_model_states,
};
use super::errors::Failure;
use super::merge::{merge_prepared_auth, merge_refreshed_auth};
use crate::executor::ExecError;

/// Per-call options for register/update (Go carries these on the context).
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOptions {
    /// Do not write the credential to the token store (file-watcher driven updates: the file on
    /// disk is already the source of truth).
    pub skip_persist: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateMode {
    Replace,
    Refresh,
    Prepare,
}

fn plain_error(msg: impl Into<String>) -> ExecError {
    let mut e = ExecError::new(0, msg);
    e.upstream_attempted = false;
    e
}

/// Whether authentication material (tokens or API keys) differs (Go: CredentialsChanged).
pub fn credentials_changed(existing: &Auth, incoming: &Auth) -> bool {
    if existing.access_token() != incoming.access_token()
        || existing.refresh_token() != incoming.refresh_token()
    {
        return true;
    }
    let id_token = |a: &Auth| {
        let t = a.meta_str("id_token");
        if t.is_empty() {
            a.meta_str("idToken")
        } else {
            t
        }
    };
    if id_token(existing) != id_token(incoming) {
        return true;
    }
    let key = |a: &Auth| {
        let k = a
            .attributes
            .get(ATTRIBUTE_API_KEY)
            .cloned()
            .unwrap_or_default();
        if k.is_empty() {
            a.meta_str("api_key")
        } else {
            k
        }
    };
    key(existing) != key(incoming)
}

/// Writes every public field of `src` onto `target`, keeping the target's private request
/// counters (`recent_requests`).
fn overlay(target: &mut Auth, src: Auth) {
    target.id = src.id;
    target.registration_epoch = src.registration_epoch;
    target.generation = src.generation;
    target.index = src.index;
    target.provider = src.provider;
    target.prefix = src.prefix;
    target.file_name = src.file_name;
    target.storage = src.storage;
    target.label = src.label;
    target.status = src.status;
    target.status_message = src.status_message;
    target.disabled = src.disabled;
    target.unavailable = src.unavailable;
    target.proxy_url = src.proxy_url;
    target.attributes = src.attributes;
    target.metadata = src.metadata;
    target.quota = src.quota;
    target.last_error = src.last_error;
    target.created_at = src.created_at;
    target.updated_at = src.updated_at;
    target.last_refreshed_at = src.last_refreshed_at;
    target.next_refresh_after = src.next_refresh_after;
    target.refresh_failures = src.refresh_failures;
    target.next_retry_after = src.next_retry_after;
    target.model_states = src.model_states;
    target.runtime = src.runtime;
    target.success = src.success;
    target.failed = src.failed;
}

impl Manager {
    /// Inserts a credential (Go: Register). Registering an existing id re-registers it with a new
    /// epoch.
    pub async fn register(&self, auth: Auth) -> Result<Auth, ExecError> {
        self.register_with(auth, UpdateOptions::default()).await
    }

    pub async fn register_with(
        &self,
        mut auth: Auth,
        opts: UpdateOptions,
    ) -> Result<Auth, ExecError> {
        normalize_credential_metadata(&mut auth.metadata);
        validate_auth_weight(&auth).map_err(|e| plain_error(format!("register auth: {e}")))?;
        if auth.id.is_empty() {
            auth.id = uuid::Uuid::new_v4().to_string();
        }
        let now = self.now();
        if auth.generation == 0 {
            auth.generation = 1;
        }
        if auth.created_at.is_none() {
            auth.created_at = Some(now);
        }
        auth.updated_at = Some(now);
        let mut cooldown_changed = normalize_model_states(&mut auth);
        if self.cooldown_disabled_for_auth(&auth) || is_disabled(&auth) {
            cooldown_changed = clear_cooldown_state_for_auth(&mut auth, now) || cooldown_changed;
        }
        auth.ensure_index();
        {
            let mut st = self.state.write();
            let existing_epoch = st.auths.get(&auth.id).map(|e| e.registration_epoch);
            let epochs = &mut st.auth_epochs;
            let slot = epochs.entry(auth.id.clone()).or_insert(0);
            if let Some(e) = existing_epoch
                && e > *slot
            {
                *slot = e;
            }
            if auth.registration_epoch > *slot {
                *slot = auth.registration_epoch;
            }
            *slot += 1;
            auth.registration_epoch = *slot;
            auth.generation = 1;
            st.auths.insert(auth.id.clone(), auth.clone());
        }
        self.queue_refresh_reschedule(&auth.id);
        if !opts.skip_persist
            && let Err(e) = self.persist(&mut auth).await
        {
            tracing::warn!(
                "failed to persist registered auth {} ({}): {e}",
                auth.provider,
                auth.id
            );
        }
        let hook = self.hook.read().clone();
        if let Some(h) = hook {
            h.on_auth_registered(&auth);
        }
        if cooldown_changed {
            self.persist_cooldown_states().await;
        }
        Ok(auth)
    }

    /// Inserts or replaces a credential: an unknown id is registered, a known one updated (Go:
    /// Register / Update as used by the service sync).
    pub async fn update(&self, auth: Auth) -> Result<Auth, ExecError> {
        self.update_with(auth, UpdateOptions::default()).await
    }

    pub async fn update_with(&self, auth: Auth, opts: UpdateOptions) -> Result<Auth, ExecError> {
        if auth.id.is_empty() {
            return self.register_with(auth, opts).await;
        }
        let exists = self.state.read().auths.contains_key(&auth.id);
        if !exists {
            return self.register_with(auth, opts).await;
        }
        match self
            .update_internal(None, auth.clone(), UpdateMode::Replace, opts)
            .await?
        {
            Some(saved) => Ok(saved),
            // Removed between the check and the update: register instead.
            None => self.register_with(auth, opts).await,
        }
    }

    /// Merges a refresh result into the latest credential (Go: UpdateRefreshedAuth).
    pub async fn update_refreshed_auth(
        &self,
        base: &Auth,
        updated: Auth,
    ) -> Result<Option<Auth>, ExecError> {
        self.update_internal(
            Some(base),
            updated,
            UpdateMode::Refresh,
            UpdateOptions::default(),
        )
        .await
    }

    /// Merges request-preparation results into the latest credential (Go: UpdatePreparedAuth).
    pub async fn update_prepared_auth(
        &self,
        base: &Auth,
        updated: Auth,
    ) -> Result<Option<Auth>, ExecError> {
        self.update_internal(
            Some(base),
            updated,
            UpdateMode::Prepare,
            UpdateOptions::default(),
        )
        .await
    }

    pub(crate) async fn update_internal(
        &self,
        base: Option<&Auth>,
        mut auth: Auth,
        mode: UpdateMode,
        opts: UpdateOptions,
    ) -> Result<Option<Auth>, ExecError> {
        if auth.id.is_empty() {
            return Ok(None);
        }
        normalize_credential_metadata(&mut auth.metadata);
        validate_auth_weight(&auth).map_err(|e| plain_error(format!("update auth: {e}")))?;
        let now = self.now();
        let mut cooldown_changed = false;
        let persist_meta_mint = matches!(mode, UpdateMode::Prepare | UpdateMode::Refresh)
            && auth.provider.trim().eq_ignore_ascii_case("meta");
        let mut result = {
            let mut st = self.state.write();
            let Some(existing) = st.auths.get(&auth.id).cloned() else {
                return Ok(None);
            };
            let slot = st.auth_epochs.entry(auth.id.clone()).or_insert(0);
            if existing.registration_epoch > *slot {
                *slot = existing.registration_epoch;
            }
            if matches!(mode, UpdateMode::Refresh | UpdateMode::Prepare)
                && let Some(b) = base
                && existing.registration_epoch != b.registration_epoch
            {
                return Err(plain_error(format!(
                    "update auth {}: stale registration epoch {} != {}",
                    auth.id, b.registration_epoch, existing.registration_epoch
                )));
            }
            match mode {
                UpdateMode::Refresh => {
                    auth = merge_refreshed_auth(base, &existing, &auth, now);
                    normalize_credential_metadata(&mut auth.metadata);
                }
                UpdateMode::Prepare => {
                    auth = merge_prepared_auth(base, &existing, &auth);
                    normalize_credential_metadata(&mut auth.metadata);
                }
                UpdateMode::Replace => {}
            }
            let epoch = *st.auth_epochs.get(&auth.id).unwrap_or(&0);
            if auth.registration_epoch != 0 && auth.registration_epoch < epoch {
                return Err(plain_error(format!(
                    "update auth {}: stale registration epoch {} < {epoch}",
                    auth.id, auth.registration_epoch
                )));
            }
            if auth.registration_epoch >= epoch {
                st.auth_epochs
                    .insert(auth.id.clone(), auth.registration_epoch);
            } else if auth.registration_epoch == 0 {
                auth.registration_epoch = epoch;
            }
            if auth.index.is_empty() {
                auth.index = existing.index.clone();
            }
            auth.success = existing.success;
            auth.failed = existing.failed;
            auth.generation = if auth.generation <= existing.generation {
                existing.generation + 1
            } else {
                auth.generation + 1
            };
            if !is_disabled(&existing) && !is_disabled(&auth) {
                if auth.model_states.is_empty() && !existing.model_states.is_empty() {
                    auth.model_states = existing.model_states.clone();
                }
                if credentials_changed(&existing, &auth) {
                    let last_unauthorized = auth.last_error.as_ref().is_some_and(|e| {
                        Failure {
                            status: e.http_status,
                            text: &e.message,
                            request_scoped: false,
                            code: None,
                            raw_message: None,
                        }
                        .is_unauthorized()
                    });
                    if has_unauthorized_auth_failure(&existing) || last_unauthorized {
                        auth.unavailable = false;
                        auth.last_error = None;
                        auth.status_message.clear();
                        auth.status = Status::Active;
                    }
                    if !clear_unauthorized_model_states(&mut auth, now).is_empty() {
                        cooldown_changed = true;
                    }
                }
                if existing.quota.exceeded
                    && existing.quota.reason == "credential_quota"
                    && existing.quota.next_recover_at.is_some_and(|t| t > now)
                {
                    auth.unavailable = existing.unavailable;
                    auth.next_retry_after = existing.next_retry_after;
                    auth.quota = existing.quota.clone();
                    if auth.status == Status::Active {
                        auth.status = existing.status;
                    }
                }
            }
            auth.updated_at = Some(now);
            cooldown_changed = normalize_model_states(&mut auth) || cooldown_changed;
            if self.cooldown_disabled_for_auth(&auth) || is_disabled(&auth) {
                cooldown_changed =
                    clear_cooldown_state_for_auth(&mut auth, now) || cooldown_changed;
            }
            auth.ensure_index();
            // A minted Meta key must reach the store before requests can use it, so it is
            // installed only after it was persisted (below).
            if !persist_meta_mint {
                let mut stored = existing;
                overlay(&mut stored, auth.clone());
                st.auths.insert(auth.id.clone(), stored);
            }
            auth
        };
        if persist_meta_mint {
            if let Err(e) = self.persist(&mut result).await {
                return Err(plain_error(format!("persist meta auth: {e}")));
            }
            // A concurrent reload or removal must not be overwritten by an obsolete mint.
            let mut st = self.state.write();
            match st.auths.get_mut(&result.id) {
                Some(stored) if stored.registration_epoch == result.registration_epoch => {
                    overlay(stored, result.clone());
                }
                _ => {
                    return Err(plain_error(format!(
                        "update auth {}: credential changed while persisting",
                        result.id
                    )));
                }
            }
        }
        self.queue_refresh_reschedule(&result.id);
        if !persist_meta_mint
            && !opts.skip_persist
            && let Err(e) = self.persist(&mut result).await
        {
            tracing::warn!(
                "failed to persist updated auth {} ({}): {e}",
                result.provider,
                result.id
            );
        }
        let hook = self.hook.read().clone();
        if let Some(h) = hook {
            h.on_auth_updated(&result);
        }
        if cooldown_changed {
            self.persist_cooldown_states().await;
        }
        Ok(Some(result))
    }

    /// Removes a credential from runtime state; callers delete the backing file (Go: Remove).
    pub async fn remove(&self, id: &str) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        let (provider, executor) = {
            let mut st = self.state.write();
            let Some(existing) = st.auths.remove(id) else {
                return;
            };
            let slot = st.auth_epochs.entry(id.to_string()).or_insert(0);
            if existing.registration_epoch > *slot {
                *slot = existing.registration_epoch;
            }
            *slot += 1;
            let provider = existing.provider.trim().to_string();
            let executor = super::executor_locked(&st, &provider);
            (provider, executor)
        };
        self.home_forget_auth(id);
        self.queue_refresh_unschedule(id);
        // Drop the per-auth refresh lock unless a refresh is holding it. (`persist_locks` stay:
        // their (epoch, generation) high-water mark rejects stale saves from the removed auth.)
        {
            let mut locks = self.refresh_locks.lock();
            if locks.get(id).is_some_and(|l| Arc::strong_count(l) == 1) {
                locks.remove(id);
            }
        }
        // Pool offsets are keyed `<auth id>|<provider key>|<model>` (see `openai_compat_model_pool_key`).
        let pool_prefix = format!("{}|", id.to_lowercase());
        self.pool_offsets
            .lock()
            .retain(|k, _| !k.starts_with(&pool_prefix));
        if let Some(aff) = self.selector().affinity() {
            aff.invalidate_auth(id);
        }
        if !provider.is_empty()
            && let Some(exec) = executor
        {
            exec.close_execution_session(super::CLOSE_ALL_EXECUTION_SESSIONS_ID)
                .await;
        }
        self.persist_cooldown_states().await;
    }

    /// Replaces all credentials with the store contents (Go: Load).
    pub async fn load(&self) -> Result<(), String> {
        let Some(store) = self.store.read().clone() else {
            return Ok(());
        };
        let items = tokio::task::spawn_blocking(move || store.list())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let mut st = self.state.write();
        let previous: Vec<String> = st.auths.keys().cloned().collect();
        st.auths.clear();
        for mut auth in items {
            if auth.id.is_empty() {
                continue;
            }
            normalize_credential_metadata(&mut auth.metadata);
            if validate_auth_weight(&auth).is_err() {
                continue;
            }
            auth.ensure_index();
            let slot = st.auth_epochs.entry(auth.id.clone()).or_insert(0);
            *slot = (*slot).max(auth.registration_epoch) + 1;
            auth.registration_epoch = *slot;
            auth.generation = 1;
            st.auths.insert(auth.id.clone(), auth);
        }
        for id in previous {
            if !st.auths.contains_key(&id) {
                *st.auth_epochs.entry(id).or_insert(0) += 1;
            }
        }
        Ok(())
    }

    /// Writes the credential to the token store when it is persistable (Go: persist): config
    /// API-key auths, runtime-only auths and auths without metadata are never written. Writes
    /// older than the last persisted `(epoch, generation)` are dropped. The store may adjust the
    /// passed auth (path attributes, merged metadata).
    pub(crate) async fn persist(&self, auth: &mut Auth) -> Result<(), String> {
        let Some(store) = self.store.read().clone() else {
            return Ok(());
        };
        validate_auth_weight(auth).map_err(|e| format!("persist auth: {e}"))?;
        if auth.is_config_api_key()
            || auth
                .attributes
                .get("runtime_only")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
            || auth.is_plugin_virtual()
            || auth.metadata.is_empty()
        {
            return Ok(());
        }
        let lock = {
            let mut locks = self.persist_locks.lock();
            locks
                .entry(auth.id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new((0, 0))))
                .clone()
        };
        let mut last = lock.lock().await;
        if (auth.registration_epoch, auth.generation) < *last {
            return Ok(());
        }
        *last = (auth.registration_epoch, auth.generation);
        let mut owned = auth.clone();
        let (saved, res) = tokio::task::spawn_blocking(move || {
            let r = store.save(&mut owned, SaveOptions::default());
            (owned, r)
        })
        .await
        .map_err(|e| e.to_string())?;
        *auth = saved;
        res.map(|_| ()).map_err(|e| e.to_string())
    }
}

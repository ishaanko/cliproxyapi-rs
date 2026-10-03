//! Recording execution results and everything that follows from them (Go: MarkResult and its
//! consumers in conductor_cooldown.go): registry projections, hooks, usage, session affinity,
//! cooldown persistence, quota reset and registry reconciliation.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use cpa_auth::Auth;
use cpa_auth::types::Status;
use cpa_core::registry::ClientModelProjection;

use super::Manager;
use super::cooldown::{
    CooldownView, ExecResult, apply_result, clear_cooldown_state_for_auth, cooldown_reason,
    cooldown_snapshot_for_auth, existing_model_state, has_model_error, is_disabled,
    is_model_state_active_cooldown, merge_model_state, model_state_is_clean,
    normalize_model_states, reset_model_state, update_aggregated_availability,
};
use super::cooldown_state::{
    CooldownStateRecord, CooldownStateStore, records_equal, records_for_auth,
};
use super::events::build_error_event_payload;
use super::usage::{UsageFacts, build_usage_record};
use super::util::{after, canonical_model_key, dedupe_strings};

impl Manager {
    /// Records an execution outcome: updates cooldown/quota state, registry availability, hooks and
    /// session affinity (Go: MarkResult). Usage is recorded by the execution paths, which know the
    /// latency and token counts. Credentials are not written to the auth store here (only the
    /// cooldown snapshot is persisted, when a cooldown store is set); callers that need that use
    /// `update`.
    pub fn mark_result(&self, result: ExecResult) {
        self.mark_result_inner(result, None);
    }

    pub(crate) fn mark_result_inner(&self, mut result: ExecResult, facts: Option<UsageFacts>) {
        if result.auth_id.is_empty() {
            return;
        }
        let policy = self.result_policy.read().clone();
        if let Some(policy) = policy {
            result = policy.apply_result_policy(result);
            if result.auth_id.is_empty() {
                return;
            }
        }
        let now = self.now();
        let track_cooldown = self.cooldown_store.read().is_some();
        let mut cooldown_changed = false;
        let snapshot = {
            let mut st = self.state.write();
            st.auths.get_mut(&result.auth_id).map(|auth| {
                let mut model_key = canonical_model_key(&result.model);
                if model_key.is_empty() && !result.route_model.trim().is_empty() {
                    model_key = self.selection_model_key_for_auth(auth, &result.route_model);
                    if model_key.is_empty() {
                        model_key = canonical_model_key(&result.route_model);
                    }
                }
                let before = track_cooldown.then(|| self.cooldown_records_for(auth, now));
                let policy = self.cooling_policy_for(auth);
                apply_result(auth, &result, &model_key, now, policy);
                if let Some(before) = before {
                    cooldown_changed =
                        !records_equal(&before, &self.cooldown_records_for(auth, now));
                }
                auth.clone()
            })
        };
        if snapshot.is_some() && cooldown_changed {
            self.persist_cooldown_states_detached();
        }
        if let Some(snap) = &snapshot {
            self.apply_projections_for(snap, now);
        }
        let hook = self.hook.read().clone();
        if let Some(hook) = hook {
            hook.on_result(&result);
        }
        self.publish_error_event(&result, snapshot.as_ref(), now);
        if let Some(aff) = self.selector().affinity() {
            aff.on_result(&result);
        }
        self.record_usage(&result, snapshot.as_ref(), facts.as_ref(), now);
    }

    pub(crate) fn record_usage(
        &self,
        result: &ExecResult,
        snapshot: Option<&Auth>,
        facts: Option<&UsageFacts>,
        now: DateTime<Utc>,
    ) {
        let tracker = self.usage.read().clone();
        if let (Some(tracker), Some(facts)) = (tracker, facts) {
            tracker.record(build_usage_record(result, snapshot, facts, now));
        }
    }

    fn publish_error_event(
        &self,
        result: &ExecResult,
        snapshot: Option<&Auth>,
        now: DateTime<Utc>,
    ) {
        if result.success || self.home_enabled() {
            return;
        }
        let sink = self.error_sink.read().clone();
        if let (Some(sink), Some(snap)) = (sink, snapshot)
            && let Some(payload) = build_error_event_payload(result, snap, now)
        {
            sink(payload);
        }
    }

    /// A failed attempt that must not suspend anything (compact-endpoint faults, count-tokens
    /// route 404s): counters, hook, event and usage only.
    pub(crate) fn record_availability_neutral_result(
        &self,
        result: ExecResult,
        facts: Option<UsageFacts>,
    ) {
        if result.auth_id.is_empty() {
            return;
        }
        let now = self.now();
        let snapshot = {
            let mut st = self.state.write();
            st.auths.get_mut(&result.auth_id).map(|auth| {
                auth.record_recent_request(now, result.success);
                if result.success {
                    auth.success += 1;
                } else {
                    auth.failed += 1;
                }
                auth.generation += 1;
                auth.updated_at = Some(now);
                auth.clone()
            })
        };
        let hook = self.hook.read().clone();
        if let Some(hook) = hook {
            hook.on_result(&result);
        }
        self.publish_error_event(&result, snapshot.as_ref(), now);
        self.record_usage(&result, snapshot.as_ref(), facts.as_ref(), now);
    }

    // ---- Registry projection ----

    /// Desired registry availability of one of the client's models (Go:
    /// clientModelProjectionForAuth).
    pub(crate) fn client_model_projection_for_auth(
        &self,
        auth: &Auth,
        route_model: &str,
        now: DateTime<Utc>,
    ) -> ClientModelProjection {
        let target = route_model.trim();
        if target.is_empty() {
            return ClientModelProjection::default();
        }
        let mut key = self.selection_model_key_for_auth(auth, target);
        if key.is_empty() {
            key = canonical_model_key(target);
        }
        let state = existing_model_state(auth, &key);
        let mut suspended = is_disabled(auth);
        if auth.quota.exceeded
            && auth.quota.reason == "credential_quota"
            && after(auth.quota.next_recover_at, now)
        {
            suspended = true;
        }
        let mut quota_exceeded = false;
        let mut reason = String::new();
        if let Some(s) = state {
            if s.status == Status::Disabled || s.unavailable || after(s.next_retry_after, now) {
                suspended = true;
            }
            if s.quota.exceeded
                && (s.quota.next_recover_at.is_none() || after(s.quota.next_recover_at, now))
            {
                quota_exceeded = true;
            }
            if suspended {
                reason = cooldown_reason(&s.status_message, &s.quota, s.last_error.as_ref());
            }
        }
        if auth.model_states.is_empty() && auth.unavailable && after(auth.next_retry_after, now) {
            // Without per-model states scheduling uses the credential-wide cooldown; agree with it.
            suspended = true;
        }
        if suspended && reason.is_empty() {
            reason = cooldown_reason(&auth.status_message, &auth.quota, auth.last_error.as_ref());
        }
        ClientModelProjection {
            model_id: target.to_string(),
            suspended,
            suspend_reason: reason,
            quota_exceeded,
        }
    }

    fn apply_projections_for(&self, snapshot: &Auth, now: DateTime<Utc>) {
        let (models, epoch) = self.registry.get_models_and_epoch_for_client(&snapshot.id);
        let projections: Vec<ClientModelProjection> = models
            .iter()
            .filter(|m| !m.id.trim().is_empty())
            .map(|m| self.client_model_projection_for_auth(snapshot, &m.id, now))
            .collect();
        if !projections.is_empty() {
            self.registry.apply_client_model_projections(
                &snapshot.id,
                epoch,
                snapshot.generation,
                &projections,
            );
        }
    }

    /// Aligns per-model runtime state with the registry after models were (re)registered for the
    /// credential: active cooldowns of still-supported models survive, stale errors reset, states
    /// of models no longer reachable (directly or via alias routes) are pruned, legacy alias-keyed
    /// states migrate to their upstream key (Go: ReconcileRegistryModelStates).
    pub fn reconcile_registry_model_states(&self, auth_id: &str) {
        if auth_id.is_empty() {
            return;
        }
        let now = self.now();
        let track_cooldown = self.cooldown_store.read().is_some();
        let mut cooldown_changed = false;
        let mut snapshot: Option<Auth> = None;
        let mut supported_models = Vec::new();
        let mut reg_epoch = 0;
        {
            let mut st = self.state.write();
            let Some(auth) = st.auths.get_mut(auth_id) else {
                return;
            };
            let before = track_cooldown.then(|| self.cooldown_records_for(auth, now));
            for _ in 0..10 {
                let (models, epoch) = self.registry.get_models_and_epoch_for_client(auth_id);
                supported_models = models;
                reg_epoch = epoch;
                let mut candidate = auth.model_states.clone();
                let mut tmp = Auth::new(auth.id.clone(), auth.provider.clone());
                tmp.attributes = auth.attributes.clone();
                tmp.model_states = std::mem::take(&mut candidate);
                let mut changed = normalize_model_states(&mut tmp);

                // Historical alias migration: move state keyed by an alias route to its target.
                if !tmp.model_states.is_empty() && !supported_models.is_empty() {
                    let mut authoritative = std::collections::HashSet::new();
                    let mut route_to_target = std::collections::HashMap::new();
                    for sm in &supported_models {
                        let route_id = sm.id.trim();
                        if route_id.is_empty() {
                            continue;
                        }
                        let canonical_route = canonical_model_key(route_id);
                        let mut target = self.selection_model_key_for_auth(auth, route_id);
                        if target.is_empty() {
                            target = canonical_route.clone();
                        }
                        if !target.is_empty() {
                            authoritative.insert(target.clone());
                        }
                        if !canonical_route.is_empty() {
                            route_to_target.insert(canonical_route, target);
                        }
                    }
                    for sm in &supported_models {
                        let route_id = sm.id.trim();
                        if route_id.is_empty() {
                            continue;
                        }
                        let canonical_route = canonical_model_key(route_id);
                        let Some(target) = route_to_target.get(&canonical_route).cloned() else {
                            continue;
                        };
                        if target.is_empty() || canonical_route.is_empty() {
                            continue;
                        }
                        if canonical_route != target && !authoritative.contains(&canonical_route) {
                            let mut alias_keys = vec![canonical_route.clone()];
                            if route_id != canonical_route {
                                alias_keys.push(route_id.to_string());
                            }
                            for alias_key in alias_keys {
                                if let Some(state) = tmp.model_states.get(&alias_key).cloned() {
                                    if !model_state_is_clean(&state)
                                        || is_model_state_active_cooldown(&state, now)
                                    {
                                        match tmp.model_states.get_mut(&target) {
                                            Some(existing) => merge_model_state(existing, &state),
                                            None => {
                                                tmp.model_states.insert(target.clone(), state);
                                            }
                                        }
                                    }
                                    tmp.model_states.remove(&alias_key);
                                    changed = true;
                                }
                            }
                        }
                    }
                }

                let mut supported = std::collections::HashSet::new();
                for m in &supported_models {
                    if m.id.trim().is_empty() {
                        continue;
                    }
                    let mut key = self.selection_model_key_for_auth(auth, &m.id);
                    if key.is_empty() {
                        key = canonical_model_key(&m.id);
                    }
                    if !key.is_empty() {
                        supported.insert(key);
                    }
                }
                let keys: Vec<String> = tmp.model_states.keys().cloned().collect();
                for model_key in keys {
                    let mut base = canonical_model_key(&model_key);
                    if base.is_empty() {
                        base = model_key.trim().to_string();
                    }
                    if !supported.contains(&base) {
                        tmp.model_states.remove(&model_key);
                        changed = true;
                        continue;
                    }
                    let Some(state) = tmp.model_states.get_mut(&model_key) else {
                        continue;
                    };
                    if model_state_is_clean(state) || is_model_state_active_cooldown(state, now) {
                        continue;
                    }
                    reset_model_state(state, now);
                    changed = true;
                }

                if self.registry.client_registration_epoch(auth_id) == reg_epoch {
                    auth.model_states = tmp.model_states;
                    if changed {
                        update_aggregated_availability(auth, now);
                        if !has_model_error(auth, now) {
                            auth.last_error = None;
                            auth.status_message.clear();
                            auth.status = Status::Active;
                        }
                        auth.generation += 1;
                        auth.updated_at = Some(now);
                    }
                    if let Some(before) = &before {
                        cooldown_changed = changed
                            || !records_equal(before, &self.cooldown_records_for(auth, now));
                    }
                    snapshot = Some(auth.clone());
                    break;
                }
            }
        }
        let Some(snapshot) = snapshot else { return };
        let projections: Vec<ClientModelProjection> = supported_models
            .iter()
            .filter(|m| !m.id.trim().is_empty())
            .map(|m| self.client_model_projection_for_auth(&snapshot, &m.id, now))
            .collect();
        self.registry.apply_client_model_projections(
            auth_id,
            reg_epoch,
            snapshot.generation,
            &projections,
        );
        if cooldown_changed {
            self.persist_cooldown_states_detached();
        }
    }

    /// Clears cooldown/quota state of one credential and resumes its registry models (Go:
    /// ResetQuota). Returns the new snapshot and the models touched, `None` when unknown.
    pub fn reset_quota(&self, auth_id: &str) -> Result<Option<(Auth, Vec<String>)>, String> {
        let auth_id = auth_id.trim();
        if auth_id.is_empty() {
            return Err("auth id is required".into());
        }
        let now = self.now();
        let registered_models: Vec<String> = self
            .registry
            .get_models_for_client(auth_id)
            .iter()
            .filter(|m| !m.id.trim().is_empty())
            .map(|m| canonical_model_key(&m.id))
            .collect();
        let track_cooldown = self.cooldown_store.read().is_some();
        let mut cooldown_changed = false;
        let (snapshot, models) = {
            let mut st = self.state.write();
            let Some(auth) = st.auths.get_mut(auth_id) else {
                return Ok(None);
            };
            let before = track_cooldown.then(|| self.cooldown_records_for(auth, now));
            let mut models: Vec<String> = Vec::new();
            for (key, state) in auth.model_states.iter_mut() {
                if key.trim().is_empty() {
                    continue;
                }
                models.push(key.clone());
                reset_model_state(state, now);
            }
            if clear_cooldown_state_for_auth(auth, now) {
                if models.is_empty() {
                    models.extend(registered_models.iter().cloned());
                }
            } else if !auth.model_states.is_empty() {
                update_aggregated_availability(auth, now);
            }
            if models.is_empty() {
                models.extend(registered_models.iter().cloned());
            }
            let models = dedupe_strings(models);
            if !is_disabled(auth) && !has_model_error(auth, now) {
                auth.last_error = None;
                auth.status_message.clear();
                auth.status = Status::Active;
            }
            auth.generation += 1;
            auth.updated_at = Some(now);
            if let Some(before) = before {
                cooldown_changed = !records_equal(&before, &self.cooldown_records_for(auth, now));
            }
            (auth.clone(), models)
        };
        self.apply_projections_for(&snapshot, now);
        if cooldown_changed {
            self.persist_cooldown_states_detached();
        }
        Ok(Some((snapshot, models)))
    }

    /// Unexpired local cooldown timers of one credential, for the management API.
    pub fn cooldown_snapshot(&self, auth_id: &str) -> Option<Vec<CooldownView>> {
        let now = self.now();
        self.state
            .read()
            .auths
            .get(auth_id)
            .map(|a| cooldown_snapshot_for_auth(a, now))
    }

    // ---- Cooldown persistence ----

    pub(crate) fn cooldown_records_for(
        &self,
        auth: &Auth,
        now: DateTime<Utc>,
    ) -> Vec<CooldownStateRecord> {
        if auth.id.is_empty() || is_disabled(auth) || self.cooldown_disabled_for_auth(auth) {
            return Vec::new();
        }
        records_for_auth(auth, now)
    }

    fn cooldown_snapshot_records(&self) -> Vec<CooldownStateRecord> {
        let now = self.now();
        let st = self.state.read();
        let mut records: Vec<CooldownStateRecord> = st
            .auths
            .values()
            .flat_map(|a| self.cooldown_records_for(a, now))
            .collect();
        records.sort_by(|a, b| {
            (&a.provider, &a.auth_id, &a.model).cmp(&(&b.provider, &b.auth_id, &b.model))
        });
        records
    }

    /// Replaces the cooldown state store (Go: SetCooldownStateStore).
    pub fn set_cooldown_state_store(&self, store: Option<Arc<dyn CooldownStateStore>>) {
        *self.cooldown_store.write() = store;
    }

    /// Takes the snapshot and saves it under the save lock, so concurrent saves are serialized
    /// and the last writer always holds the newest state (Go: snapshot inside the store lock).
    fn save_cooldown_snapshot(&self, store: &dyn CooldownStateStore) {
        let _guard = self.cooldown_save_lock.lock();
        let records = self.cooldown_snapshot_records();
        if let Err(e) = store.save(&records) {
            tracing::warn!("failed to persist cooldown state: {e}");
        }
    }

    /// Saves the current cooldown records without blocking the caller (file I/O runs on the
    /// blocking pool when a runtime is available, inline otherwise).
    pub(crate) fn persist_cooldown_states_detached(&self) {
        let Some(store) = self.cooldown_store.read().clone() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let this = self.clone();
                handle.spawn_blocking(move || this.save_cooldown_snapshot(store.as_ref()));
            }
            Err(_) => self.save_cooldown_snapshot(store.as_ref()),
        }
    }

    /// Saves the current cooldown records and waits for the write.
    pub async fn persist_cooldown_states(&self) {
        let Some(store) = self.cooldown_store.read().clone() else {
            return;
        };
        let this = self.clone();
        let _ =
            tokio::task::spawn_blocking(move || this.save_cooldown_snapshot(store.as_ref())).await;
    }

    /// Restores unexpired persisted records into registered credentials (Go: RestoreCooldownStates).
    pub async fn restore_cooldown_states(&self) -> Result<(), String> {
        let Some(store) = self.cooldown_store.read().clone() else {
            return Ok(());
        };
        let records = tokio::task::spawn_blocking(move || store.load())
            .await
            .map_err(|e| e.to_string())??;
        if records.is_empty() {
            return Ok(());
        }
        let now = self.now();
        let (model_records, auth_records): (Vec<_>, Vec<_>) = records
            .into_iter()
            .partition(|r| !r.model.trim().is_empty());
        {
            let mut st = self.state.write();
            for r in model_records.iter().chain(auth_records.iter()) {
                self.restore_cooldown_record(&mut st, r, now);
            }
        }
        self.persist_cooldown_states().await;
        Ok(())
    }

    fn restore_cooldown_record(
        &self,
        st: &mut super::State,
        record: &CooldownStateRecord,
        now: DateTime<Utc>,
    ) -> bool {
        let auth_id = record.auth_id.trim().to_string();
        if auth_id.is_empty()
            || record.next_retry_after.is_none()
            || !after(record.next_retry_after, now)
        {
            return false;
        }
        let cfg = self.cfg();
        let Some(auth) = st.auths.get_mut(&auth_id) else {
            return false;
        };
        if is_disabled(auth) || self.cooldown_disabled_for_auth_cfg(auth, &cfg) {
            return false;
        }
        let updated_at = record.updated_at.unwrap_or(now);
        let reason = record.reason.trim().to_string();
        let model = record.model.trim().to_string();
        let mut quota = record.quota.clone();
        if quota.exceeded && quota.next_recover_at.is_none() {
            quota.next_recover_at = record.next_retry_after;
        }
        if model.is_empty() {
            auth.unavailable = true;
            auth.status = Status::Error;
            auth.next_retry_after = record.next_retry_after;
            super::cooldown::apply_cooldown_fields(&mut auth.quota, &quota);
            auth.quota =
                super::cooldown::merge_quota_observation(std::mem::take(&mut auth.quota), &quota);
            auth.generation += 1;
            auth.updated_at = Some(updated_at);
            if !reason.is_empty() {
                auth.status_message = reason;
            }
            auth.last_error = record.last_error.clone();
            return true;
        }
        let incoming = cpa_auth::types::ModelState {
            unavailable: true,
            status: Status::Error,
            status_message: reason,
            next_retry_after: record.next_retry_after,
            quota,
            last_error: record.last_error.clone(),
            updated_at: Some(updated_at),
        };
        match super::cooldown::ensure_model_state(auth, &model) {
            Some(state) => merge_model_state(state, &incoming),
            None => return false,
        }
        auth.generation += 1;
        auth.updated_at = Some(updated_at);
        update_aggregated_availability(auth, now);
        true
    }

    /// Wipes cooldown state of credentials whose cooling is now disabled (or that are disabled).
    pub(crate) fn clear_disabled_cooldown_states(&self) -> bool {
        let now = self.now();
        let cfg = self.cfg();
        let mut st = self.state.write();
        let mut changed = false;
        for auth in st.auths.values_mut() {
            if !self.cooldown_disabled_for_auth_cfg(auth, &cfg) && !is_disabled(auth) {
                continue;
            }
            changed |= clear_cooldown_state_for_auth(auth, now);
        }
        changed
    }
}

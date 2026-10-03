//! A Home-dispatched credential selection (Go: `home_selection.go`): the auth Home handed out,
//! the executor serving it, and the execution scope whose end releases the credential's
//! concurrency slot at Home. Resources bound to the selection (attempt cancellation, executor
//! sessions) are closed when it ends.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cpa_auth::Auth;
use cpa_core::registry::ModelInfo;
use cpa_home::conn::Kill;
use cpa_home::executionregistry::{CloseFn, RegistryError, ReleaseTicket, Scope};
use parking_lot::{Mutex, RwLock};

use super::home_concurrency::{recognized_concurrency_suffix, ASCII_WHITESPACE};
use super::models::rewrite_model_for_auth;
use crate::executor::{DynExecutor, ExecutionLifecycle};

pub(crate) const HOME_UPSTREAM_MODEL_ATTRIBUTE: &str = "home_upstream_model";
pub(crate) const HOME_FORCE_MAPPING_ATTRIBUTE: &str = "home_force_mapping";
pub(crate) const HOME_ORIGINAL_ALIAS_ATTRIBUTE: &str = "home_original_alias";

/// Closers run in reverse order of registration when the selection ends.
#[derive(Default)]
pub struct ExecutionResources {
    state: Mutex<(bool, Vec<CloseFn>)>,
}

impl ExecutionResources {
    /// Adds a closer. After the resources closed, the closer runs immediately and the add fails.
    pub fn add(&self, close: CloseFn) -> Result<(), RegistryError> {
        {
            let mut s = self.state.lock();
            if !s.0 {
                s.1.push(close);
                return Ok(());
            }
        }
        if let Err(e) = close() {
            tracing::warn!("Home execution resource close failed: {e}");
        }
        Err(RegistryError::NotAccepting)
    }

    pub fn close(&self) -> Result<(), String> {
        let closers = {
            let mut s = self.state.lock();
            if s.0 {
                return Ok(());
            }
            s.0 = true;
            std::mem::take(&mut s.1)
        };
        let mut errors = Vec::new();
        for close in closers.into_iter().rev() {
            if let Err(e) = close() {
                errors.push(e);
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }
}

#[derive(Default)]
struct CancelState {
    closed: bool,
    next: u64,
    cancels: HashMap<u64, Arc<Kill>>,
}

/// Cancellation handles of the attempts running on one selection.
#[derive(Default)]
pub struct AttemptCancels {
    state: Mutex<CancelState>,
}

impl AttemptCancels {
    fn add(&self, kill: Arc<Kill>) -> Result<u64, RegistryError> {
        let mut s = self.state.lock();
        if s.closed {
            kill.kill();
            return Err(RegistryError::NotAccepting);
        }
        s.next += 1;
        let token = s.next;
        s.cancels.insert(token, kill);
        Ok(token)
    }

    fn remove(&self, token: u64) -> Option<Arc<Kill>> {
        self.state.lock().cancels.remove(&token)
    }

    /// Cancels every running attempt and rejects new ones.
    pub fn close(&self) {
        let cancels = {
            let mut s = self.state.lock();
            if s.closed {
                return;
            }
            s.closed = true;
            std::mem::take(&mut s.cancels)
        };
        for kill in cancels.into_values() {
            kill.kill();
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.state.lock().cancels.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One attempt's cancellation scope: cancelled when the selection ends or on [`release`].
///
/// [`release`]: AttemptGuard::release
pub struct AttemptGuard {
    cancels: Arc<AttemptCancels>,
    token: u64,
    kill: Arc<Kill>,
    released: AtomicBool,
}

impl AttemptGuard {
    /// The cancellation switch executor calls must observe.
    pub fn cancel(&self) -> Arc<Kill> {
        self.kill.clone()
    }

    /// Detaches and cancels the attempt (idempotent).
    pub fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        self.cancels.remove(self.token);
        self.kill.kill();
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Fields Home or the conductor fill in after the scope exists.
#[derive(Default)]
struct Meta {
    model_info: Option<ModelInfo>,
    configuration_update_support: Option<bool>,
    accounted_model: String,
    request_retry: Option<i64>,
    canonical_session_id: String,
    parent_session_id: String,
}

struct SelInner {
    auth: RwLock<Option<Auth>>,
    executor: DynExecutor,
    provider: String,
    scope: Scope,
    resources: Arc<ExecutionResources>,
    attempt_cancels: Arc<AttemptCancels>,
    meta: Mutex<Meta>,
    retained: AtomicBool,
    runtime_auth_bound: AtomicBool,
    ended: AtomicBool,
    id: u64,
}

impl Drop for SelInner {
    fn drop(&mut self) {
        // A selection dropped without an explicit end (cancelled request) still releases Home.
        if !self.ended.swap(true, Ordering::SeqCst) {
            self.scope.end("dropped");
        }
    }
}

static NEXT_SELECTION_ID: AtomicU64 = AtomicU64::new(1);

/// Keeps a Home execution scope separate from its auth. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct HomeDispatchSelection {
    inner: Arc<SelInner>,
}

impl HomeDispatchSelection {
    pub(crate) fn new(
        auth: Auth,
        executor: DynExecutor,
        provider: &str,
        scope: Scope,
    ) -> Result<Self, RegistryError> {
        let resources = Arc::new(ExecutionResources::default());
        let attempt_cancels = Arc::new(AttemptCancels::default());
        let ac = attempt_cancels.clone();
        if let Err(e) = resources.add(Box::new(move || {
            ac.close();
            Ok(())
        })) {
            attempt_cancels.close();
            scope.end("attempt_cancel_bind_failed");
            return Err(e);
        }
        let res = resources.clone();
        if let Err(e) = scope.bind(Box::new(move || res.close())) {
            let _ = resources.close();
            scope.end("resource_controller_bind_failed");
            return Err(e);
        }
        Ok(HomeDispatchSelection {
            inner: Arc::new(SelInner {
                auth: RwLock::new(Some(auth)),
                executor,
                provider: provider.trim().to_string(),
                scope,
                resources,
                attempt_cancels,
                meta: Mutex::new(Meta::default()),
                retained: AtomicBool::new(false),
                runtime_auth_bound: AtomicBool::new(false),
                ended: AtomicBool::new(false),
                id: NEXT_SELECTION_ID.fetch_add(1, Ordering::SeqCst),
            }),
        })
    }

    pub fn executor(&self) -> DynExecutor {
        self.inner.executor.clone()
    }

    pub fn provider(&self) -> &str {
        &self.inner.provider
    }

    /// Process-unique id, to tell selections apart (Go compares pointers).
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    pub fn same(&self, other: &HomeDispatchSelection) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn set_model_info(&self, info: Option<ModelInfo>, support: Option<bool>) {
        let mut m = self.inner.meta.lock();
        m.model_info = info;
        m.configuration_update_support = support;
    }

    pub fn model_info(&self) -> (Option<ModelInfo>, Option<bool>) {
        let m = self.inner.meta.lock();
        (m.model_info.clone(), m.configuration_update_support)
    }

    pub fn set_request_retry(&self, retry: i64) {
        self.inner.meta.lock().request_retry = Some(retry);
    }

    pub fn request_retry(&self) -> Option<i64> {
        self.inner.meta.lock().request_retry
    }

    pub fn set_accounted_model(&self, model: &str) {
        self.inner.meta.lock().accounted_model = model.to_string();
    }

    pub fn accounted_model(&self) -> String {
        self.inner.meta.lock().accounted_model.clone()
    }

    pub fn set_sessions(&self, canonical: &str, parent: &str) {
        let mut m = self.inner.meta.lock();
        m.canonical_session_id = canonical.to_string();
        m.parent_session_id = parent.to_string();
    }

    pub fn canonical_session_id(&self) -> String {
        self.inner.meta.lock().canonical_session_id.clone()
    }

    pub fn parent_session_id(&self) -> String {
        self.inner.meta.lock().parent_session_id.clone()
    }

    /// Runtime-auth binding for websocket sessions (Go: `runtimeAuthBound` CAS).
    pub(crate) fn mark_runtime_auth_bound(&self) -> bool {
        self.inner
            .runtime_auth_bound
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub(crate) fn unmark_runtime_auth_bound(&self) {
        self.inner.runtime_auth_bound.store(false, Ordering::SeqCst);
    }

    /// Adds a resource closed when this selection ends or drains.
    pub fn bind(&self, close: CloseFn) -> Result<(), RegistryError> {
        self.inner.resources.add(close)
    }

    /// Creates a selection-owned cancellation scope for one execution attempt.
    pub fn attempt_context(&self) -> Result<AttemptGuard, RegistryError> {
        let kill = Arc::new(Kill::default());
        let token = self.inner.attempt_cancels.add(kill.clone())?;
        Ok(AttemptGuard {
            cancels: self.inner.attempt_cancels.clone(),
            token,
            kill,
            released: AtomicBool::new(false),
        })
    }

    /// Transfers selection ownership from a request to an execution session.
    pub fn retain(&self) {
        if !self.inner.ended.load(Ordering::SeqCst) {
            self.inner.retained.store(true, Ordering::SeqCst);
        }
    }

    pub fn retained(&self) -> bool {
        self.inner.retained.load(Ordering::SeqCst) && !self.inner.ended.load(Ordering::SeqCst)
    }

    pub fn active(&self) -> bool {
        !self.inner.ended.load(Ordering::SeqCst)
    }

    /// Closes all bound resources and releases the Home execution scope once.
    pub fn end(&self, reason: &str) {
        let _ = self.end_with_release(reason);
    }

    /// Like [`HomeDispatchSelection::end`], returning the release acknowledgement ticket.
    pub fn end_with_release(&self, reason: &str) -> Option<ReleaseTicket> {
        self.inner.ended.store(true, Ordering::SeqCst);
        self.inner.scope.end_with_release(reason.trim())
    }

    /// Updates the selection after Home returned refreshed credentials, keeping its routing
    /// attributes.
    pub fn replace_auth(&self, auth: &Auth) {
        let mut updated = auth.clone();
        let mut guard = self.inner.auth.write();
        if let Some(previous) = guard.as_ref() {
            preserve_home_routing_attributes(&mut updated, previous);
        }
        *guard = Some(updated);
    }

    /// A standalone auth copy.
    pub fn clone_auth(&self) -> Option<Auth> {
        self.inner.auth.read().clone()
    }

    /// An auth copy adapted for a retained canonical route.
    pub fn clone_auth_for_route(&self, route_model: &str) -> Option<Auth> {
        let auth = self.clone_auth()?;
        if !self.retained() {
            return Some(auth);
        }
        Some(clone_retained_home_auth_for_route(auth, route_model))
    }
}

impl std::fmt::Debug for HomeDispatchSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HomeDispatchSelection")
            .field("id", &self.inner.id)
            .field("provider", &self.inner.provider)
            .finish_non_exhaustive()
    }
}

impl ExecutionLifecycle for HomeDispatchSelection {
    fn bind(&self, close: CloseFn) -> Result<(), String> {
        HomeDispatchSelection::bind(self, close).map_err(|e| e.to_string())
    }

    fn retain(&self) {
        HomeDispatchSelection::retain(self);
    }
}

fn preserve_home_routing_attributes(updated: &mut Auth, previous: &Auth) {
    for key in [HOME_UPSTREAM_MODEL_ATTRIBUTE, HOME_FORCE_MAPPING_ATTRIBUTE, HOME_ORIGINAL_ALIAS_ATTRIBUTE] {
        let value = previous.attributes.get(key).map(|v| v.trim()).unwrap_or("");
        if !value.is_empty() {
            updated.attributes.insert(key.to_string(), value.to_string());
        }
    }
}

fn clone_retained_home_auth_for_route(mut auth: Auth, route_model: &str) -> Auth {
    let upstream = auth.attr(HOME_UPSTREAM_MODEL_ATTRIBUTE);
    if upstream.is_empty() {
        return auth;
    }
    let (upstream_base, _) = split_recognized_reasoning_suffix(&upstream);
    let (_, route_suffix) = split_recognized_reasoning_suffix(route_model);
    auth.attributes
        .insert(HOME_UPSTREAM_MODEL_ATTRIBUTE.into(), format!("{upstream_base}{route_suffix}"));
    if auth.attr(HOME_FORCE_MAPPING_ATTRIBUTE).eq_ignore_ascii_case("true") {
        let alias = rewrite_model_for_auth(route_model, &auth).trim().to_string();
        auth.attributes.insert(HOME_ORIGINAL_ALIAS_ATTRIBUTE.into(), alias);
    }
    auth
}

/// `(base, "(suffix)")` for a recognized reasoning suffix, else `(model, "")`.
fn split_recognized_reasoning_suffix(model: &str) -> (String, String) {
    let model = model.trim_matches(|c| ASCII_WHITESPACE.contains(c));
    if !model.ends_with(')') {
        return (model.to_string(), String::new());
    }
    let Some(open) = model.rfind('(') else {
        return (model.to_string(), String::new());
    };
    if !recognized_concurrency_suffix(&model[open + 1..model.len() - 1]) {
        return (model.to_string(), String::new());
    }
    let base = model[..open].trim_matches(|c| ASCII_WHITESPACE.contains(c));
    if base.is_empty() {
        return (model.to_string(), String::new());
    }
    (base.to_string(), model[open..].to_string())
}

//! Asking Home for a credential (Go: `pickHomeDispatchSelection` in `conductor_home.go`).
//!
//! A dispatch reserves a slot in the lifetime's execution registry, `RPOP`s Home, validates the
//! response (concurrency tuple, auth identity, executor) and turns it into a
//! [`HomeDispatchSelection`] whose end releases the credential at Home. The network exchange and
//! everything after it run in a detached task, so a request cancelled mid-dispatch still ends
//! its scope (the selection's drop releases Home).

use std::collections::HashSet;
use std::time::SystemTime;

use cpa_auth::Auth;
use cpa_core::registry::ModelInfo;
use cpa_home::executionregistry::{Scope, ScopeSpec};
use cpa_home::{DispatchParams, HomeError};
use serde::Deserialize;
use serde_json::Value;

use super::Manager;
use super::errors::{auth_error, auth_not_found};
use super::home::{
    HomeDispatchBundle, downstream_websocket, home_auth_count_from_metadata, home_excluded_auth_ids_from_metadata,
    home_execution_session_id, home_retry_round_from_metadata, requested_model_from_metadata, trimmed_meta,
};
use super::home_concurrency::{
    canonical_dispatch_model, decode_concurrency_envelope, decode_dispatch_error, home_unavailable,
    install_concurrency_scope, install_error, invalid_home_concurrency_response, valid_canonical_concurrency_model_key,
    verify_accounted_identity,
};
use super::home_selection::{
    HOME_FORCE_MAPPING_ATTRIBUTE, HOME_ORIGINAL_ALIAS_ATTRIBUTE, HOME_UPSTREAM_MODEL_ATTRIBUTE, HomeDispatchSelection,
};
use super::models::executor_key_from_auth;
use super::pick::pinned_auth_id;
use crate::executor::{ExecError, Options, meta};

#[derive(Deserialize, Default)]
struct DispatchResponse {
    #[serde(default)]
    model: String,
    #[serde(default)]
    auth_index: String,
    #[serde(default)]
    user_api_key: String,
    #[serde(default)]
    request_retry: Option<i64>,
    #[serde(default)]
    force_mapping: bool,
    #[serde(default)]
    original_alias: String,
    #[serde(default)]
    model_info: Option<Value>,
    #[serde(default)]
    auth: Option<Value>,
}

/// The selection plus the downstream user API key Home attached to it.
pub(crate) struct Dispatched {
    pub selection: HomeDispatchSelection,
    pub user_api_key: String,
}

/// `X-Goog-Api-Key` carrying a query-string credential (`?key=` / `?auth_token=`) when the
/// request has no header credential of its own (Go: `homeDispatchHeaders`).
fn home_dispatch_headers(opts: &Options) -> http::HeaderMap {
    let mut out = opts.headers.clone();
    let query_key = opts
        .query
        .iter()
        .filter(|(k, _)| k == "key")
        .chain(opts.query.iter().filter(|(k, _)| k == "auth_token"))
        .map(|(_, v)| v.trim())
        .find(|v| !v.is_empty());
    let Some(api_key) = query_key else { return out };
    if out.contains_key(http::header::AUTHORIZATION) || out.contains_key("x-goog-api-key") || out.contains_key("x-api-key") {
        return out;
    }
    if let Ok(v) = http::HeaderValue::from_str(api_key) {
        out.insert("x-goog-api-key", v);
    }
    out
}

impl Manager {
    /// Go: `pickHomeDispatchSelection`. `credential_policy` is the fixed policy to dispatch
    /// under (empty for none).
    pub(crate) async fn pick_home_dispatch_selection(
        &self,
        model: &str,
        opts: &mut Options,
        credential_policy: &str,
    ) -> Result<HomeDispatchSelection, ExecError> {
        let mut requested_model = model.trim().to_string();
        if requested_model.is_empty() {
            requested_model = requested_model_from_metadata(&opts.metadata, model);
        }
        let pinned = pinned_auth_id(&opts.metadata);
        let retry_round = home_retry_round_from_metadata(&opts.metadata);
        let excluded_list = home_excluded_auth_ids_from_metadata(&opts.metadata);
        let excluded: HashSet<String> = excluded_list.iter().cloned().collect();
        if let Some(retained) = self.retained_home_session_selection(opts, &requested_model, &excluded).await? {
            return Ok(retained);
        }
        let execution_session = home_execution_session_id(&opts.metadata);
        if !execution_session.is_empty() && !pinned.is_empty() {
            self.end_mismatched_home_session_selections(&execution_session, &pinned, &requested_model, true).await?;
        }

        let Some(bundle) = self.home_dispatch_bundle() else {
            return Err(auth_error("home_unavailable", "home dispatch bundle unavailable", 503));
        };
        if !bundle.client.heartbeat_ok() {
            return Err(auth_error("home_unavailable", "home control center unavailable", 503));
        }
        if !pinned.is_empty() && excluded.contains(&pinned) {
            return Err(auth_error("auth_not_found", "pinned auth is unavailable in the current retry round", 503));
        }
        let pending = bundle
            .registry
            .begin_dispatch()
            .map_err(|_| home_unavailable("home execution registry unavailable", true))?;

        if !opts.metadata.contains_key(meta::SESSION_AFFINITY_MODEL) && !requested_model.is_empty() {
            opts.metadata.insert(meta::SESSION_AFFINITY_MODEL.into(), Value::String(requested_model.clone()));
        }
        let (session_id, parent_session_id) = self.home_dispatch_session_ids(opts);
        if !session_id.is_empty() {
            opts.metadata.insert(meta::CANONICAL_SESSION_ID.into(), Value::String(session_id.clone()));
            if parent_session_id.is_empty() {
                opts.metadata.remove(meta::PARENT_SESSION_ID);
            } else {
                opts.metadata.insert(meta::PARENT_SESSION_ID.into(), Value::String(parent_session_id.clone()));
            }
        }
        let mut headers = home_dispatch_headers(opts);
        let node_kind = trimmed_meta(&opts.metadata, "node_kind");
        if !node_kind.is_empty()
            && let Ok(v) = http::HeaderValue::from_str(&node_kind)
        {
            headers.insert("x-node-kind", v);
        }
        let task = DispatchTask {
            manager: self.clone(),
            bundle,
            requested_model,
            pinned,
            retry_round,
            excluded: excluded_list,
            session_id,
            parent_session_id,
            headers,
            count: home_auth_count_from_metadata(&opts.metadata),
            credential_policy: credential_policy.trim().to_string(),
            kind: if downstream_websocket(&opts.metadata) {
                "websocket"
            } else if opts.stream {
                "stream"
            } else {
                "http"
            },
            request_id: trimmed_meta(&opts.metadata, super::usage::META_REQUEST_ID),
            execution_session: home_execution_session_id(&opts.metadata),
            downstream_ws: downstream_websocket(&opts.metadata),
            pending,
        };
        let dispatched = tokio::spawn(task.run())
            .await
            .map_err(|e| home_unavailable(&format!("home dispatch task failed: {e}"), true))??;
        if !dispatched.user_api_key.is_empty() {
            opts.metadata.insert(super::usage::META_CLIENT_API_KEY.into(), Value::String(dispatched.user_api_key));
        }
        Ok(dispatched.selection)
    }
}

struct DispatchTask {
    manager: Manager,
    bundle: std::sync::Arc<HomeDispatchBundle>,
    requested_model: String,
    pinned: String,
    retry_round: i64,
    excluded: Vec<String>,
    session_id: String,
    parent_session_id: String,
    headers: http::HeaderMap,
    count: i64,
    credential_policy: String,
    kind: &'static str,
    request_id: String,
    execution_session: String,
    downstream_ws: bool,
    pending: cpa_home::executionregistry::PendingDispatch,
}

impl DispatchTask {
    async fn run(self) -> Result<Dispatched, ExecError> {
        let client = self.bundle.client.clone();
        let registry = self.bundle.registry.clone();
        let params = DispatchParams {
            model: &self.requested_model,
            session_id: &self.session_id,
            parent_session_id: &self.parent_session_id,
            headers: self.headers.clone(),
            count: self.count,
            credential_policy: &self.credential_policy,
            retry_round: Some(self.retry_round),
            excluded_auth_ids: (!self.excluded.is_empty()).then(|| self.excluded.clone()),
            pinned_auth_id: &self.pinned,
        };
        let raw = match client.rpop_auth(&params).await {
            Ok(raw) => raw,
            Err(e) => {
                if e.is_ambiguous_dispatch() {
                    client.abort_ambiguous_dispatch();
                }
                self.pending.end();
                return Err(match e {
                    HomeError::AuthNotFound => auth_error("auth_not_found", &e.to_string(), 503),
                    e => home_unavailable(&e.to_string(), true),
                });
            }
        };

        let (envelope, envelope_err) = decode_concurrency_envelope(&raw);
        if envelope_err.is_err() {
            if envelope.present {
                client.abort_ambiguous_dispatch();
            }
            self.pending.end();
            return Err(if envelope.present {
                invalid_home_concurrency_response("Home returned malformed concurrency tuple")
            } else {
                auth_error("invalid_auth", "home returned invalid auth payload", 502)
            });
        }

        let base_scope = ScopeSpec {
            request_id: self.request_id.clone(),
            model: self.requested_model.clone(),
            kind: self.kind.to_string(),
            started_at: Some(SystemTime::now()),
            ..Default::default()
        };
        let mut scope: Option<Scope> = None;
        if envelope.present {
            match install_concurrency_scope(&registry, &self.pending, &envelope.tuple, base_scope.clone()) {
                Ok(s) => scope = Some(s),
                Err(e) => {
                    client.abort_ambiguous_dispatch();
                    self.pending.end();
                    return Err(e);
                }
            }
        }
        let end_scope = |scope: &Option<Scope>| match scope {
            Some(s) => s.end("local_validation_failed"),
            None => self.pending.end(),
        };

        if let Some(err) = decode_dispatch_error(&raw) {
            if envelope.present {
                client.abort_ambiguous_dispatch();
                end_scope(&scope);
                return Err(invalid_home_concurrency_response("Home returned both accounted concurrency and an error"));
            }
            self.pending.end();
            return Err(err);
        }

        let invalid_payload = || auth_error("invalid_auth", "home returned invalid auth payload", 502);
        let dispatch: DispatchResponse = match serde_json::from_slice(&raw) {
            Ok(d) => d,
            Err(_) => {
                end_scope(&scope);
                return Err(invalid_payload());
            }
        };
        let mut auth: Auth = match dispatch.auth.as_ref().and_then(|v| serde_json::from_value::<Auth>(v.clone()).ok()) {
            Some(a) if !a.id.trim().is_empty() => a,
            // Backward compatibility: older Home instances returned the auth directly.
            _ => match serde_json::from_slice::<Auth>(&raw) {
                Ok(a) => a,
                Err(_) => {
                    end_scope(&scope);
                    return Err(invalid_payload());
                }
            },
        };
        let observed_model = canonical_dispatch_model(&dispatch.model, &self.requested_model);
        if envelope.present {
            let (observed_key, valid) = valid_canonical_concurrency_model_key(&observed_model);
            if !valid || envelope.tuple.model != observed_key {
                client.abort_ambiguous_dispatch();
                end_scope(&scope);
                return Err(invalid_home_concurrency_response("Home concurrency model does not match dispatched model"));
            }
        }
        let mut base_scope = base_scope;
        if !envelope.present {
            base_scope.model = observed_model;
        }

        let upstream_model = dispatch.model.trim();
        if !upstream_model.is_empty() {
            auth.attributes.insert(HOME_UPSTREAM_MODEL_ATTRIBUTE.into(), upstream_model.to_string());
        }
        let original_alias = dispatch.original_alias.trim();
        if dispatch.force_mapping && !original_alias.is_empty() {
            auth.attributes.insert(HOME_FORCE_MAPPING_ATTRIBUTE.into(), "true".into());
            auth.attributes.insert(HOME_ORIGINAL_ALIAS_ATTRIBUTE.into(), original_alias.to_string());
        }
        if auth.id.trim().is_empty() {
            end_scope(&scope);
            return Err(auth_error("invalid_auth", "home returned auth without id", 502));
        }
        if !self.pinned.is_empty() && auth.id.trim() != self.pinned {
            end_scope(&scope);
            return Err(auth_error(
                "auth_not_found",
                "home returned an auth that does not match the pinned credential",
                503,
            ));
        }
        if let Err(e) = verify_accounted_identity(&envelope.tuple, &auth, &dispatch.auth_index) {
            end_scope(&scope);
            return Err(e);
        }
        let logical_provider = auth.provider.trim().to_lowercase();
        let executor_key = executor_key_from_auth(&auth);
        if logical_provider.is_empty() || executor_key.is_empty() {
            end_scope(&scope);
            return Err(auth_error("invalid_auth", "home returned auth without provider", 502));
        }
        let home_auth_index = dispatch.auth_index.trim();
        if home_auth_index.is_empty() {
            auth.ensure_index();
        } else {
            auth.index = home_auth_index.to_string();
        }

        let executor = self.manager.executor(&executor_key).or_else(|| {
            (!auth.attr("base_url").is_empty()).then(|| self.manager.executor("openai-compatibility")).flatten()
        });
        let Some(executor) = executor else {
            end_scope(&scope);
            return Err(auth_error("executor_not_found", "executor not registered", 502));
        };
        if scope.is_none() {
            let spec = ScopeSpec {
                request_id: base_scope.request_id.clone(),
                credential_id: auth.id.trim().to_string(),
                model: base_scope.model.clone(),
                kind: base_scope.kind.clone(),
                started_at: base_scope.started_at,
                accounted: false,
            };
            match install_concurrency_scope(&registry, &self.pending, &Default::default(), spec) {
                Ok(s) => scope = Some(s),
                Err(e) => {
                    client.abort_ambiguous_dispatch();
                    self.pending.end();
                    return Err(e);
                }
            }
        }
        let Some(scope) = scope else {
            return Err(install_error(cpa_home::executionregistry::RegistryError::NotAccepting));
        };

        let selection = match HomeDispatchSelection::new(auth.clone(), executor, &logical_provider, scope) {
            Ok(s) => s,
            Err(_) => return Err(home_unavailable("home execution registry unavailable", true)),
        };
        let mut support = None;
        let mut info = None;
        if let Some(raw_info) = &dispatch.model_info {
            support = raw_info.get("support_configuration_update").and_then(Value::as_bool);
            if let Ok(mut mi) = serde_json::from_value::<ModelInfo>(raw_info.clone())
                && !mi.id.trim().is_empty()
            {
                mi.id = mi.id.trim().to_string();
                mi.support_configuration_update = support.unwrap_or(false);
                info = Some(mi);
            }
        }
        selection.set_model_info(info, support);
        if self.pinned.is_empty()
            && let Some(retry) = dispatch.request_retry
            && retry >= 0
        {
            selection.set_request_retry(retry);
        }
        if envelope.present {
            selection.set_accounted_model(&envelope.tuple.model);
        }
        if !self.execution_session.is_empty() && self.downstream_ws {
            let aid = auth.id.trim().to_string();
            if let Err(e) = self
                .manager
                .end_mismatched_home_session_selections(&self.execution_session, &aid, &self.requested_model, true)
                .await
            {
                selection.end("target_change_release_failed");
                return Err(e);
            }
        }
        selection.set_sessions(&self.session_id, &self.parent_session_id);
        Ok(Dispatched { selection, user_api_key: dispatch.user_api_key.trim().to_string() })
    }
}

impl Manager {
    /// Go: `SelectHomeAuthWithCredentialPolicy`: a policy-constrained Home dispatch whose
    /// execution scope the caller keeps (end it when done).
    pub async fn select_home_auth_with_credential_policy(
        &self,
        provider: &str,
        model: &str,
        policy: &str,
        opts: Options,
    ) -> Result<HomeDispatchSelection, ExecError> {
        let policy = super::pick::normalize_credential_policy(policy);
        if policy.is_empty() {
            return Err(auth_error("invalid_credential_policy", "credential policy is invalid", 400));
        }
        if !self.home_enabled() {
            return Err(auth_error("home_unavailable", "home control center unavailable", 503));
        }
        let eligibility = super::pick::Eligibility { credential_policy: policy.clone(), ..Default::default() };
        self.select_home_matching(provider, model, &policy, opts, |auth| eligibility.allows(auth), "credential_policy_mismatch")
            .await
    }

    /// Go: `SelectHomeAuthByKind`.
    pub async fn select_home_auth_by_kind(
        &self,
        provider: &str,
        model: &str,
        required_kind: &str,
        opts: Options,
    ) -> Result<HomeDispatchSelection, ExecError> {
        let required = required_kind.trim().to_lowercase();
        if required != cpa_auth::types::AUTH_KIND_API_KEY && required != cpa_auth::types::AUTH_KIND_OAUTH {
            return Err(auth_error("invalid_auth_kind", "required auth kind is invalid", 400));
        }
        if !self.home_enabled() {
            return Err(auth_error("home_unavailable", "home control center unavailable", 503));
        }
        self.select_home_matching(provider, model, "", opts, |auth| auth.auth_kind() == required, "auth_kind_mismatch").await
    }

    /// Dispatches until Home returns an auth matching `provider` and `matches`, ending (and
    /// excluding) each mismatching selection.
    async fn select_home_matching(
        &self,
        provider: &str,
        model: &str,
        policy: &str,
        mut opts: Options,
        matches: impl Fn(&Auth) -> bool,
        mismatch_reason: &str,
    ) -> Result<HomeDispatchSelection, ExecError> {
        let mut home_auth_count = home_auth_count_from_metadata(&opts.metadata);
        let mut tried: HashSet<String> = HashSet::new();
        loop {
            let mut selection_opts = super::home::with_home_auth_count(opts.clone(), home_auth_count);
            selection_opts = super::home::with_home_excluded_auth_ids(selection_opts, &tried);
            let selection = self.pick_home_dispatch_selection(model, &mut selection_opts, policy).await?;
            opts.metadata = selection_opts.metadata;
            let provider_matches = provider.trim().is_empty() || selection.provider().eq_ignore_ascii_case(provider.trim());
            let auth = selection.clone_auth();
            if provider_matches && auth.as_ref().is_some_and(&matches) {
                return Ok(selection);
            }
            let auth_id = auth.map(|a| a.id.trim().to_string()).unwrap_or_default();
            let reason = if provider_matches { mismatch_reason } else { "provider_mismatch" };
            self.end_home_selection_before_redispatch(&selection, reason).await?;
            if auth_id.is_empty() {
                return Err(auth_not_found("selected auth has no ID"));
            }
            if !tried.insert(auth_id) {
                return Err(auth_not_found("selector repeatedly returned an ineligible auth"));
            }
            home_auth_count += 1;
        }
    }

    /// Go: `RefreshHomeSelectionAfterUnauthorized`: only reuses a newer snapshot already
    /// installed by Home; never refreshes or mutates Home-owned credentials.
    pub fn refresh_home_selection_after_unauthorized(
        &self,
        selection: &HomeDispatchSelection,
        failed_auth: Option<&Auth>,
    ) -> (Option<Auth>, bool) {
        let current = selection.clone_auth();
        let failed = failed_auth.cloned().or_else(|| current.clone());
        if let (Some(cur), Some(failed)) = (&current, &failed)
            && cur.id == failed.id
        {
            let (a, b) = (cur.access_token(), failed.access_token());
            if !a.is_empty() && !b.is_empty() && a != b {
                return (current, true);
            }
        }
        (current, false)
    }
}

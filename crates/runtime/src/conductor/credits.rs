//! Antigravity AI-credits last-resort fallback (Go: antigravity_credits.go and the credits parts
//! of conductor_home.go).
//!
//! When every Antigravity credential is rate limited and `quota-exceeded.antigravity-credits` is
//! on, Claude models are retried once more on credentials that still have AI credits, with
//! [`ANTIGRAVITY_CREDITS_METADATA_KEY`] set in the request metadata so the executor injects the
//! credits payload. Executors report credit availability through the hint store.

use std::time::Instant;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use cpa_auth::Auth;

use super::cooldown::{ExecResult, is_disabled};
use super::errors::{
    CODE_AUTH_NOT_FOUND, CODE_AUTH_UNAVAILABLE, CODE_MODEL_COOLDOWN, result_error_from_error,
};
use super::exec::{
    ensure_requested_model_metadata, publish_selected_auth_metadata, requested_model_alias,
};
use super::models::{
    executor_key_from_auth, resolve_attempt_alias_result, rewrite_model_in_response,
};
use super::pick::pinned_auth_id;
use super::usage::{UsageFacts, tokens_from_response};
use super::{Manager, executor_locked};
use crate::executor::{DynExecutor, ExecError, Options, Request, Response, StreamResult};

/// Request metadata flag telling the Antigravity executor to inject `enabledCreditTypes`.
pub const ANTIGRAVITY_CREDITS_METADATA_KEY: &str = "antigravity_use_credits";

/// Latest known AI-credits state of one credential.
#[derive(Debug, Clone, Default)]
pub struct AntigravityCreditsHint {
    pub known: bool,
    pub available: bool,
    pub credit_amount: f64,
    pub min_credit_amount: f64,
    pub paid_tier_id: String,
    pub updated_at: Option<DateTime<Utc>>,
}

struct CreditsCandidate {
    auth: Auth,
    executor: DynExecutor,
    provider: String,
}

static HINTS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, AntigravityCreditsHint>>,
> = std::sync::LazyLock::new(Default::default);

/// Records the latest known AI-credits state of a credential. Process-wide (Go: a global sync.Map)
/// so executors can report it without a manager handle.
pub fn set_antigravity_credits_hint(auth_id: &str, mut hint: AntigravityCreditsHint) {
    let id = auth_id.trim();
    if id.is_empty() {
        return;
    }
    if hint.updated_at.is_none() {
        hint.updated_at = Some(Utc::now());
    }
    HINTS.lock().insert(id.to_string(), hint);
}

pub fn antigravity_credits_hint(auth_id: &str) -> Option<AntigravityCreditsHint> {
    HINTS.lock().get(auth_id.trim()).cloned()
}

pub fn has_known_antigravity_credits_hint(auth_id: &str) -> bool {
    antigravity_credits_hint(auth_id).is_some_and(|h| h.known)
}

impl Manager {
    pub(crate) fn should_attempt_antigravity_credits_fallback(
        &self,
        last_err: &ExecError,
        providers: &[String],
    ) -> bool {
        if !providers
            .iter()
            .any(|p| p.trim().eq_ignore_ascii_case("antigravity"))
        {
            return false;
        }
        if !self.cfg().quota_exceeded.antigravity_credits {
            return false;
        }
        match last_err.status {
            429 | 503 => true,
            0 => matches!(
                last_err.auth_code.as_deref(),
                Some(CODE_AUTH_NOT_FOUND) | Some(CODE_AUTH_UNAVAILABLE) | Some(CODE_MODEL_COOLDOWN)
            ),
            _ => false,
        }
    }

    /// Antigravity credentials eligible for the credits fallback: known-with-credits first, then
    /// unknown, each sorted by id (Go: findAllAntigravityCreditsCandidateAuths).
    fn credits_candidates(&self, route_model: &str, opts: &Options) -> Vec<CreditsCandidate> {
        if !route_model.trim().to_lowercase().contains("claude") {
            return Vec::new();
        }
        let pinned = pinned_auth_id(&opts.metadata);
        let st = self.state.read();
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        let hints = HINTS.lock();
        for auth in st.auths.values() {
            if is_disabled(auth) || !auth.provider.trim().eq_ignore_ascii_case("antigravity") {
                continue;
            }
            if !pinned.is_empty() && auth.id != pinned {
                continue;
            }
            let key = executor_key_from_auth(auth);
            let Some(executor) = executor_locked(&st, &key) else {
                continue;
            };
            let cand = CreditsCandidate {
                auth: auth.clone(),
                executor,
                provider: key,
            };
            match hints.get(&auth.id) {
                Some(h) if h.known => {
                    if h.available {
                        known.push(cand);
                    }
                }
                _ => unknown.push(cand),
            }
        }
        known.sort_by(|a, b| a.auth.id.cmp(&b.auth.id));
        unknown.sort_by(|a, b| a.auth.id.cmp(&b.auth.id));
        known.extend(unknown);
        known
    }

    pub(crate) async fn try_antigravity_credits_execute(
        &self,
        req: &Request,
        opts: &Options,
    ) -> Result<Option<Response>, ExecError> {
        let route_model = req.model.clone();
        for mut c in self.credits_candidates(&route_model, opts) {
            let mut credits_opts = ensure_requested_model_metadata(opts.clone(), &route_model);
            credits_opts.metadata.insert(
                ANTIGRAVITY_CREDITS_METADATA_KEY.into(),
                serde_json::Value::Bool(true),
            );
            match self.prepare_request_auth(&c.executor, &c.auth).await {
                Ok(prepared) => c.auth = prepared,
                Err(_) => continue,
            }
            publish_selected_auth_metadata(&mut credits_opts, &c.auth);
            let (models, pooled, alias) =
                self.execution_model_candidates_with_alias(&c.auth, &route_model);
            for upstream_model in &models {
                let result_model =
                    self.state_model_for_execution(&c.auth, &route_model, upstream_model, pooled);
                let mut exec_req = req.clone();
                exec_req.model = upstream_model.clone();
                let started = Instant::now();
                let res = c
                    .executor
                    .execute(&c.auth, exec_req, credits_opts.clone())
                    .await;
                let mut result = ExecResult {
                    auth_id: c.auth.id.clone(),
                    provider: c.provider.clone(),
                    model: result_model,
                    route_model: route_model.clone(),
                    success: res.is_ok(),
                    retry_after: None,
                    credential_scope: false,
                    error: None,
                    options: credits_opts.clone(),
                    skip_quota_observation: false,
                    response_headers: Default::default(),
                };
                let mut facts = UsageFacts {
                    latency: started.elapsed(),
                    upstream_model: upstream_model.clone(),
                    requested_model: requested_model_alias(&credits_opts, &route_model),
                    ..Default::default()
                };
                match res {
                    Err(err) => {
                        result.error = Some(result_error_from_error(&err));
                        result.retry_after = err.retry_after;
                        result.credential_scope = err.credential_scoped;
                        result.response_headers = err.recorded_headers();
                        let scoped = result.credential_scope;
                        self.mark_result_inner(result, Some(facts));
                        if scoped {
                            break;
                        }
                    }
                    Ok(mut resp) => {
                        facts.tokens = tokens_from_response(
                            credits_opts.response_format_or_source(),
                            &resp.payload,
                            &resp.metadata,
                        );
                        result.response_headers = resp.headers.clone();
                        self.mark_result_inner(result, Some(facts));
                        let attempt = resolve_attempt_alias_result(
                            &self.cfg(),
                            &c.auth,
                            &route_model,
                            upstream_model,
                            &alias,
                        );
                        if attempt.force_mapping && !attempt.original_alias.trim().is_empty() {
                            resp.payload = Bytes::from(rewrite_model_in_response(
                                &resp.payload,
                                attempt.original_alias.trim(),
                            ));
                        }
                        return Ok(Some(resp));
                    }
                }
            }
        }
        Ok(None)
    }

    pub(crate) async fn try_antigravity_credits_execute_stream(
        &self,
        req: &Request,
        opts: &Options,
    ) -> Result<Option<StreamResult>, ExecError> {
        let route_model = req.model.clone();
        for mut c in self.credits_candidates(&route_model, opts) {
            let mut credits_opts = ensure_requested_model_metadata(opts.clone(), &route_model);
            credits_opts.metadata.insert(
                ANTIGRAVITY_CREDITS_METADATA_KEY.into(),
                serde_json::Value::Bool(true),
            );
            match self.prepare_request_auth(&c.executor, &c.auth).await {
                Ok(prepared) => c.auth = prepared,
                Err(_) => continue,
            }
            publish_selected_auth_metadata(&mut credits_opts, &c.auth);
            let (models, pooled, alias) =
                self.execution_model_candidates_with_alias(&c.auth, &route_model);
            if models.is_empty() {
                continue;
            }
            if let Ok(stream) = self
                .stream_with_model_pool(
                    &c.executor,
                    c.auth.clone(),
                    &c.provider,
                    req,
                    &credits_opts,
                    &route_model,
                    None,
                    &models,
                    pooled,
                    &alias,
                    None,
                )
                .await
            {
                return Ok(Some(stream));
            }
        }
        Ok(None)
    }
}

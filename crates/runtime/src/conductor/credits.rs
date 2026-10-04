//! Antigravity AI-credits last-resort fallback (Go: antigravity_credits.go and the credits parts
//! of conductor_home.go).
//!
//! When every Antigravity credential is rate limited and `quota-exceeded.antigravity-credits` is
//! on, Claude models are retried once more on credentials that still have AI credits, with
//! [`ANTIGRAVITY_CREDITS_METADATA_KEY`] set in the request metadata so the executor injects the
//! credits payload. Executors report credit availability through the hint store.

use std::future::Future;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use cpa_auth::Auth;
use cpa_home::HomeError;
use cpa_home::kv::{self, Kv};
use serde::{Deserialize, Serialize};

use super::cooldown::{ExecResult, is_disabled};
use super::errors::{
    CODE_AUTH_NOT_FOUND, CODE_AUTH_UNAVAILABLE, CODE_MODEL_COOLDOWN, auth_error,
    result_error_from_error,
};
use super::exec::{
    ensure_requested_model_metadata, publish_selected_auth_metadata, requested_model_alias,
};
use super::models::{
    executor_key_from_auth, resolve_attempt_alias_result, rewrite_model_in_response,
};
use super::pick::pinned_auth_id;
use super::usage::{UsageFacts, tokens_from_response};
use crate::usage_report::UsageCollector;
use super::{Manager, executor_locked};
use crate::executor::{DynExecutor, ExecError, Options, Request, Response, StreamResult};

/// Request metadata flag telling the Antigravity executor to inject `enabledCreditTypes`.
pub const ANTIGRAVITY_CREDITS_METADATA_KEY: &str = "antigravity_use_credits";

/// Latest known AI-credits state of one credential. Serialized with Go's field names so a Home
/// shared with Go nodes reads the same JSON (`updated_at` is a Go `time.Time`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AntigravityCreditsHint {
    #[serde(rename = "Known")]
    pub known: bool,
    #[serde(rename = "Available")]
    pub available: bool,
    #[serde(rename = "CreditAmount")]
    pub credit_amount: f64,
    #[serde(rename = "MinCreditAmount")]
    pub min_credit_amount: f64,
    #[serde(rename = "PaidTierID")]
    pub paid_tier_id: String,
    #[serde(rename = "UpdatedAt", with = "go_time")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Go's `time.Time` JSON: RFC 3339 with trimmed fractional seconds, zero time as `None`.
mod go_time {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    const ZERO: &str = "0001-01-01T00:00:00Z";

    pub fn serialize<S: Serializer>(v: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_str(ZERO),
            Some(t) => {
                let full = t.to_rfc3339_opts(SecondsFormat::Nanos, true);
                // Trim trailing fractional zeros like RFC3339Nano.
                let trimmed = match full.strip_suffix('Z') {
                    Some(body) if body.contains('.') => {
                        format!("{}Z", body.trim_end_matches('0').trim_end_matches('.'))
                    }
                    _ => full,
                };
                s.serialize_str(&trimmed)
            }
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        let Some(raw) = Option::<String>::deserialize(d)? else {
            return Ok(None);
        };
        if raw == ZERO {
            return Ok(None);
        }
        DateTime::parse_from_rfc3339(&raw)
            .map(|t| Some(t.with_timezone(&Utc)))
            .map_err(serde::de::Error::custom)
    }
}

struct CreditsCandidate {
    auth: Auth,
    executor: DynExecutor,
    provider: String,
}

static HINTS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, AntigravityCreditsHint>>,
> = std::sync::LazyLock::new(Default::default);

/// Home KV lifetime of a published hint.
const HOME_HINT_TTL: Duration = Duration::from_secs(30 * 60);

fn home_hint_key(auth_id: &str) -> String {
    format!("cpa:antigravity:credits-hint:{}", auth_id.trim())
}

/// Drives a Home KV operation from synchronous code; `None` when there is no multi-thread
/// runtime to block on, which callers treat as a failed Home call.
fn block_on_home<F: Future>(fut: F) -> Option<F::Output> {
    cpa_home::kv::run_blocking(fut).ok()
}

fn stamped(mut hint: AntigravityCreditsHint) -> AntigravityCreditsHint {
    if hint.updated_at.is_none() {
        hint.updated_at = Some(Utc::now());
    }
    hint
}

fn set_local_hint(id: &str, hint: AntigravityCreditsHint) {
    HINTS.lock().insert(id.to_string(), stamped(hint));
}

/// Records the latest known AI-credits state of a credential (Go: SetAntigravityCreditsHint).
/// Process-wide so executors can report it without a manager handle. In Home mode the hint goes
/// to Home KV for 30 minutes (best effort) instead of the local map; from synchronous code that
/// needs a multi-thread runtime, otherwise the write is dropped.
pub fn set_antigravity_credits_hint(auth_id: &str, hint: AntigravityCreditsHint) {
    let id = auth_id.trim();
    if id.is_empty() {
        return;
    }
    if !cpa_home::kv::is_home_mode() {
        set_local_hint(id, hint);
        return;
    }
    let hint = stamped(hint);
    if block_on_home(kv::kv_set_json_best_effort(
        &home_hint_key(id),
        &hint,
        HOME_HINT_TTL,
    ))
    .is_none()
    {
        tracing::error!(
            "home kv best-effort set failed prefix=cpa:antigravity:*: no multi-thread runtime"
        );
    }
}

/// [`set_antigravity_credits_hint`] for async callers.
pub async fn set_antigravity_credits_hint_async(auth_id: &str, hint: AntigravityCreditsHint) {
    let id = auth_id.trim();
    if id.is_empty() {
        return;
    }
    if !cpa_home::kv::is_home_mode() {
        set_local_hint(id, hint);
        return;
    }
    kv::kv_set_json_best_effort(&home_hint_key(id), &stamped(hint), HOME_HINT_TTL).await;
}

/// Latest known state for request-time paths (Go: GetAntigravityCreditsHintRequired). In Home
/// mode a KV failure is an error; `Ok(None)` is a miss.
pub async fn get_antigravity_credits_hint_required(
    auth_id: &str,
) -> Result<Option<AntigravityCreditsHint>, HomeError> {
    let id = auth_id.trim();
    if id.is_empty() {
        return Ok(None);
    }
    match kv::kv_get_json_required::<AntigravityCreditsHint>(&home_hint_key(id)).await {
        Kv::Home(res) => res,
        Kv::NotHome => Ok(HINTS.lock().get(id).cloned()),
    }
}

/// Latest known state; a Home failure reads as unknown (Go: GetAntigravityCreditsHint). From
/// synchronous code in Home mode this needs a multi-thread runtime, otherwise it reads as unknown.
pub fn antigravity_credits_hint(auth_id: &str) -> Option<AntigravityCreditsHint> {
    if !cpa_home::kv::is_home_mode() {
        return HINTS.lock().get(auth_id.trim()).cloned();
    }
    block_on_home(get_antigravity_credits_hint_required(auth_id))
        .and_then(Result::ok)
        .flatten()
}

/// [`antigravity_credits_hint`] for async callers.
pub async fn antigravity_credits_hint_async(auth_id: &str) -> Option<AntigravityCreditsHint> {
    get_antigravity_credits_hint_required(auth_id)
        .await
        .ok()
        .flatten()
}

pub fn has_known_antigravity_credits_hint(auth_id: &str) -> bool {
    antigravity_credits_hint(auth_id).is_some_and(|h| h.known)
}

/// [`has_known_antigravity_credits_hint`] for async callers.
pub async fn has_known_antigravity_credits_hint_async(auth_id: &str) -> bool {
    antigravity_credits_hint_async(auth_id)
        .await
        .is_some_and(|h| h.known)
}

/// Go: the `home_fallback_unsupported` error of the local credits fallback in Home mode.
fn home_fallback_unsupported() -> ExecError {
    auth_error(
        "home_fallback_unsupported",
        "Home does not support Antigravity credits fallback",
        503,
    )
}

/// Go: antigravityCreditsKVUnavailableError.
fn kv_unavailable_error(cause: &HomeError) -> ExecError {
    auth_error(
        "home_kv_unavailable",
        &format!("home kv store unavailable: {cause}"),
        503,
    )
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
        let cfg = self.cfg();
        // Home dispatches elsewhere; the local credits fallback never runs there.
        if cfg.home.enabled || !cfg.quota_exceeded.antigravity_credits {
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
    /// unknown, each sorted by id (Go: findAllAntigravityCreditsCandidateAuths). Hints are read
    /// with the request-time getter, so an unavailable Home KV store fails the lookup.
    async fn credits_candidates(
        &self,
        route_model: &str,
        opts: &Options,
    ) -> Result<Vec<CreditsCandidate>, ExecError> {
        if self.cfg().home.enabled || !route_model.trim().to_lowercase().contains("claude") {
            return Ok(Vec::new());
        }
        let pinned = pinned_auth_id(&opts.metadata);
        let mut candidates = Vec::new();
        {
            let st = self.state.read();
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
                candidates.push(CreditsCandidate {
                    auth: auth.clone(),
                    executor,
                    provider: key,
                });
            }
        }
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for cand in candidates {
            match get_antigravity_credits_hint_required(&cand.auth.id)
                .await
                .map_err(|e| kv_unavailable_error(&e))?
            {
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
        Ok(known)
    }

    pub(crate) async fn try_antigravity_credits_execute(
        &self,
        req: &Request,
        opts: &Options,
    ) -> Result<Option<Response>, ExecError> {
        if self.cfg().home.enabled {
            return Err(home_fallback_unsupported());
        }
        let route_model = req.model.clone();
        for mut c in self.credits_candidates(&route_model, opts).await? {
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
                let usage = UsageCollector::new();
                let mut credits_opts = credits_opts.clone();
                credits_opts.usage_collector = Some(usage.clone());
                let started = Instant::now();
                credits_opts.api_log.reset_response_headers();
                let res = c
                    .executor
                    .execute(&c.auth, exec_req, credits_opts.clone())
                    .await
                    .map_err(|e| e.with_attempt_headers(&credits_opts.api_log));
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
                    reports: usage.take(),
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
                        if facts.reports.is_empty() {
                            facts.tokens = tokens_from_response(
                                credits_opts.response_format_or_source(),
                                &resp.payload,
                                &resp.metadata,
                            );
                        }
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
        if self.cfg().home.enabled {
            return Err(home_fallback_unsupported());
        }
        let route_model = req.model.clone();
        for mut c in self.credits_candidates(&route_model, opts).await? {
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

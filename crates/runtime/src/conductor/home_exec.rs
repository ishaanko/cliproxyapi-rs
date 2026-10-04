//! Execution loops of Home mode (Go: `conductor_home_execution.go` and the Home branches of
//! `executeStreamMixedOnce`). Every attempt dispatches through Home, runs on the local executor
//! with the returned credential, reports to Home instead of mutating local auth state, and ends
//! its selection so Home can release the credential.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_home::conn::Kill;
use http::HeaderMap;
use serde_json::Value;
use tokio::sync::mpsc;

use super::Manager;
use super::cooldown::ExecResult;
use super::errors::{
    Failure, auth_not_found, executor_not_found, is_connection_lifecycle_error,
    is_request_retry_round_error, result_error_from_error,
};
use super::exec::{
    Fail, Kind, auth_selection_model, call_unary, ensure_requested_model_metadata, execution_model_for_auth_selection,
    executor_for_auth, publish_selected_auth_metadata, requested_model_alias, stream_error_result,
};
use super::home::{
    RoundTiming, RoundTimingResult, downstream_websocket, is_home_dispatch_cooldown, is_home_next_round_immediately_available,
    is_home_retry_round_exhausted, mark_home_retry_round_exhausted, observe_home_cooldown_retry_limit,
    pending_home_retry_round_delay, preferred_home_error, should_return_last_error_on_pick_failure, with_home_auth_count,
    with_home_excluded_auth_ids, with_home_retry_round,
};
use super::home_selection::HomeDispatchSelection;
use super::models::{
    attach_resolved_execution_model_info, resolve_attempt_alias_result, rewrite_model_in_response,
};
use super::pick::pinned_auth_id;
use super::rules;
use super::usage::{UsageFacts, tokens_from_response};
use super::detach::DetachGuard;
use crate::usage_report::UsageCollector;
use crate::executor::{ChunkRx, DynExecutor, ExecError, HomeErrKind, Metadata, Options, Request, Response, StreamResult, meta};

fn canceled_error() -> ExecError {
    let mut e = ExecError::new(0, "context canceled");
    e.upstream_attempted = false;
    e
}

fn timing_of(err: &ExecError) -> RoundTimingResult {
    match err.retry_after {
        Some(d) if !d.is_zero() => RoundTimingResult::After(d),
        _ => RoundTimingResult::None,
    }
}

/// Go: `shouldExcludeHomeAuthAfterStreamError`: connection-lifecycle failures and websocket
/// transport fallbacks (426) may retry the same credential once.
fn should_exclude_home_auth_after_stream_error(downstream_ws: bool, err: &ExecError) -> bool {
    if is_connection_lifecycle_error(err) {
        return false;
    }
    !(downstream_ws && err.status == 426)
}

/// Sets the selection's session identity on the attempt metadata (Go: the `execOpts.Metadata`
/// copy in `executeHomeOnce`).
fn apply_selection_sessions(metadata: &mut Metadata, selection: &HomeDispatchSelection) {
    let canonical = selection.canonical_session_id();
    if canonical.is_empty() {
        return;
    }
    let parent = selection.parent_session_id();
    metadata.insert(meta::CANONICAL_SESSION_ID.into(), Value::String(canonical.clone()));
    if !parent.is_empty() && parent != canonical {
        metadata.insert(meta::PARENT_SESSION_ID.into(), Value::String(parent));
    } else {
        metadata.remove(meta::PARENT_SESSION_ID);
    }
}

async fn call_unary_cancellable(
    kind: Kind,
    executor: &DynExecutor,
    auth: &Auth,
    req: Request,
    opts: Options,
    cancel: &Kill,
) -> Result<Response, ExecError> {
    tokio::select! {
        _ = cancel.wait() => Err(canceled_error()),
        r = call_unary(kind, executor, auth, req, opts) => r,
    }
}

/// Home may name the user API key of the request (Go sets it on the shared gin context); the
/// dispatch wrote it into the pick options, so forward it to the execution options too.
fn carry_home_user_api_key(pick_opts: &Options, opts: &mut Options) {
    if let Some(key) = pick_opts.metadata.get(super::usage::META_CLIENT_API_KEY) {
        opts.metadata.insert(super::usage::META_CLIENT_API_KEY.into(), key.clone());
    }
}

impl Manager {
    /// Retry decision of Home mode (Go: the `HomeEnabled` branches of
    /// `shouldRetryAfterErrorWithAttempted`). `Err` fails fast without a retry.
    pub(crate) fn home_should_retry_after_error(
        &self,
        err: &ExecError,
        attempt: i64,
        metadata: &Metadata,
        max_wait: Duration,
        retry_limit: &mut i64,
    ) -> (Duration, bool) {
        const NO: (Duration, bool) = (Duration::ZERO, false);
        if matches!(err.home, Some(HomeErrKind::ConcurrencyBusy)) {
            return NO;
        }
        let status = err.status;
        if status == 200 || Failure::of_exec(err).is_request_invalid() {
            return NO;
        }
        if is_home_dispatch_cooldown(err) {
            observe_home_cooldown_retry_limit(err, retry_limit, pinned_auth_id(metadata).is_empty());
        }
        if let Some(HomeErrKind::RetryRoundExhausted { retry_now, retry_after_invalid, .. }) = &err.home {
            if !is_request_retry_round_error(err) || !self.home_retry_allowed(attempt, *retry_limit) {
                return NO;
            }
            if *retry_now {
                return (Duration::ZERO, true);
            }
            if *retry_after_invalid {
                return NO;
            }
            if let Some(ra) = err.retry_after {
                if !ra.is_zero() && (max_wait.is_zero() || ra > max_wait) {
                    return NO;
                }
                return (ra, true);
            }
            // Home will provide a cooldown error on the next round if all credentials are still
            // cooling down; otherwise retry immediately.
            return (Duration::ZERO, true);
        }
        if status != 429 || !self.home_retry_allowed(attempt, *retry_limit) {
            return NO;
        }
        match err.retry_after {
            Some(ra) if !ra.is_zero() && !max_wait.is_zero() && ra <= max_wait => (ra, true),
            _ => NO,
        }
    }

    // ---------------------------------------------------------------- unary

    /// Go: `executeHome`.
    pub(crate) async fn execute_home(
        &self,
        kind: Kind,
        req: Request,
        opts: Options,
    ) -> Result<Response, ExecError> {
        let _session_lock = self.lock_home_websocket_session(&opts).await;
        let rs = self.retry_settings();
        let (max_retry_credentials, max_wait) = (rs.max_retry_credentials, rs.max_retry_interval);
        let mut retry_limit: i64 = -1;
        let mut attempt: i64 = 0;
        let mut round_pending = false;
        let mut round_waited = false;
        let mut preferred_upstream: Option<ExecError> = None;
        loop {
            let fail = match self
                .execute_home_once(kind, &req, &opts, max_retry_credentials, &mut retry_limit, attempt)
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(f) => f,
            };
            if fail.stop {
                return Err(fail.err);
            }
            let err = fail.err;
            if err.upstream_attempted {
                preferred_upstream = Some(err.clone());
            }
            if round_pending
                && let Some(wait) =
                    pending_home_retry_round_delay(&err, max_wait, &mut retry_limit, pinned_auth_id(&opts.metadata).is_empty())
                && self.home_retry_allowed(attempt - 1, retry_limit)
            {
                if round_waited {
                    return Err(err);
                }
                self.wait_for_cooldown(wait, max_wait).await;
                round_waited = true;
                continue;
            }
            let (wait, retry) = self.home_should_retry_after_error(&err, attempt, &opts.metadata, max_wait, &mut retry_limit);
            if !retry {
                if preferred_upstream.is_some() && is_home_retry_round_exhausted(&err) {
                    return Err(preferred_home_error(err, preferred_upstream.as_ref()));
                }
                return Err(err);
            }
            self.wait_for_cooldown(wait, max_wait).await;
            attempt += 1;
            round_pending = true;
            round_waited = false;
        }
    }

    /// Go: `executeHomeOnce`: one retry round, each credential at most once.
    #[allow(clippy::too_many_arguments)]
    async fn execute_home_once(
        &self,
        kind: Kind,
        req: &Request,
        opts: &Options,
        max_retry_credentials: i64,
        retry_limit: &mut i64,
        retry_round: i64,
    ) -> Result<Response, Fail> {
        let route_model = auth_selection_model(opts, &req.model);
        let response_alias = requested_model_alias(opts, &route_model);
        let (execution_model, restore_execution_model) = execution_model_for_auth_selection(opts, &req.model);
        let mut opts = ensure_requested_model_metadata(opts.clone(), &route_model);
        let pinned_empty = pinned_auth_id(&opts.metadata).is_empty();
        let mut tried: HashSet<String> = HashSet::new();
        let mut attempted: HashSet<String> = HashSet::new();
        let mut last_err: Option<Fail> = None;
        let mut upstream_err: Option<Fail> = None;
        let mut timing = RoundTiming::default();
        let mut home_auth_count: i64 = 1;
        loop {
            if max_retry_credentials > 0 && attempted.len() as i64 >= max_retry_credentials {
                return Err(match last_err {
                    Some(l) => mark_fail(preferred_fail(l, upstream_err.as_ref()), timing.result(), true),
                    None => auth_not_found("no auth available").into(),
                });
            }
            let mut pick_opts = with_home_retry_round(opts.clone(), retry_round);
            pick_opts = with_home_auth_count(pick_opts, home_auth_count);
            pick_opts = with_home_excluded_auth_ids(pick_opts, &tried);
            let selection = match self.pick_home_dispatch_selection(&route_model, &mut pick_opts, "").await {
                Ok(s) => {
                    carry_home_user_api_key(&pick_opts, &mut opts);
                    s
                }
                Err(e) => {
                    let preferred = || match &last_err {
                        Some(l) => preferred_fail(l.clone(), upstream_err.as_ref()),
                        None => Fail::from(e.clone()),
                    };
                    if last_err.is_some() && is_home_dispatch_cooldown(&e) {
                        observe_home_cooldown_retry_limit(&e, retry_limit, pinned_empty);
                        return Err(mark_fail(preferred(), timing_of(&e), false));
                    }
                    if should_return_last_error_on_pick_failure(true, last_err.as_ref().map(|f| &f.err), &e) {
                        return Err(mark_fail(
                            preferred(),
                            timing.result(),
                            is_home_next_round_immediately_available(&e),
                        ));
                    }
                    return Err(e.into());
                }
            };
            home_auth_count += 1;
            let executor = selection.executor();
            let provider = selection.provider().to_string();
            let Some(auth) = selection.clone_auth_for_route(&route_model) else {
                selection.end("missing_execution_target");
                return Err(executor_not_found().into());
            };
            self.observe_home_retry_limit(&auth, Some(&selection), retry_limit);
            if tried.contains(&auth.id) {
                self.end_home_selection_before_redispatch(&selection, "repeated_auth").await?;
                return Err(match last_err {
                    Some(l) => mark_fail(preferred_fail(l, upstream_err.as_ref()), timing.result(), false),
                    None => super::home::home_request_retry_exceeded_error().into(),
                });
            }
            tried.insert(auth.id.clone());
            attempted.insert(auth.id.clone());
            tracing::debug!("Home selected auth {} ({provider}) for model {route_model}", auth.id);
            if let Err(e) = self.bind_home_selection_runtime_auth(&opts, &selection) {
                selection.end("runtime_auth_bind_failed");
                return Err(e.into());
            }
            publish_selected_auth_metadata(&mut opts, &auth);
            let guard = match selection.attempt_context() {
                Ok(g) => g,
                Err(e) => {
                    selection.end("attempt_bind_failed");
                    return Err(super::home_concurrency::install_error(e).into());
                }
            };
            let (mut models, mut pooled, mut alias_result) = self.prepared_execution_models_with_alias(&auth, &route_model);
            if alias_result.force_mapping && !response_alias.is_empty() {
                alias_result.original_alias = response_alias.clone();
            }
            if models.len() > 1 {
                models.truncate(1);
                pooled = false;
            }
            if models.is_empty() {
                guard.release();
                self.end_home_selection_before_redispatch(&selection, "no_execution_models").await?;
                let e = auth_not_found("no execution models available");
                timing.observe(&e);
                last_err = Some(e.into());
                continue;
            }
            let prepared = match self.prepare_home_request_auth(&executor, &selection).await {
                Ok(p) => p,
                Err(e) => {
                    let mut state_model = self.selection_model_key_for_auth(&auth, &route_model);
                    if state_model.is_empty() {
                        state_model = super::util::canonical_model_key(&route_model);
                    }
                    let result = ExecResult {
                        auth_id: auth.id.clone(),
                        provider: provider.clone(),
                        model: state_model,
                        route_model: route_model.clone(),
                        success: false,
                        retry_after: None,
                        credential_scope: false,
                        error: Some(result_error_from_error(&e)),
                        options: opts.clone(),
                        skip_quota_observation: kind == Kind::Count,
                        response_headers: e.recorded_headers(),
                    };
                    self.report_home_result(result, Some(&auth), None);
                    guard.release();
                    self.end_home_selection_before_redispatch(&selection, "prepare_failed").await?;
                    timing.observe(&e);
                    last_err = Some(e.into());
                    continue;
                }
            };
            let cfg = self.cfg();
            let mut credential_scope_break = false;
            for upstream_model in &models {
                let result_model = self.state_model_for_execution(&prepared, &route_model, upstream_model, pooled);
                let mut exec_req = req.clone();
                exec_req.model = if restore_execution_model { execution_model.clone() } else { upstream_model.clone() };
                let mut exec_opts = opts.clone();
                exec_opts.lifecycle = Some(Arc::new(selection.clone()));
                apply_selection_sessions(&mut exec_opts.metadata, &selection);
                attach_resolved_execution_model_info(
                    &cfg,
                    &mut exec_req,
                    &prepared,
                    &route_model,
                    upstream_model,
                    restore_execution_model,
                );
                if !restore_execution_model {
                    let (info, support) = selection.model_info();
                    super::home_model_info::attach_resolved_home_model_info(
                        &mut exec_req,
                        &prepared,
                        &route_model,
                        info,
                        support,
                    );
                }
                let executor_for_call = executor_for_auth(executor.clone(), &prepared);
                let usage = UsageCollector::new();
                exec_opts.usage_collector = Some(usage.clone());
                let started = Instant::now();
                // A client that hangs up drops this future mid-call: Go reports the failed
                // result (and its usage) with the cancelled context error.
                let mut detach = DetachGuard::new(
                    self.clone(),
                    (kind == Kind::Execute).then(|| usage.clone()),
                    exec_opts.api_log.clone(),
                    ExecResult {
                        auth_id: prepared.id.clone(),
                        provider: provider.clone(),
                        model: result_model.clone(),
                        route_model: route_model.clone(),
                        success: false,
                        retry_after: None,
                        credential_scope: false,
                        error: None,
                        options: exec_opts.clone(),
                        skip_quota_observation: kind == Kind::Count,
                        response_headers: HeaderMap::new(),
                    },
                    UsageFacts {
                        stream: false,
                        upstream_model: upstream_model.clone(),
                        requested_model: requested_model_alias(&exec_opts, &route_model),
                        ..Default::default()
                    },
                    started,
                )
                .home(prepared.clone(), true);
                let res = detach
                    .run(call_unary_cancellable(kind, &executor_for_call, &prepared, exec_req.clone(), exec_opts.clone(), &guard.cancel()))
                    .await;
                let latency = started.elapsed();
                if let Err(err) = &res {
                    if err.upstream_attempted {
                        upstream_err = Some(err.clone().into());
                    }
                    if kind == Kind::Count && Failure::of_exec(err).is_unauthorized() {
                        let body = err.body.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                        self.report_home_unauthorized(&prepared, &provider, &result_model, &access_token_sha256(&prepared), &body);
                    }
                    tracing::warn!(
                        "Home upstream failure: provider={provider} model={upstream_model} auth={} status={}",
                        prepared.id,
                        err.status
                    );
                }
                let mut result = detach.take_result();
                result.success = res.is_ok();
                let mut facts = UsageFacts {
                    latency,
                    stream: false,
                    upstream_model: upstream_model.clone(),
                    requested_model: requested_model_alias(&exec_opts, &route_model),
                    reports: usage.take(),
                    ..Default::default()
                };
                match res {
                    Ok(mut resp) => {
                        result.response_headers = resp.headers.clone();
                        if facts.reports.is_empty() {
                            facts.tokens = tokens_from_response(exec_opts.response_format_or_source(), &resp.payload, &resp.metadata);
                        }
                        self.report_home_result(result, Some(&prepared), (kind == Kind::Execute).then_some(facts));
                        guard.release();
                        let attempt_alias =
                            resolve_attempt_alias_result(&cfg, &prepared, &route_model, upstream_model, &alias_result);
                        if attempt_alias.force_mapping && !attempt_alias.original_alias.trim().is_empty() {
                            resp.payload = Bytes::from(rewrite_model_in_response(&resp.payload, attempt_alias.original_alias.trim()));
                        }
                        if !self.retain_home_websocket_selection(&opts, &route_model, &selection) {
                            selection.end("completed");
                        }
                        return Ok(resp);
                    }
                    Err(err) => {
                        result.error = Some(result_error_from_error(&err));
                        result.retry_after = err.retry_after;
                        result.response_headers = err.recorded_headers();
                        if err.credential_scoped {
                            result.credential_scope = true;
                        }
                        let action = rules::match_action(&prepared, &err, &cfg);
                        rules::apply_action_to_result(action, &mut result);
                        let credential_scope = result.credential_scope;
                                        self.report_home_result(result, Some(&prepared), (kind == Kind::Execute).then_some(facts));
                        last_err = Some(err.clone().into());
                        if action.is_some() {
                            if rules::is_stop(action) {
                                guard.release();
                                selection.end("request_stopped");
                                return Err(Fail::stop(err));
                            }
                            if credential_scope {
                                credential_scope_break = true;
                                break;
                            }
                            continue;
                        }
                        if Failure::of_exec(&err).is_request_invalid() {
                            guard.release();
                            selection.end("request_invalid");
                            return Err(err.into());
                        }
                        if credential_scope {
                            credential_scope_break = true;
                            break;
                        }
                    }
                }
            }
            let _ = credential_scope_break;
            if let Some(l) = &last_err {
                timing.observe(&l.err);
            }
            guard.release();
            self.end_home_selection_before_redispatch(&selection, "execution_failed").await?;
        }
    }

    /// Go: `reportHomeUnauthorized`: a result-only zero-token record for an upstream 401 that did
    /// not go through an executor usage reporter.
    pub fn report_home_unauthorized(&self, auth: &Auth, provider: &str, model: &str, access_token_sha256: &str, failure_body: &str) {
        let mut auth = auth.clone();
        let mut auth_index = auth.index.trim().to_string();
        if auth_index.is_empty() {
            auth_index = auth.ensure_index().trim().to_string();
        }
        let hash = access_token_sha256.trim();
        if auth_index.is_empty() || hash.is_empty() {
            return;
        }
        let body = if failure_body.is_empty() { "upstream unauthorized" } else { failure_body };
        let provider = if provider.trim().is_empty() { auth.provider.trim() } else { provider.trim() };
        let record = crate::usage::UsageRecord {
            timestamp: self.now(),
            source: auth.attr("label"),
            auth_index,
            auth_type: auth.auth_kind().to_string(),
            provider: provider.to_string(),
            executor_type: "home-result".into(),
            model: model.trim().to_string(),
            failed: true,
            fail: crate::usage::UsageFailure { status_code: 401, body: body.to_string() },
            ..Default::default()
        };
        let tracker = self.usage.read().clone();
        if let Some(tracker) = tracker {
            tracker.record(record);
        }
    }

    // ---------------------------------------------------------------- streaming

    /// Go: `ExecuteStream` in Home mode.
    pub(crate) async fn execute_home_stream(
        &self,
        req: Request,
        opts: Options,
    ) -> Result<StreamResult, ExecError> {
        let _session_lock = self.lock_home_websocket_session(&opts).await;
        let rs = self.retry_settings();
        let (max_retry_credentials, max_wait) = (rs.max_retry_credentials, rs.max_retry_interval);
        let mut retry_limit: i64 = -1;
        let mut attempt: i64 = 0;
        let mut round_pending = false;
        let mut round_waited = false;
        let mut preferred_upstream: Option<Fail> = None;
        let last: Fail = loop {
            let fail = match self
                .execute_home_stream_once(&req, &opts, max_retry_credentials, &mut retry_limit, attempt)
                .await
            {
                Ok(s) => return Ok(s),
                Err(f) => f,
            };
            if fail.err.upstream_attempted {
                preferred_upstream = Some(fail.clone());
            }
            if round_pending
                && let Some(wait) = pending_home_retry_round_delay(
                    &fail.err,
                    max_wait,
                    &mut retry_limit,
                    pinned_auth_id(&opts.metadata).is_empty(),
                )
                && self.home_retry_allowed(attempt - 1, retry_limit)
            {
                if round_waited {
                    return Err(fail.err);
                }
                self.wait_for_cooldown(wait, max_wait).await;
                round_waited = true;
                continue;
            }
            if fail.stop {
                return Err(fail.err);
            }
            let (wait, retry) = self.home_should_retry_after_error(&fail.err, attempt, &opts.metadata, max_wait, &mut retry_limit);
            if !retry {
                break fail;
            }
            self.wait_for_cooldown(wait, max_wait).await;
            attempt += 1;
            round_pending = true;
            round_waited = false;
        };
        let mut last = last;
        if preferred_upstream.is_some() && is_home_retry_round_exhausted(&last.err) {
            last.err = preferred_home_error(last.err, preferred_upstream.as_ref().map(|f| &f.err));
        }
        if let Some(headers) = last.bootstrap_headers {
            return Ok(stream_error_result(headers, last.err));
        }
        Err(last.err)
    }

    /// Go: `executeStreamMixedOnce` with Home enabled.
    #[allow(clippy::too_many_arguments)]
    async fn execute_home_stream_once(
        &self,
        req: &Request,
        opts: &Options,
        max_retry_credentials: i64,
        retry_limit: &mut i64,
        retry_round: i64,
    ) -> Result<StreamResult, Fail> {
        let route_model = auth_selection_model(opts, &req.model);
        let response_alias = requested_model_alias(opts, &route_model);
        let (execution_model, restore_execution_model) = execution_model_for_auth_selection(opts, &req.model);
        let mut opts = ensure_requested_model_metadata(opts.clone(), &route_model);
        let downstream_ws = downstream_websocket(&opts.metadata);
        let pinned_empty = pinned_auth_id(&opts.metadata).is_empty();
        let mut home_auth_count: i64 = 1;
        let mut tried: HashSet<String> = HashSet::new();
        let mut excluded: HashSet<String> = HashSet::new();
        let mut same_auth_retries: HashMap<String, i64> = HashMap::new();
        let mut last_home_auth_id = String::new();
        let mut same_auth_retry_pending = false;
        let mut attempted: HashSet<String> = HashSet::new();
        let mut last_err: Option<Fail> = None;
        let mut upstream_err: Option<Fail> = None;
        let mut timing = RoundTiming::default();
        loop {
            let allow_same_auth_retry = same_auth_retry_pending
                && !last_home_auth_id.is_empty()
                && same_auth_retries.get(&last_home_auth_id).copied().unwrap_or(0) == 0;
            if max_retry_credentials > 0 && attempted.len() as i64 >= max_retry_credentials && !allow_same_auth_retry {
                return Err(match last_err {
                    Some(l) => mark_fail(preferred_fail(l, upstream_err.as_ref()), timing.result(), true),
                    None => auth_not_found("no auth available").into(),
                });
            }
            let mut pick_opts = with_home_retry_round(opts.clone(), retry_round);
            pick_opts = with_home_auth_count(pick_opts, home_auth_count);
            pick_opts = with_home_excluded_auth_ids(pick_opts, &excluded);
            let selection = match self.pick_home_dispatch_selection(&route_model, &mut pick_opts, "").await {
                Ok(s) => {
                    carry_home_user_api_key(&pick_opts, &mut opts);
                    s
                }
                Err(e) => {
                    let preferred = || match &last_err {
                        Some(l) => preferred_fail(l.clone(), upstream_err.as_ref()),
                        None => Fail::from(e.clone()),
                    };
                    if last_err.is_some() && is_home_dispatch_cooldown(&e) {
                        observe_home_cooldown_retry_limit(&e, retry_limit, pinned_empty);
                        return Err(mark_fail(preferred(), timing_of(&e), false));
                    }
                    if should_return_last_error_on_pick_failure(true, last_err.as_ref().map(|f| &f.err), &e) {
                        return Err(mark_fail(preferred(), timing.result(), is_home_next_round_immediately_available(&e)));
                    }
                    return Err(e.into());
                }
            };
            let executor = selection.executor();
            let provider = selection.provider().to_string();
            let Some(mut auth) = selection.clone_auth_for_route(&route_model) else {
                selection.end("missing_execution_target");
                return Err(executor_not_found().into());
            };
            self.observe_home_retry_limit(&auth, Some(&selection), retry_limit);
            if allow_same_auth_retry
                && max_retry_credentials > 0
                && attempted.len() as i64 >= max_retry_credentials
                && auth.id != last_home_auth_id
            {
                self.end_home_selection_before_redispatch(&selection, "max_retry_credentials").await?;
                return Err(match last_err {
                    Some(l) => mark_fail(preferred_fail(l, upstream_err.as_ref()), timing.result(), true),
                    None => auth_not_found("no auth available").into(),
                });
            }
            if !last_home_auth_id.is_empty() && auth.id != last_home_auth_id {
                same_auth_retry_pending = false;
            }
            // A legacy Home may ignore excluded_auth_ids and return the same credential again.
            // Credentials excluded from this round are rejected; the explicit same-auth retry path
            // intentionally leaves the credential out of `excluded`.
            if tried.contains(&auth.id) {
                if excluded.contains(&auth.id) {
                    self.end_home_selection_before_redispatch(&selection, "repeated_excluded_auth").await?;
                    return Err(match last_err {
                        Some(l) => mark_fail(preferred_fail(l, upstream_err.as_ref()), timing.result(), false),
                        None => super::home::home_request_retry_exceeded_error().into(),
                    });
                }
                let n = same_auth_retries.entry(auth.id.clone()).or_insert(0);
                *n += 1;
                if *n > 1 {
                    // A fresh selection may retry the same auth once (connection lifecycle or
                    // authorization recovery); repeated failures must rotate away.
                    excluded.insert(auth.id.clone());
                    self.end_home_selection_before_redispatch(&selection, "repeated_same_auth").await?;
                    continue;
                }
            }
            tracing::debug!("Home selected auth {} ({provider}) for model {route_model}", auth.id);
            if let Err(e) = self.bind_home_selection_runtime_auth(&opts, &selection) {
                selection.end("runtime_auth_bind_failed");
                return Err(e.into());
            }
            publish_selected_auth_metadata(&mut opts, &auth);
            tried.insert(auth.id.clone());
            let guard = match selection.attempt_context() {
                Ok(g) => g,
                Err(e) => {
                    selection.end("attempt_bind_failed");
                    return Err(super::home_concurrency::install_error(e).into());
                }
            };
            let (mut models, mut pooled, mut alias_result) = self.prepared_execution_models_with_alias(&auth, &route_model);
            if alias_result.force_mapping && !response_alias.is_empty() {
                alias_result.original_alias = response_alias.clone();
            }
            if models.is_empty() {
                excluded.insert(auth.id.clone());
                last_home_auth_id.clone_from(&auth.id);
                same_auth_retry_pending = false;
                guard.release();
                self.end_home_selection_before_redispatch(&selection, "no_execution_models").await?;
                continue;
            }
            attempted.insert(auth.id.clone());
            match self.prepare_home_request_auth(&executor, &selection).await {
                Ok(prepared) => auth = prepared,
                Err(e) => {
                    let mut exclude_auth = should_exclude_home_auth_after_stream_error(downstream_ws, &e);
                    if same_auth_retries.get(&auth.id).copied().unwrap_or(0) > 0 {
                        exclude_auth = true;
                    }
                    if exclude_auth {
                        excluded.insert(auth.id.clone());
                    }
                    last_home_auth_id.clone_from(&auth.id);
                    same_auth_retry_pending = !exclude_auth;
                    let mut state_model = self.selection_model_key_for_auth(&auth, &route_model);
                    if state_model.is_empty() {
                        state_model = super::util::canonical_model_key(&route_model);
                    }
                    let result = ExecResult {
                        auth_id: auth.id.clone(),
                        provider: provider.clone(),
                        model: state_model,
                        route_model: route_model.clone(),
                        success: false,
                        retry_after: None,
                        credential_scope: false,
                        error: Some(result_error_from_error(&e)),
                        options: pick_opts.clone(),
                        skip_quota_observation: false,
                        response_headers: e.recorded_headers(),
                    };
                    self.report_home_result(result, Some(&auth), None);
                    guard.release();
                    timing.observe(&e);
                    last_err = Some(e.into());
                    self.end_home_selection_before_redispatch(&selection, "prepare_failed").await?;
                    continue;
                }
            }
            let mut exec_req = sanitize_downstream_websocket_fallback_request(downstream_ws, &auth, req);
            if !restore_execution_model {
                let (info, support) = selection.model_info();
                super::home_model_info::attach_resolved_home_model_info(&mut exec_req, &auth, &route_model, info, support);
            }
            let mut exec_opts = opts.clone();
            exec_opts.lifecycle = Some(Arc::new(selection.clone()));
            apply_selection_sessions(&mut exec_opts.metadata, &selection);
            if models.len() > 1 {
                models.truncate(1);
                pooled = false;
            }
            let home_ctx = super::stream::HomeStreamCtx { cancel: guard.cancel() };
            let pool_result = self
                .stream_with_model_pool(
                    &executor,
                    auth.clone(),
                    &provider,
                    &exec_req,
                    &exec_opts,
                    &route_model,
                    restore_execution_model.then_some(execution_model.as_str()),
                    &models,
                    pooled,
                    &alias_result,
                    Some(&home_ctx),
                )
                .await;
            match pool_result {
                Err(fail) => {
                    if fail.err.upstream_attempted {
                        upstream_err = Some(fail.clone());
                    }
                    let mut exclude_auth = should_exclude_home_auth_after_stream_error(downstream_ws, &fail.err);
                    if same_auth_retries.get(&auth.id).copied().unwrap_or(0) > 0 {
                        exclude_auth = true;
                    }
                    if exclude_auth {
                        excluded.insert(auth.id.clone());
                    }
                    last_home_auth_id.clone_from(&auth.id);
                    same_auth_retry_pending = !exclude_auth;
                    guard.release();
                    self.end_home_selection_before_redispatch(&selection, "stream_start_failed").await?;
                    let action = rules::match_action(&auth, &fail.err, &self.cfg());
                    if action.is_some() && rules::is_stop(action) {
                        return Err(Fail::stop(fail.err));
                    }
                    if action.is_none() && Failure::of_exec(&fail.err).is_request_invalid() {
                        return Err(fail);
                    }
                    timing.observe(&fail.err);
                    last_err = Some(fail);
                    home_auth_count += 1;
                    continue;
                }
                Ok(stream) => {
                    if self.retain_home_websocket_selection(&opts, &route_model, &selection) {
                        return Ok(wrap_home_stream(stream, None, guard));
                    }
                    return Ok(wrap_home_stream(stream, Some(selection), guard));
                }
            }
        }
    }
}

fn preferred_fail(fallback: Fail, upstream: Option<&Fail>) -> Fail {
    let Some(up) = upstream else { return fallback };
    let err = preferred_home_error(fallback.err.clone(), Some(&up.err));
    Fail { err, stop: false, bootstrap_headers: up.bootstrap_headers.clone().or(fallback.bootstrap_headers) }
}

fn mark_fail(fail: Fail, timing: RoundTimingResult, retry_now: bool) -> Fail {
    Fail { err: mark_home_retry_round_exhausted(fail.err, timing, retry_now), ..fail }
}

fn access_token_sha256(auth: &Auth) -> String {
    use sha2::{Digest, Sha256};
    let token = auth.access_token();
    if token.is_empty() { String::new() } else { hex::encode(Sha256::digest(token.as_bytes())) }
}

/// Go: `sanitizeDownstreamWebsocketFallbackRequest`: a request falling back from a websocket
/// transport must not carry the websocket-only `generate` flag.
fn sanitize_downstream_websocket_fallback_request(downstream_ws: bool, auth: &Auth, req: &Request) -> Request {
    let mut req = req.clone();
    if !downstream_ws || super::home::auth_websockets_enabled(auth) || req.payload.is_empty() {
        return req;
    }
    let mut value = cpa_json::parse(&req.payload);
    if value.is_null() {
        return req;
    }
    cpa_json::delete(&mut value, "generate");
    if let Ok(bytes) = serde_json::to_vec(&value) {
        req.payload = Bytes::from(bytes);
    }
    req
}

/// Go: `wrapHomeStream`: forwards the stream and ends the attempt (and, for non-retained
/// selections, the selection) when it is done.
fn wrap_home_stream(
    mut result: StreamResult,
    selection: Option<HomeDispatchSelection>,
    guard: super::home_selection::AttemptGuard,
) -> StreamResult {
    let (tx, rx) = mpsc::channel(1);
    let headers = result.headers.clone();
    let mut upstream = std::mem::replace(&mut result.chunks, ChunkRx::closed());
    let usage = result.usage.take();
    let cancel = guard.cancel();
    let has_selection = selection.is_some();
    tokio::spawn(async move {
        let mut forward = true;
        loop {
            let chunk = tokio::select! {
                _ = cancel.wait() => break,
                _ = tx.closed() => break,
                chunk = upstream.recv() => match chunk {
                    Some(c) => c,
                    None => break,
                },
            };
            if !forward {
                continue;
            }
            let failed = chunk.is_err();
            if tx.send(chunk).await.is_err() {
                break;
            }
            if failed && has_selection {
                forward = false;
            }
        }
        drop(upstream);
        guard.release();
        if let Some(selection) = selection {
            selection.end("stream_closed");
        }
    });
    let mut out = StreamResult::new(headers, rx);
    out.usage = usage;
    out
}

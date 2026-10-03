//! The execution loops (Go: conductor_execution.go Execute/ExecuteCount/ExecuteStream).
//!
//! Structure: an outer loop over retry rounds; each round (`mixed_once`) picks credentials one by
//! one (every credential at most once, bounded by `max-retry-credentials`) and fails over
//! immediately; sleeping happens only between rounds. Errors that blame the request (invalid
//! request, request-scoped `stop` rules) end the whole call without rotating or cooling.

use std::collections::HashSet;
use std::time::Instant;

use bytes::Bytes;
use cpa_auth::Auth;
use http::HeaderMap;
use serde_json::Value;

use super::cooldown::ExecResult;
use super::errors::{
    CODE_FORCE_COOLDOWN, Failure, auth_not_found, executor_not_found,
    is_count_tokens_endpoint_not_found, is_responses_compact_availability_neutral,
    is_responses_compact_request_fault, provider_not_found, result_error_from_error,
};
use super::models::{
    AliasResult, attach_resolved_execution_model_info, resolve_attempt_alias_result,
    rewrite_model_in_response,
};
use super::pick::{Eligibility, Picked};
use super::rules;
use super::session;
use super::usage::{UsageFacts, tokens_from_response};
use super::util::meta_string;
use super::{Manager, session as session_mod};
use crate::executor::{
    DynExecutor, ExecError, Metadata, Options, Request, Response, StreamResult, meta,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Execute,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoundKind {
    Unary(Kind),
    Stream,
}

/// A failed attempt as it travels up the loops.
#[derive(Debug, Clone)]
pub(crate) struct Fail {
    pub err: ExecError,
    /// A request-scoped `stop` rule ended the call: never retried.
    pub stop: bool,
    /// Upstream headers of a stream that failed during bootstrap.
    pub bootstrap_headers: Option<HeaderMap>,
}

impl From<ExecError> for Fail {
    fn from(err: ExecError) -> Self {
        Fail {
            err,
            stop: false,
            bootstrap_headers: None,
        }
    }
}

impl Fail {
    pub fn stop(err: ExecError) -> Self {
        Fail {
            err,
            stop: true,
            bootstrap_headers: None,
        }
    }
}

pub(crate) enum Outcome {
    Response(Response),
    Stream(StreamResult),
}

/// Prefer the last real upstream failure over a synthesized one (Go: preferredExecutionAttemptError).
pub(crate) fn preferred(fallback: Fail, upstream: Option<&Fail>) -> Fail {
    match upstream {
        Some(u) => u.clone(),
        None => fallback,
    }
}

pub(crate) fn normalize_providers(providers: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(providers.len());
    for p in providers {
        let p = p.trim().to_lowercase();
        if p.is_empty() || out.contains(&p) {
            continue;
        }
        out.push(p);
    }
    out
}

pub(crate) fn auth_selection_model(opts: &Options, fallback: &str) -> String {
    let m = meta_string(&opts.metadata, meta::AUTH_SELECTION_MODEL);
    if m.is_empty() {
        fallback.trim().to_string()
    } else {
        m
    }
}

/// `(model, true)` when the executor must be called with the original model although routing used
/// a different selection model.
pub(crate) fn execution_model_for_auth_selection(opts: &Options, model: &str) -> (String, bool) {
    let model = model.trim();
    if model.is_empty() {
        return (String::new(), false);
    }
    if auth_selection_model(opts, model) == model {
        return (String::new(), false);
    }
    (model.to_string(), true)
}

pub(crate) fn ensure_requested_model_metadata(mut opts: Options, requested: &str) -> Options {
    let requested = requested.trim();
    if requested.is_empty() || !meta_string(&opts.metadata, meta::REQUESTED_MODEL).is_empty() {
        return opts;
    }
    opts.metadata.insert(
        meta::REQUESTED_MODEL.into(),
        Value::String(requested.into()),
    );
    opts
}

pub(crate) fn requested_model_alias(opts: &Options, fallback: &str) -> String {
    let m = meta_string(&opts.metadata, meta::REQUESTED_MODEL);
    if m.is_empty() {
        fallback.trim().to_string()
    } else {
        m
    }
}

/// Publishes the chosen credential to the request metadata and the caller's callback (Go:
/// publishSelectedAuthMetadata).
pub(crate) fn publish_selected_auth_metadata(opts: &mut Options, auth: &Auth) {
    let id = auth.id.trim();
    let index = auth.index.trim();
    if !id.is_empty() {
        opts.metadata
            .insert(meta::SELECTED_AUTH_ID.into(), Value::String(id.into()));
    }
    if !index.is_empty() {
        opts.metadata.insert(
            meta::SELECTED_AUTH_INDEX.into(),
            Value::String(index.into()),
        );
    }
    if let Some(cb) = &opts.selected_auth {
        (cb.0)(id, index);
    }
}

/// Fills `canonical_session_id` from the request when no stage did (Go: ensureCanonicalSessionMetadata).
pub(crate) fn ensure_canonical_session_metadata(
    metadata: &mut Metadata,
    headers: &HeaderMap,
    payload: &[u8],
) {
    if metadata
        .get(meta::CANONICAL_SESSION_ID)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
    {
        return;
    }
    let id = session_mod::canonical_session_id(headers, payload, metadata);
    if !id.is_empty() {
        metadata.insert(meta::CANONICAL_SESSION_ID.into(), Value::String(id));
    }
}

pub(crate) fn is_claude_oauth(auth: &Auth) -> bool {
    auth.provider.trim().eq_ignore_ascii_case("claude")
        && auth.attr("auth_kind").eq_ignore_ascii_case("oauth")
}

/// Claude OAuth credentials map a cancelled request to a plain return: no result is recorded so
/// the credential is not penalized for the client hanging up.
pub(crate) fn claude_cancelled(auth: &Auth, err: &ExecError) -> bool {
    is_claude_oauth(auth)
        && err.status == 0
        && err.message.trim().eq_ignore_ascii_case("context canceled")
}

pub(crate) fn executor_for_auth(executor: DynExecutor, auth: &Auth) -> DynExecutor {
    if auth.auth_kind() == cpa_auth::types::AUTH_KIND_API_KEY
        && let Some(scoped) = executor.for_api_key()
    {
        return scoped;
    }
    executor
}

/// What one credential attempt decided.
pub(crate) enum AuthAttempt {
    Success(Outcome),
    /// End the call with this failure.
    Return(Fail),
    /// This credential failed; try the next one.
    Next(Fail),
}

impl Manager {
    /// Derives the session ids once per request (outside any lock) when affinity is enabled.
    pub(crate) fn prepare_affinity_ids(&self, opts: &mut Options) {
        if self.selector().affinity().is_some() {
            super::selector::resolve_affinity_ids(
                &opts.headers,
                &opts.original_request,
                &mut opts.metadata,
            );
        }
    }

    pub(crate) async fn execute_unary(
        &self,
        kind: Kind,
        providers: &[String],
        mut req: Request,
        mut opts: Options,
    ) -> Result<Response, ExecError> {
        session::enrich(&mut req, &mut opts);
        self.prepare_affinity_ids(&mut opts);
        let normalized = normalize_providers(providers);
        if normalized.is_empty() {
            return Err(provider_not_found("no provider supplied"));
        }
        if self.home_enabled() {
            return self.execute_home(kind, req, opts).await;
        }
        let rs = self.retry_settings();
        let retry_model = auth_selection_model(&opts, &req.model);
        let mut preferred_upstream: Option<Fail> = None;
        let mut attempt: i64 = 0;
        let last: Fail = loop {
            let mut round_attempted = HashSet::new();
            match self
                .mixed_once(
                    RoundKind::Unary(kind),
                    &normalized,
                    &req,
                    &opts,
                    rs.max_retry_credentials,
                    attempt,
                    rs.request_retry,
                    &mut round_attempted,
                )
                .await
            {
                Ok(Outcome::Response(resp)) => return Ok(resp),
                Ok(Outcome::Stream(_)) => unreachable!("unary round returned a stream"),
                Err(fail) => {
                    if fail.stop {
                        return Err(fail.err);
                    }
                    if fail.err.upstream_attempted {
                        preferred_upstream = Some(fail.clone());
                    }
                    let (wait, retry) = self.should_retry_after_error(
                        &fail.err,
                        attempt,
                        &normalized,
                        &retry_model,
                        rs.max_retry_interval,
                        rs.request_retry,
                        &round_attempted,
                        &opts.metadata,
                    );
                    if !retry {
                        break fail;
                    }
                    self.wait_for_cooldown(wait, rs.max_retry_interval).await;
                    attempt += 1;
                }
            }
        };
        let last = preferred(last, preferred_upstream.as_ref());
        if kind == Kind::Execute
            && self.should_attempt_antigravity_credits_fallback(&last.err, &normalized)
            && let Some(resp) = self.try_antigravity_credits_execute(&req, &opts).await?
        {
            return Ok(resp);
        }
        Err(last.err)
    }

    pub(crate) async fn execute_stream_rounds(
        &self,
        providers: &[String],
        mut req: Request,
        mut opts: Options,
    ) -> Result<StreamResult, ExecError> {
        session::enrich(&mut req, &mut opts);
        self.prepare_affinity_ids(&mut opts);
        let normalized = normalize_providers(providers);
        if normalized.is_empty() {
            return Err(provider_not_found("no provider supplied"));
        }
        if self.home_enabled() {
            return self.execute_home_stream(req, opts).await;
        }
        let rs = self.retry_settings();
        let retry_model = auth_selection_model(&opts, &req.model);
        let mut preferred_upstream: Option<Fail> = None;
        let mut attempt: i64 = 0;
        let last: Fail = loop {
            let mut round_attempted = HashSet::new();
            match self
                .mixed_once(
                    RoundKind::Stream,
                    &normalized,
                    &req,
                    &opts,
                    rs.max_retry_credentials,
                    attempt,
                    rs.request_retry,
                    &mut round_attempted,
                )
                .await
            {
                Ok(Outcome::Stream(s)) => return Ok(s),
                Ok(Outcome::Response(_)) => unreachable!("stream round returned a response"),
                Err(fail) => {
                    if fail.err.upstream_attempted {
                        preferred_upstream = Some(fail.clone());
                    }
                    if fail.stop {
                        return Err(fail.err);
                    }
                    let (wait, retry) = self.should_retry_after_error(
                        &fail.err,
                        attempt,
                        &normalized,
                        &retry_model,
                        rs.max_retry_interval,
                        rs.request_retry,
                        &round_attempted,
                        &opts.metadata,
                    );
                    if !retry {
                        break fail;
                    }
                    self.wait_for_cooldown(wait, rs.max_retry_interval).await;
                    attempt += 1;
                }
            }
        };
        let last = preferred(last, preferred_upstream.as_ref());
        if self.should_attempt_antigravity_credits_fallback(&last.err, &normalized)
            && let Some(stream) = self
                .try_antigravity_credits_execute_stream(&req, &opts)
                .await?
        {
            return Ok(stream);
        }
        if let Some(headers) = last.bootstrap_headers {
            return Ok(stream_error_result(headers, last.err));
        }
        Err(last.err)
    }

    /// One retry round (Go: executeMixedOnce / executeCountMixedOnce / executeStreamMixedOnce).
    #[allow(clippy::too_many_arguments)]
    async fn mixed_once(
        &self,
        kind: RoundKind,
        providers: &[String],
        req: &Request,
        opts: &Options,
        max_retry_credentials: i64,
        retry_round: i64,
        default_retry: i64,
        round_attempted: &mut HashSet<String>,
    ) -> Result<Outcome, Fail> {
        if providers.is_empty() {
            return Err(provider_not_found("no provider supplied").into());
        }
        let route_model = auth_selection_model(opts, &req.model);
        let (execution_model, restore_execution_model) =
            execution_model_for_auth_selection(opts, &req.model);
        let mut opts = ensure_requested_model_metadata(opts.clone(), &route_model);
        let eligibility = Eligibility::from_meta(&opts.metadata);
        let mut tried = self.request_retry_round_exclusions(retry_round, default_retry);
        let mut attempted: HashSet<String> = HashSet::new();
        let mut last_err: Option<Fail> = None;
        let mut upstream_err: Option<Fail> = None;
        loop {
            if max_retry_credentials > 0 && attempted.len() as i64 >= max_retry_credentials {
                return Err(match last_err {
                    Some(l) => preferred(l, upstream_err.as_ref()),
                    None => auth_not_found("no auth available").into(),
                });
            }
            let scheduler_provider = "mixed";
            let picked = match self.pick_next_mixed_plugin(
                scheduler_provider,
                providers,
                &route_model,
                &mut opts,
                &tried,
                &eligibility,
            ).await {
                Ok(p) => p,
                Err(e) => {
                    return Err(match last_err {
                        Some(l) => preferred(l, upstream_err.as_ref()),
                        None => e.into(),
                    });
                }
            };
            let Picked {
                auth,
                executor,
                provider,
            } = picked;
            publish_selected_auth_metadata(&mut opts, &auth);
            round_attempted.insert(auth.id.clone());
            tried.insert(auth.id.clone());

            let (models, pooled, alias_result) =
                self.prepared_execution_models_with_alias(&auth, &route_model);
            if models.is_empty() {
                continue;
            }
            attempted.insert(auth.id.clone());

            let mut auth = auth;
            match self.prepare_request_auth(&executor, &auth).await {
                Ok(prepared) => auth = prepared,
                Err(err) => {
                    if claude_cancelled(&auth, &err) {
                        return Err(err.into());
                    }
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
                        error: Some(result_error_from_error(&err)),
                        options: opts.clone(),
                        skip_quota_observation: matches!(kind, RoundKind::Unary(Kind::Count)),
                        response_headers: err.recorded_headers(),
                    };
                    self.mark_result_inner(result, None);
                    last_err = Some(err.into());
                    continue;
                }
            }
            let executor = executor_for_auth(executor, &auth);

            let attempt = match kind {
                RoundKind::Unary(k) => {
                    self.attempt_unary(
                        k,
                        auth,
                        executor,
                        &provider,
                        &route_model,
                        &models,
                        pooled,
                        &alias_result,
                        req,
                        &opts,
                        restore_execution_model.then_some(execution_model.as_str()),
                        &mut upstream_err,
                    )
                    .await
                }
                RoundKind::Stream => {
                    self.attempt_stream(
                        auth,
                        executor,
                        &provider,
                        &route_model,
                        models,
                        pooled,
                        &alias_result,
                        req,
                        &opts,
                        restore_execution_model.then_some(execution_model.as_str()),
                        &mut upstream_err,
                    )
                    .await
                }
            };
            match attempt {
                AuthAttempt::Success(o) => return Ok(o),
                AuthAttempt::Return(f) => return Err(f),
                AuthAttempt::Next(fail) => last_err = Some(fail),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn attempt_unary(
        &self,
        kind: Kind,
        mut auth: Auth,
        executor: DynExecutor,
        provider: &str,
        route_model: &str,
        models: &[String],
        pooled: bool,
        alias_result: &AliasResult,
        req: &Request,
        opts: &Options,
        restore_model: Option<&str>,
        upstream_err: &mut Option<Fail>,
    ) -> AuthAttempt {
        let mut auth_err: Option<ExecError> = None;
        let mut did_refresh = false;
        for upstream_model in models {
            let result_model =
                self.state_model_for_execution(&auth, route_model, upstream_model, pooled);
            let mut exec_req = req.clone();
            exec_req.model = restore_model.map_or_else(|| upstream_model.clone(), str::to_string);
            let mut exec_opts = opts.clone();
            let payload: Bytes = if exec_opts.original_request.is_empty() {
                exec_req.payload.clone()
            } else {
                exec_opts.original_request.clone()
            };
            ensure_canonical_session_metadata(
                &mut exec_opts.metadata,
                &exec_opts.headers,
                &payload,
            );
            let requested_alias = requested_model_alias(&exec_opts, route_model);
            match super::plugin_hooks::apply_request_after_auth_interceptor(
                &executor, provider, exec_req, exec_opts, &requested_alias,
            )
            .await
            {
                Ok((r, o)) => {
                    exec_req = r;
                    exec_opts = o;
                }
                Err(e) => return AuthAttempt::Return(Fail::stop(e)),
            }
            let cfg = self.cfg();
            attach_resolved_execution_model_info(
                &cfg,
                &mut exec_req,
                &auth,
                route_model,
                upstream_model,
                restore_model.is_some(),
            );

            let started = Instant::now();
            let mut res =
                call_unary(kind, &executor, &auth, exec_req.clone(), exec_opts.clone()).await;
            let mut latency = started.elapsed();
            if let Err(err) = &res {
                if err.upstream_attempted {
                    *upstream_err = Some(err.clone().into());
                }
                if let Some(refreshed) = self
                    .try_refresh_after_unauthorized(&auth, err, did_refresh)
                    .await
                {
                    auth = refreshed;
                    did_refresh = true;
                    let started = Instant::now();
                    res = call_unary(kind, &executor, &auth, exec_req.clone(), exec_opts.clone())
                        .await;
                    latency = started.elapsed();
                    if let Err(err2) = &res
                        && err2.upstream_attempted
                    {
                        *upstream_err = Some(err2.clone().into());
                    }
                }
            }
            if let Err(err) = &res
                && claude_cancelled(&auth, err)
            {
                return AuthAttempt::Return(err.clone().into());
            }

            let mut result = ExecResult {
                auth_id: auth.id.clone(),
                provider: provider.to_string(),
                model: result_model,
                route_model: route_model.to_string(),
                success: res.is_ok(),
                retry_after: None,
                credential_scope: false,
                error: None,
                options: exec_opts.clone(),
                skip_quota_observation: kind == Kind::Count,
                response_headers: HeaderMap::new(),
            };
            let mut facts = UsageFacts {
                latency,
                stream: false,
                upstream_model: upstream_model.clone(),
                requested_model: requested_model_alias(&exec_opts, route_model),
                ..Default::default()
            };
            match res {
                Err(err) => {
                    result.error = Some(result_error_from_error(&err));
                    result.retry_after = err.retry_after;
                    result.response_headers = err.recorded_headers();
                    if kind == Kind::Execute {
                        result.credential_scope = err.credential_scoped;
                    }
                    let action = rules::match_action(&auth, &err, &cfg);
                    rules::apply_action_to_result(action, &mut result);
                    let neutral = match kind {
                        Kind::Execute => is_responses_compact_availability_neutral(
                            &exec_opts.alt,
                            &err,
                            result.error.as_ref(),
                        ),
                        Kind::Count => {
                            is_count_tokens_endpoint_not_found(&err, &exec_req.model)
                                && result
                                    .error
                                    .as_ref()
                                    .is_none_or(|e| e.code != CODE_FORCE_COOLDOWN)
                        }
                    };
                    if !neutral && kind == Kind::Count && err.credential_scoped {
                        result.credential_scope = true;
                    }
                    let credential_scope = result.credential_scope;
                    // Token counting is not generation traffic: no usage record.
                    let usage_facts = (kind == Kind::Execute).then_some(facts);
                    if neutral {
                        self.record_availability_neutral_result(result, usage_facts);
                    } else {
                        self.mark_result_inner(result, usage_facts);
                    }
                    if action.is_some() {
                        if rules::is_stop(action) {
                            return AuthAttempt::Return(Fail::stop(err));
                        }
                        auth_err = Some(err);
                        if credential_scope {
                            break;
                        }
                        continue;
                    }
                    let compact_fault = kind == Kind::Execute
                        && is_responses_compact_request_fault(&exec_opts.alt, &err);
                    if compact_fault || Failure::of_exec(&err).is_request_invalid() {
                        return AuthAttempt::Return(err.into());
                    }
                    auth_err = Some(err);
                    if credential_scope {
                        break;
                    }
                }
                Ok(mut resp) => {
                    result.response_headers = resp.headers.clone();
                    facts.tokens = tokens_from_response(
                        exec_opts.response_format_or_source(),
                        &resp.payload,
                        &resp.metadata,
                    );
                    self.mark_result_inner(result, (kind == Kind::Execute).then_some(facts));
                    let attempt_alias = resolve_attempt_alias_result(
                        &cfg,
                        &auth,
                        route_model,
                        upstream_model,
                        alias_result,
                    );
                    if attempt_alias.force_mapping
                        && !attempt_alias.original_alias.trim().is_empty()
                    {
                        resp.payload = Bytes::from(rewrite_model_in_response(
                            &resp.payload,
                            attempt_alias.original_alias.trim(),
                        ));
                    }
                    return AuthAttempt::Success(Outcome::Response(resp));
                }
            }
        }
        match auth_err {
            Some(e) => AuthAttempt::Next(e.into()),
            None => AuthAttempt::Next(executor_not_found().into()),
        }
    }
}

pub(crate) async fn call_unary(
    kind: Kind,
    executor: &DynExecutor,
    auth: &Auth,
    req: Request,
    opts: Options,
) -> Result<Response, ExecError> {
    match kind {
        Kind::Execute => executor.execute(auth, req, opts).await,
        Kind::Count => executor.count_tokens(auth, req, opts).await,
    }
}

/// A one-chunk stream carrying only the error, with the failed upstream's headers.
pub(crate) fn stream_error_result(headers: HeaderMap, err: ExecError) -> StreamResult {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    // The channel has capacity for the single error chunk, so this cannot block or fail.
    let _ = tx.try_send(Err(err));
    StreamResult::new(headers, rx)
}

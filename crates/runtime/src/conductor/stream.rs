//! Streaming attempts (Go: conductor_stream.go).
//!
//! Failover is only possible before the first payload chunk. `stream_with_model_pool` reads the
//! stream up to that chunk (the "bootstrap"): an error or empty stream there fails the attempt and
//! the caller moves on to the next pooled model / credential. Once the first payload is buffered
//! the stream is handed to a wrapper task that forwards chunks, rewrites force-mapped model names,
//! tracks usage, and records exactly one result (failure on the first error chunk, success on a
//! clean end, no result when the client hung up). Executor-published usage records are recorded as
//! they appear, whether or not the client is still reading.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_auth::Auth;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::executor::Metadata;

use super::Manager;
use super::cooldown::ExecResult;
use super::errors::{Failure, empty_stream, result_error_from_error};
use super::exec::{
    AuthAttempt, Fail, Outcome, ensure_canonical_session_metadata, is_claude_oauth, preferred,
    publish_selected_auth_metadata, requested_model_alias,
};
use super::models::{
    AliasResult, attach_resolved_execution_model_info, resolve_attempt_alias_result,
};
use super::rewriter::StreamRewriter;
use super::rules;
use super::usage::{StreamUsage, UsageFacts};
use crate::usage_report::UsageCollector;
use crate::executor::{DynExecutor, ExecError, Options, Request, StreamResult};

type Chunk = Result<Bytes, ExecError>;

/// Home-dispatched attempt context: results go to Home instead of local auth state and the
/// attempt can be cancelled when its selection ends.
pub(crate) struct HomeStreamCtx {
    pub cancel: std::sync::Arc<cpa_home::conn::Kill>,
}

/// Starts the upstream stream, abandoning it when a Home attempt is cancelled.
async fn start_stream(
    executor: &DynExecutor,
    auth: &Auth,
    req: Request,
    opts: Options,
    home: Option<&HomeStreamCtx>,
) -> Result<StreamResult, ExecError> {
    let Some(home) = home else {
        return executor.execute_stream(auth, req, opts).await;
    };
    tokio::select! {
        _ = home.cancel.wait() => {
            let mut e = ExecError::new(0, "context canceled");
            e.upstream_attempted = false;
            Err(e)
        }
        r = executor.execute_stream(auth, req, opts) => r,
    }
}

/// Reads chunks until the first non-empty payload. `Ok((buffered, closed))`: `closed` means the
/// channel ended before any payload; `Err` is a bootstrap failure.
async fn read_stream_bootstrap(
    rx: &mut mpsc::Receiver<Chunk>,
) -> Result<(Vec<Bytes>, bool), ExecError> {
    let mut buffered = Vec::with_capacity(1);
    loop {
        match rx.recv().await {
            None => return Ok((buffered, true)),
            Some(Err(e)) => return Err(e),
            Some(Ok(bytes)) => {
                let has_payload = !bytes.is_empty();
                buffered.push(bytes);
                if has_payload {
                    return Ok((buffered, false));
                }
            }
        }
    }
}

fn bootstrap_fail(err: ExecError, headers: &http::HeaderMap) -> Fail {
    Fail {
        err,
        stop: false,
        bootstrap_headers: Some(headers.clone()),
    }
}

impl Manager {
    /// Records an attempt's outcome: locally for normal dispatch, to Home for ephemeral ones.
    fn record_attempt(&self, ephemeral: bool, auth: &Auth, result: ExecResult, facts: UsageFacts) {
        if ephemeral {
            self.report_home_result(result, Some(auth), Some(facts));
        } else {
            self.mark_result_inner(result, Some(facts));
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn attempt_stream(
        &self,
        auth: Auth,
        executor: DynExecutor,
        provider: &str,
        route_model: &str,
        models: Vec<String>,
        pooled: bool,
        alias_result: &AliasResult,
        req: &Request,
        opts: &Options,
        restore_model: Option<&str>,
        upstream_err: &mut Option<Fail>,
    ) -> AuthAttempt {
        let res = self
            .stream_with_model_pool(
                &executor,
                auth.clone(),
                provider,
                req,
                opts,
                route_model,
                restore_model,
                &models,
                pooled,
                alias_result,
                None,
            )
            .await;
        match res {
            Ok(stream) => AuthAttempt::Success(Outcome::Stream(stream)),
            Err(fail) => {
                if fail.err.upstream_attempted {
                    *upstream_err = Some(fail.clone());
                }
                if fail.stop {
                    return AuthAttempt::Return(fail);
                }
                let action = rules::match_action(&auth, &fail.err, &self.cfg());
                if action.is_some() {
                    if rules::is_stop(action) {
                        return AuthAttempt::Return(Fail::stop(fail.err));
                    }
                    return AuthAttempt::Next(fail);
                }
                if Failure::of_exec(&fail.err).is_request_invalid() {
                    return AuthAttempt::Return(fail);
                }
                AuthAttempt::Next(fail)
            }
        }
    }

    /// Tries each pooled upstream model of one credential until a stream bootstraps (Go:
    /// executeStreamWithModelPool, non-Home).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn stream_with_model_pool(
        &self,
        executor: &DynExecutor,
        mut auth: Auth,
        provider: &str,
        req: &Request,
        opts: &Options,
        route_model: &str,
        execution_model: Option<&str>,
        exec_models: &[String],
        pooled: bool,
        alias_result: &AliasResult,
        home: Option<&HomeStreamCtx>,
    ) -> Result<StreamResult, Fail> {
        let ephemeral = home.is_some();
        let cfg = self.cfg();
        let mut last_err: Option<ExecError> = None;
        let mut upstream_err: Option<Fail> = None;
        let mut did_refresh = false;
        for (idx, exec_model) in exec_models.iter().enumerate() {
            let result_model =
                self.state_model_for_execution(&auth, route_model, exec_model, pooled);
            let mut exec_req = req.clone();
            exec_req.model = execution_model.map_or_else(|| exec_model.clone(), str::to_string);
            let mut exec_opts = opts.clone();
            attach_resolved_execution_model_info(
                &cfg,
                &mut exec_req,
                &auth,
                route_model,
                exec_model,
                execution_model.is_some(),
            );
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
                executor, provider, exec_req, exec_opts, &requested_alias,
            )
            .await
            {
                Ok((r, o)) => {
                    exec_req = r;
                    exec_opts = o;
                }
                Err(e) => return Err(Fail::stop(e)),
            }

            let usage = UsageCollector::new();
            exec_opts.usage_collector = Some(usage.clone());
            let started = Instant::now();
            let make_result = |auth: &Auth,
                               error: &ExecError,
                               credential_scope: bool,
                               opts: &Options| ExecResult {
                auth_id: auth.id.clone(),
                provider: provider.to_string(),
                model: result_model.clone(),
                route_model: route_model.to_string(),
                success: false,
                retry_after: error.retry_after,
                credential_scope,
                error: Some(result_error_from_error(error)),
                options: opts.clone(),
                skip_quota_observation: false,
                response_headers: error.recorded_headers(),
            };
            let facts = |started: Instant, tokens| UsageFacts {
                latency: started.elapsed(),
                stream: true,
                upstream_model: exec_model.clone(),
                requested_model: requested_model_alias(opts, route_model),
                tokens,
                reports: usage.take(),
                ..Default::default()
            };

            let mut res = start_stream(executor, &auth, exec_req.clone(), exec_opts.clone(), home).await;
            if let Err(err) = &res {
                if err.upstream_attempted {
                    upstream_err = Some(err.clone().into());
                }
                // Home-dispatched credentials are never refreshed locally.
                let refreshed = if ephemeral {
                    None
                } else {
                    self.try_refresh_after_unauthorized(&auth, err, did_refresh).await
                };
                if let Some(refreshed) = refreshed {
                    auth = refreshed;
                    did_refresh = true;
                    publish_selected_auth_metadata(&mut exec_opts, &auth);
                    res = start_stream(executor, &auth, exec_req.clone(), exec_opts.clone(), home).await;
                    if let Err(e2) = &res
                        && e2.upstream_attempted
                    {
                        upstream_err = Some(e2.clone().into());
                    }
                }
            }
            if let Err(err) = &res
                && !ephemeral
                && super::exec::claude_cancelled(&auth, err)
            {
                // No result mark, but the reporter's failure record still counts.
                let result = make_result(&auth, err, false, &exec_opts);
                self.record_usage_only(&result, Some(&auth), facts(started, Default::default()));
                return Err(err.clone().into());
            }
            let mut stream = match res {
                Ok(s) => s,
                Err(err) => {
                    let credential_scope = err.credential_scoped;
                    let mut result = make_result(&auth, &err, credential_scope, &exec_opts);
                    let action = rules::match_action(&auth, &err, &cfg);
                    rules::apply_action_to_result(action, &mut result);
                    let credential_scope = result.credential_scope;
                    self.record_attempt(ephemeral, &auth, result, facts(started, Default::default()));
                    if action.is_some() {
                        if rules::is_stop(action) {
                            return Err(Fail::stop(err));
                        }
                        last_err = Some(err.clone());
                        if credential_scope {
                            return Err(preferred(err.into(), upstream_err.as_ref()));
                        }
                        continue;
                    }
                    if Failure::of_exec(&err).is_request_invalid() {
                        return Err(err.into());
                    }
                    last_err = Some(err.clone());
                    if credential_scope {
                        return Err(preferred(err.into(), upstream_err.as_ref()));
                    }
                    continue;
                }
            };

            let mut boot = read_stream_bootstrap(&mut stream.chunks).await;
            if let Err(e) = &boot
                && e.upstream_attempted
            {
                upstream_err = Some(bootstrap_fail(e.clone(), &stream.headers));
            }
            if let Err(boot_err) = &boot {
                let refreshed = if ephemeral {
                    None
                } else {
                    self.try_refresh_after_unauthorized(&auth, boot_err, did_refresh).await
                };
                if let Some(refreshed) = refreshed {
                    drop(std::mem::replace(&mut stream.chunks, mpsc::channel(1).1));
                    auth = refreshed;
                    did_refresh = true;
                    publish_selected_auth_metadata(&mut exec_opts, &auth);
                    match start_stream(executor, &auth, exec_req.clone(), exec_opts.clone(), home).await {
                        Err(retry_err) => {
                            if retry_err.upstream_attempted {
                                upstream_err = Some(retry_err.clone().into());
                            }
                            boot = Err(retry_err);
                            stream = StreamResult::new(Default::default(), mpsc::channel(1).1);
                        }
                        Ok(retry_stream) => {
                            stream = retry_stream;
                            boot = read_stream_bootstrap(&mut stream.chunks).await;
                        }
                    }
                    if let Err(e) = &boot
                        && e.upstream_attempted
                    {
                        upstream_err = Some(bootstrap_fail(e.clone(), &stream.headers));
                    }
                }
            }
            if let Err(e) = &boot
                && !ephemeral
                && super::exec::claude_cancelled(&auth, e)
            {
                let result = make_result(&auth, e, false, &exec_opts);
                self.record_usage_only(&result, Some(&auth), facts(started, Default::default()));
                return Err(e.clone().into());
            }

            let (buffered, closed) = match boot {
                Err(boot_err) => {
                    let action = rules::match_action(&auth, &boot_err, &cfg);
                    let credential_scope = boot_err.credential_scoped;
                    let mut result = make_result(&auth, &boot_err, credential_scope, &exec_opts);
                    rules::apply_action_to_result(action, &mut result);
                    let credential_scope = result.credential_scope;
                    let record = |m: &Manager, result: ExecResult| {
                        m.record_attempt(ephemeral, &auth, result, facts(started, Default::default()))
                    };
                    if action.is_some() {
                        record(self, result);
                        if rules::is_stop(action) {
                            return Err(Fail::stop(boot_err));
                        }
                        last_err = Some(boot_err.clone());
                        if credential_scope {
                            return Err(preferred(
                                bootstrap_fail(boot_err, &stream.headers),
                                upstream_err.as_ref(),
                            ));
                        }
                        continue;
                    }
                    if Failure::of_exec(&boot_err).is_request_invalid() {
                        record(self, result);
                        return Err(boot_err.into());
                    }
                    record(self, result);
                    if idx + 1 < exec_models.len() {
                        last_err = Some(boot_err.clone());
                        if credential_scope {
                            return Err(preferred(
                                bootstrap_fail(boot_err, &stream.headers),
                                upstream_err.as_ref(),
                            ));
                        }
                        continue;
                    }
                    return Err(preferred(
                        bootstrap_fail(boot_err, &stream.headers),
                        upstream_err.as_ref(),
                    ));
                }
                Ok(v) => v,
            };

            if closed && buffered.is_empty() {
                let empty = empty_stream("upstream stream closed before first payload");
                let current = bootstrap_fail(empty.clone(), &stream.headers);
                upstream_err = Some(current.clone());
                let mut result = make_result(&auth, &empty, false, &exec_opts);
                result.retry_after = None;
                self.record_attempt(ephemeral, &auth, result, facts(started, Default::default()));
                if idx + 1 < exec_models.len() {
                    last_err = Some(empty);
                    continue;
                }
                return Err(preferred(current, upstream_err.as_ref()));
            }

            let attempt_alias =
                resolve_attempt_alias_result(&cfg, &auth, route_model, exec_model, alias_result);
            let wrap = WrapCtx {
                manager: self.clone(),
                auth_id: auth.id.clone(),
                provider: provider.to_string(),
                result_model,
                route_model: route_model.to_string(),
                upstream_model: exec_model.clone(),
                requested_model: requested_model_alias(opts, route_model),
                options: exec_opts,
                alias: attempt_alias,
                started,
                response_headers: stream.headers.clone(),
                cfg: cfg.clone(),
                claude_oauth: !ephemeral && is_claude_oauth(&auth),
                home_auth: ephemeral.then(|| auth.clone()),
            };
            return Ok(wrap_stream(
                wrap,
                stream.headers,
                buffered,
                if closed { None } else { Some(stream.chunks) },
                stream.usage,
            ));
        }
        let err = last_err.unwrap_or_else(|| {
            let mut e = super::errors::auth_not_found("no upstream model available");
            e.upstream_attempted = false;
            e
        });
        Err(preferred(err.into(), upstream_err.as_ref()))
    }
}

struct WrapCtx {
    manager: Manager,
    auth_id: String,
    provider: String,
    result_model: String,
    route_model: String,
    upstream_model: String,
    requested_model: String,
    options: Options,
    alias: AliasResult,
    started: Instant,
    /// Upstream response headers, for passive quota observation.
    response_headers: http::HeaderMap,
    cfg: std::sync::Arc<cpa_config::Config>,
    /// Claude OAuth credentials record no success for a stream the client abandoned.
    claude_oauth: bool,
    /// Home-dispatched attempt: the credential snapshot results are reported with.
    home_auth: Option<Auth>,
}

/// Forwards the bootstrapped stream, then records one result.
fn wrap_stream(
    ctx: WrapCtx,
    headers: http::HeaderMap,
    buffered: Vec<Bytes>,
    remaining: Option<mpsc::Receiver<Chunk>>,
    executor_usage: Option<tokio::sync::oneshot::Receiver<Value>>,
) -> StreamResult {
    let (tx, rx) = mpsc::channel::<Chunk>(32);
    tokio::spawn(async move {
        let WrapCtx {
            manager,
            auth_id,
            provider,
            result_model,
            route_model,
            upstream_model,
            requested_model,
            options,
            alias,
            started,
            response_headers,
            cfg,
            claude_oauth,
            home_auth,
        } = ctx;
        let mut rewriter = (alias.force_mapping && !alias.original_alias.trim().is_empty())
            .then(|| StreamRewriter::new(alias.original_alias.trim()));
        let mut usage = StreamUsage::new(options.response_format_or_source());
        let reports = options.usage_collector.clone().unwrap_or_default();
        let mut ttft: Option<Duration> = None;
        let mut failed = false;
        let mut client_gone = false;
        let mut send_failed = false;
        // Some report was already recorded (per-response records of a long stream): the final
        // mark must not add a response-derived fallback record on top.
        let mut published_any = false;
        let mut pending: VecDeque<Chunk> = buffered.into_iter().map(Ok).collect();
        let mut remaining = remaining;

        // Records the reports published so far as success events, independent of the result mark
        // and of whether the client is still reading (Go publishes each response's usage when it
        // completes, and deferred on exit). Returns whether anything was recorded.
        let drain_reports = |manager: &Manager, ttft: Option<Duration>| -> bool {
            let recs = reports.take();
            if recs.is_empty() {
                return false;
            }
            let auth = home_auth.clone().or_else(|| manager.get(&auth_id));
            let result = ExecResult {
                auth_id: auth_id.clone(),
                provider: provider.clone(),
                model: result_model.clone(),
                route_model: route_model.clone(),
                success: true,
                retry_after: None,
                credential_scope: false,
                error: None,
                options: options.clone(),
                skip_quota_observation: false,
                response_headers: response_headers.clone(),
            };
            let facts = UsageFacts {
                latency: started.elapsed(),
                ttft,
                stream: true,
                upstream_model: upstream_model.clone(),
                requested_model: requested_model.clone(),
                reports: recs,
                ..Default::default()
            };
            manager.record_usage_only(&result, auth.as_ref(), facts);
            true
        };

        let record_failure =
            |manager: &Manager, err: &ExecError, usage: &StreamUsage, ttft: Option<Duration>, published_any: bool| {
                let auth = home_auth.clone().or_else(|| manager.get(&auth_id));
                let mut result = ExecResult {
                    auth_id: auth_id.clone(),
                    provider: provider.clone(),
                    model: result_model.clone(),
                    route_model: route_model.clone(),
                    success: false,
                    retry_after: err.retry_after,
                    credential_scope: err.credential_scoped,
                    error: Some(result_error_from_error(err)),
                    options: options.clone(),
                    skip_quota_observation: false,
                    response_headers: if err.headers.is_empty() {
                        response_headers.clone()
                    } else {
                        err.headers.clone()
                    },
                };
                if let Some(auth) = auth {
                    let action = rules::match_action(&auth, err, &cfg);
                    rules::apply_action_to_result(action, &mut result);
                }
                let recs = reports.take();
                let facts = (!recs.is_empty() || !published_any).then(|| UsageFacts {
                    latency: started.elapsed(),
                    ttft,
                    stream: true,
                    tokens: usage.tokens.clone(),
                    upstream_model: upstream_model.clone(),
                    requested_model: requested_model.clone(),
                    reports: recs,
                });
                match &home_auth {
                    Some(a) => manager.report_home_result(result, Some(a), facts),
                    None => manager.mark_result_inner(result, facts),
                }
            };

        loop {
            published_any |= drain_reports(&manager, ttft);
            let item = match pending.pop_front() {
                Some(i) => i,
                None => match remaining.as_mut() {
                    // Waiting on an idle upstream: notice the client leaving so the upstream
                    // stream is dropped (closed) promptly instead of at its next chunk.
                    Some(rx) => tokio::select! {
                        _ = tx.closed() => {
                            client_gone = true;
                            break;
                        }
                        item = rx.recv() => match item {
                            Some(i) => i,
                            None => break,
                        },
                    },
                    None => break,
                },
            };
            match item {
                Err(err) => {
                    if !failed {
                        failed = true;
                        record_failure(&manager, &err, &usage, ttft, published_any);
                    }
                    if tx.send(Err(err)).await.is_err() {
                        send_failed = true;
                        break;
                    }
                }
                Ok(payload) => {
                    if payload.is_empty() {
                        continue;
                    }
                    if ttft.is_none() {
                        ttft = Some(started.elapsed());
                    }
                    // Executors with a reporter publish exact usage; the scan is only the
                    // fallback for those without one.
                    if !reports.has_reporter() {
                        usage.observe(&payload);
                    }
                    published_any |= drain_reports(&manager, ttft);
                    let payload = match rewriter.as_mut() {
                        Some(r) => Bytes::from(r.rewrite_payload(&payload)),
                        None => payload,
                    };
                    if payload.is_empty() {
                        continue;
                    }
                    if tx.send(Ok(payload)).await.is_err() {
                        send_failed = true;
                        break;
                    }
                }
            }
        }
        drop(remaining);
        if !send_failed
            && !client_gone
            && let Some(r) = rewriter.as_mut()
            && let Some(tail) = r.finish()
            && !tail.is_empty()
            && tx.send(Ok(Bytes::from(tail))).await.is_err()
        {
            send_failed = true;
        }
        if send_failed {
            // The client hung up while sending: no result mark, but published usage counts.
            drain_reports(&manager, ttft);
            return;
        }
        if failed {
            return;
        }
        if client_gone && claude_oauth {
            // Claude OAuth records no success for an abandoned stream, only its usage.
            drain_reports(&manager, ttft);
            return;
        }
        let recs = reports.take();
        if recs.is_empty()
            && !published_any
            && let Some(mut rx) = executor_usage
            && let Ok(u) = rx.try_recv()
        {
            let mut meta = Metadata::new();
            meta.insert(super::usage::META_USAGE.to_string(), u);
            let t = super::usage::tokens_from_response(options.response_format_or_source(), b"", &meta);
            if t != Default::default() {
                usage.tokens = t;
            }
        }
        let result = ExecResult {
            auth_id,
            provider,
            model: result_model,
            route_model,
            success: true,
            retry_after: None,
            credential_scope: false,
            error: None,
            options,
            skip_quota_observation: false,
            response_headers,
        };
        // Reports already recorded as they were published need no fallback record on top.
        let facts = (!recs.is_empty() || !published_any).then(|| UsageFacts {
            latency: started.elapsed(),
            ttft,
            stream: true,
            tokens: usage.tokens,
            upstream_model,
            requested_model,
            reports: recs,
        });
        match &home_auth {
            Some(a) => manager.report_home_result(result, Some(a), facts),
            None => manager.mark_result_inner(result, facts),
        }
    });
    StreamResult::new(headers, rx)
}

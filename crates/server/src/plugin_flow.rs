//! Plugin-aware execution flows (Go: `executeWithAuthManagerFormats`,
//! `executeCountWithAuthManager`, `executeStreamWithAuthManagerFormats` and
//! `streamWithPluginExecutor`): the same pipeline as the plain one in `exec.rs` with model
//! routing, interceptors and lifecycle notifications around it.

use std::sync::Arc;

use bytes::Bytes;
use cpa_core::format::Format;
use cpa_executors::helps::usage::StreamUsageBuffer;
use cpa_plugin::usage_helpers::observe_plugin_executor_stream_usage;
use cpa_runtime::executor::StreamResult;
use tokio::sync::mpsc;

use crate::error::{ErrorMessage, enrich_auth_selection_error, exec_error_message, is_selection_error};
use crate::exec::{
    ExecArgs, ExecOk, ExecStream, Initial, Pipeline, adjust_providers_for_entry, bootstrap_eligible, request_details,
    validate_payload,
};
use crate::plugin_exec::{AfterAuthCapture, Lifecycle, PluginCx, RouteDecision, StreamTransform};
use crate::sse_validate::SseJsonValidator;

fn native_interactions_error() -> ErrorMessage {
    ErrorMessage::new(400, "agent is only supported for native interactions execution")
}

/// `validateNativeInteractionsExecution`.
fn validate_native_interactions(a: &ExecArgs<'_>, route: &RouteDecision) -> Result<(), ErrorMessage> {
    let forced = a.forced_provider.map(|p| p.trim().to_lowercase()).unwrap_or_default();
    if forced.is_empty() || a.entry != Format::Interactions {
        return Ok(());
    }
    if !route.executor_plugin_id.is_empty() {
        return Err(native_interactions_error());
    }
    let route_provider = route.provider.trim().to_lowercase();
    if !route_provider.is_empty() && route_provider != forced {
        return Err(native_interactions_error());
    }
    Ok(())
}

impl Pipeline {
    /// `providersForExecution` with a model router decision.
    fn providers_routed(&self, a: &ExecArgs<'_>, route: &RouteDecision) -> Result<(Vec<String>, String), ErrorMessage> {
        let forced = a.forced_provider.map(|p| p.trim().to_lowercase()).unwrap_or_default();
        let original = a.model;
        if !forced.is_empty() {
            if !route.executor_plugin_id.is_empty() {
                return Err(native_interactions_error());
            }
            let rp = route.provider.trim().to_lowercase();
            if !rp.is_empty() && rp != forced {
                return Err(native_interactions_error());
            }
            let mut normalized = a.model.trim().to_string();
            if normalized.is_empty() {
                normalized = original.trim().to_string();
            }
            crate::exec::validate_image_only_model(&normalized, a.allow_image_model)?;
            return Ok((vec![forced], normalized));
        }
        if !route.provider.is_empty() {
            let normalized = if route.model.is_empty() { original.to_string() } else { route.model.clone() };
            crate::exec::validate_image_only_model(&normalized, a.allow_image_model)?;
            return Ok((vec![route.provider.clone()], normalized));
        }
        request_details(a.model, a.allow_image_model)
    }

    fn plugin_providers(&self, a: &ExecArgs<'_>, route: &RouteDecision) -> Result<(Vec<String>, String), ErrorMessage> {
        let (providers, normalized) = self.providers_routed(a, route)?;
        Ok((adjust_providers_for_entry(a.entry, providers), normalized))
    }

    /// Non-stream and count execution with plugins.
    pub(crate) async fn execute_plugins(&self, pcx: &PluginCx, a: ExecArgs<'_>, count: bool) -> Result<ExecOk, ErrorMessage> {
        let original = a.model.to_string();
        let route = self.apply_model_router(pcx, &a, false).await;
        if !count {
            validate_native_interactions(&a, &route)?;
        }
        let response_protocol = a.exit.unwrap_or(a.entry);
        if !route.executor_plugin_id.is_empty() {
            return if count {
                self.count_with_plugin_executor(pcx, &a, &route.executor_plugin_id).await
            } else {
                self.execute_with_plugin_executor(pcx, &a, &route.executor_plugin_id).await
            };
        }
        let (providers, normalized) = self.plugin_providers(&a, &route)?;
        let (req, mut opts) = self.build_request(&a, &normalized, false, count);
        let capture = Arc::new(AfterAuthCapture::default());
        let lifecycle = Lifecycle::new(pcx, a.entry, &normalized, &original, false, &opts.metadata);
        opts.request_after_auth = self.after_auth_interceptor(pcx, capture.clone(), lifecycle.request_id.clone());
        opts.websocket_response_observer = self.ws_observer(pcx, lifecycle.request_id.clone());
        let (req, opts) = match self.intercept_before_auth(pcx, a.entry, &original, &lifecycle.request_id, req, opts).await {
            Ok(v) => v,
            Err(e) => {
                lifecycle.complete_error(&e);
                return Err(e);
            }
        };
        let result = if count {
            self.state.manager.execute_count(&providers, req.clone(), opts.clone()).await
        } else {
            self.state.manager.execute(&providers, req.clone(), opts.clone()).await
        };
        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                let msg = exec_error_message(&enrich_auth_selection_error(&e, &providers, &normalized));
                lifecycle.complete_error(&msg);
                return Err(msg);
            }
        };
        let (executed_req, executed_opts) = capture.apply(req, opts);
        let passthrough = a.internal_source || self.settings.passthrough_headers;
        let handler_type = if count { a.entry } else { response_protocol };
        let (body, headers) = self
            .apply_response_interceptors(
                pcx,
                &lifecycle.request_id,
                handler_type,
                &normalized,
                &original,
                &executed_opts,
                &resp.headers,
                &executed_opts.original_request,
                &executed_req.payload,
                resp.payload,
                passthrough,
            )
            .await;
        lifecycle.complete("succeeded", 200, None);
        Ok(ExecOk { body, headers })
    }

    /// `streamWithPluginExecutor`.
    async fn stream_with_plugin_executor(&self, pcx: &PluginCx, a: &ExecArgs<'_>, executor_plugin_id: &str) -> ExecStream {
        let response_protocol = a.exit.unwrap_or(a.entry);
        let (req, opts) = self.plugin_executor_request(pcx, a, true, response_protocol);
        let lifecycle = Lifecycle::new(pcx, a.entry, a.model, a.model, true, &opts.metadata);
        let (req, opts) = match self.intercept_before_auth(pcx, a.entry, a.model, &lifecycle.request_id, req, opts).await {
            Ok(v) => v,
            Err(e) => {
                lifecycle.complete_error(&e);
                return ExecStream::failed(e);
            }
        };
        let (req, opts) = match self.intercept_after_plugin_route(pcx, executor_plugin_id, a.entry, a.model, &lifecycle.request_id, req, opts).await {
            Ok(v) => v,
            Err(e) => {
                lifecycle.complete_error(&e);
                return ExecStream::failed(e);
            }
        };
        let reporter = (!a.internal_source).then(|| {
            let r = self.route_usage_reporter(executor_plugin_id, a, &opts);
            r.set_translated_reasoning_effort(&req.payload, a.entry.as_str());
            r
        });
        let stream = match pcx.host.execute_plugin_executor_stream(&pcx.ctx, executor_plugin_id, req.clone(), opts.clone()).await {
            Ok(s) => s,
            Err(e) => {
                if let Some(r) = &reporter
                    && !pcx.ctx.has_nested()
                {
                    r.publish_failure(&e);
                }
                let msg = exec_error_message(&e);
                lifecycle.complete_error(&msg);
                return ExecStream::failed(msg);
            }
        };
        let passthrough = a.internal_source || self.settings.passthrough_headers;
        let mut transform = StreamTransform::new(pcx, &lifecycle.request_id, response_protocol, a.model, a.model, &stream.headers);
        transform.set_request(&req, &opts);
        transform.init_headers().await;
        let headers = transform.downstream_headers(passthrough);
        let new_validator = || (response_protocol == Format::OpenAIResponse).then(SseJsonValidator::default);
        let validator = new_validator();
        let (tx, rx) = mpsc::channel(1);
        let pcx2 = pcx.clone();
        let protocol = response_protocol;
        tokio::spawn(async move {
            let mut usage = StreamUsageBuffer::default();
            let mut stream = stream;
            let mut validator = validator;
            let mut outcome: (&str, i64, Option<String>) = ("succeeded", 200, None);
            loop {
                let next = tokio::select! {
                    i = stream.chunks.recv() => i,
                    () = tx.closed() => { outcome = ("canceled", 0, Some("context canceled".into())); break; }
                };
                let Some(item) = next else {
                    if let Some(v) = validator.as_mut()
                        && let Err(msg) = v.finish()
                    {
                        outcome = ("failed", 502, Some(msg.clone()));
                        let _ = tx.send(Err(ErrorMessage::new(502, msg))).await;
                    }
                    break;
                };
                match item {
                    Err(err) => {
                        let msg = exec_error_message(&err);
                        outcome = ("failed", i64::from(msg.status_or_500()), Some(err.message.clone()));
                        let _ = tx.send(Err(msg)).await;
                        break;
                    }
                    Ok(chunk) => {
                        if chunk.is_empty() {
                            continue;
                        }
                        observe_plugin_executor_stream_usage(protocol.as_str(), &chunk, &mut usage);
                        let Some(payload) = transform.transform(chunk).await else { continue };
                        let payload = match validate_payload(&mut validator, payload) {
                            Ok(Some(p)) => p,
                            Ok(None) => continue,
                            Err(e) => {
                                outcome = ("failed", 502, Some(e.text.clone()));
                                let _ = tx.send(Err(e)).await;
                                break;
                            }
                        };
                        if tx.send(Ok(payload.clone())).await.is_err() {
                            outcome = ("canceled", 0, Some("context canceled".into()));
                            break;
                        }
                        transform.delivered(&payload);
                    }
                }
            }
            lifecycle.complete(outcome.0, outcome.1, outcome.2.as_deref());
            if let Some(r) = reporter
                && !pcx2.ctx.has_nested()
            {
                if outcome.0 != "succeeded" {
                    let e = cpa_runtime::executor::ExecError::new(0, outcome.2.clone().unwrap_or_default());
                    r.publish_buffer_failure(&usage, &e);
                } else {
                    r.publish_buffer(&usage);
                    r.ensure_published();
                }
            }
        });
        ExecStream { headers, rx }
    }

    /// Streaming execution with plugins.
    pub(crate) async fn execute_stream_plugins(&self, pcx: &PluginCx, a: ExecArgs<'_>) -> ExecStream {
        let original = a.model.to_string();
        let route = self.apply_model_router(pcx, &a, true).await;
        if let Err(e) = validate_native_interactions(&a, &route) {
            return ExecStream::failed(e);
        }
        let response_protocol = a.exit.unwrap_or(a.entry);
        if !route.executor_plugin_id.is_empty() {
            return self.stream_with_plugin_executor(pcx, &a, &route.executor_plugin_id).await;
        }
        let (providers, normalized) = match self.plugin_providers(&a, &route) {
            Ok(v) => v,
            Err(e) => return ExecStream::failed(e),
        };
        let (req, mut opts) = self.build_request(&a, &normalized, true, false);
        let capture = Arc::new(AfterAuthCapture::default());
        let lifecycle = Lifecycle::new(pcx, a.entry, &normalized, &original, true, &opts.metadata);
        opts.request_after_auth = self.after_auth_interceptor(pcx, capture.clone(), lifecycle.request_id.clone());
        opts.websocket_response_observer = self.ws_observer(pcx, lifecycle.request_id.clone());
        let (req, opts) = match self.intercept_before_auth(pcx, a.entry, &original, &lifecycle.request_id, req, opts).await {
            Ok(v) => v,
            Err(e) => {
                lifecycle.complete_error(&e);
                return ExecStream::failed(e);
            }
        };
        let enrich = |e: &cpa_runtime::executor::ExecError| enrich_auth_selection_error(e, &providers, &normalized);
        let first = self.state.manager.execute_stream(&providers, req.clone(), opts.clone()).await;
        let mut stream: StreamResult = match first {
            Ok(s) => s,
            Err(e) => {
                let msg = exec_error_message(&enrich(&e));
                lifecycle.complete_error(&msg);
                return ExecStream::failed(msg);
            }
        };
        let passthrough = a.internal_source || self.settings.passthrough_headers;
        let mut transform = StreamTransform::new(pcx, &lifecycle.request_id, response_protocol, &normalized, &original, &stream.headers);
        let new_validator = || (response_protocol == Format::OpenAIResponse).then(SseJsonValidator::default);
        let mut validator = new_validator();
        let max_retries = self.settings.bootstrap_retries;
        let mut retries = 0u32;
        let mut bootstrap_payload: Option<Bytes> = None;
        let mut bootstrap_err: Option<ErrorMessage> = None;
        loop {
            let (er, eo) = capture.apply(req.clone(), opts.clone());
            transform.set_request(&er, &eo);
            match read_initial_t(&mut stream, &mut validator, &mut transform).await {
                Initial::Payload(p) => {
                    bootstrap_payload = Some(p);
                    break;
                }
                Initial::Closed => break,
                Initial::Rejected(err) => {
                    bootstrap_err = Some(err);
                    break;
                }
                Initial::Failed(err) => {
                    if retries >= max_retries || !bootstrap_eligible(err.status) {
                        bootstrap_err = Some(exec_error_message(&err));
                        break;
                    }
                    retries += 1;
                    match self.state.manager.execute_stream(&providers, req.clone(), opts.clone()).await {
                        Err(retry_err) => {
                            let original_err = exec_error_message(&err);
                            bootstrap_err = Some(if is_selection_error(&retry_err) && original_err.status_or_500() >= 500 {
                                original_err
                            } else {
                                exec_error_message(&enrich(&retry_err))
                            });
                            break;
                        }
                        Ok(next) => {
                            transform.reset(&next.headers);
                            stream = next;
                            validator = new_validator();
                        }
                    }
                }
            }
        }
        let headers = transform.downstream_headers(passthrough);
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            if let Some(err) = bootstrap_err {
                let outcome = if err.direct.is_some() { "rejected" } else { "failed" };
                lifecycle.complete(outcome, i64::from(err.status), Some(&err.text));
                let _ = tx.send(Err(err)).await;
                return;
            }
            if let Some(payload) = bootstrap_payload {
                let sent = tokio::select! {
                    r = tx.send(Ok(payload.clone())) => r.is_ok(),
                    () = tx.closed() => false,
                };
                if !sent {
                    lifecycle.canceled();
                    return;
                }
                transform.delivered(&payload);
            }
            forward_rest_t(stream, validator, transform, tx, lifecycle).await;
        });
        ExecStream { headers, rx }
    }
}

/// `readInitialStreamChunks` with the stream interceptors.
async fn read_initial_t(stream: &mut StreamResult, validator: &mut Option<SseJsonValidator>, transform: &mut StreamTransform) -> Initial {
    loop {
        match stream.chunks.recv().await {
            None => {
                transform.init_headers().await;
                return Initial::Closed;
            }
            Some(Err(err)) => return Initial::Failed(err),
            Some(Ok(chunk)) => {
                if chunk.is_empty() {
                    continue;
                }
                let Some(payload) = transform.transform(chunk).await else { continue };
                match validate_payload(validator, payload) {
                    Ok(Some(p)) => return Initial::Payload(p),
                    Ok(None) => continue,
                    Err(e) => return Initial::Rejected(e),
                }
            }
        }
    }
}

/// Post-bootstrap pump with interceptors and the lifecycle outcome.
async fn forward_rest_t(
    mut stream: StreamResult,
    mut validator: Option<SseJsonValidator>,
    mut transform: StreamTransform,
    tx: mpsc::Sender<Result<Bytes, ErrorMessage>>,
    lifecycle: Arc<Lifecycle>,
) {
    loop {
        let next = tokio::select! {
            item = stream.chunks.recv() => item,
            () = tx.closed() => { lifecycle.canceled(); return; }
        };
        let Some(item) = next else {
            if let Some(v) = validator.as_mut()
                && let Err(msg) = v.finish()
            {
                lifecycle.complete("failed", 502, Some(&msg));
                let _ = tx.send(Err(ErrorMessage::new(502, msg))).await;
                return;
            }
            lifecycle.complete("succeeded", 200, None);
            return;
        };
        match item {
            Err(err) => {
                let msg = exec_error_message(&err);
                lifecycle.complete("failed", i64::from(msg.status_or_500()), Some(&err.message));
                let _ = tx.send(Err(msg)).await;
                return;
            }
            Ok(chunk) => {
                if chunk.is_empty() {
                    continue;
                }
                let Some(payload) = transform.transform(chunk).await else { continue };
                match validate_payload(&mut validator, payload) {
                    Ok(Some(p)) => {
                        if tx.send(Ok(p.clone())).await.is_err() {
                            lifecycle.canceled();
                            return;
                        }
                        transform.delivered(&p);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        lifecycle.complete("failed", 502, Some(&e.text));
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        }
    }
}

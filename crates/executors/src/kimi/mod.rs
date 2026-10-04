//! Kimi (Moonshot) executor: port of kimi_executor.go, kimi_thinking_replay.go and
//! helps/kimi_responses.go.
//!
//! Three wire paths share one credential and header set (see [`headers`]):
//! - source `claude`: delegated to the Claude executor against Kimi's Anthropic-compatible
//!   endpoint, wrapped by the thinking replay cache ([`replay`]);
//! - source `openai-response`: Responses passthrough to `{base}/v1/responses` with the
//!   `apply_patch` bridge;
//! - everything else: translated to OpenAI chat completions at `{base}/v1/chat/completions`.
//!
//! The Claude delegate is injected with [`new_with_claude`] because the Claude executor lives in
//! its own module; without one, Claude-format requests fail with 501.

mod headers;
mod normalize;
mod replay;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::kimi::{apply_refresh_to_auth, resolve_kimi_domain_from_auth, DeviceFlowClient};
use cpa_auth::storage::TokenStorage;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::thinking::{parse_suffix, ThinkingError};
use cpa_json::J;
use cpa_runtime::executor::{
    DynExecutor, ExecError, Executor, Metadata, Options, Request, Response, StreamResult,
};
use cpa_translator::{Ctx, Format, Param};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::helps::http_request;
use crate::helps::home_refresh::refresh_auth_via_home;
use crate::helps::apply_patch::{
    gateway_error, patch_failure, apply_patch_original_request, apply_patch_requested,
    finalize_apply_patch_stream, initialize_apply_patch_stream, record_apply_patch_stream_failure,
};
use crate::helps::apply_patch_responses::{
    ApplyPatchResponsesState, normalize_apply_patch_responses_request_body,
};
use crate::helps::oauth_scope::config_for_api_key;
use crate::helps::payload::{
    PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model, set_bool_if_different,
};
use crate::helps::proxy::new_proxy_aware_http_client;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::sse::{KIMI_SCANNER_BUFFER, LineReader, STREAM_SCANNER_BUFFER};
use crate::helps::status::{status_err, transport_error};
use crate::openai_compat::log::record_request;
use crate::helps::translate::{RequestTranslation, translate_request_pair};
use crate::helps::thinking::apply_request_thinking;
use crate::helps::usage::{
    StreamUsageBuffer, UsageReporter, parse_codex_usage, parse_openai_usage,
};
use crate::ConfigRx;

/// The Claude executor serving Kimi must use this as its upstream model normalizer.
pub use normalize::normalize_kimi_upstream_model;

use headers::kimi_headers;
use normalize::{
    normalize_kimi_responses_input, normalize_kimi_temperature, normalize_kimi_tool_message_links,
    normalize_kimi_tools, resolve_kimi_chat_url,
    resolve_kimi_claude_base_url, resolve_kimi_responses_url,
};

const PROVIDER: &str = "kimi";
const EXECUTOR_TYPE: &str = "KimiExecutor";
const CHANNEL_CAPACITY: usize = 16;

/// Kimi executor; cheap to clone around. `claude` serves the Claude-format path.
pub struct KimiExecutor {
    cfg: ConfigRx,
    claude: Option<DynExecutor>,
    /// Config view without OAuth-only settings (Go: ForAPIKey).
    api_key_scope: bool,
    /// Replaces the OAuth host in refresh calls (tests).
    oauth_host: Option<String>,
}

/// Kimi executor without a Claude delegate.
pub fn new(cfg: ConfigRx) -> DynExecutor {
    new_with_claude(cfg, None)
}

/// Kimi executor whose Claude-format requests run on `claude` (an executor configured for the
/// Kimi Anthropic-compatible endpoint: Kimi model normalization, `kimi` request logs).
pub fn new_with_claude(cfg: ConfigRx, claude: Option<DynExecutor>) -> DynExecutor {
    Arc::new(KimiExecutor { cfg, claude, api_key_scope: false, oauth_host: None })
}

/// Maps a thinking pipeline error to its HTTP status (400, or 500 for apply failures).
fn thinking_error(err: ThinkingError) -> ExecError {
    ExecError::new(err.status_code(), err.message)
}

/// Go: EndApplyPatchStream. Frames that fail a patch stream ended without its terminator, then
/// the gateway error when the stream failed.
fn end_patch_stream(param: &mut Param, reporter: &UsageReporter) -> (Vec<Vec<u8>>, Option<ExecError>) {
    let frames = finalize_apply_patch_stream(param);
    (frames, patch_failure(param, reporter))
}

/// The access token: metadata `access_token`, else attributes `access_token` / `api_key` (Go:
/// kimiCreds).
fn kimi_creds(auth: &Auth) -> String {
    if let Some(v) = auth.metadata.get("access_token").and_then(Value::as_str)
        && !v.trim().is_empty()
    {
        return v.to_string();
    }
    for key in ["access_token", "api_key"] {
        if let Some(v) = auth.attributes.get(key)
            && !v.is_empty()
        {
            return v.clone();
        }
    }
    String::new()
}

fn usage_response_metadata(detail: &crate::helps::usage::Detail) -> Metadata {
    let mut metadata = Metadata::new();
    metadata.insert("usage".to_string(), UsageReporter::usage_metadata(detail));
    metadata
}

/// Publishes whatever usage a finished stream buffered and hands it to the conductor.
fn finish_stream_usage(reporter: &UsageReporter, buffer: &StreamUsageBuffer, usage_tx: oneshot::Sender<Value>) {
    if !reporter.publish_buffer(buffer) {
        reporter.ensure_published();
    }
    if let Some(detail) = buffer.detail_ref() {
        let _ = usage_tx.send(UsageReporter::usage_metadata(detail));
    }
}

impl KimiExecutor {
    fn config(&self) -> Arc<Config> {
        let cfg = self.cfg.borrow().clone();
        if self.api_key_scope { config_for_api_key(&cfg) } else { cfg }
    }

    fn claude_delegate(&self) -> Result<&DynExecutor, ExecError> {
        self.claude
            .as_ref()
            .ok_or_else(|| status_err(501, "kimi executor: claude-format delegate is not configured"))
    }

    /// Clone of `auth` with `base_url` pointing at the Anthropic-compatible root.
    fn claude_auth(auth: &Auth) -> Auth {
        let mut auth = auth.clone();
        let base = resolve_kimi_claude_base_url(Some(&auth));
        auth.attributes.insert("base_url".to_string(), base);
        auth
    }

    fn http_client(&self, cfg: &Config, auth: &Auth, opts: &Options) -> reqwest::Client {
        new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(auth), None)
    }

    /// POSTs `body` to `url` with Kimi headers; transport errors carry no status.
    async fn send(
        &self,
        cfg: &Config,
        auth: &Auth,
        opts: &Options,
        url: &str,
        body: Vec<u8>,
        stream: bool,
        reporter: &UsageReporter,
    ) -> Result<reqwest::Response, ExecError> {
        let token = kimi_creds(auth);
        let headers = kimi_headers(&token, stream, auth, &opts.headers);
        tracing::debug!(target: "cpa::upstream", provider = PROVIDER, url, "kimi upstream request");
        record_request(&opts.api_log, cfg, PROVIDER, Some(auth), "POST", url, &headers, &body);
        // Go: reporter.TrackHTTPClient (first response byte is the TTFT).
        reporter.start_response_ttft();
        let resp = self
            .http_client(cfg, auth, opts)
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|e| {
                let err = transport_error(&e);
                opts.api_log.record_api_response_error(cfg, &err.message);
                err
            })?;
        opts.api_log.record_api_response_metadata(cfg, resp.status().as_u16(), resp.headers());
        Ok(resp)
    }

    /// Non-2xx upstream response as `statusErr{code, msg: body}`; the body lands in the request log.
    async fn upstream_error(cfg: &Config, opts: &Options, resp: reqwest::Response, reporter: &UsageReporter) -> ExecError {
        let status = resp.status().as_u16();
        let body = reporter.read_body_tracked(resp, false).await.unwrap_or_default();
        opts.api_log.append_api_response_chunk(cfg, &body);
        tracing::debug!(
            target: "cpa::upstream",
            status,
            "kimi request error: {}",
            crate::helps::logging::summarize_error_body("", &body)
        );
        status_err(status, String::from_utf8_lossy(&body).into_owned())
    }

    // ------------------------------------------------------------ chat completions

    /// Translated, thinking-adjusted and normalized chat body (shared by stream and non-stream).
    fn prepare_chat_body(
        &self,
        cfg: &Config,
        req: &Request,
        opts: &Options,
        base_model: &str,
        stream: bool,
        reporter: &UsageReporter,
    ) -> Result<Vec<u8>, ExecError> {
        let from = opts.source_format;
        let original_source: &[u8] = if opts.original_request.is_empty() { &req.payload } else { &opts.original_request };
        let translation = RequestTranslation::new(&opts.headers, Some(cfg), from, Format::OpenAI, base_model, stream);
        let (original_translated, mut body, _) = translate_request_pair(&translation, original_source, &req.payload);

        // Strip kimi- prefix and any [1m] suffix for the upstream API.
        let upstream_model = normalize_kimi_upstream_model(base_model);
        reporter.set_upstream_model(&upstream_model);
        let mut root = cpa_json::parse(&body);
        cpa_json::set(&mut root, "model", upstream_model);
        body = cpa_json::to_vec(&root);

        body = apply_request_thinking(&body, req, opts, from.as_str(), "kimi", PROVIDER, false).map_err(thinking_error)?;
        if stream {
            let mut root = cpa_json::parse(&body);
            cpa_json::set(&mut root, "stream_options.include_usage", true);
            body = cpa_json::to_vec(&root);
        }
        let requested_model = payload_requested_model(opts, &req.model);
        let request_path = payload_request_path(opts);
        body = apply_payload_config(
            &PayloadRequest {
                cfg: Some(cfg),
                target_executor: "",
                model: base_model,
                protocol: Format::OpenAI.as_str(),
                from_protocol: from.as_str(),
                root: "",
                requested_model: &requested_model,
                request_path: &request_path,
                headers: Some(&opts.headers),
            },
            &body,
            &original_translated,
        );
        body = normalize_kimi_tool_message_links(&body);
        body = normalize_kimi_tools(&body);
        body = normalize_kimi_temperature(&body);
        reporter.set_translated_reasoning_effort(&body, PROVIDER);
        Ok(body)
    }

    async fn execute_chat(
        &self,
        auth: &Auth,
        req: Request,
        opts: Options,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        let cfg = self.config();
        let response_format = opts.response_format_or_source();
        let base_model = parse_suffix(&req.model).model_name;
        let body = self.prepare_chat_body(&cfg, &req, &opts, &base_model, false, reporter)?;

        let url = resolve_kimi_chat_url(Some(auth));
        let resp = self.send(&cfg, auth, &opts, &url, body.clone(), false, reporter).await?;
        if !resp.status().is_success() {
            return Err(Self::upstream_error(&cfg, &opts, resp, reporter).await);
        }
        let headers = resp.headers().clone();
        let data = reporter.read_body_tracked(resp, false).await.map_err(|e| {
            let err = transport_error(&e);
            opts.api_log.record_api_response_error(&cfg, &err.message);
            err
        })?;
        opts.api_log.append_api_response_chunk(&cfg, &data);
        reporter.observe_response_model(&data);

        let mut param = Param::default();
        // Translates with the model name as requested (suffix kept) so clients see their alias.
        let out = cpa_translator::translate_non_stream(
            &Ctx::default(),
            Format::OpenAI,
            response_format,
            &req.model,
            &apply_patch_original_request(&req, &opts),
            &body,
            &data,
            &mut param,
        );
        let out = match out {
            Some(out) if !out.is_empty() && crate::helps::apply_patch::apply_patch_translation_error(&param).is_none() => out,
            _ => return Err(gateway_error()),
        };
        let detail = parse_openai_usage(&data);
        reporter.publish(detail.clone());
        let out = if response_format == Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out };
        Ok(Response { payload: Bytes::from(out), metadata: usage_response_metadata(&detail), headers })
    }

    async fn execute_chat_stream(
        &self,
        auth: &Auth,
        req: Request,
        opts: Options,
        reporter: &UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        let cfg = self.config();
        let response_format = opts.response_format_or_source();
        let base_model = parse_suffix(&req.model).model_name;
        let body = self.prepare_chat_body(&cfg, &req, &opts, &base_model, true, reporter)?;

        let url = resolve_kimi_chat_url(Some(auth));
        let resp = self.send(&cfg, auth, &opts, &url, body.clone(), true, reporter).await?;
        if !resp.status().is_success() {
            return Err(Self::upstream_error(&cfg, &opts, resp, reporter).await);
        }
        let headers = resp.headers().clone();
        let apply_original = apply_patch_original_request(&req, &opts);
        let model = req.model;
        let reporter = reporter.clone();
        let api_log = opts.api_log.clone();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (usage_tx, usage_rx) = oneshot::channel();

        tokio::spawn(async move {
            let mut lines = LineReader::from_response_tracked(resp, KIMI_SCANNER_BUFFER, &reporter, false);
            let mut usage = StreamUsageBuffer::default();
            let mut param = Param::default();
            let ctx = Ctx::default();
            initialize_apply_patch_stream(Format::OpenAI, response_format, &model, &apply_original, &body, &mut param);
            let mut scan_err = None;
            while let Some(line) = lines.next_line_or_closed(&tx).await {
                let line = match line {
                    Ok(line) => line,
                    Err(e) => {
                        scan_err = Some(e);
                        break;
                    }
                };
                api_log.append_api_response_chunk(&cfg, &line);
                reporter.observe_response_model(&line);
                usage.observe_openai_stream(&line);
                let chunks = cpa_translator::translate_stream(
                    &ctx, Format::OpenAI, response_format, &model, &apply_original, &body, &line, &mut param,
                );
                record_apply_patch_stream_failure(&param, &reporter, &gateway_error());
                for chunk in chunks {
                    let chunk = if response_format == Format::OpenAIResponse { ensure_responses_usage_details(&chunk) } else { chunk };
                    if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                        finish_stream_usage(&reporter, &usage, usage_tx);
                        return;
                    }
                }
                if let Some(err) = patch_failure(&param, &reporter) {
                    let _ = tx.send(Err(err)).await;
                    finish_stream_usage(&reporter, &usage, usage_tx);
                    return;
                }
            }
            let (frames, failure) = end_patch_stream(&mut param, &reporter);
            for frame in frames {
                if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                    finish_stream_usage(&reporter, &usage, usage_tx);
                    return;
                }
            }
            if let Some(err) = failure {
                let _ = tx.send(Err(err)).await;
                finish_stream_usage(&reporter, &usage, usage_tx);
                return;
            }
            let done = cpa_translator::translate_stream(
                &ctx, Format::OpenAI, response_format, &model, &apply_original, &body, b"[DONE]", &mut param,
            );
            record_apply_patch_stream_failure(&param, &reporter, &gateway_error());
            for chunk in done {
                let chunk = if response_format == Format::OpenAIResponse { ensure_responses_usage_details(&chunk) } else { chunk };
                if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                    finish_stream_usage(&reporter, &usage, usage_tx);
                    return;
                }
            }
            if let Some(err) = scan_err {
                let err = ExecError::from(err);
                api_log.record_api_response_error(&cfg, &err.message);
                reporter.publish_failure(&err);
                let _ = tx.send(Err(err)).await;
            }
            finish_stream_usage(&reporter, &usage, usage_tx);
        });

        let mut result = StreamResult::new(headers, rx);
        result.usage = Some(usage_rx);
        Ok(result)
    }

    // ------------------------------------------------------------ responses

    /// Responses request body: canonical model, forced `stream`, thinking, payload rules, patch
    /// bridge declarations, contiguous tool outputs, schema and temperature fixes.
    fn prepare_responses_body(
        &self,
        cfg: &Config,
        req: &Request,
        opts: &Options,
        base_model: &str,
        stream: bool,
        reporter: &UsageReporter,
    ) -> Result<Vec<u8>, ExecError> {
        let upstream_model = normalize_kimi_upstream_model(base_model);
        reporter.set_upstream_model(&upstream_model);
        let mut root = cpa_json::parse(&req.payload);
        cpa_json::set(&mut root, "model", upstream_model);
        set_bool_if_different(&mut root, "stream", stream);
        let mut body = cpa_json::to_vec(&root);

        body = apply_request_thinking(&body, req, opts, opts.source_format.as_str(), Format::Codex.as_str(), PROVIDER, false)
            .map_err(thinking_error)?;
        let requested_model = payload_requested_model(opts, &req.model);
        let request_path = payload_request_path(opts);
        body = apply_payload_config(
            &PayloadRequest {
                cfg: Some(cfg),
                target_executor: "",
                model: base_model,
                protocol: Format::OpenAIResponse.as_str(),
                from_protocol: opts.source_format.as_str(),
                root: "",
                requested_model: &requested_model,
                request_path: &request_path,
                headers: Some(&opts.headers),
            },
            &body,
            &req.payload,
        );
        body = normalize_apply_patch_responses_request_body(&body).map_err(|e| ExecError::new(0, e))?;
        body = normalize_kimi_responses_input(&body);
        body = normalize_kimi_tools(&body);
        body = normalize_kimi_temperature(&body);
        reporter.set_translated_reasoning_effort(&body, PROVIDER);
        Ok(body)
    }

    async fn execute_responses(
        &self,
        auth: &Auth,
        req: Request,
        opts: Options,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(status_err(501, "/responses/compact not supported"));
        }
        let cfg = self.config();
        let response_format = opts.response_format_or_source();
        let base_model = parse_suffix(&req.model).model_name;
        let body = self.prepare_responses_body(&cfg, &req, &opts, &base_model, false, reporter)?;

        let url = resolve_kimi_responses_url(Some(auth));
        let resp = self.send(&cfg, auth, &opts, &url, body.clone(), false, reporter).await?;
        if !resp.status().is_success() {
            return Err(Self::upstream_error(&cfg, &opts, resp, reporter).await);
        }
        let headers = resp.headers().clone();
        let data = reporter.read_body_tracked(resp, false).await.map_err(|e| {
            let err = transport_error(&e);
            opts.api_log.record_api_response_error(&cfg, &err.message);
            err
        })?;
        opts.api_log.append_api_response_chunk(&cfg, &data);
        reporter.observe_response_model(&data);

        let original_request = apply_patch_original_request(&req, &opts);
        let mut bridge = ApplyPatchResponsesState::new(opts.source_format, &original_request, &original_request);
        let mut out = match bridge.bridge.transform_non_stream(&data) {
            Ok(out) if !(out.is_empty() && apply_patch_requested(&original_request)) => out,
            _ => return Err(gateway_error()),
        };
        if response_format != Format::OpenAIResponse {
            let mut param = Param::default();
            let translated = cpa_translator::translate_non_stream(
                &Ctx::default(), Format::OpenAIResponse, response_format, &req.model, &original_request, &body, &out, &mut param,
            );
            out = match translated {
                Some(t) if !t.is_empty() => t,
                _ => return Err(gateway_error()),
            };
        }
        let mut metadata = Metadata::new();
        let observed = match parse_codex_usage(&data) {
            Some(d) if d.total_tokens > 0 || d.input_tokens > 0 => Some(d),
            _ => Some(parse_openai_usage(&data)).filter(|d| d.total_tokens > 0 || d.input_tokens > 0),
        };
        if let Some(detail) = observed {
            reporter.publish(detail.clone());
            metadata = usage_response_metadata(&detail);
        }
        Ok(Response { payload: Bytes::from(out), metadata, headers })
    }

    async fn execute_responses_stream(
        &self,
        auth: &Auth,
        req: Request,
        opts: Options,
        reporter: &UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(status_err(400, "streaming not supported for /responses/compact"));
        }
        let cfg = self.config();
        let response_format = opts.response_format_or_source();
        let base_model = parse_suffix(&req.model).model_name;
        let body = self.prepare_responses_body(&cfg, &req, &opts, &base_model, true, reporter)?;

        let url = resolve_kimi_responses_url(Some(auth));
        let resp = self.send(&cfg, auth, &opts, &url, body.clone(), true, reporter).await?;
        if !resp.status().is_success() {
            return Err(Self::upstream_error(&cfg, &opts, resp, reporter).await);
        }
        let headers = resp.headers().clone();
        let original_request = apply_patch_original_request(&req, &opts);
        let source_format = opts.source_format;
        let model = req.model;
        let reporter = reporter.clone();
        let api_log = opts.api_log.clone();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (usage_tx, usage_rx) = oneshot::channel();

        tokio::spawn(async move {
            let mut lines = LineReader::from_response_tracked(resp, STREAM_SCANNER_BUFFER, &reporter, false);
            let mut bridge = ApplyPatchResponsesState::new(source_format, &original_request, &original_request);
            let mut usage = StreamUsageBuffer::default();
            let mut param = Param::default();
            let mut emitter = ResponsesEmitter {
                tx: &tx,
                reporter: &reporter,
                response_format,
                model: &model,
                original: &original_request,
                body: &body,
            };

            let mut scan_err = None;
            let mut stopped = false;
            while let Some(line) = lines.next_line_or_closed(&tx).await {
                let line = match line {
                    Ok(line) => line,
                    Err(e) => {
                        scan_err = Some(e);
                        break;
                    }
                };
                api_log.append_api_response_chunk(&cfg, &line);
                reporter.observe_response_model(&line);
                observe_responses_usage(&mut usage, &line);

                let (converted, err) = bridge.stream(&line);
                if err.is_some() {
                    reporter.publish_failure(&gateway_error());
                }
                for event in &converted {
                    if !emitter.emit(event, &mut param).await {
                        stopped = true;
                        break;
                    }
                }
                if stopped {
                    break;
                }
                if err.is_some() {
                    let failure = gateway_error();
                    reporter.publish_failure(&failure);
                    let _ = tx.send(Err(failure)).await;
                    stopped = true;
                    break;
                }
            }
            if !stopped {
                let (events, err) = bridge.finish_stream();
                if err.is_some() {
                    reporter.publish_failure(&gateway_error());
                }
                for event in &events {
                    if !emitter.emit(event, &mut param).await {
                        stopped = true;
                        break;
                    }
                }
                if !stopped && err.is_some() {
                    let failure = gateway_error();
                    reporter.publish_failure(&failure);
                    let _ = tx.send(Err(failure)).await;
                    stopped = true;
                }
            }
            if !stopped && let Some(e) = scan_err {
                let err = ExecError::from(e);
                api_log.record_api_response_error(&cfg, &err.message);
                reporter.publish_failure(&err);
                let _ = tx.send(Err(err)).await;
            }
            finish_stream_usage(&reporter, &usage, usage_tx);
        });

        let mut result = StreamResult::new(headers, rx);
        result.usage = Some(usage_rx);
        Ok(result)
    }

    // ------------------------------------------------------------ claude delegation

    async fn execute_claude(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let delegate = self.claude_delegate()?;
        let auth = Self::claude_auth(auth);
        let (prepared, scope) = replay::prepare_request(req, &opts);
        match delegate.execute(&auth, prepared, opts).await {
            Ok(resp) => {
                replay::cache_response(&scope, &resp.payload);
                Ok(resp)
            }
            Err(err) => {
                if scope.replay_applied && replay::should_clear_after_error(&err) {
                    replay::clear_content(&scope);
                }
                Err(err)
            }
        }
    }

    async fn execute_claude_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let delegate = self.claude_delegate()?;
        let auth = Self::claude_auth(auth);
        let (prepared, scope) = replay::prepare_request(req, &opts);
        match delegate.execute_stream(&auth, prepared, opts).await {
            Ok(result) => Ok(replay::wrap_stream(result, scope)),
            Err(err) => {
                if scope.replay_applied && replay::should_clear_after_error(&err) {
                    replay::clear_content(&scope);
                }
                Err(err)
            }
        }
    }
}

/// Emits one converted Responses line to the client in the response format (Go:
/// `emitTranslatedLine`).
struct ResponsesEmitter<'a> {
    tx: &'a mpsc::Sender<Result<Bytes, ExecError>>,
    reporter: &'a UsageReporter,
    response_format: Format,
    model: &'a str,
    original: &'a [u8],
    body: &'a [u8],
}

impl ResponsesEmitter<'_> {
    /// False when the stream must stop (client gone or a retained patch failure was sent).
    async fn emit(&mut self, line: &[u8], param: &mut Param) -> bool {
        record_apply_patch_stream_failure(param, self.reporter, &gateway_error());
        if self.response_format == Format::OpenAIResponse {
            let mut payload = Vec::with_capacity(line.len() + 1);
            payload.extend_from_slice(line);
            payload.push(b'\n');
            return self.tx.send(Ok(Bytes::from(payload))).await.is_ok();
        }
        let chunks = cpa_translator::translate_stream(
            &Ctx::default(),
            Format::OpenAIResponse,
            self.response_format,
            self.model,
            self.original,
            self.body,
            line,
            param,
        );
        record_apply_patch_stream_failure(param, self.reporter, &gateway_error());
        for chunk in chunks {
            if self.tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                return false;
            }
        }
        match patch_failure(param, self.reporter) {
            Some(err) => {
                let _ = self.tx.send(Err(err)).await;
                false
            }
            None => true,
        }
    }
}

/// Records usage from a terminal Responses event line.
fn observe_responses_usage(usage: &mut StreamUsageBuffer, line: &[u8]) {
    let Some(rest) = line.strip_prefix(b"data:") else {
        return;
    };
    let data = crate::helps::text::trim_space(rest);
    let event_type = cpa_json::parse(data).g("type").str();
    if !matches!(event_type.as_str(), "response.completed" | "response.incomplete" | "response.done") {
        return;
    }
    match parse_codex_usage(data) {
        Some(d) if d.total_tokens > 0 || d.input_tokens > 0 => usage.observe(d, true),
        _ => {
            let d = parse_openai_usage(data);
            if d.total_tokens > 0 || d.input_tokens > 0 {
                usage.observe(d, true);
            }
        }
    }
}

/// Device id of the credential: metadata `device_id`, then the token storage's.
fn resolve_device_id(auth: &Auth) -> String {
    let from_meta = auth.metadata.get("device_id").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if !from_meta.is_empty() {
        return from_meta.to_string();
    }
    match &auth.storage {
        Some(TokenStorage::Kimi(s)) => s.device_id.trim().to_string(),
        _ => String::new(),
    }
}

#[async_trait]
impl Executor for KimiExecutor {
    fn identifier(&self) -> &str {
        PROVIDER
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        if opts.source_format == Format::Claude {
            return self.execute_claude(auth, req, opts).await;
        }
        let base_model = parse_suffix(&req.model).model_name;
        let reporter = UsageReporter::new(PROVIDER, EXECUTOR_TYPE, &base_model, Some(auth), Some(&opts));
        let result = if opts.source_format == Format::OpenAIResponse {
            self.execute_responses(auth, req, opts, &reporter).await
        } else {
            self.execute_chat(auth, req, opts, &reporter).await
        };
        reporter.track_failure(&result);
        result
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        if opts.source_format == Format::Claude {
            return self.execute_claude_stream(auth, req, opts).await;
        }
        let base_model = parse_suffix(&req.model).model_name;
        let reporter = UsageReporter::new(PROVIDER, EXECUTOR_TYPE, &base_model, Some(auth), Some(&opts));
        let result = if opts.source_format == Format::OpenAIResponse {
            self.execute_responses_stream(auth, req, opts, &reporter).await
        } else {
            self.execute_chat_stream(auth, req, opts, &reporter).await
        };
        reporter.track_failure(&result);
        result
    }

    /// Refreshes the OAuth token with the stored refresh token; credentials without one are
    /// returned unchanged. Failures carry no status (Go returns plain errors here).
    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        if let Some(result) = refresh_auth_via_home(&self.config(), auth).await {
            return result;
        }
        let refresh_token = auth.metadata.get("refresh_token").and_then(Value::as_str).filter(|v| !v.trim().is_empty());
        let Some(refresh_token) = refresh_token else {
            return Ok(auth.clone());
        };
        let cfg = self.config();
        let domain = resolve_kimi_domain_from_auth(auth);
        let http = new_proxy_aware_http_client("", Some(&cfg), Some(auth), Some(Duration::from_secs(30)));
        let mut client = DeviceFlowClient::with_client(http, domain, &resolve_device_id(auth));
        if let Some(host) = &self.oauth_host {
            client = client.with_oauth_host(host);
        }
        let td = client.refresh_token(refresh_token).await.map_err(|e| ExecError::new(0, e.to_string()))?;
        let mut refreshed = auth.clone();
        apply_refresh_to_auth(&mut refreshed, &td);
        Ok(refreshed)
    }

    /// Token counting runs on the Claude delegate against the Anthropic-compatible endpoint.
    async fn count_tokens(&self, auth: &Auth, mut req: Request, opts: Options) -> Result<Response, ExecError> {
        if opts.source_format == Format::OpenAIResponse {
            let normalized = normalize_apply_patch_responses_request_body(&req.payload).map_err(|e| ExecError::new(0, e))?;
            req.payload = Bytes::from(normalized);
        }
        let delegate = self.claude_delegate()?;
        delegate.count_tokens(&Self::claude_auth(auth), req, opts).await
    }

    fn for_api_key(&self) -> Option<DynExecutor> {
        Some(Arc::new(KimiExecutor {
            cfg: self.cfg.clone(),
            claude: self.claude.as_ref().map(|c| c.for_api_key().unwrap_or_else(|| c.clone())),
            api_key_scope: true,
            oauth_host: self.oauth_host.clone(),
        }))
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }

    /// Go: KimiExecutor.PrepareRequest (a blank token leaves `Authorization` as is).
    async fn prepare_request(&self, req: &mut reqwest::Request, auth: &Auth) -> Result<(), ExecError> {
        let token = kimi_creds(auth);
        if !token.trim().is_empty() {
            http_request::set_header(req, "Authorization", &format!("Bearer {token}"));
        }
        http_request::apply_attr_headers(req, auth);
        Ok(())
    }

    /// Go: KimiExecutor.HttpRequest.
    async fn http_request(&self, auth: &Auth, mut req: reqwest::Request) -> Result<reqwest::Response, ExecError> {
        self.prepare_request(&mut req, auth).await?;
        let client = new_proxy_aware_http_client("", Some(&self.config()), Some(auth), None);
        http_request::execute(&client, req).await
    }
}

#[cfg(test)]
mod tests;

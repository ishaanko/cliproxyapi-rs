//! Codex over HTTP + SSE: `Execute`, `ExecuteStream` and `/responses/compact` (Go:
//! codex_executor_execute.go and codex_executor_stream.go).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, Response, StreamResult};
use cpa_translator::{Ctx, Param};
use http::HeaderMap;
use tokio::sync::{mpsc, oneshot};

use super::CodexExecutor;
use super::logging::error_text;
use crate::helps::logging::UpstreamRequestLog;
use crate::helps::claude_input_tokens::ClaudeInputTokenState;
use crate::helps::logging::ApiLogHandle;
use super::creds::{base_url, codex_creds};
use super::headers::{apply_codex_headers, apply_model_header_overrides, apply_routing_hint, header_value, set_header};
use super::multi_agent_v2::restore_response;
use super::reasoning::{ReplayScope, cache_replay_from_completed, clear_replay_on_invalid_signature};
use super::request::{Mode, Prepared, apply_prompt_cache_and_ids, prepare, prompt_cache_id};
use super::terminal::{
    BOOTSTRAP_MAX_BUFFERED_BYTES, BOOTSTRAP_MAX_BUFFERED_FRAMES, OutputItems, has_meaningful_output_delta,
    is_bootstrap_bufferable_event, is_overload_bootstrap_failure, is_terminal_empty_incomplete, new_bootstrap_overload_err,
    new_empty_incomplete_stream_error, new_incomplete_stream_error,
    new_status_err_with_cooling, normalize_completion, patch_completed_output, status_error, terminal_failure_err,
};
use crate::helps::apply_patch::{APPLY_PATCH_UPSTREAM_ERROR_MESSAGE, apply_patch_translation_error};
use crate::helps::proxy::new_proxy_aware_http_client;
use crate::helps::tls_fingerprint::new_utls_http_client;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::sse::{LineReader, STREAM_SCANNER_BUFFER};
use crate::helps::ttft::observe_responses_token_event_doc;
use cpa_json::lazy::Doc;
use crate::helps::usage::{parse::parse_codex_usage, parse::parse_openai_usage, reporter::UsageReporter};

/// Message of the 502 recorded when the upstream closes before any payload.
const EMPTY_STREAM_MESSAGE: &str = "upstream stream closed before first payload";

/// Channel depth between the SSE reader task and the conductor.
const STREAM_CHANNEL_CAPACITY: usize = 64;

/// `data:` frame payload of an SSE line (trimmed), `None` for other lines.
fn data_payload(line: &[u8]) -> Option<&[u8]> {
    line.strip_prefix(b"data:").map(crate::helps::text::trim_space)
}

fn usage_metadata(detail: &crate::helps::usage::accounting::Detail) -> Metadata {
    let mut meta = Metadata::new();
    meta.insert("usage".to_string(), UsageReporter::usage_metadata(detail));
    meta
}

/// Go `publishCodexImageToolUsage`: the image tool's own usage becomes a record of its model.
pub(super) fn publish_image_tool_usage(reporter: &UsageReporter, body: &[u8], completed: &[u8]) {
    let Some(detail) = crate::helps::usage::parse::parse_codex_image_tool_usage(completed) else { return };
    reporter.ensure_published();
    reporter.publish_additional_model(&image_generation_tool_model(body), detail);
}

/// Go `codexImageGenerationToolModel`: the `image_generation` tool's model, else the default.
fn image_generation_tool_model(body: &[u8]) -> String {
    let root = cpa_json::parse(body);
    for tool in root.g("tools").array() {
        if tool.g("type").str() != "image_generation" {
            continue;
        }
        let model = tool.g("model").str();
        if !model.trim().is_empty() {
            return model.trim().to_string();
        }
        break;
    }
    "gpt-image-2".to_string()
}

/// `Codex` sends keepalives as events Grok clients cannot parse; they become SSE comments.
fn is_grok_client(headers: &HeaderMap) -> bool {
    cpa_misc::grokbuild::is_grok_client_user_agent(&header_value(headers, "User-Agent"))
}

fn grok_keepalive_line(line: &[u8], is_grok: bool) -> Option<Vec<u8>> {
    (is_grok && cpa_misc::grokbuild::is_keepalive_sse_line(line)).then(cpa_misc::grokbuild::keepalive_sse_comment)
}

impl CodexExecutor {
    pub(super) fn http_url(base: &str, path: &str) -> String {
        format!("{}{}", base_url(base), path)
    }

    /// Builds the upstream POST: `(url, headers, body)` with cache identity, headers, routing hint
    /// and model overrides applied.
    #[allow(clippy::too_many_arguments)]
    fn build_http_request(
        &self,
        cfg: &Config,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        prepared: &Prepared,
        stream: bool,
        path: &str,
    ) -> (String, HeaderMap, Vec<u8>) {
        let (api_key, configured_base) = codex_creds(auth);
        let url = Self::http_url(&configured_base, path);
        let cache_id = prompt_cache_id(prepared.from, req, opts, &prepared.body, true);
        let body = apply_prompt_cache_and_ids(prepared.body.clone(), &cache_id);
        let mut headers = HeaderMap::new();
        if !cache_id.is_empty() {
            set_header(&mut headers, "Session-Id", &cache_id);
        }
        let session_id = self.session_id(opts, &req.payload);
        apply_codex_headers(&mut headers, auth, &api_key, stream, cfg, &opts.headers, session_id.as_deref());
        apply_routing_hint(&mut headers, auth, &prepared.base_model, &body, &opts.headers, session_id.as_deref());
        apply_model_header_overrides(&mut headers, &prepared.base_model);
        (url, headers, body)
    }

    pub(super) async fn send_http(
        &self,
        cfg: &Config,
        auth: &Auth,
        opts: &Options,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
        reporter: &UsageReporter,
    ) -> Result<reqwest::Response, ExecError> {
        opts.api_log.record_api_request(cfg, UpstreamRequestLog::from_auth("codex", Some(auth), "POST", url, &headers, &body));
        reporter.start_response_ttft();
        let fallback = new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(auth), None);
        let client = new_utls_http_client(&opts.proxy_url, Some(cfg), Some(auth), fallback);
        match client.post(url).headers(headers).body(body).send().await {
            Ok(resp) => {
                opts.api_log.record_api_response_metadata(cfg, resp.status().as_u16(), resp.headers());
                Ok(resp)
            }
            Err(e) => {
                let err = e.exec_error();
                opts.api_log.record_api_response_error(cfg, &error_text(&err));
                Err(err)
            }
        }
    }

    /// Error for a non-2xx response; also drops stale reasoning replay state. The streaming
    /// path fails on a body read error (`strict`), the others keep the bytes read so far.
    async fn http_status_error(&self, cfg: &Config, opts: &Options, scope: &ReplayScope, resp: reqwest::Response, strict: bool, reporter: &UsageReporter) -> ExecError {
        let status = resp.status().as_u16();
        let content_type = header_value(resp.headers(), "Content-Type");
        let (data, read_err) = read_all_lenient(resp, Some(reporter)).await;
        if let (true, Some(read_err)) = (strict, read_err) {
            opts.api_log.record_api_response_error(cfg, &read_err);
            return ExecError::new(0, read_err);
        }
        // A failed replay cleanup replaces the upstream error (Go returns the cleanup error).
        if let Err(replay_err) = clear_replay_on_invalid_signature(scope, status, &data) {
            return replay_err;
        }
        opts.api_log.append_api_response_chunk(cfg, &data);
        tracing::debug!(
            "request error, error status: {status}, error message: {}",
            crate::helps::logging::summarize_error_body(&content_type, &data)
        );
        new_status_err_with_cooling(status, &data, cfg.codex.model_level_cooling)
    }

    /// Non-stream Responses call: the upstream is streamed, the terminal event is translated.
    pub(super) async fn execute_http(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let reporter = self.reporter("CodexExecutor", auth, &req, &opts);
        let result = self.execute_http_reported(auth, req, opts, &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn execute_http_reported(&self, auth: &Auth, req: Request, opts: Options, reporter: &UsageReporter) -> Result<Response, ExecError> {
        let cfg = self.config();
        if opts.alt == "responses/compact" {
            return self.execute_compact(&cfg, auth, req, opts, reporter).await;
        }
        let prepared = prepare(&cfg, auth, &req, &opts, Mode::Execute)?;
        reporter.set_translated_reasoning_effort(&prepared.body, prepared.to.as_str());
        let (url, headers, body) = self.build_http_request(&cfg, auth, &req, &opts, &prepared, true, "/responses");
        let resp = self.send_http(&cfg, auth, &opts, &url, headers, body, reporter).await?;
        if !resp.status().is_success() {
            return Err(self.http_status_error(&cfg, &opts, &prepared.replay_scope, resp, false, reporter).await);
        }
        let resp_headers = resp.headers().clone();
        let (data, read_err) = read_all_lenient(resp, Some(reporter)).await;
        opts.api_log.append_api_response_chunk(&cfg, &data);
        let modelc = cfg.codex.model_level_cooling;

        let mut items = OutputItems::default();
        let mut saw_output_delta = false;
        for line in data.split(|b| *b == b'\n') {
            let Some(payload) = data_payload(line) else { continue };
            let event_data = restore_response(payload, prepared.optimize_multi_agent_v2);
            reporter.observe_response_model(&event_data);
            let event = cpa_json::parse(&event_data);
            let event_type = event.g("type").str();
            if has_meaningful_output_delta(&event) {
                saw_output_delta = true;
            }
            if let Some((err, terminal_body)) = terminal_failure_err(&event, modelc) {
                clear_replay_on_invalid_signature(&prepared.replay_scope, err.status, &terminal_body)?;
                return Err(err);
            }
            if event_type == "response.output_item.done" {
                items.collect(&event, &event_data);
                continue;
            }
            if event_type != "response.completed" && event_type != "response.incomplete" {
                continue;
            }
            if is_terminal_empty_incomplete(&event, items.len(), saw_output_delta) {
                return Err(new_empty_incomplete_stream_error());
            }
            let completed = patch_completed_output(&event_data, &items);
            if event_type == "response.completed" {
                cache_replay_from_completed(&prepared.replay_scope, &cpa_json::parse(&completed));
            }
            let mut param = Param::default();
            let out = cpa_translator::translate_non_stream(
                &Ctx::default(),
                prepared.to,
                prepared.response_format,
                &req.model,
                &prepared.original_payload,
                &prepared.body,
                &completed,
                &mut param,
            );
            let out = match out {
                Some(out) if apply_patch_translation_error(&param).is_none() && !out.is_empty() => out,
                _ => return Err(status_error(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)),
            };
            // The main model's usage is published first so the image tool's record cannot take its place.
            let detail = parse_codex_usage(&event_data);
            if let Some(detail) = &detail {
                reporter.publish(detail.clone());
            }
            publish_image_tool_usage(reporter, &prepared.body, &event_data);
            let out = if prepared.response_format == cpa_translator::Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out };
            let metadata = detail.as_ref().map(usage_metadata).unwrap_or_default();
            return Ok(Response { payload: Bytes::from(out), metadata, headers: resp_headers });
        }
        if let Some(read_err) = read_err {
            opts.api_log.record_api_response_error(&cfg, &read_err);
        }
        Err(new_incomplete_stream_error())
    }

    /// `/responses/compact`: one JSON response, no streaming.
    async fn execute_compact(&self, cfg: &Config, auth: &Auth, req: Request, opts: Options, reporter: &UsageReporter) -> Result<Response, ExecError> {
        let prepared = prepare(cfg, auth, &req, &opts, Mode::Compact)?;
        reporter.set_translated_reasoning_effort(&prepared.body, prepared.to.as_str());
        let (url, headers, body) = self.build_http_request(cfg, auth, &req, &opts, &prepared, false, "/responses/compact");
        let resp = self.send_http(cfg, auth, &opts, &url, headers, body, reporter).await?;
        if !resp.status().is_success() {
            return Err(self.http_status_error(cfg, &opts, &prepared.replay_scope, resp, false, reporter).await);
        }
        let resp_headers = resp.headers().clone();
        let data = match read_all_marking(resp, reporter).await {
            Ok(data) => data,
            Err(e) => {
                let err = crate::helps::status::transport_error(&e);
                opts.api_log.record_api_response_error(cfg, &error_text(&err));
                return Err(err);
            }
        };
        opts.api_log.append_api_response_chunk(cfg, &data);
        let upstream = restore_response(&data, prepared.optimize_multi_agent_v2);
        let mut param = Param::default();
        let out = cpa_translator::translate_non_stream(
            &Ctx::default(),
            prepared.to,
            prepared.response_format,
            &req.model,
            &prepared.original_payload,
            &prepared.body,
            &upstream,
            &mut param,
        );
        let out = match out {
            Some(out) if apply_patch_translation_error(&param).is_none() && !out.is_empty() => out,
            _ => return Err(status_error(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)),
        };
        let detail = parse_openai_usage(&upstream);
        reporter.publish(detail.clone());
        let out = if prepared.response_format == cpa_translator::Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out };
        let metadata = if detail.has_token_usage() { usage_metadata(&detail) } else { Metadata::new() };
        Ok(Response { payload: Bytes::from(out), metadata, headers: resp_headers })
    }

    /// Streaming Responses call over SSE, with optional bootstrap buffering so an overload
    /// rejection inside an HTTP 200 stream can fail over before the headers are committed.
    pub(super) async fn execute_stream_http(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let reporter = self.reporter("CodexExecutor", auth, &req, &opts);
        let result = self.execute_stream_http_reported(auth, req, opts, &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn execute_stream_http_reported(&self, auth: &Auth, req: Request, opts: Options, reporter: &UsageReporter) -> Result<StreamResult, ExecError> {
        let cfg = self.config();
        if opts.alt == "responses/compact" {
            return Err(status_error(400, "streaming not supported for /responses/compact"));
        }
        let prepared = prepare(&cfg, auth, &req, &opts, Mode::Stream)?;
        reporter.set_translated_reasoning_effort(&prepared.body, prepared.to.as_str());
        let (url, headers, body) = self.build_http_request(&cfg, auth, &req, &opts, &prepared, true, "/responses");
        let resp = self.send_http(&cfg, auth, &opts, &url, headers, body, reporter).await?;
        if !resp.status().is_success() {
            return Err(self.http_status_error(&cfg, &opts, &prepared.replay_scope, resp, true, reporter).await);
        }
        let upstream_headers = resp.headers().clone();
        let buffering = cfg.codex.stream_bootstrap_buffering;
        let bootstrap_timeout = Duration::from_nanos(cfg.codex.stream_bootstrap_timeout_duration().0.max(0) as u64);
        let mut stream = HttpStream::new(&cfg, &req, &opts, prepared, reporter.clone());
        let mut lines = LineReader::from_response(resp, STREAM_SCANNER_BUFFER);
        let mut scan_error = None;
        let (usage_tx, usage_rx) = oneshot::channel::<Value>();

        let mut buffered: Vec<Vec<u8>> = Vec::new();
        let mut initial: Vec<Vec<u8>> = Vec::new();
        let mut bootstrap_terminal_err: Option<ExecError> = None;
        let mut immediate_terminal = false;
        let mut started = false;
        if buffering {
            let bootstrap_start = Instant::now();
            let (mut buffered_frames, mut buffered_bytes) = (0usize, 0usize);
            while let Some(next) = lines.next_line().await {
                let line = match next {
                    Ok(line) => line,
                    Err(err) => {
                        scan_error = Some(err);
                        break;
                    }
                };
                match stream.step(&line) {
                    Step::Failure { err, body } => {
                        if let Err(replay_err) = stream.clear_replay(&err, &body) {
                            reporter.publish_failure(&replay_err);
                            return Err(replay_err);
                        }
                        reporter.publish_failure(&err);
                        if is_overload_bootstrap_failure(&body) {
                            let time_reached = !bootstrap_timeout.is_zero() && bootstrap_start.elapsed() >= bootstrap_timeout;
                            if !time_reached {
                                return Err(new_bootstrap_overload_err(&body));
                            }
                        }
                        bootstrap_terminal_err = Some(err);
                        break;
                    }
                    Step::EmptyIncomplete(err) => {
                        reporter.publish_failure(&err);
                        bootstrap_terminal_err = Some(err);
                        break;
                    }
                    Step::Frame { chunks, handshake, terminal_success } => {
                        if handshake && !terminal_success {
                            let frame_bytes = line.len() + chunks.iter().map(Vec::len).sum::<usize>();
                            let time_reached = !bootstrap_timeout.is_zero() && bootstrap_start.elapsed() >= bootstrap_timeout;
                            if !time_reached && buffered_frames < BOOTSTRAP_MAX_BUFFERED_FRAMES && buffered_bytes + frame_bytes <= BOOTSTRAP_MAX_BUFFERED_BYTES {
                                buffered_frames += 1;
                                buffered_bytes += frame_bytes;
                                buffered.extend(chunks);
                                continue;
                            }
                        }
                        initial = chunks;
                        started = true;
                        immediate_terminal = terminal_success;
                        break;
                    }
                }
            }
            if !started && bootstrap_terminal_err.is_none() {
                if let Some(err) = scan_error {
                    opts.api_log.record_api_response_error(&cfg, &err.to_string());
                    let err: ExecError = err.into();
                    reporter.publish_failure(&err);
                    return Err(err);
                }
                if buffered.is_empty() && initial.is_empty() {
                    opts.api_log.record_api_response_error(&cfg, EMPTY_STREAM_MESSAGE);
                    reporter.publish_failure(&status_error(502, EMPTY_STREAM_MESSAGE));
                    let (_tx, rx) = mpsc::channel(1);
                    return Ok(StreamResult::new(upstream_headers, rx));
                }
                let err = new_incomplete_stream_error();
                opts.api_log.record_api_response_error(&cfg, &error_text(&err));
                reporter.publish_failure(&err);
                return Err(err);
            }
        }

        let capacity = (buffered.len() + initial.len() + 1).max(STREAM_CHANNEL_CAPACITY);
        let (tx, rx) = mpsc::channel(capacity);
        let mut emitted = 0usize;
        for chunk in buffered.into_iter().chain(initial) {
            if !chunk.is_empty() {
                emitted += 1;
            }
            let _ = tx.try_send(Ok(Bytes::from(chunk)));
        }
        let mut result = StreamResult::new(upstream_headers, rx);
        result.usage = Some(usage_rx);
        if let Some(err) = bootstrap_terminal_err {
            let _ = tx.try_send(Err(err));
            return Ok(result);
        }
        if immediate_terminal {
            if let Some(meta) = stream.usage_value() {
                let _ = usage_tx.send(meta);
            }
            return Ok(result);
        }
        tokio::spawn(async move {
            stream.run(lines, tx, usage_tx, emitted).await;
        });
        Ok(result)
    }
}

/// Reads a body to the end, marking TTFT when the first non-empty chunk arrives (Go: the
/// `TrackHTTPClient` body wrapper marks the first byte of any body).
pub(super) async fn read_all_marking(mut resp: reqwest::Response, reporter: &UsageReporter) -> Result<Vec<u8>, reqwest::Error> {
    let mut data = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if data.is_empty() && !chunk.is_empty() {
            reporter.mark_first_response_byte();
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

/// Reads a body to the end (Go `io.ReadAll`): on a read error the bytes received so far are
/// returned together with the rendered error.
async fn read_all_lenient(mut resp: reqwest::Response, reporter: Option<&UsageReporter>) -> (Vec<u8>, Option<String>) {
    let mut data = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if let (true, Some(reporter)) = (data.is_empty() && !chunk.is_empty(), reporter) {
                    reporter.mark_first_response_byte();
                }
                data.extend_from_slice(&chunk);
            }
            Ok(None) => return (data, None),
            Err(e) => return (data, Some(crate::helps::status::transport_message(&e))),
        }
    }
}

/// What one upstream line turned into.
enum Step {
    Frame { chunks: Vec<Vec<u8>>, handshake: bool, terminal_success: bool },
    Failure { err: ExecError, body: Vec<u8> },
    EmptyIncomplete(ExecError),
}

/// Per-stream translation state of the SSE path.
struct HttpStream {
    from_model: String,
    prepared: Prepared,
    request_body: Vec<u8>,
    param: Param,
    claude_tokens: ClaudeInputTokenState,
    items: OutputItems,
    saw_output_delta: bool,
    model_level_cooling: bool,
    is_grok: bool,
    usage: Option<crate::helps::usage::accounting::Detail>,
    cfg: Arc<Config>,
    api_log: ApiLogHandle,
    reporter: UsageReporter,
}

impl HttpStream {
    fn new(cfg: &Arc<Config>, req: &Request, opts: &Options, prepared: Prepared, reporter: UsageReporter) -> Self {
        // Response translators receive the translated request before the prompt cache identity.
        let request_body = prepared.body.clone();
        let claude_tokens = ClaudeInputTokenState::new(prepared.from, prepared.to, prepared.response_format, &prepared.original_payload);
        HttpStream {
            from_model: req.model.clone(),
            request_body,
            param: Param::default(),
            claude_tokens,
            items: OutputItems::default(),
            saw_output_delta: false,
            model_level_cooling: cfg.codex.model_level_cooling,
            is_grok: is_grok_client(&opts.headers),
            usage: None,
            cfg: Arc::clone(cfg),
            api_log: opts.api_log.clone(),
            reporter,
            prepared,
        }
    }

    fn clear_replay(&self, err: &ExecError, body: &[u8]) -> Result<(), ExecError> {
        clear_replay_on_invalid_signature(&self.prepared.replay_scope, err.status, body)
    }

    fn usage_value(&self) -> Option<Value> {
        self.usage.as_ref().map(UsageReporter::usage_metadata)
    }

    fn translate(&mut self, line: &[u8]) -> Vec<Vec<u8>> {
        self.claude_tokens.translate_stream(
            self.prepared.to,
            self.prepared.response_format,
            &self.from_model,
            &self.prepared.original_payload,
            &self.request_body,
            line,
            &mut self.param,
        )
    }

    /// Handles one upstream SSE line (Go: the body of the scan loop).
    fn step(&mut self, line: &[u8]) -> Step {
        self.api_log.append_api_response_chunk(&self.cfg, line);
        self.reporter.record_first_packet();
        if let Some(transformed) = grok_keepalive_line(line, self.is_grok) {
            let chunks = self.translate(&transformed);
            return Step::Frame { chunks, handshake: true, terminal_success: false };
        }
        let Some(payload) = data_payload(line) else {
            let chunks = self.translate(line);
            return Step::Frame { chunks, handshake: true, terminal_success: false };
        };
        let mut data = restore_response(payload, self.prepared.optimize_multi_agent_v2);
        let event = Doc::new(&data);
        observe_responses_token_event_doc(&self.reporter, &data, &event);
        let event_type = event.g("type").str();
        if let Some((err, body)) = terminal_failure_err(&event, self.model_level_cooling) {
            self.api_log.record_api_response_error(&self.cfg, &error_text(&err));
            return Step::Failure { err, body };
        }
        if has_meaningful_output_delta(&event) {
            self.saw_output_delta = true;
        }
        if is_terminal_empty_incomplete(&event, self.items.len(), self.saw_output_delta) {
            let err = new_empty_incomplete_stream_error();
            self.api_log.record_api_response_error(&self.cfg, &error_text(&err));
            return Step::EmptyIncomplete(err);
        }
        let mut handshake = is_bootstrap_bufferable_event(&event_type, &data, &event);
        let mut terminal_success = false;
        match event_type.as_str() {
            "response.output_item.done" => self.items.collect(&event, &data),
            "response.completed" | "response.incomplete" | "response.done" => {
                terminal_success = true;
                handshake = false;
                data = normalize_completion(&data);
                self.usage = parse_codex_usage(&data);
                match &self.usage {
                    Some(detail) => self.reporter.publish(detail.clone()),
                    None => self.reporter.ensure_published(),
                }
                publish_image_tool_usage(&self.reporter, &self.request_body, &data);
                if !self.prepared.native {
                    data = patch_completed_output(&data, &self.items);
                }
                if event_type == "response.completed" || event_type == "response.done" {
                    cache_replay_from_completed(&self.prepared.replay_scope, &cpa_json::parse(&data));
                }
            }
            _ => {}
        }
        let mut translated = b"data: ".to_vec();
        translated.extend_from_slice(&data);
        let chunks = self.translate(&translated);
        Step::Frame { chunks, handshake, terminal_success }
    }

    /// Streams the rest of the response after the bootstrap decision.
    async fn run(
        mut self,
        mut lines: LineReader,
        tx: mpsc::Sender<Result<Bytes, ExecError>>,
        usage_tx: oneshot::Sender<Value>,
        mut emitted: usize,
    ) {
        loop {
            let next = tokio::select! {
                _ = tx.closed() => {
                    // Go: `if ctx.Err() != nil { return }`, nothing is recorded.
                    self.reporter.abandon();
                    return;
                }
                next = lines.next_line() => next,
            };
            let Some(line) = next else { break };
            let line = match line {
                Ok(line) => line,
                Err(err) => {
                    self.api_log.record_api_response_error(&self.cfg, &err.to_string());
                    break;
                }
            };
            match self.step(&line) {
                Step::Failure { err, body } => {
                    let err = self.clear_replay(&err, &body).err().unwrap_or(err);
                    self.reporter.publish_failure(&err);
                    let _ = tx.send(Err(err)).await;
                    return;
                }
                Step::EmptyIncomplete(err) => {
                    self.reporter.publish_failure(&err);
                    let _ = tx.send(Err(err)).await;
                    return;
                }
                Step::Frame { chunks, terminal_success, .. } => {
                    for chunk in chunks {
                        let non_empty = !chunk.is_empty();
                        if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                            self.reporter.abandon();
                            return;
                        }
                        if non_empty {
                            emitted += 1;
                        }
                    }
                    if terminal_success {
                        if let Some(meta) = self.usage_value() {
                            let _ = usage_tx.send(meta);
                        }
                        return;
                    }
                }
            }
        }
        if emitted == 0 {
            // "upstream stream closed before first payload": no chunk, the conductor sees an empty stream.
            self.api_log.record_api_response_error(&self.cfg, EMPTY_STREAM_MESSAGE);
            self.reporter.publish_failure(&status_error(502, EMPTY_STREAM_MESSAGE));
            return;
        }
        let err = new_incomplete_stream_error();
        self.api_log.record_api_response_error(&self.cfg, &error_text(&err));
        self.reporter.publish_failure(&err);
        let _ = tx.send(Err(err)).await;
    }
}

//! Streaming xAI execution (Go: xai_executor_stream.go).
//!
//! Upstream SSE lines are normalized (reasoning text to reasoning summary events, namespaced
//! and aliased tools restored, internal X Search traces hidden), passed through the apply_patch
//! bridge and translated to the client format line by line. `event:` lines are held until their
//! `data:` line is known because normalization can change the event name.

use std::collections::BTreeMap;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_json::Value;
use cpa_runtime::executor::{ExecError, Options, Request, StreamResult};
use cpa_translator::Param;
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};

use super::XaiExecutor;
use super::execute::{content_type, input_has_item_type, read_body};
use super::replay::{ReplayScope, cache_reasoning_replay_from_completed};
use super::request::{IDENTIFIER, PreparedRequest, apply_chat_headers, chat_base_url, creds, log_resolved_base_url, prepare_responses_request};
use super::response::{
    InternalXSearchResponseFilter, NamespaceRestorer, collect_output_item_done, normalize_reasoning_summary_data,
    normalize_reasoning_summary_data_events, normalize_reasoning_summary_event_line, patch_completed_output,
    restore_client_web_search_name, status_err_for_body,
};
use super::util::s;
use crate::helps::apply_patch::{
    APPLY_PATCH_UPSTREAM_ERROR_MESSAGE, ChunkSender, record_apply_patch_stream_failure,
};
use crate::helps::session::ensure_session_id;
use crate::helps::sse::{LineReader, STREAM_SCANNER_BUFFER, ScanError};
use crate::helps::status::status_err;
use crate::helps::text::trim_space;
use crate::helps::usage::{StreamUsageBuffer, UsageReporter, parse_codex_usage};
use crate::openai_compat::claude_input_tokens::{ClaudeInputTokenState, translate_stream_with_claude_input_tokens};
use crate::helps::status::transport_message;
use crate::openai_compat::translate::{observe_body, stop_apply_patch_stream};

impl XaiExecutor {
    /// Go: ExecuteStream.
    pub(super) async fn execute_stream_chat(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
    ) -> Result<StreamResult, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(status_err(400, "streaming not supported for /responses/compact"));
        }
        if input_has_item_type(&req.payload, "compaction_trigger") {
            return self.execute_compaction_trigger_stream(auth, req, opts).await;
        }
        let cfg = self.config();
        let session = ensure_session_id(None, "", opts, &req.payload);
        let (token, _) = creds(Some(auth));
        let base_url = chat_base_url(Some(auth));
        log_resolved_base_url(&base_url);

        let prepared = prepare_responses_request(&cfg, req, opts, true)?;
        let reporter = self.new_reporter(&prepared.base_model, auth, opts);
        let result = async {
            reporter.set_translated_reasoning_effort(&prepared.body, IDENTIFIER);
            let url = format!("{}/responses", base_url.trim_end_matches('/'));
            let headers = apply_chat_headers(Some(auth), &token, true, &prepared.session_id, opts, session.as_deref())?;
            let resp = self.send(&cfg, auth, opts, &reporter, &url, headers, prepared.body.clone()).await?;
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            if !(200..300).contains(&status) {
                let data = read_body(&reporter, resp).await?;
                tracing::debug!(
                    "request error, error status: {status}, error message: {}",
                    crate::helps::logging::summarize_error_body(&content_type(&resp_headers), &data)
                );
                return Err(status_err_for_body(status, &data));
            }
            Ok((resp, resp_headers))
        }
        .await;
        let (resp, resp_headers) = match result {
            Ok(v) => v,
            Err(err) => {
                reporter.publish_failure(&err);
                return Err(err);
            }
        };
        Ok(spawn_stream(resp, resp_headers, prepared, req.model.clone(), reporter))
    }
}

/// One upstream data payload moving through the normalization steps.
enum Event {
    Json(Value),
    Raw(Vec<u8>),
}

impl Event {
    fn bytes(&self) -> Vec<u8> {
        match self {
            Event::Json(v) => cpa_json::to_vec(v),
            Event::Raw(b) => b.clone(),
        }
    }

    fn event_type(&self) -> String {
        match self {
            Event::Json(v) => s(v, "type"),
            Event::Raw(_) => String::new(),
        }
    }
}

struct XaiStream {
    out: ChunkSender,
    reporter: UsageReporter,
    prepared: PreparedRequest,
    model: String,
    param: Param,
    claude: ClaudeInputTokenState,
    usage: StreamUsageBuffer,
    items_by_index: BTreeMap<i64, Value>,
    items_fallback: Vec<Value>,
    filter: InternalXSearchResponseFilter,
    restorer: NamespaceRestorer,
    replay_scope: ReplayScope,
}

impl XaiStream {
    fn gateway_error() -> ExecError {
        status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)
    }

    fn translate(&mut self, line: &[u8]) -> Vec<Vec<u8>> {
        translate_stream_with_claude_input_tokens(
            self.prepared.to,
            self.prepared.response_format,
            &self.model,
            &self.prepared.original_payload,
            &self.prepared.body,
            line,
            &mut self.param,
            &mut self.claude,
        )
    }

    /// Go: emitTranslatedLine. Runs one SSE line through the apply_patch bridge and the
    /// translator and delivers the chunks; false stops the stream.
    async fn emit(&mut self, translated_line: &[u8]) -> bool {
        let (lines, err_bridge) = self.prepared.apply_patch.stream(translated_line);
        if err_bridge.is_some() {
            self.reporter.publish_failure(&Self::gateway_error());
        }
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        for mut line in lines {
            if let Some(rest) = line.strip_prefix(b"data:") {
                let event_data = trim_space(rest).to_vec();
                let parsed = cpa_json::parse(&event_data);
                match s(&parsed, "type").as_str() {
                    "response.output_item.done" => {
                        collect_output_item_done(&parsed, &mut self.items_by_index, &mut self.items_fallback);
                    }
                    "response.completed" | "response.incomplete" => {
                        // Reconstruct only after the bridge restored dispatcher children.
                        let mut completed = patch_completed_output(&parsed, &self.items_by_index, &self.items_fallback);
                        normalize_reasoning_summary_data(&mut completed);
                        if s(&completed, "type") == "response.completed" {
                            // Only completed responses carry replayable terminal state.
                            cache_reasoning_replay_from_completed(&self.replay_scope, &completed);
                        }
                        let kept = line.trim_ascii_end_matches_crlf();
                        let ending = line[kept..].to_vec();
                        let mut rebuilt = b"data: ".to_vec();
                        rebuilt.extend_from_slice(&cpa_json::to_vec(&completed));
                        rebuilt.extend_from_slice(&ending);
                        line = rebuilt;
                    }
                    _ => {}
                }
            }
            chunks.extend(self.translate(&line));
        }
        record_apply_patch_stream_failure(&self.param, &self.reporter, &Self::gateway_error());
        for chunk in chunks {
            if self.out.send(Ok(Bytes::from(chunk))).await.is_err() {
                return false;
            }
        }
        if stop_apply_patch_stream(&mut self.param, &self.reporter, &self.out, Self::gateway_error()).await {
            return false;
        }
        if err_bridge.is_some() {
            let err = Self::gateway_error();
            self.reporter.publish_failure(&err);
            let _ = self.out.send(Err(err)).await;
            return false;
        }
        true
    }

    /// Normalization steps between the raw upstream payload and the bridge. `None` drops the
    /// event (filtered X Search trace).
    fn prepare_event(&mut self, mut event: Event) -> Option<Event> {
        self.prepared.apply_patch.remember_dispatcher_event(&event.bytes());
        if let Event::Json(v) = &mut event {
            self.restorer.restore(v);
            if !self.prepared.web_search_alias.is_empty() {
                restore_client_web_search_name(v, &self.prepared.web_search_alias);
            }
            if !self.filter.apply(v) {
                return None;
            }
        }
        Some(event)
    }

    async fn run(&mut self, mut lines: LineReader) {
        let mut pending_event_line: Option<Vec<u8>> = None;
        let mut scan_err: Option<ScanError> = None;
        while let Some(next) = lines.next_line().await {
            let line = match next {
                Ok(line) => line,
                Err(err) => {
                    scan_err = Some(err);
                    break;
                }
            };
            if line.starts_with(b"event:") {
                if let Some(pending) = pending_event_line.take()
                    && !self.emit(&normalize_reasoning_summary_event_line(&pending, "")).await
                {
                    return;
                }
                pending_event_line = Some(line.to_vec());
                continue;
            }
            if let Some(rest) = line.strip_prefix(b"data:") {
                let data = trim_space(rest);
                let list: Vec<Event> = if cpa_json::valid(data) {
                    normalize_reasoning_summary_data_events(cpa_json::parse(data)).into_iter().map(Event::Json).collect()
                } else {
                    vec![Event::Raw(data.to_vec())]
                };
                let has_pending = pending_event_line.is_some();
                for (i, event) in list.into_iter().enumerate() {
                    let Some(event) = self.prepare_event(event) else {
                        if has_pending && i == 0 {
                            pending_event_line = None;
                        }
                        continue;
                    };
                    let event_bytes = event.bytes();
                    if event_bytes.is_empty() {
                        if has_pending && i == 0 {
                            pending_event_line = None;
                        }
                        continue;
                    }
                    self.reporter.observe_response_model(&event_bytes);
                    let normalized_event_name = event.event_type();
                    if normalized_event_name == "response.completed" || normalized_event_name == "response.incomplete" {
                        if let Some(detail) = parse_codex_usage(&event_bytes) {
                            self.usage.observe(detail, true);
                        }
                    }
                    if has_pending {
                        let mut event_line = format!("event: {normalized_event_name}").into_bytes();
                        if i == 0
                            && let Some(pending) = pending_event_line.take()
                        {
                            event_line = normalize_reasoning_summary_event_line(&pending, &normalized_event_name);
                        }
                        if !self.emit(&event_line).await {
                            return;
                        }
                    }
                    let mut data_line = b"data: ".to_vec();
                    data_line.extend_from_slice(&event_bytes);
                    if !self.emit(&data_line).await {
                        return;
                    }
                }
                continue;
            }
            if let Some(pending) = pending_event_line.take()
                && !self.emit(&normalize_reasoning_summary_event_line(&pending, "")).await
            {
                return;
            }
            if !self.emit(&line).await {
                return;
            }
        }
        if let Some(pending) = pending_event_line.take() {
            let _ = self.emit(&normalize_reasoning_summary_event_line(&pending, "")).await;
        }
        let (finish_events, err_finish) = self.prepared.apply_patch.finish_stream();
        if err_finish.is_some() {
            self.reporter.publish_failure(&Self::gateway_error());
        }
        for event in finish_events {
            for chunk in self.translate(&event) {
                if self.out.send(Ok(Bytes::from(chunk))).await.is_err() {
                    return;
                }
            }
        }
        if err_finish.is_some() {
            let err = Self::gateway_error();
            self.reporter.publish_failure(&err);
            let _ = self.out.send(Err(err)).await;
            return;
        }
        if let Some(err) = scan_err {
            let err = ExecError::from(err);
            self.reporter.publish_failure(&err);
            let _ = self.out.send(Err(err)).await;
        }
    }
}

trait TrimCrlf {
    /// Length of the slice without trailing `\r` and `\n` bytes.
    fn trim_ascii_end_matches_crlf(&self) -> usize;
}

impl TrimCrlf for Vec<u8> {
    fn trim_ascii_end_matches_crlf(&self) -> usize {
        let mut end = self.len();
        while end > 0 && (self[end - 1] == b'\r' || self[end - 1] == b'\n') {
            end -= 1;
        }
        end
    }
}

fn spawn_stream(
    resp: reqwest::Response,
    headers: http::HeaderMap,
    prepared: PreparedRequest,
    model: String,
    reporter: UsageReporter,
) -> StreamResult {
    let (tx, rx) = mpsc::channel(16);
    let (usage_tx, usage_rx) = oneshot::channel();
    let lines = LineReader::new(
        Box::pin(observe_body(reporter.clone(), resp.bytes_stream(), false).map(|r| r.map_err(|e| transport_message(&e)))),
        STREAM_SCANNER_BUFFER,
    );
    tokio::spawn(async move {
        let claude = ClaudeInputTokenState::new(prepared.from, prepared.to, prepared.response_format, &prepared.original_payload);
        let filter =
            InternalXSearchResponseFilter::new(prepared.filter_internal_x_search, prepared.client_declared_tools.clone());
        let restorer = NamespaceRestorer::new(prepared.namespace_tools.clone());
        let replay_scope = prepared.replay_scope.clone();
        let mut stream = XaiStream {
            out: tx,
            reporter,
            prepared,
            model,
            param: Param::default(),
            claude,
            usage: StreamUsageBuffer::default(),
            items_by_index: BTreeMap::new(),
            items_fallback: Vec::new(),
            filter,
            restorer,
            replay_scope,
        };
        stream.run(lines).await;
        stream.reporter.publish_buffer(&stream.usage);
        if let Some(detail) = stream.usage.detail() {
            let _ = usage_tx.send(UsageReporter::usage_metadata(&detail));
        }
    });
    let mut result = StreamResult::new(headers, rx);
    result.usage = Some(usage_rx);
    result
}

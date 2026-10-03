//! Streaming Messages call (Go: ExecuteStream in claude_executor_stream.go).

use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_runtime::executor::{ExecError, Options, Request, StreamResult};
use cpa_translator::{Ctx, Format, Param};
use tokio::sync::{mpsc, oneshot};

use super::diagnostics::{commit_claude_continuity_state, observe_claude_stream_line};
use super::execute::Prepared;
use super::fast_error::wrap_claude_fast_request_error;
use super::request::header_value;
use super::thinking_replay::{clear_claude_thinking_replay_content, should_clear_kimi_thinking_replay_after_error, wrap_claude_thinking_replay_stream};
use super::tool_remap::restore_claude_oauth_tool_names_from_stream_line;
use super::ClaudeExecutor;
use crate::helps::apply_patch::{
    apply_patch_original_request, apply_patch_translation_error, end_apply_patch_stream, gateway_error,
    initialize_apply_patch_stream, record_apply_patch_stream_failure, stop_apply_patch_stream,
};
use crate::helps::logging::ApiLogHandle;
use crate::helps::sse::{LineReader, STREAM_SCANNER_BUFFER};
use crate::helps::status::status_err;
use crate::helps::text::trim_space;
use crate::helps::usage::{StreamUsageBuffer, UsageReporter};

impl ClaudeExecutor {
    /// Streaming Messages call. Claude-format clients get whole SSE events (one chunk per event,
    /// stopping after the event carrying `message_stop`); other formats get each line through the
    /// stream translator.
    pub(super) async fn execute_stream_impl(
        &self,
        cfg: &Arc<Config>,
        auth: &Auth,
        req: Request,
        opts: Options,
        reporter: UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(status_err(501, "/responses/compact not supported"));
        }
        let response_format = opts.response_format_or_source();
        let prepared = self.prepare_messages_request(cfg, auth, req, &opts, true, &reporter)?;
        let replay_scope = prepared.replay_scope.clone();
        let resp = match self.send_upstream(cfg, auth, &opts, &prepared).await {
            Ok(resp) => resp,
            Err(err) => {
                if replay_scope.replay_applied && should_clear_kimi_thinking_replay_after_error(Some(&err)) {
                    clear_claude_thinking_replay_content(&replay_scope);
                }
                return Err(err);
            }
        };
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        let (tx, rx) = mpsc::channel::<Result<Bytes, ExecError>>(1);
        let (usage_tx, usage_rx) = oneshot::channel();
        let original_request = apply_patch_original_request(&prepared.req, &opts);
        let task_headers = resp_headers.clone();
        let task_cfg = Arc::clone(cfg);
        let api_log = opts.api_log.clone();
        tokio::spawn(async move {
            let mut usage = StreamUsageBuffer::default();
            let outcome = run_stream(
                resp,
                status,
                &task_headers,
                &prepared,
                &original_request,
                response_format,
                &reporter,
                &mut usage,
                &tx,
                (&task_cfg, &api_log),
            )
            .await;
            if let Err(err) = outcome {
                let err = wrap_claude_fast_request_error(prepared.fast_request, status, err);
                api_log.record_api_response_error(&task_cfg, &err.message);
                reporter.publish_buffer_failure(&usage, &err);
                if prepared.replay_scope.replay_applied && should_clear_kimi_thinking_replay_after_error(Some(&err)) {
                    clear_claude_thinking_replay_content(&prepared.replay_scope);
                }
                let _ = tx.send(Err(err)).await;
            } else {
                reporter.publish_buffer(&usage);
            }
            reporter.ensure_published();
            if let Some(detail) = usage.detail() {
                let _ = usage_tx.send(UsageReporter::usage_metadata(&detail));
            }
        });
        let mut result = StreamResult::new(resp_headers, rx);
        result.usage = Some(usage_rx);
        if replay_scope.valid() {
            result = wrap_claude_thinking_replay_stream(result, replay_scope.clone());
        }
        Ok(result)
    }
}

/// Pumps the upstream body to the client channel. `Err` is a terminal stream failure that the
/// caller reports; a closed channel (client gone) ends the pump with `Ok`.
#[allow(clippy::too_many_arguments)]
async fn run_stream(
    resp: reqwest::Response,
    status: u16,
    resp_headers: &http::HeaderMap,
    p: &Prepared,
    original_request: &[u8],
    response_format: Format,
    reporter: &UsageReporter,
    usage: &mut StreamUsageBuffer,
    tx: &mpsc::Sender<Result<Bytes, ExecError>>,
    (cfg, api_log): (&Config, &ApiLogHandle),
) -> Result<(), ExecError> {
    let to = Format::Claude;
    let mut lines = LineReader::from_stream(
        resp.bytes_stream().map_err(|e| crate::helps::status::transport_message(&e)),
        STREAM_SCANNER_BUFFER,
    );
    let mut upstream_message_id = String::new();
    let mut upstream_completed = false;
    let restore_error = |err: super::tool_remap::ClaudeMcpAliasRestoreError| {
        let mut err = err.into_exec_error();
        err.message = format!("restore Claude OAuth tool name from streaming response: {}", err.message);
        err
    };

    // The Claude-format client receives upstream events verbatim, one chunk per event.
    if response_format == to {
        let mut event: Vec<u8> = Vec::new();
        let mut scan_error: Option<ExecError> = None;
        while let Some(line) = lines.next_line_or_closed(tx).await {
            let line = match line {
                Ok(line) => line,
                Err(err) => {
                    scan_error = Some(err.into());
                    break;
                }
            };
            observe_claude_stream_line(&line, &mut upstream_message_id, &mut upstream_completed);
            api_log.append_api_response_chunk(cfg, &line);
            reporter.observe_response_model(&line);
            usage.observe_claude_stream(&line);
            let restored = restore_claude_oauth_tool_names_from_stream_line(&line, &p.tool_reverse_map).map_err(restore_error)?;
            event.extend_from_slice(&restored);
            event.push(b'\n');
            if trim_space(&restored).is_empty() {
                if !event.is_empty() && tx.send(Ok(Bytes::from(std::mem::take(&mut event)))).await.is_err() {
                    return Ok(());
                }
                if upstream_completed {
                    break;
                }
            }
        }
        if !event.is_empty() && tx.send(Ok(Bytes::from(event))).await.is_err() {
            return Ok(());
        }
        if !upstream_completed && let Some(err) = scan_error {
            return Err(err);
        }
        if upstream_completed {
            commit_claude_continuity_state(&p.diagnostics_state, &upstream_message_id, &header_value(resp_headers, "request-id"));
        }
        let _ = status;
        return Ok(());
    }

    // Other formats go through the stream translator.
    let mut param = Param::default();
    initialize_apply_patch_stream(to, response_format, &p.req.model, original_request, &p.body_for_translation, &mut param);
    let mut scan_error: Option<ExecError> = None;
    while let Some(line) = lines.next_line_or_closed(tx).await {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                scan_error = Some(err.into());
                break;
            }
        };
        observe_claude_stream_line(&line, &mut upstream_message_id, &mut upstream_completed);
        api_log.append_api_response_chunk(cfg, &line);
        reporter.observe_response_model(&line);
        usage.observe_claude_stream(&line);
        let restored = restore_claude_oauth_tool_names_from_stream_line(&line, &p.tool_reverse_map).map_err(restore_error)?;
        let mut chunks = cpa_translator::translate_stream(
            &Ctx::default(),
            to,
            response_format,
            &p.req.model,
            original_request,
            &p.body_for_translation,
            &restored,
            &mut param,
        );
        if response_format == Format::OpenAIResponse && apply_patch_translation_error(&param).is_none() {
            for chunk in &mut chunks {
                *chunk = crate::helps::responses_usage::ensure_responses_usage_details(chunk);
            }
        }
        record_apply_patch_stream_failure(&param, reporter, &gateway_error());
        for chunk in chunks {
            if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                return Ok(());
            }
        }
        // A retained tool-input failure ends the stream after its one translated frame.
        if stop_apply_patch_stream(&param, reporter, tx, gateway_error()).await {
            return Ok(());
        }
        if upstream_completed {
            break;
        }
    }
    // EOF check before any synthetic success: finalize frames, then the gateway error if failed.
    if end_apply_patch_stream(&mut param, reporter, tx, gateway_error()).await {
        return Ok(());
    }
    if !upstream_completed && let Some(err) = scan_error {
        return Err(err);
    }
    if upstream_completed {
        commit_claude_continuity_state(&p.diagnostics_state, &upstream_message_id, &header_value(resp_headers, "request-id"));
    }
    Ok(())
}

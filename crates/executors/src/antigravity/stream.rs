//! Streaming execution (Go: antigravity_executor_stream.go).

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_runtime::executor::{ExecError, Options, Request, StreamResult};
use cpa_translator::{Ctx, Format, Param, translate_stream};
use http::HeaderValue;
use tokio::sync::{mpsc, oneshot};

use super::AntigravityExecutor;
use crate::helps::claude_input_tokens::ClaudeInputTokenState;
use super::compaction::{build_compaction_stream_chunks, has_responses_compaction_trigger};
use super::credits::clear_credits_failure_state;
use super::execute::expand_capsules_in_request;
use super::grounding::{resolve_grounding_urls, should_resolve_grounding_urls};
use super::pipeline::{Mode, Prepared, base_model_of};
use super::replay_capture::ReplayAccumulator;
use crate::helps::apply_patch::{
    apply_patch_original_request, end_apply_patch_stream, gateway_error, initialize_apply_patch_stream,
    record_apply_patch_stream_failure, stop_apply_patch_stream,
};
use crate::helps::proxy::effective_proxy_url;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::sse::{LineReader, STREAM_SCANNER_BUFFER};
use crate::helps::text::json_payload;
use crate::helps::usage::{
    StreamUsageBuffer, UsageReporter, filter_sse_usage_metadata, parse_antigravity_stream_usage,
};

const STREAM_CHANNEL_CAPACITY: usize = 16;

impl AntigravityExecutor {
    pub(crate) async fn execute_stream_impl(
        &self,
        auth: &Auth,
        mut req: Request,
        mut opts: Options,
    ) -> Result<StreamResult, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(ExecError::new(400, "streaming not supported for /responses/compact"));
        }
        expand_capsules_in_request(&mut req, &mut opts)?;
        if has_responses_compaction_trigger(&req.payload) || has_responses_compaction_trigger(&opts.original_request) {
            return self.execute_compaction_stream(auth, req, opts).await;
        }
        let cfg = self.cfg();
        let base_model = base_model_of(&req.model);
        // The stream translators read `alt` as empty: SSE output.
        self.check_short_cooldown(&cfg, auth, &base_model, &opts).await?;

        let p = self.prepare(cfg, auth, &mut req, &opts, Mode::Stream).await?;
        match self.start_stream(p, req, opts).await {
            Ok(result) => Ok(result),
            Err((p, err)) => {
                p.reporter.publish_failure(&err);
                Err(err)
            }
        }
    }

    async fn start_stream(
        &self,
        p: Prepared,
        req: Request,
        opts: Options,
    ) -> Result<StreamResult, (Prepared, ExecError)> {
        let resp = match self.send(&p).await {
            Ok(r) => r,
            Err(e) => return Err((p, e)),
        };
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        if !(200..300).contains(&status) {
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return Err((p, crate::helps::status::transport_error(&e))),
            };
            let err = self.handle_upstream_error(&p, status, &body).await;
            return Err((p, err));
        }
        if p.use_credits {
            clear_credits_failure_state(&p.auth);
        }
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let (usage_tx, usage_rx) = oneshot::channel();
        let this = self.clone();
        tokio::spawn(async move {
            this.pump_stream(p, req, opts, resp, tx, usage_tx).await;
        });
        let mut result = StreamResult::new(headers, rx);
        result.usage = Some(usage_rx);
        Ok(result)
    }

    /// Reads the upstream SSE, translates each chunk and feeds the client channel; at a clean EOF
    /// it emits the synthetic terminal event, commits the replay ledger and publishes usage.
    async fn pump_stream(
        &self,
        p: Prepared,
        req: Request,
        opts: Options,
        resp: reqwest::Response,
        out: mpsc::Sender<Result<Bytes, ExecError>>,
        usage_tx: oneshot::Sender<serde_json::Value>,
    ) {
        let reporter = p.reporter.clone();
        let mut accumulator = ReplayAccumulator::new(&p.replay_scope, &p.request_payload);
        let mut reader = LineReader::from_response(resp, STREAM_SCANNER_BUFFER);
        let mut claude_tokens = ClaudeInputTokenState::new(p.from, Format::Antigravity, p.response_format, &p.original_payload);
        let mut usage = StreamUsageBuffer::default();
        let mut param = Param::default();
        let original = apply_patch_original_request(&req, &opts);
        let ctx = Ctx { alt: Some(String::new()) };
        initialize_apply_patch_stream(Format::Antigravity, p.response_format, &req.model, &original, &p.translated, &mut param);

        let translate = |param: &mut Param, raw: &[u8], claude_tokens: &mut ClaudeInputTokenState| -> Vec<Vec<u8>> {
            let mut chunks = translate_stream(
                &ctx,
                Format::Antigravity,
                p.response_format,
                &req.model,
                &original,
                &p.translated,
                raw,
                param,
            );
            if param.tool_input_error.is_some() {
                return chunks;
            }
            if p.response_format == Format::OpenAIResponse {
                for chunk in &mut chunks {
                    *chunk = ensure_responses_usage_details(chunk);
                }
            }
            claude_tokens.apply(&mut chunks);
            chunks
        };

        let proxy = effective_proxy_url(&opts.proxy_url, Some(&p.auth), Some(&p.cfg));
        let resolve_grounding = should_resolve_grounding_urls(p.from, &p.original_payload, &p.translated);
        // `finished` stays true until the client left or an apply_patch failure ended the stream;
        // a read error is held back so the apply_patch end hook still runs first (Go order).
        let mut finished = true;
        let mut read_error = None;

        'lines: while let Some(line) = reader.next_line_or_closed(&out).await {
            let line = match line {
                Ok(l) => l,
                Err(err) => {
                    read_error = Some(ExecError::from(err));
                    break 'lines;
                }
            };
            reporter.mark_first_response_byte();
            if let Some(acc) = accumulator.as_mut() {
                acc.observe_sse_line(&line);
            }
            // Keep usage only on the terminal chunk.
            let line = filter_sse_usage_metadata(&line);
            let Some(payload) = json_payload(&line) else { continue };
            reporter.observe_response_model(payload);
            if let Some(detail) = parse_antigravity_stream_usage(payload) {
                usage.observe(detail, true);
            }
            let payload = if resolve_grounding {
                resolve_grounding_urls(&proxy, payload.to_vec()).await
            } else {
                payload.to_vec()
            };
            let chunks = translate(&mut param, &payload, &mut claude_tokens);
            record_apply_patch_stream_failure(&param, &reporter, &gateway_error());
            for chunk in chunks {
                if out.send(Ok(Bytes::from(chunk))).await.is_err() {
                    finished = false;
                    break 'lines;
                }
            }
            if stop_apply_patch_stream(&param, &reporter, &out, gateway_error()).await {
                finished = false;
                break 'lines;
            }
        }

        if finished && end_apply_patch_stream(&mut param, &reporter, &out, gateway_error()).await {
            finished = false;
        }
        if let Some(err) = read_error.take().filter(|_| finished) {
            reporter.publish_failure(&err);
            let _ = out.send(Err(err)).await;
        } else if finished {
            // Only a clean end of stream may produce a synthetic terminal event: translating
            // [DONE] after a read error would report a truncated stream as complete.
            let tail = translate(&mut param, b"[DONE]", &mut claude_tokens);
            record_apply_patch_stream_failure(&param, &reporter, &gateway_error());
            for chunk in tail {
                if out.send(Ok(Bytes::from(chunk))).await.is_err() {
                    finished = false;
                    break;
                }
            }
            if finished {
                if let Some(acc) = accumulator.as_mut() {
                    acc.commit();
                }
                // The reporter keeps the first outcome: publish buffered usage before the
                // fallback record.
                reporter.publish_buffer(&usage);
                reporter.ensure_published();
            }
        }
        // Any other exit still publishes what was observed (no-op after a failure outcome).
        reporter.publish_buffer(&usage);
        if let Some(record) = reporter.record() {
            let _ = usage_tx.send(UsageReporter::usage_metadata(&record.detail));
        }
    }

    /// Streamed compaction: the summary is generated non-stream and replayed as canned SSE.
    async fn execute_compaction_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let o = self.run_compaction_summary(auth, req, opts).await?;
        let chunks = build_compaction_stream_chunks(&o.base_model, &o.capsule, o.input_tokens, o.output_tokens, o.total_tokens);
        let (tx, rx) = mpsc::channel(chunks.len().max(1));
        for chunk in chunks {
            // The channel holds every frame, so this never blocks.
            let _ = tx.try_send(Ok(Bytes::from(chunk)));
        }
        drop(tx);
        let mut headers = o.headers;
        headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        Ok(StreamResult::new(headers, rx))
    }
}

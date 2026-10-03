//! Non-streaming xAI execution (Go: xai_executor_execute.go): chat via the stream-oriented
//! upstream path, `/responses/compact`, and the compaction trigger stream.

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_json::J;
use cpa_runtime::conductor::usage::META_USAGE;
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, Response, StreamResult};
use cpa_translator::{Ctx, Format, Param};
use http::HeaderMap;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::XaiExecutor;
use super::request::{
    PreparedRequest, apply_chat_headers, apply_headers, chat_base_url, compact_base_url, creds, log_resolved_base_url,
    prepare_responses_request, prepare_responses_request_to,
};
use super::response::{
    InternalXSearchResponseFilter, NamespaceRestorer, collect_output_item_done, normalize_reasoning_summary_data,
    patch_completed_output, restore_client_web_search_name, status_err_for_body,
};
use super::replay::{cache_reasoning_replay_from_completed, clear_reasoning_replay_after_compaction};
use super::util::{at, items, s, ts};
use crate::helps::apply_patch::{APPLY_PATCH_UPSTREAM_ERROR_MESSAGE, apply_patch_translation_error};
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::session::ensure_session_id;
use crate::helps::status::status_err;
use crate::helps::text::trim_space;
use crate::helps::usage::{UsageReporter, parse_codex_usage, parse_openai_usage};
use crate::helps::status::transport_error;

const EXECUTOR_TYPE: &str = "XAIExecutor";

impl XaiExecutor {
    pub(super) fn new_reporter(&self, model: &str, auth: &Auth, opts: &Options) -> UsageReporter {
        UsageReporter::new(super::request::IDENTIFIER, EXECUTOR_TYPE, model, Some(auth), Some(opts))
    }

    /// Go: Execute for the chat path. The upstream is always read as a stream and the terminal
    /// response event is converted to a single response.
    pub(super) async fn execute_chat(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
    ) -> Result<Response, ExecError> {
        let cfg = self.config();
        let session = ensure_session_id(None, "", opts, &req.payload);
        let (token, _) = creds(Some(auth));
        let base_url = chat_base_url(Some(auth));
        log_resolved_base_url(&base_url);

        let mut prepared = prepare_responses_request(&cfg, req, opts, true)?;
        let reporter = self.new_reporter(&prepared.base_model, auth, opts);
        let result = self.execute_chat_prepared(&cfg, auth, req, opts, &mut prepared, &token, &base_url, session.as_deref(), &reporter).await;
        reporter.track_failure(&result);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_chat_prepared(
        &self,
        cfg: &cpa_config::Config,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        prepared: &mut PreparedRequest,
        token: &str,
        base_url: &str,
        session: Option<&str>,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        reporter.set_translated_reasoning_effort(&prepared.body, super::request::IDENTIFIER);
        let url = format!("{}/responses", base_url.trim_end_matches('/'));
        let headers = apply_chat_headers(Some(auth), token, true, &prepared.session_id, opts, session)?;
        self.record_request(cfg, auth, opts, &url, &headers, &prepared.body);
        let resp = self
            .send(cfg, auth, opts, reporter, &url, headers, prepared.body.clone())
            .await?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        if !(200..300).contains(&status) {
            let data = read_body(cfg, opts, reporter, resp).await?;
            tracing::debug!(
                "request error, error status: {status}, error message: {}",
                crate::helps::logging::summarize_error_body(&content_type(&resp_headers), &data)
            );
            return Err(status_err_for_body(status, &data));
        }
        let data = read_body(cfg, opts, reporter, resp).await?;

        let mut output_items_by_index = std::collections::BTreeMap::new();
        let mut output_items_fallback: Vec<Value> = Vec::new();
        let mut response_filter =
            InternalXSearchResponseFilter::new(prepared.filter_internal_x_search, prepared.client_declared_tools.clone());
        let mut namespace_restorer = NamespaceRestorer::new(prepared.namespace_tools.clone());
        for line in data.split(|b| *b == b'\n') {
            let Some(rest) = line.strip_prefix(b"data:") else { continue };
            let data_bytes = trim_space(rest);
            // Non-JSON payloads (for example `[DONE]`) skip the JSON-only steps.
            let event_data: Vec<u8> = match cpa_json::valid(data_bytes).then(|| cpa_json::parse(data_bytes)) {
                Some(mut v) => {
                    normalize_reasoning_summary_data(&mut v);
                    prepared.apply_patch.remember_dispatcher_event(&cpa_json::to_vec(&v));
                    namespace_restorer.restore(&mut v);
                    if !prepared.web_search_alias.is_empty() {
                        restore_client_web_search_name(&mut v, &prepared.web_search_alias);
                    }
                    if !response_filter.apply(&mut v) {
                        continue;
                    }
                    cpa_json::to_vec(&v)
                }
                None => {
                    prepared.apply_patch.remember_dispatcher_event(data_bytes);
                    data_bytes.to_vec()
                }
            };
            if event_data.is_empty() {
                continue;
            }
            let (events, err_bridge) = prepared.apply_patch.transform(&event_data);
            if err_bridge.is_some() {
                return Err(status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE));
            }
            for event in events {
                reporter.observe_response_model(&event);
                let parsed = cpa_json::parse(&event);
                let event_type = s(&parsed, "type");
                match event_type.as_str() {
                    "response.output_item.done" => {
                        collect_output_item_done(&parsed, &mut output_items_by_index, &mut output_items_fallback);
                    }
                    "response.completed" | "response.incomplete" => {
                        let mut completed = patch_completed_output(&parsed, &output_items_by_index, &output_items_fallback);
                        normalize_reasoning_summary_data(&mut completed);
                        if event_type == "response.completed" {
                            // A truncated turn carries no replayable terminal state, so only a
                            // completed response may refresh the reasoning replay cache.
                            cache_reasoning_replay_from_completed(&prepared.replay_scope, &completed);
                        }
                        let completed_bytes = cpa_json::to_vec(&completed);
                        let mut param = Param::default();
                        let out = cpa_translator::translate_non_stream(
                            &Ctx::default(),
                            prepared.to,
                            prepared.response_format,
                            &req.model,
                            &prepared.original_payload,
                            &prepared.body,
                            &completed_bytes,
                            &mut param,
                        );
                        let out = match out {
                            Some(out) if !out.is_empty() && apply_patch_translation_error(&param).is_none() => out,
                            _ => return Err(status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)),
                        };
                        let mut metadata = Metadata::new();
                        if let Some(detail) = parse_codex_usage(&event_data) {
                            metadata.insert(META_USAGE.to_string(), UsageReporter::usage_metadata(&detail));
                            reporter.publish(detail);
                        }
                        let out = if prepared.response_format == Format::OpenAIResponse {
                            ensure_responses_usage_details(&out)
                        } else {
                            out
                        };
                        return Ok(Response { payload: Bytes::from(out), metadata, headers: resp_headers });
                    }
                    _ => {}
                }
            }
        }
        if prepared.apply_patch.finish().is_err() {
            return Err(status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE));
        }
        Err(status_err(
            408,
            "xai stream error: stream disconnected before response.completed or response.incomplete",
        ))
    }

    /// Go: executeCompact.
    pub(super) async fn execute_compact(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
    ) -> Result<Response, ExecError> {
        let (mut prepared, data, headers, reporter) = self.execute_compact_request(auth, req, opts).await?;
        let result = async {
            let converted = prepared
                .apply_patch
                .bridge
                .transform_non_stream(&data)
                .map_err(|_| status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE))?;
            let mut param = Param::default();
            let out = cpa_translator::translate_non_stream(
                &Ctx::default(),
                prepared.to,
                prepared.response_format,
                &req.model,
                &prepared.original_payload,
                &prepared.body,
                &converted,
                &mut param,
            );
            let out = match out {
                Some(out) if !out.is_empty() && apply_patch_translation_error(&param).is_none() => out,
                _ => return Err(status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)),
            };
            let detail = parse_openai_usage(&data);
            let mut metadata = Metadata::new();
            metadata.insert(META_USAGE.to_string(), UsageReporter::usage_metadata(&detail));
            reporter.publish(detail);
            let out = if prepared.response_format == Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out };
            Ok(Response { payload: Bytes::from(out), metadata, headers })
        }
        .await;
        reporter.track_failure(&result);
        result
    }

    /// Go: executeCompactRequest. Sends the compact request and returns the prepared request,
    /// the upstream body and headers, and the usage reporter of the attempt.
    pub(super) async fn execute_compact_request(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
    ) -> Result<(PreparedRequest, Bytes, HeaderMap, UsageReporter), ExecError> {
        let cfg = self.config();
        let session = ensure_session_id(None, "", opts, &req.payload);
        let (token, _) = creds(Some(auth));
        // Compact must not use the chat base URL: the CLI chat proxy answers 404 for
        // /responses/compact and a 404 cools the whole xAI auth pool down.
        let base_url = compact_base_url(Some(auth));
        log_resolved_base_url(&base_url);

        let mut prepared = prepare_responses_request_to(&cfg, req, opts, false, Format::OpenAIResponse)?;
        let mut body = cpa_json::parse(&prepared.body);
        cpa_json::delete(&mut body, "stream");
        cpa_json::delete(&mut body, "tools");
        // Compact deletes tools after preparation, which may have kept image_generation and
        // rewritten its forced choice to "required"; drop the leftover selection.
        super::tools::normalize_tool_choice_for_tools(&mut body);
        for field in ["max_output_tokens", "temperature", "top_p", "top_k", "stop"] {
            cpa_json::delete(&mut body, field);
        }
        remove_input_items_by_type(&mut body, "compaction_trigger");
        let previous_response_id = ts(&cpa_json::parse(&req.payload), "previous_response_id");
        if !previous_response_id.is_empty() {
            cpa_json::set(&mut body, "previous_response_id", previous_response_id);
        }
        prepared.body = cpa_json::to_vec(&body);

        let reporter = self.new_reporter(&prepared.base_model, auth, opts);
        let result = async {
            reporter.set_translated_reasoning_effort(&prepared.body, super::request::IDENTIFIER);
            let url = format!("{}/responses/compact", base_url.trim_end_matches('/'));
            // Official API and custom compact endpoints use standard API headers, not the CLI
            // chat-proxy identity headers.
            let headers = apply_headers(Some(auth), &token, false, &prepared.session_id, opts, session.as_deref())?;
            self.record_request(&cfg, auth, opts, &url, &headers, &prepared.body);
            let resp = self.send(&cfg, auth, opts, &reporter, &url, headers, prepared.body.clone()).await?;
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            let data = read_body(&cfg, opts, &reporter, resp).await?;
            if !(200..300).contains(&status) {
                tracing::debug!(
                    "request error, error status: {status}, error message: {}",
                    crate::helps::logging::summarize_error_body(&content_type(&resp_headers), &data)
                );
                return Err(status_err_for_body(status, &data));
            }
            reporter.observe_response_model(&data);
            clear_reasoning_replay_after_compaction(&prepared.replay_scope);
            Ok((data, resp_headers))
        }
        .await;
        match result {
            Ok((data, headers)) => Ok((prepared, data, headers, reporter)),
            Err(err) => {
                reporter.publish_failure(&err);
                Err(err)
            }
        }
    }

    /// Go: executeCompactionTriggerStream. A `compaction_trigger` input item runs the compact
    /// endpoint and replays its result as a synthetic Responses event stream.
    pub(super) async fn execute_compaction_trigger_stream(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
    ) -> Result<StreamResult, ExecError> {
        let (prepared, data, mut headers, reporter) = self.execute_compact_request(auth, req, opts).await?;
        reporter.publish(parse_openai_usage(&data));
        headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("text/event-stream"));
        let chunks = build_compaction_trigger_stream_chunks(&prepared, &data);
        let (tx, rx) = mpsc::channel(chunks.len().max(1));
        for chunk in chunks {
            // The channel holds every chunk, so sending cannot fail or block.
            let _ = tx.try_send(Ok(Bytes::from(chunk)));
        }
        drop(tx);
        Ok(StreamResult::new(headers, rx))
    }
}

pub(super) fn content_type(headers: &HeaderMap) -> String {
    headers.get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string()
}

/// Reads a whole upstream body, marking the first byte for TTFT, and records it in the
/// request log (Go: `io.ReadAll` then `RecordAPIResponseError` / `AppendAPIResponseChunk`).
pub(super) async fn read_body(
    cfg: &cpa_config::Config,
    opts: &Options,
    reporter: &UsageReporter,
    resp: reqwest::Response,
) -> Result<Bytes, ExecError> {
    use futures_util::StreamExt;
    let mut stream = Box::pin(reporter.observe_body_stream(resp.bytes_stream(), false));
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => buf.extend_from_slice(&chunk),
            Err(e) => {
                let err = transport_error(&e);
                opts.api_log.record_api_response_error(cfg, &err.message);
                return Err(err);
            }
        }
    }
    opts.api_log.append_api_response_chunk(cfg, &buf);
    Ok(Bytes::from(buf))
}

/// Go: xaiInputHasItemType.
pub(super) fn input_has_item_type(body: &[u8], item_type: &str) -> bool {
    let parsed = cpa_json::parse(body);
    items(&parsed, "input").iter().any(|item| s(item, "type") == item_type)
}

/// Go: xaiRemoveInputItemsByType.
fn remove_input_items_by_type(body: &mut Value, item_type: &str) {
    let Some(input) = at(body, "input").and_then(Value::as_array) else { return };
    let kept: Vec<Value> = input.iter().filter(|i| s(i, "type") != item_type).cloned().collect();
    cpa_json::set(body, "input", Value::Array(kept));
}

fn build_sse_frame(event_name: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(event_name.len() + data.len() + 16);
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event_name.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\n\n");
    out
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Go: xaiCompactionResponseID.
fn compaction_response_id(compact: &Value) -> String {
    let response_id = ts(compact, "id");
    if !response_id.is_empty() {
        if response_id.starts_with("resp_") {
            return response_id;
        }
        return format!("resp_{}", response_id.strip_prefix("cmp_").unwrap_or(&response_id));
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("resp_xai_compaction_{nanos}")
}

/// Go: xaiCompactionItemID.
fn compaction_item_id(response_id: &str) -> String {
    match response_id.strip_prefix("resp_") {
        Some(suffix) if !suffix.is_empty() => format!("cmp_{suffix}"),
        _ => format!("cmp_{response_id}"),
    }
}

/// Go: xaiCompactionOutputItem.
fn compaction_output_item(compact: &Value, response_id: &str) -> Value {
    let mut item = match at(compact, "output.0") {
        Some(v) if v.is_object() || v.is_array() => v.clone(),
        _ => json!({"type": "compaction"}),
    };
    if !item.g("type").exists() {
        cpa_json::set(&mut item, "type", "compaction");
    }
    if !item.g("id").exists() {
        cpa_json::set(&mut item, "id", compaction_item_id(response_id));
    }
    item
}

/// Go: xaiBuildCompactionBaseResponse.
fn compaction_base_response(
    prepared: &PreparedRequest,
    compact: &Value,
    response_id: &str,
    created_at: i64,
    status: &str,
) -> Value {
    let mut response = json!({
        "id": "",
        "object": "response",
        "created_at": 0,
        "status": "",
        "background": false,
        "error": null,
        "incomplete_details": null,
        "output": [],
    });
    cpa_json::set(&mut response, "id", response_id);
    cpa_json::set(&mut response, "created_at", created_at);
    cpa_json::set(&mut response, "status", status);
    let model = s(compact, "model");
    if !model.is_empty() {
        cpa_json::set(&mut response, "model", model);
    } else if !prepared.base_model.is_empty() {
        cpa_json::set(&mut response, "model", prepared.base_model.clone());
    }
    let body = cpa_json::parse(&prepared.body);
    for field in [
        "instructions",
        "max_output_tokens",
        "max_tool_calls",
        "parallel_tool_calls",
        "previous_response_id",
        "prompt_cache_key",
        "reasoning",
        "text",
        "tool_choice",
        "tools",
        "top_logprobs",
        "top_p",
        "truncation",
        "user",
        "metadata",
    ] {
        if let Some(v) = at(&body, field) {
            cpa_json::set(&mut response, field, v.clone());
        }
    }
    response
}

/// Go: xaiBuildCompactionTriggerStreamChunks.
fn build_compaction_trigger_stream_chunks(prepared: &PreparedRequest, compact_data: &[u8]) -> Vec<Vec<u8>> {
    let compact = cpa_json::parse(compact_data);
    let response_id = compaction_response_id(&compact);
    let now = now_unix();
    let mut created_at = compact.g("created_at").int();
    if created_at == 0 {
        created_at = now;
    }
    let mut completed_at = compact.g("completed_at").int();
    if completed_at == 0 {
        completed_at = now;
    }
    let item = compaction_output_item(&compact, &response_id);
    let output = Value::Array(vec![item.clone()]);

    let mut created_response = compaction_base_response(prepared, &compact, &response_id, created_at, "in_progress");
    let mut in_progress_response = compaction_base_response(prepared, &compact, &response_id, created_at, "in_progress");
    let mut completed_response = compaction_base_response(prepared, &compact, &response_id, created_at, "completed");
    let mut request_model_name = ts(&cpa_json::parse(&prepared.original_payload), "model");
    if request_model_name.is_empty() {
        request_model_name = prepared.base_model.clone();
    }
    if request_model_name.is_empty() {
        request_model_name = s(&compact, "model");
    }
    if !request_model_name.is_empty() {
        cpa_json::set(&mut created_response, "model", request_model_name.clone());
        cpa_json::set(&mut in_progress_response, "model", request_model_name);
    }
    cpa_json::set(&mut completed_response, "completed_at", completed_at);
    cpa_json::set(&mut completed_response, "output", output);
    if let Some(usage) = at(&compact, "usage") {
        cpa_json::set(&mut completed_response, "usage", usage.clone());
    }

    let mut created_payload = json!({"type": "response.created", "sequence_number": 0});
    cpa_json::set(&mut created_payload, "response", created_response);
    let mut in_progress_payload = json!({"type": "response.in_progress", "sequence_number": 1});
    cpa_json::set(&mut in_progress_payload, "response", in_progress_response);
    let mut added_payload = json!({"type": "response.output_item.added", "sequence_number": 2, "output_index": 0});
    cpa_json::set(&mut added_payload, "item", item.clone());
    let keepalive_payload = json!({"type": "keepalive", "sequence_number": 3});
    let mut done_payload = json!({"type": "response.output_item.done", "sequence_number": 4, "output_index": 0});
    cpa_json::set(&mut done_payload, "item", item);
    let mut completed_payload = json!({"type": "response.completed", "sequence_number": 5});
    cpa_json::set(&mut completed_payload, "response", completed_response);
    let completed_bytes = ensure_responses_usage_details(&cpa_json::to_vec(&completed_payload));

    vec![
        build_sse_frame("response.created", &cpa_json::to_vec(&created_payload)),
        build_sse_frame("response.in_progress", &cpa_json::to_vec(&in_progress_payload)),
        build_sse_frame("response.output_item.added", &cpa_json::to_vec(&added_payload)),
        build_sse_frame("keepalive", &cpa_json::to_vec(&keepalive_payload)),
        build_sse_frame("response.output_item.done", &cpa_json::to_vec(&done_payload)),
        build_sse_frame("response.completed", &completed_bytes),
    ]
}

//! Non-stream execution (Go: antigravity_executor_execute.go).

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_json::J;
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, Response};
use cpa_translator::{Ctx, Format, Param, translate_non_stream};
use serde_json::{Map, Value, json};

use super::AntigravityExecutor;
use super::compaction::{
    build_compaction_response, expand_compaction_capsules, extract_summary_text, has_responses_compaction_item,
    has_responses_compaction_trigger, prepare_summary_payload, seal_compaction,
};
use super::credits::clear_credits_failure_state;
use super::grounding::{resolve_grounding_urls, should_resolve_grounding_urls};
use super::pipeline::{Mode, Prepared, base_model_of, pre_send};
use super::replay_capture::{ReplayAccumulator, cache_reasoning_replay_from_response};
use crate::helps::apply_patch::{APPLY_PATCH_UPSTREAM_ERROR_MESSAGE, apply_patch_original_request};
use crate::helps::proxy::effective_proxy_url;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::sse::{LineReader, STREAM_SCANNER_BUFFER};
use crate::helps::text::json_payload;
use crate::helps::usage::{
    UsageReporter, filter_sse_usage_metadata, parse_antigravity_usage, parse_openai_usage,
};

/// Expands compaction capsules in the request (and original request) into developer messages.
pub(crate) fn expand_capsules_in_request(req: &mut Request, opts: &mut Options) -> Result<(), ExecError> {
    if !has_responses_compaction_item(&req.payload) {
        return Ok(());
    }
    let expanded = expand_compaction_capsules(&req.payload).map_err(|e| pre_send(ExecError::new(400, e)))?;
    if !opts.original_request.is_empty() {
        opts.original_request = match expand_compaction_capsules(&opts.original_request) {
            Ok(v) => v.into(),
            Err(_) => expanded.clone().into(),
        };
    }
    req.payload = expanded.into();
    Ok(())
}

/// Whether the (non-stream) request is a compaction request.
fn is_compaction_request(req: &Request, opts: &Options) -> bool {
    opts.alt == "responses/compact"
        || has_responses_compaction_trigger(&req.payload)
        || has_responses_compaction_trigger(&opts.original_request)
}

/// Models served through an aggregated stream even for non-stream clients.
fn aggregates_stream(base_model: &str) -> bool {
    base_model.to_lowercase().contains("claude")
        || base_model.contains("gemini-3-pro")
        || base_model.contains("gemini-3.1-flash-image")
}

pub(crate) fn usage_metadata_of(reporter: &UsageReporter) -> Metadata {
    let mut md = Metadata::new();
    if let Some(record) = reporter.record() {
        md.insert("usage".into(), UsageReporter::usage_metadata(&record.detail));
    }
    md
}

impl AntigravityExecutor {
    pub(crate) async fn execute_impl(&self, auth: &Auth, mut req: Request, mut opts: Options) -> Result<Response, ExecError> {
        let cfg = self.cfg();
        expand_capsules_in_request(&mut req, &mut opts)?;
        if is_compaction_request(&req, &opts) {
            return self.execute_compaction(auth, req, opts).await;
        }
        let base_model = base_model_of(&req.model);
        self.check_short_cooldown(&cfg, auth, &base_model, &opts).await?;

        let mode = if aggregates_stream(&base_model) { Mode::AggregatedStream } else { Mode::NonStream };
        let p = self.prepare(cfg, auth, &mut req, &opts, mode).await?;
        let result = match mode {
            Mode::AggregatedStream => self.run_aggregated(&p, &req, &opts).await,
            _ => self.run_non_stream(&p, &req, &opts).await,
        };
        if let Err(err) = &result {
            p.reporter.publish_failure(err);
        }
        result
    }

    async fn run_non_stream(&self, p: &Prepared, req: &Request, opts: &Options) -> Result<Response, ExecError> {
        let resp = self.send(p).await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(|e| crate::helps::status::transport_error(&e))?;
        p.reporter.mark_first_response_byte();
        if !(200..300).contains(&status) {
            return Err(self.handle_upstream_error(p, status, &body).await);
        }
        if p.use_credits {
            clear_credits_failure_state(&p.auth);
        }
        cache_reasoning_replay_from_response(&p.replay_scope, &p.request_payload, &body);
        let body = self.resolve_web_search_grounding(p, opts, body.to_vec()).await;
        self.finish_non_stream(p, req, opts, body, headers)
    }

    async fn run_aggregated(&self, p: &Prepared, req: &Request, opts: &Options) -> Result<Response, ExecError> {
        let resp = self.send(p).await?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        if !(200..300).contains(&status) {
            let body = resp.bytes().await.map_err(|e| crate::helps::status::transport_error(&e))?;
            return Err(self.handle_upstream_error(p, status, &body).await);
        }
        if p.use_credits {
            clear_credits_failure_state(&p.auth);
        }
        let mut accumulator = ReplayAccumulator::new(&p.replay_scope, &p.request_payload);
        let mut reader = LineReader::from_response(resp, STREAM_SCANNER_BUFFER);
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(line) = reader.next_line().await {
            let line = line.map_err(ExecError::from)?;
            p.reporter.mark_first_response_byte();
            if let Some(acc) = accumulator.as_mut() {
                acc.observe_sse_line(&line);
            }
            let line = filter_sse_usage_metadata(&line);
            let Some(payload) = json_payload(&line) else { continue };
            p.reporter.observe_response_model(payload);
            buffer.extend_from_slice(payload);
            buffer.push(b'\n');
        }
        if let Some(acc) = accumulator.as_mut() {
            acc.commit();
        }
        let aggregated = convert_stream_to_non_stream(&buffer);
        let aggregated = self.resolve_web_search_grounding(p, opts, aggregated).await;
        self.finish_non_stream(p, req, opts, aggregated, headers)
    }

    async fn resolve_web_search_grounding(&self, p: &Prepared, opts: &Options, body: Vec<u8>) -> Vec<u8> {
        if !should_resolve_grounding_urls(p.from, &p.original_payload, &p.translated) {
            return body;
        }
        let proxy = effective_proxy_url(&opts.proxy_url, Some(&p.auth), Some(&p.cfg));
        resolve_grounding_urls(&proxy, body).await
    }

    /// Translates the upstream body to the client format and publishes usage.
    fn finish_non_stream(
        &self,
        p: &Prepared,
        req: &Request,
        opts: &Options,
        body: Vec<u8>,
        headers: http::HeaderMap,
    ) -> Result<Response, ExecError> {
        p.reporter.observe_response_model(&body);
        let mut param = Param::default();
        let ctx = Ctx { alt: Some(opts.alt.clone()) };
        let original = apply_patch_original_request(req, opts);
        let converted = translate_non_stream(
            &ctx,
            Format::Antigravity,
            p.response_format,
            &req.model,
            &original,
            &p.translated,
            &body,
            &mut param,
        );
        let converted = match converted {
            Some(c) if !c.is_empty() && param.tool_input_error.is_none() => c,
            _ => return Err(ExecError::new(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)),
        };
        p.reporter.publish(parse_antigravity_usage(&body));
        let converted = if p.response_format == Format::OpenAIResponse {
            ensure_responses_usage_details(&converted)
        } else {
            converted
        };
        p.reporter.ensure_published();
        Ok(Response { payload: Bytes::from(converted), metadata: usage_metadata_of(&p.reporter), headers })
    }

    /// Summary request for `/responses/compact` or a `compaction_trigger` item. Returns the
    /// sealed capsule data for the caller to shape (JSON or SSE).
    pub(crate) async fn run_compaction_summary(
        &self,
        auth: &Auth,
        req: Request,
        opts: Options,
    ) -> Result<CompactionOutcome, ExecError> {
        let base_model = base_model_of(&req.model);
        let payload: Bytes =
            if req.payload.is_empty() && !opts.original_request.is_empty() { opts.original_request.clone() } else { req.payload.clone() };
        let summary_req = Request {
            model: req.model.clone(),
            payload: prepare_summary_payload(&payload, &base_model).into(),
            format: Format::OpenAIResponse,
            metadata: req.metadata.clone(),
        };
        let mut summary_opts = opts.clone();
        summary_opts.alt = String::new();
        summary_opts.stream = false;
        summary_opts.original_request = Bytes::new();
        summary_opts.source_format = Format::OpenAIResponse;
        summary_opts.response_format = Some(Format::OpenAIResponse);

        let summary = Box::pin(self.execute_impl(auth, summary_req, summary_opts)).await?;
        let text = extract_summary_text(&summary.payload).map_err(|e| ExecError::new(0, format!("extract summary: {e}")))?;
        let capsule =
            seal_compaction(&text, &base_model).map_err(|e| ExecError::new(0, format!("seal compaction capsule: {e}")))?;

        let v = cpa_json::parse(&summary.payload);
        let mut input_tokens = v.g("usage.input_tokens").int();
        let mut output_tokens = v.g("usage.output_tokens").int();
        let mut total_tokens = v.g("usage.total_tokens").int();
        if total_tokens == 0 && input_tokens == 0 {
            let usage = parse_openai_usage(&summary.payload);
            input_tokens = usage.input_tokens;
            output_tokens = usage.output_tokens;
            total_tokens = usage.total_tokens;
        }
        Ok(CompactionOutcome { base_model, capsule, input_tokens, output_tokens, total_tokens, headers: summary.headers })
    }

    async fn execute_compaction(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let o = self.run_compaction_summary(auth, req, opts).await?;
        Ok(Response {
            payload: build_compaction_response(&o.base_model, &o.capsule, o.input_tokens, o.output_tokens, o.total_tokens)
                .into(),
            metadata: Metadata::new(),
            headers: o.headers,
        })
    }
}

pub(crate) struct CompactionOutcome {
    pub base_model: String,
    pub capsule: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub headers: http::HeaderMap,
}

// ---------------------------------------------------------------- stream aggregation

/// `{"text": ...}` and `{"thought": true, ...}` segments are merged; function and inline data
/// parts are kept as-is. Parts go through a map round trip like Go's, which sorts keys and turns
/// numbers into float64 text.
fn normalize_part(part: &Value) -> Value {
    let mut m = match part {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    let mut sig = part.g("thoughtSignature").str();
    if sig.is_empty() {
        sig = part.g("thought_signature").str();
    }
    if !sig.is_empty() {
        m.insert("thoughtSignature".into(), Value::String(sig));
        m.shift_remove("thought_signature");
    }
    if let Some(inline) = m.get("inline_data").cloned() {
        m.insert("inlineData".into(), inline);
        m.shift_remove("inline_data");
    }
    let v = Value::Object(m);
    match cpa_core::util::go_json_sorted(&v, cpa_core::util::GoJsonStyle::MARSHAL_ANY) {
        Some(s) => cpa_json::parse_str(&s),
        None => v,
    }
}

/// Merges the stream chunks into one non-stream `{"response":{...},"traceId":...}` body.
pub(crate) fn convert_stream_to_non_stream(stream: &[u8]) -> Vec<u8> {
    let mut response_template: Option<Value> = None;
    let mut trace_id = String::new();
    let mut finish_reason = String::new();
    let mut model_version = String::new();
    let mut response_id = String::new();
    let mut role = String::new();
    let mut usage: Option<Value> = None;
    let mut parts: Vec<Value> = Vec::new();
    let mut pending_kind = "";
    let mut pending_text = String::new();
    let mut pending_sig = String::new();

    fn flush(parts: &mut Vec<Value>, kind: &mut &str, text: &mut String, sig: &mut String) {
        match *kind {
            "text" => {
                if !text.trim().is_empty() {
                    parts.push(json!({"text": text.clone()}));
                }
            }
            "thought" => {
                if !(text.trim().is_empty() && sig.is_empty()) {
                    let mut part = Map::new();
                    part.insert("text".into(), Value::String(text.clone()));
                    part.insert("thought".into(), Value::Bool(true));
                    if !sig.is_empty() {
                        part.insert("thoughtSignature".into(), Value::String(sig.clone()));
                    }
                    parts.push(Value::Object(part));
                }
            }
            _ => return,
        }
        *kind = "";
        text.clear();
        sig.clear();
    }

    for line in stream.split(|&b| b == b'\n') {
        let trimmed = crate::helps::text::trim_space(line);
        if trimmed.is_empty() || !cpa_json::valid(trimmed) {
            continue;
        }
        let root = cpa_json::parse(trimmed);
        let response_node = if root.g("response").exists() {
            root.g("response").value()
        } else if root.g("candidates").exists() {
            root.clone()
        } else {
            continue;
        };
        response_template = Some(response_node.clone());

        let trace = root.g("traceId");
        if trace.exists() && !trace.str().is_empty() {
            trace_id = trace.str();
        }
        let role_node = response_node.g("candidates.0.content.role");
        if role_node.exists() {
            role = role_node.str();
        }
        let finish = response_node.g("candidates.0.finishReason");
        if finish.exists() && !finish.str().is_empty() {
            finish_reason = finish.str();
        }
        let mv = response_node.g("modelVersion");
        if mv.exists() && !mv.str().is_empty() {
            model_version = mv.str();
        }
        let rid = response_node.g("responseId");
        if rid.exists() && !rid.str().is_empty() {
            response_id = rid.str();
        }
        if let Some(u) = response_node.g("usageMetadata").into_value() {
            usage = Some(u);
        } else if let Some(u) = root.g("usageMetadata").into_value() {
            usage = Some(u);
        }

        let parts_node = response_node.g("candidates.0.content.parts");
        if !parts_node.is_array() {
            continue;
        }
        for part in parts_node.array() {
            let part = part.value();
            let has_function_call = part.g("functionCall").exists();
            let has_inline_data = part.g("inlineData").exists() || part.g("inline_data").exists();
            let mut sig = part.g("thoughtSignature").str();
            if sig.is_empty() {
                sig = part.g("thought_signature").str();
            }
            let text = part.g("text").str();
            let thought = part.g("thought").bool();

            if has_function_call || has_inline_data {
                flush(&mut parts, &mut pending_kind, &mut pending_text, &mut pending_sig);
                parts.push(normalize_part(&part));
                continue;
            }
            if thought || part.g("text").exists() {
                let kind = if thought { "thought" } else { "text" };
                if !pending_kind.is_empty() && pending_kind != kind {
                    flush(&mut parts, &mut pending_kind, &mut pending_text, &mut pending_sig);
                }
                pending_kind = kind;
                pending_text.push_str(&text);
                if kind == "thought" && !sig.is_empty() {
                    pending_sig = sig;
                }
                continue;
            }
            flush(&mut parts, &mut pending_kind, &mut pending_text, &mut pending_sig);
            parts.push(normalize_part(&part));
        }
    }
    flush(&mut parts, &mut pending_kind, &mut pending_text, &mut pending_sig);

    let mut template =
        response_template.unwrap_or_else(|| json!({"candidates": [{"content": {"role": "model", "parts": []}}]}));
    cpa_json::set(&mut template, "candidates.0.content.parts", Value::Array(parts));
    if !role.is_empty() {
        cpa_json::set(&mut template, "candidates.0.content.role", role);
    }
    if !finish_reason.is_empty() {
        cpa_json::set(&mut template, "candidates.0.finishReason", finish_reason);
    }
    if !model_version.is_empty() {
        cpa_json::set(&mut template, "modelVersion", model_version);
    }
    if !response_id.is_empty() {
        cpa_json::set(&mut template, "responseId", response_id);
    }
    if let Some(u) = usage {
        cpa_json::set(&mut template, "usageMetadata", u);
    } else if !template.g("usageMetadata").exists() {
        cpa_json::set(&mut template, "usageMetadata.promptTokenCount", 0);
        cpa_json::set(&mut template, "usageMetadata.candidatesTokenCount", 0);
        cpa_json::set(&mut template, "usageMetadata.totalTokenCount", 0);
    }

    let mut output = json!({"response": template, "traceId": ""});
    if !trace_id.is_empty() {
        cpa_json::set(&mut output, "traceId", trace_id);
    }
    cpa_json::to_vec(&output)
}

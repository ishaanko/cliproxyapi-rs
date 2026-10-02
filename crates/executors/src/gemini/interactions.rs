//! Native Gemini Interactions API requests (Go: executeInteractions / executeInteractionsStream and
//! the interactions helpers of gemini_executor.go). Used when a `gemini-interactions` credential
//! serves an Interactions-capable client protocol.

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::thinking::parse_suffix;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Options, Request, Response, StreamResult};
use cpa_translator::{Ctx, Format, Param};
use http::{HeaderMap, HeaderValue};

use super::common::{
    GL_API_VERSION, PumpSetup, StreamPump, apply_patch_gateway_error, error_body, observed_lines, post_json, read_body,
    set_model, thinking_error, translate_request, upstream_error, usage_metadata,
};
use super::executor::{GeminiExecutor, request_headers, resolve_base_url};
use crate::helps::apply_patch::{apply_patch_original_request, apply_patch_translation_error};
use crate::helps::payload::{
    PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model, set_bool_if_different,
};
use crate::helps::proxy::new_proxy_aware_http_client;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::text::trim_space;
use crate::helps::thinking::{api_key_model_is_compat, apply_request_thinking};
use crate::helps::usage::{parse_interactions_stream_usage, parse_interactions_usage};

/// Default `Api-Revision` for native Interactions requests.
const API_REVISION: &str = "2026-05-20";

/// Translates the client payload to Interactions; native Interactions payloads pass through.
fn translate_body(opts: &Options, model: &str, payload: &[u8], stream: bool, is_compat: bool) -> Vec<u8> {
    if opts.source_format == Format::Interactions {
        return payload.to_vec();
    }
    translate_request(&opts.headers, opts.source_format, Format::Interactions, model, payload, stream, is_compat)
}

/// Returns `(payload-config baseline, working payload)`; identical inputs translate once.
fn translate_request_pair(
    opts: &Options,
    model: &str,
    payload: &[u8],
    stream: bool,
    is_compat: bool,
) -> (Vec<u8>, Vec<u8>) {
    let source: &[u8] = if opts.original_request.is_empty() { payload } else { &opts.original_request };
    let working = translate_body(opts, model, payload, stream, is_compat);
    if source == payload {
        return (working.clone(), working);
    }
    (translate_body(opts, model, source, stream, is_compat), working)
}

/// Aligns step ids with the Interactions schema: `function_call` takes `id` (not `call_id`),
/// every other step and every content part rejects `id` (Go:
/// sanitizeGeminiInteractionsUnsupportedInputIDs).
pub(super) fn sanitize_unsupported_input_ids(body: &[u8]) -> Vec<u8> {
    let mut v = cpa_json::parse(body);
    let Some(Value::Array(input)) = v.g("input").v().cloned() else {
        return body.to_vec();
    };
    for (i, item) in input.iter().enumerate() {
        if item.g("type").str() == "function_call" {
            if !item.g("id").exists() && item.g("call_id").exists() {
                cpa_json::set(&mut v, &format!("input.{i}.id"), item.g("call_id").str());
            }
            if item.g("call_id").exists() {
                cpa_json::delete(&mut v, &format!("input.{i}.call_id"));
            }
        } else if item.g("id").exists() {
            cpa_json::delete(&mut v, &format!("input.{i}.id"));
        }
        let Some(Value::Array(content)) = item.get("content") else { continue };
        for (j, part) in content.iter().enumerate() {
            if part.g("id").exists() {
                cpa_json::delete(&mut v, &format!("input.{i}.content.{j}.id"));
            }
        }
    }
    cpa_json::to_vec(&v)
}

/// Sets `Api-Revision` from the client request when the auth headers did not, then the default.
fn apply_revision_headers(headers: &mut HeaderMap, client_headers: &HeaderMap) {
    if !headers.contains_key("api-revision")
        && let Some(revision) = client_headers.get("api-revision").filter(|v| !v.is_empty())
    {
        headers.insert("api-revision", revision.clone());
    }
    if !headers.contains_key("api-revision") {
        headers.insert("api-revision", HeaderValue::from_static(API_REVISION));
    }
}

struct InteractionsRequest {
    body: Vec<u8>,
    headers: HeaderMap,
    url: String,
}

fn prepare(
    cfg: &Config,
    auth: &Auth,
    req: &Request,
    opts: &Options,
    session_id: Option<&str>,
    target_name: &str,
    stream: bool,
) -> Result<InteractionsRequest, ExecError> {
    let is_compat = api_key_model_is_compat(req);
    let (original_translated, mut body) = translate_request_pair(opts, target_name, &req.payload, stream, is_compat);
    if cpa_json::parse(&body).g("model").exists() && !target_name.is_empty() {
        let mut v = cpa_json::parse(&body);
        set_model(&mut v, target_name);
        body = cpa_json::to_vec(&v);
    }
    let from = opts.source_format.as_str();
    let body = apply_request_thinking(&body, req, opts, from, Format::Interactions.as_str(), "gemini", false)
        .map_err(thinking_error)?;
    let requested_model = payload_requested_model(opts, &req.model);
    let request_path = payload_request_path(opts);
    let payload_req = PayloadRequest {
        cfg: Some(cfg),
        target_executor: "",
        model: target_name,
        protocol: Format::Interactions.as_str(),
        from_protocol: from,
        root: "",
        requested_model: &requested_model,
        request_path: &request_path,
        headers: Some(&opts.headers),
    };
    let body = apply_payload_config(&payload_req, &body, &original_translated);
    let mut body = sanitize_unsupported_input_ids(&body);
    if stream {
        let mut v = cpa_json::parse(&body);
        set_bool_if_different(&mut v, "stream", true);
        body = cpa_json::to_vec(&v);
    }
    let url = format!("{}/{GL_API_VERSION}/interactions", resolve_base_url(auth));
    let mut headers = request_headers(auth, opts, session_id)?;
    apply_revision_headers(&mut headers, &opts.headers);
    Ok(InteractionsRequest { body, headers, url })
}

pub(super) async fn execute(
    exec: &GeminiExecutor,
    cfg: &Config,
    auth: &Auth,
    req: Request,
    opts: Options,
    session_id: Option<String>,
) -> Result<Response, ExecError> {
    let target_name = parse_suffix(&req.model).model_name;
    let reporter = exec.reporter(&target_name, auth, &opts);
    let result = async {
        let prepared = prepare(cfg, auth, &req, &opts, session_id.as_deref(), &target_name, false)?;
        let client = new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(auth), None);
        reporter.start_response_ttft();
        let resp = post_json(&client, &prepared.url, prepared.headers, prepared.body.clone()).await?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        let data = read_body(resp).await?;
        reporter.mark_first_response_byte();
        if !(200..300).contains(&status) {
            return Err(upstream_error(status, &data));
        }
        reporter.observe_response_model(&data);
        let target_format = opts.response_format_or_source();
        let mut param = Param::default();
        let original = apply_patch_original_request(&req, &opts);
        let out = cpa_translator::translate_non_stream(
            &Ctx::default(),
            Format::Interactions,
            target_format,
            &req.model,
            &original,
            &prepared.body,
            &data,
            &mut param,
        );
        let out = match out {
            Some(out) if apply_patch_translation_error(&param).is_none() && !out.is_empty() => out,
            _ => return Err(apply_patch_gateway_error()),
        };
        let detail = parse_interactions_usage(&data);
        reporter.publish(detail.clone());
        let out = if target_format == Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out };
        Ok(Response { payload: Bytes::from(out), metadata: usage_metadata(&detail), headers: resp_headers })
    }
    .await;
    reporter.track_failure(&result);
    result
}

pub(super) async fn execute_stream(
    exec: &GeminiExecutor,
    cfg: &Config,
    auth: &Auth,
    req: Request,
    opts: Options,
    session_id: Option<String>,
) -> Result<StreamResult, ExecError> {
    let target_name = parse_suffix(&req.model).model_name;
    let reporter = exec.reporter(&target_name, auth, &opts);
    let result = async {
        let prepared = prepare(cfg, auth, &req, &opts, session_id.as_deref(), &target_name, true)?;
        let client = new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(auth), None);
        reporter.start_response_ttft();
        let resp = post_json(&client, &prepared.url, prepared.headers, prepared.body.clone()).await?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        if !(200..300).contains(&status) {
            return Err(upstream_error(status, &error_body(resp).await));
        }
        let response_format = opts.response_format_or_source();
        let (mut pump, rx, usage_rx) = StreamPump::new(PumpSetup {
            reporter: reporter.clone(),
            from: opts.source_format,
            upstream: Format::Interactions,
            response: response_format,
            req: &req,
            opts: &opts,
            body: prepared.body,
            ctx: Ctx::default(),
        });
        let reporter = reporter.clone();
        tokio::spawn(async move {
            let mut lines = observed_lines(reporter.clone(), resp);
            let mut frame: Vec<u8> = Vec::new();
            let mut scan_err = None;
            loop {
                let line = tokio::select! {
                    _ = pump.tx.closed() => return,
                    line = lines.next_line() => line,
                };
                let line = match line {
                    None => break,
                    Some(Ok(line)) => line,
                    Some(Err(err)) => {
                        scan_err = Some(err);
                        break;
                    }
                };
                if trim_space(&line).is_empty() {
                    if !emit_frame(&mut pump, &reporter, &mut frame, response_format).await {
                        return;
                    }
                    continue;
                }
                if !frame.is_empty() {
                    frame.push(b'\n');
                }
                frame.extend_from_slice(&line);
            }
            if !emit_frame(&mut pump, &reporter, &mut frame, response_format).await {
                return;
            }
            if pump.end_apply_patch().await {
                return;
            }
            if let Some(err) = scan_err {
                pump.fail(err.into()).await;
            }
            pump.finish();
        });
        let mut stream = StreamResult::new(resp_headers, rx);
        stream.usage = Some(usage_rx);
        Ok(stream)
    }
    .await;
    reporter.track_failure(&result);
    result
}

/// Handles one accumulated SSE frame: usage and model observation, verbatim forwarding for
/// Interactions clients, translation otherwise.
async fn emit_frame(
    pump: &mut StreamPump,
    reporter: &crate::helps::usage::UsageReporter,
    frame: &mut Vec<u8>,
    response_format: Format,
) -> bool {
    let raw_frame = std::mem::take(frame);
    let trimmed = trim_space(&raw_frame);
    if trimmed.is_empty() {
        return true;
    }
    let mut payload = sse_payload(&raw_frame);
    if payload.is_empty() && sse_done(&raw_frame) {
        payload = b"[DONE]".to_vec();
    }
    if payload.is_empty() && trimmed.first() == Some(&b'{') {
        payload = trimmed.to_vec();
    }
    if !payload.is_empty() {
        reporter.observe_response_model(&payload);
        if let Some(detail) = parse_interactions_stream_usage(&payload) {
            pump.usage.observe(detail, true);
        }
    }
    if response_format == Format::Interactions {
        let mut visible = raw_frame.clone();
        while visible.last().is_some_and(|b| *b == b'\r' || *b == b'\n') {
            visible.pop();
        }
        visible.extend_from_slice(b"\n\n");
        return pump.send_raw(visible).await;
    }
    if payload.is_empty() {
        return true;
    }
    pump.feed(&payload).await
}

/// JSON payload of an SSE frame: the whole frame when it is a bare object, else its `data:` lines
/// joined by newlines (`[DONE]` lines skipped). Empty when there is none.
pub(super) fn sse_payload(frame: &[u8]) -> Vec<u8> {
    let trimmed = trim_space(frame);
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed[0] == b'{' {
        return trimmed.to_vec();
    }
    let mut payload: Vec<u8> = Vec::new();
    for line in frame.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !trim_space(line).starts_with(b"data:") {
            continue;
        }
        let idx = line.windows(5).position(|w| w == b"data:").unwrap_or(0);
        let data = trim_space(&line[idx + 5..]);
        if data.is_empty() || data == b"[DONE]" {
            continue;
        }
        if !payload.is_empty() {
            payload.push(b'\n');
        }
        payload.extend_from_slice(data);
    }
    payload
}

/// Whether a frame is the terminator: a bare `[DONE]`, a `data: [DONE]` line or `event: done`.
pub(super) fn sse_done(frame: &[u8]) -> bool {
    if trim_space(frame) == b"[DONE]" {
        return true;
    }
    let mut saw_done_event = false;
    for line in frame.split(|b| *b == b'\n') {
        let line = trim_space(line.strip_suffix(b"\r").unwrap_or(line));
        if line.eq_ignore_ascii_case(b"event: done") {
            saw_done_event = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"data:")
            && trim_space(rest) == b"[DONE]"
        {
            return true;
        }
    }
    saw_done_event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_ids_follow_interactions_schema() {
        let body = br#"{"input":[{"type":"function_call","call_id":"c1","name":"f"},{"type":"function_result","call_id":"c1","id":"x"},{"type":"user_input","id":"u","content":[{"type":"text","id":"p","text":"hi"}]}]}"#;
        let v = cpa_json::parse(&sanitize_unsupported_input_ids(body));
        assert_eq!(v.g("input.0.id").str(), "c1");
        assert!(!v.g("input.0.call_id").exists());
        assert_eq!(v.g("input.1.call_id").str(), "c1");
        assert!(!v.g("input.1.id").exists());
        assert!(!v.g("input.2.id").exists());
        assert!(!v.g("input.2.content.0.id").exists());
        assert_eq!(v.g("input.2.content.0.text").str(), "hi");
        assert_eq!(sanitize_unsupported_input_ids(b"{}"), b"{}".to_vec());
    }

    #[test]
    fn frame_payload_and_done_detection() {
        assert_eq!(sse_payload(b"event: x\ndata: {\"a\":1}\r\ndata: {\"b\":2}"), b"{\"a\":1}\n{\"b\":2}".to_vec());
        assert_eq!(sse_payload(b"  {\"a\":1} "), b"{\"a\":1}".to_vec());
        assert!(sse_payload(b"data: [DONE]").is_empty());
        assert!(sse_done(b"data: [DONE]"));
        assert!(sse_done(b"event: done\ndata: {}"));
        assert!(sse_done(b"[DONE]"));
        assert!(!sse_done(b"data: {}"));
    }

    #[test]
    fn api_revision_prefers_auth_header_then_client_then_default() {
        let mut client = HeaderMap::new();
        client.insert("api-revision", HeaderValue::from_static("2030-01-01"));
        let mut headers = HeaderMap::new();
        apply_revision_headers(&mut headers, &client);
        assert_eq!(headers["api-revision"], "2030-01-01");
        let mut custom = HeaderMap::new();
        custom.insert("api-revision", HeaderValue::from_static("custom"));
        apply_revision_headers(&mut custom, &client);
        assert_eq!(custom["api-revision"], "custom");
        let mut none = HeaderMap::new();
        apply_revision_headers(&mut none, &HeaderMap::new());
        assert_eq!(none["api-revision"], API_REVISION);
    }
}

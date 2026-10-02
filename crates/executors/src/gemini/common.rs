//! Request/response plumbing shared by the Gemini API, Interactions, Vertex and AI Studio
//! executors: request translation (with API-key model compatibility), body fixes, headers, HTTP
//! sending and the streaming pump that feeds upstream frames through the response translator.

use std::collections::HashMap;
use std::future::Future;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::registry::lookup_model_info;
use cpa_core::thinking::ThinkingError;
use cpa_json::{J, Value, json};
use cpa_runtime::executor::{ExecError, Options, Request};
use cpa_translator::{Ctx, Format, Param};
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value as Json;
use tokio::sync::{mpsc, oneshot};

use crate::helps::claude_input_tokens::{ClaudeInputTokenState, translate_stream_with_claude_input_tokens};
use crate::helps::apply_patch::{
    ChunkSender, end_apply_patch_stream, gateway_error, record_apply_patch_stream_failure,
};
use crate::helps::translate::{RequestTranslation, translate_request as translate_request_shared};
use crate::helps::sse::{LineReader, STREAM_SCANNER_BUFFER};
use crate::helps::status::status_err;
use crate::helps::usage::{StreamUsageBuffer, UsageReporter};

/// Base URL of the Google Generative Language API.
pub(crate) const GL_ENDPOINT: &str = "https://generativelanguage.googleapis.com";
/// API version used for Gemini requests.
pub(crate) const GL_API_VERSION: &str = "v1beta";
/// Go's default transport user agent; the executors never set their own.
pub(crate) const GO_DEFAULT_UA: &str = "Go-http-client/1.1";

/// Marks a failure that happened before any request reached the upstream.
pub(crate) fn pre_send(mut err: ExecError) -> ExecError {
    err.upstream_attempted = false;
    err
}

/// A thinking-pipeline failure as a client error (HTTP 400, or 500 when an applier failed).
pub(crate) fn thinking_error(err: ThinkingError) -> ExecError {
    pre_send(ExecError::new(err.status_code(), err.message))
}

/// `statusErr{code, msg: body}` for a non-2xx upstream response.
pub(crate) fn upstream_error(status: u16, body: &[u8]) -> ExecError {
    status_err(status, String::from_utf8_lossy(body).into_owned())
}

/// `/responses/compact` is not supported by the Google executors.
pub(crate) fn compact_unsupported() -> ExecError {
    pre_send(status_err(501, "/responses/compact not supported"))
}

/// True when `req.metadata["action"]` is `countTokens`.
pub(crate) fn is_count_tokens_action(req: &Request) -> bool {
    req.metadata.get("action").and_then(Value::as_str) == Some("countTokens")
}

/// Upstream-bound original payload: `opts.original_request`, else the request payload.
pub(crate) fn original_payload<'a>(req: &'a Request, opts: &'a Options) -> &'a [u8] {
    if opts.original_request.is_empty() { &req.payload } else { &opts.original_request }
}

// ---------------------------------------------------------------- request translation

/// Translates a client payload to `to` through the shared stages (see
/// [`crate::helps::translate`]). Only a native Gemini client's malformed JSON is special: Go's
/// sjson-based normalizer leaves such a body as unusable fragments that the later body edits
/// discard, so nothing from the normalizer (default safety settings) survives; pass it through.
pub(crate) fn translate_request(
    cfg: &Config,
    headers: &HeaderMap,
    from: Format,
    to: Format,
    model: &str,
    payload: &[u8],
    stream: bool,
    is_compat: bool,
) -> Vec<u8> {
    if from == Format::Gemini && to == Format::Gemini && !cpa_json::valid(payload) {
        return payload.to_vec();
    }
    let translation = RequestTranslation::new(headers, Some(cfg), from, to, model, stream).compat(is_compat);
    translate_request_shared(&translation, payload).0
}

/// Translates the payload-config baseline and the working payload; identical inputs are
/// translated once. Returns `(original, working)`.
pub(crate) fn translate_request_pair(
    cfg: &Config,
    headers: &HeaderMap,
    from: Format,
    to: Format,
    model: &str,
    original: &[u8],
    working: &[u8],
    stream: bool,
    is_compat: bool,
) -> (Vec<u8>, Vec<u8>) {
    let translated_original = translate_request(cfg, headers, from, to, model, original, stream, is_compat);
    if original == working {
        let copy = translated_original.clone();
        return (translated_original, copy);
    }
    let translated_working = translate_request(cfg, headers, from, to, model, working, stream, is_compat);
    (translated_original, translated_working)
}

// ---------------------------------------------------------------- body fixes

/// `gemini-2.5-flash-image-preview` needs an input image when an aspect ratio is requested: a
/// white PNG of that ratio plus an instruction is prepended to the first content turn (Go:
/// fixGeminiImageAspectRatio).
pub(crate) fn fix_gemini_image_aspect_ratio(model: &str, body: Vec<u8>) -> Vec<u8> {
    if model != "gemini-2.5-flash-image-preview" {
        return body;
    }
    let mut v = cpa_json::parse(&body);
    let aspect = v.g("generationConfig.imageConfig.aspectRatio");
    if !aspect.exists() {
        return body;
    }
    let aspect = aspect.str();
    if let Some(Json::Array(contents)) = v.g("contents").v()
        && !contents.is_empty()
    {
        let has_inline_data = contents.iter().any(|content| match content.get("parts") {
            Some(Json::Array(parts)) => parts.iter().any(|part| part.g("inlineData").exists()),
            _ => false,
        });
        if !has_inline_data {
            let image = cpa_core::util::create_white_image_base64(&aspect).unwrap_or_default();
            let mut new_parts = vec![
                json!({"text": "Based on the following requirements, create an image within the uploaded picture. The new content *MUST* completely cover the entire area of the original picture, maintaining its exact proportions, and *NO* blank areas should appear."}),
                json!({"inlineData": {"mime_type": "image/png", "data": image}}),
            ];
            if let Some(Json::Array(parts)) = contents[0].get("parts") {
                new_parts.extend(parts.iter().cloned());
            }
            cpa_json::set(&mut v, "contents.0.parts", Json::Array(new_parts));
            cpa_json::set(&mut v, "generationConfig.responseModalities", json!(["IMAGE", "TEXT"]));
        }
    }
    cpa_json::delete(&mut v, "generationConfig.imageConfig");
    cpa_json::to_vec(&v)
}

/// Clamps `generationConfig.maxOutputTokens` to the registry's output limit for the model (Go:
/// capGeminiMaxOutputTokens).
pub(crate) fn cap_gemini_max_output_tokens(v: &mut Value, model: &str) {
    let max_out = v.g("generationConfig.maxOutputTokens");
    if !max_out.is_number() {
        return;
    }
    let Some(info) = lookup_model_info(model, Some("gemini")) else {
        return;
    };
    let mut limit = info.output_token_limit;
    if limit <= 0 {
        limit = info.max_completion_tokens;
    }
    if limit <= 0 || max_out.int() <= limit {
        return;
    }
    cpa_json::set(v, "generationConfig.maxOutputTokens", limit);
}

/// Sets `model` unless it already holds that string (Go: SetStringIfDifferent).
pub(crate) fn set_model(v: &mut Value, model: &str) {
    crate::helps::payload::set_string_if_different(v, "model", model);
}

// ---------------------------------------------------------------- headers and HTTP

/// `Content-Type: application/json` plus Go's default user agent.
pub(crate) fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(http::header::USER_AGENT, HeaderValue::from_static(GO_DEFAULT_UA));
    headers
}

/// Sets a header, failing like net/http does for values that are not valid on the wire.
pub(crate) fn set_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<(), ExecError> {
    let value = HeaderValue::from_str(value)
        .map_err(|_| pre_send(ExecError::new(0, format!("invalid header field value for {name:?}"))))?;
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}

/// Auth attributes as the map the custom-header helpers take.
pub(crate) fn attrs_map(auth: &Auth) -> HashMap<String, String> {
    auth.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Applies the credential's `header:<Name>` attributes (config `headers:`) over the built-in
/// headers, resolving `$Header` and `$CPA-SESSION-ID` values.
pub(crate) fn apply_custom_headers(headers: &mut HeaderMap, auth: &Auth, opts: &Options, session_id: Option<&str>) {
    cpa_core::util::apply_custom_headers_from_attrs(headers, &attrs_map(auth), Some(&opts.headers), session_id);
}

fn transport_failure(err: &reqwest::Error) -> ExecError {
    crate::helps::status::transport_error(err)
}

/// Sends a JSON POST; transport failures carry no status.
pub(crate) async fn post_json(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<reqwest::Response, ExecError> {
    client.post(url).headers(headers).body(body).send().await.map_err(|e| transport_failure(&e))
}

/// Reads a response body in full.
pub(crate) async fn read_body(resp: reqwest::Response) -> Result<Bytes, ExecError> {
    resp.bytes().await.map_err(|e| transport_failure(&e))
}

/// Line reader over a streaming response body that marks the first response byte for TTFT (Go:
/// the TTFT-tracking round tripper). Takes the reporter by value so the stream is `'static`.
pub(crate) fn observed_lines(reporter: UsageReporter, resp: reqwest::Response) -> LineReader {
    reporter.start_response_ttft();
    let mut marked = false;
    let stream = resp
        .bytes_stream()
        .inspect(move |item| {
            if !marked && item.as_ref().is_ok_and(|b| !b.is_empty()) {
                marked = true;
                reporter.mark_first_response_byte();
            }
        })
        .map(|item| item.map_err(|e| crate::helps::status::transport_message(&e)));
    LineReader::from_stream(stream, STREAM_SCANNER_BUFFER)
}

/// Body of a non-2xx response; a read failure keeps whatever arrived (Go ignores the read error
/// so the status error is still reported).
pub(crate) async fn error_body(resp: reqwest::Response) -> Bytes {
    resp.bytes().await.unwrap_or_default()
}

/// Joins a header-carrying `Response` metadata map with the usage object the conductor reads.
pub(crate) fn usage_metadata(detail: &crate::helps::usage::Detail) -> HashMap<String, Value> {
    HashMap::from([("usage".to_string(), UsageReporter::usage_metadata(detail))])
}

// ---------------------------------------------------------------- streaming pump

/// Per-stream state: feeds upstream frames through the response translator and the apply_patch
/// bridge to the client channel, tracking usage for the conductor. `feed` and friends return
/// `false` when the stream must stop (client gone or a fatal translation failure).
pub(crate) struct StreamPump {
    pub tx: ChunkSender,
    pub reporter: UsageReporter,
    pub upstream: Format,
    pub response: Format,
    pub model: String,
    pub original: Bytes,
    pub body: Vec<u8>,
    pub param: Param,
    pub ctx: Ctx,
    pub claude: ClaudeInputTokenState,
    pub usage: StreamUsageBuffer,
    usage_tx: Option<oneshot::Sender<Value>>,
    failed: bool,
}

/// Inputs that fix a stream's translation context.
pub(crate) struct PumpSetup<'a> {
    pub reporter: UsageReporter,
    pub from: Format,
    pub upstream: Format,
    pub response: Format,
    pub req: &'a Request,
    pub opts: &'a Options,
    /// Translated upstream request body.
    pub body: Vec<u8>,
    pub ctx: Ctx,
}

impl StreamPump {
    /// Builds the pump plus the receiving half handed to the conductor (`StreamResult`).
    pub fn new(setup: PumpSetup<'_>) -> (Self, mpsc::Receiver<Result<Bytes, ExecError>>, oneshot::Receiver<Value>) {
        let (tx, rx) = mpsc::channel(1);
        let (usage_tx, usage_rx) = oneshot::channel();
        let original = crate::helps::apply_patch::apply_patch_original_request(setup.req, setup.opts);
        let claude_request = if setup.opts.original_request.is_empty() {
            setup.req.payload.clone()
        } else {
            setup.opts.original_request.clone()
        };
        let mut pump = StreamPump {
            tx,
            reporter: setup.reporter,
            upstream: setup.upstream,
            response: setup.response,
            model: setup.req.model.clone(),
            original,
            body: setup.body,
            param: Param::default(),
            ctx: setup.ctx,
            claude: ClaudeInputTokenState::new(setup.from, setup.upstream, setup.response, &claude_request),
            usage: StreamUsageBuffer::default(),
            usage_tx: Some(usage_tx),
            failed: false,
        };
        crate::helps::apply_patch::initialize_apply_patch_stream(
            pump.upstream,
            pump.response,
            &pump.model,
            &pump.original,
            &pump.body,
            &mut pump.param,
        );
        (pump, rx, usage_rx)
    }

    /// Translates one upstream payload and sends every resulting frame; false stops the stream.
    pub async fn feed(&mut self, payload: &[u8]) -> bool {
        self.feed_with(payload, |line| line.to_vec()).await
    }

    /// [`feed`](Self::feed) with a rewrite applied to every frame before it is sent.
    pub async fn feed_with(&mut self, payload: &[u8], rewrite: impl Fn(&[u8]) -> Vec<u8>) -> bool {
        let lines = translate_stream_with_claude_input_tokens(
            &self.ctx,
            self.upstream,
            self.response,
            &self.model,
            &self.original,
            &self.body,
            payload,
            &mut self.param,
            Some(&mut self.claude),
        );
        record_apply_patch_stream_failure(&self.param, &self.reporter, &gateway_error());
        for line in lines {
            if self.tx.send(Ok(Bytes::from(rewrite(&line)))).await.is_err() {
                return false;
            }
        }
        !self.stop_if_failed().await
    }

    /// Delivers the sanitized gateway error when the translator recorded a tool input failure;
    /// true when the stream failed. (The `helps` async variants hold `&Param` across an await,
    /// which `Param` (not `Sync`) forbids inside spawned tasks.)
    fn stop_if_failed(&self) -> impl Future<Output = bool> + Send + use<> {
        let err = gateway_error();
        let failed = record_apply_patch_stream_failure(&self.param, &self.reporter, &err);
        let tx = self.tx.clone();
        async move {
            if !failed {
                return false;
            }
            let _ = tx.send(Err(err)).await;
            true
        }
    }

    /// Sends a frame that bypasses translation (Interactions passthrough).
    pub fn send_raw(&self, frame: Vec<u8>) -> impl Future<Output = bool> + Send + use<> {
        let tx = self.tx.clone();
        async move { tx.send(Ok(Bytes::from(frame))).await.is_ok() }
    }

    /// Fails an apply_patch stream that ended early; true when the caller must stop.
    pub async fn end_apply_patch(&mut self) -> bool {
        end_apply_patch_stream(&mut self.param, &self.reporter, &self.tx, gateway_error()).await
    }

    /// Publishes a stream failure and delivers it to the client.
    pub async fn fail(&mut self, err: ExecError) {
        self.failed = true;
        self.reporter.publish_failure(&err);
        let _ = self.tx.send(Err(err)).await;
    }

    /// Completes a stream: hands the executor-measured usage to the conductor, then drops the
    /// pump, which publishes the usage record and closes the client channel.
    pub fn finish(mut self) {
        if let (false, Some(detail), Some(tx)) = (self.failed, self.usage.detail(), self.usage_tx.take()) {
            let _ = tx.send(UsageReporter::usage_metadata(&detail));
        }
    }
}

impl Drop for StreamPump {
    /// Publishes the observed usage (a failure published earlier wins) and guarantees a record,
    /// whichever way the stream ended (Go: the deferred `streamUsage.Publish` and
    /// `EnsurePublished`).
    fn drop(&mut self) {
        self.reporter.publish_buffer(&self.usage);
        self.reporter.ensure_published();
    }
}

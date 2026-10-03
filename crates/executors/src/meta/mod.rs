//! Meta (Muse, api.meta.ai) executor: port of meta_executor*.go and helps/meta_tools.go.
//!
//! Upstream speaks the Responses dialect (`codex` format) at `{base_url}/responses` and is always
//! called with `stream: true`; non-stream requests collect the SSE and convert the terminal
//! `response.completed` event. Credentials are two-stage: a device-flow DCA token is exchanged for
//! a long-lived API key on demand (`prepare_request_auth` / `refresh`).

mod codex;
mod creds;
mod errors;
mod tools;

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_auth::meta::{MetaAuth, MintedKeyResponse, apply_mint_to_auth, extract_dca_token};
use cpa_auth::singleflight::SingleFlight;
use cpa_config::Config;
use cpa_core::thinking::{ThinkingError, parse_suffix};
use cpa_json::J;
use cpa_runtime::apilog::ApiLogHandle;
use cpa_runtime::executor::{DynExecutor, ExecError, Executor, Metadata, Options, Request, Response, StreamResult};
use cpa_translator::{Ctx, Format, Param};
use http::header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use tokio::sync::{mpsc, oneshot};

use crate::helps::http_request;
use crate::ConfigRx;
use crate::helps::apply_patch::{
    gateway_error, patch_failure, apply_patch_translation_error, record_apply_patch_stream_failure,
};
use crate::helps::apply_patch_responses::{
    ApplyPatchResponsesState, normalize_apply_patch_responses_request_with_original,
};
use crate::helps::codex_tool_integers::normalize_codex_tool_integer_types;
use crate::helps::oauth_scope::config_for_api_key;
use crate::helps::openai_responses_signature::sanitize_openai_responses_reasoning_encrypted_content;
use crate::helps::payload::{
    PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model, set_bool_if_different,
    set_string_if_different,
};
use crate::helps::proxy::{effective_proxy_url, new_proxy_aware_http_client};
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::session::ensure_session_id;
use crate::helps::sse::{LineReader, STREAM_SCANNER_BUFFER};
use crate::helps::status::{status_err, transport_error};
use crate::helps::thinking::{api_key_model_is_compat, apply_request_thinking};
use crate::helps::translate::{RequestTranslation, translate_request};
use crate::helps::token_count::tokenizer_for_model;
use crate::helps::usage::{StreamUsageBuffer, UsageReporter, parse_codex_usage};
use crate::openai_compat::log::record_request;

use crate::helps::claude_input_tokens::ClaudeInputTokenState;
use codex::{
    OutputItems, count_codex_input_tokens, normalize_codex_instructions,
};
use creds::{enrich_auth, meta_creds};
use errors::{meta_as_completed_event, meta_stream_event_error, wrap_meta_upstream_error};
use tools::sanitize_meta_web_search_tools;

const PROVIDER: &str = "meta";
const EXECUTOR_TYPE: &str = "MetaExecutor";
const CHANNEL_CAPACITY: usize = 16;
/// Upstream User-Agent (the Muse CLI build Meta expects).
pub(crate) const USER_AGENT: &str =
    "muse-build/1.3.0 (interactive; macos-aarch64; build ac7280f2aca67769d1455a8847bb502b617d50f6)";

/// One mint per DCA token at a time.
static MINT_FLIGHT: LazyLock<SingleFlight<MintedKeyResponse>> = LazyLock::new(SingleFlight::default);

pub struct MetaExecutor {
    cfg: ConfigRx,
    /// Config view without OAuth-only settings (Go: ForAPIKey).
    api_key_scope: bool,
    /// Mint endpoint override (tests); otherwise `META_MINT_URL` or the default.
    mint_url: Option<String>,
}

/// Registers the Meta executor.
pub fn new(cfg: ConfigRx) -> DynExecutor {
    Arc::new(MetaExecutor { cfg, api_key_scope: false, mint_url: None })
}

fn thinking_error(err: ThinkingError) -> ExecError {
    ExecError::new(err.status_code(), err.message)
}

/// Request after translation and Meta-specific shaping.
struct Prepared {
    apply_patch: ApplyPatchResponsesState,
    base_model: String,
    from: Format,
    response_format: Format,
    original_payload: Vec<u8>,
    body: Vec<u8>,
}

const TO: Format = Format::Codex;

impl MetaExecutor {
    fn config(&self) -> Arc<Config> {
        let cfg = self.cfg.borrow().clone();
        if self.api_key_scope { config_for_api_key(&cfg) } else { cfg }
    }

    /// Translated and shaped Responses request (Go: prepareResponsesRequest).
    fn prepare_responses_request(
        &self,
        cfg: &Config,
        req: &Request,
        opts: &Options,
        stream: bool,
    ) -> Result<Prepared, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let original_source: &[u8] = if opts.original_request.is_empty() { &req.payload } else { &opts.original_request };
        let original_payload = original_source.to_vec();
        let is_compat = api_key_model_is_compat(req);
        // Go passes no target executor here, so Codex clients still get the integer tool fix.
        let translation = RequestTranslation::new(&opts.headers, Some(cfg), from, TO, &base_model, stream).compat(is_compat);
        let original_translated = translate_request(&translation, &original_payload).0;
        let mut body = translate_request(&translation, &req.payload).0;

        body = apply_request_thinking(&body, req, opts, from.as_str(), TO.as_str(), PROVIDER, false).map_err(thinking_error)?;

        let requested_model = payload_requested_model(opts, &req.model);
        let request_path = payload_request_path(opts);
        body = apply_payload_config(
            &PayloadRequest {
                cfg: Some(cfg),
                target_executor: "",
                model: &base_model,
                protocol: PROVIDER,
                from_protocol: from.as_str(),
                root: "",
                requested_model: &requested_model,
                request_path: &request_path,
                headers: Some(&opts.headers),
            },
            &body,
            &original_translated,
        );
        let mut root = cpa_json::parse(&body);
        set_string_if_different(&mut root, "model", &base_model);
        set_bool_if_different(&mut root, "stream", stream);
        for key in ["generate", "prompt_cache_retention", "safety_identifier", "stream_options", "client_metadata"] {
            cpa_json::delete(&mut root, key);
        }
        body = cpa_json::to_vec(&root);
        let apply_patch = ApplyPatchResponsesState::new(from, &original_payload, &original_translated);
        body = normalize_apply_patch_responses_request_with_original(&body, Some(&original_payload))
            .map_err(|e| ExecError::new(0, e))?;
        body = normalize_codex_instructions(&body);
        body = sanitize_openai_responses_reasoning_encrypted_content("meta executor", &body);
        body = sanitize_meta_web_search_tools(&body);
        body = normalize_codex_tool_integer_types(&body, Some(&opts.headers));

        Ok(Prepared { apply_patch, base_model, from, response_format, original_payload, body })
    }

    /// Resolves a usable API key, minting one from a DCA token when needed, and returns the
    /// enriched credential for request building (Go: ensureAuth).
    async fn ensure_auth(&self, auth: &Auth) -> Result<Auth, ExecError> {
        let mut auth = auth.clone();
        let (_, mut token) = meta_creds(Some(&auth));
        if token.is_empty() && !extract_dca_token(&auth).is_empty() {
            auth = self.refresh_auth(&auth).await?;
            token = meta_creds(Some(&auth)).1;
        }
        if token.is_empty() {
            if auth.is_config_api_key() {
                return Err(status_err(
                    401,
                    "meta executor: meta-api-key requires a valid API key (DCA tokens require OAuth storage)",
                ));
            }
            return Err(status_err(401, "meta executor: missing API key or access token"));
        }
        Ok(enrich_auth(&auth))
    }

    /// Mints an API key from the DCA token, or returns the auth unchanged when it already has a
    /// key (Go: Refresh).
    async fn refresh_auth(&self, auth: &Auth) -> Result<Auth, ExecError> {
        let dca_token = extract_dca_token(auth);
        if dca_token.is_empty() {
            if !meta_creds(Some(auth)).1.is_empty() {
                return Ok(auth.clone());
            }
            return Err(status_err(401, "meta executor: missing API key or DCA token"));
        }

        let cfg = self.config();
        let proxy = effective_proxy_url("", Some(auth), Some(&cfg));
        let mint_url = self.mint_url.clone();
        let token = dca_token.clone();
        let minted = MINT_FLIGHT
            .run(&dca_token, move || async move {
                let mut svc = MetaAuth::new(&proxy)?;
                if let Some(url) = mint_url {
                    svc.set_mint_url(&url);
                }
                svc.mint_api_key(&token).await
            })
            .await
            .map_err(|e| ExecError::new(0, format!("meta executor: mint API key failed: {e}")))?;
        if minted.api_key.is_empty() {
            return Err(ExecError::new(0, "meta executor: mint API key returned empty key"));
        }
        let mut refreshed = auth.clone();
        apply_mint_to_auth(&mut refreshed, &dca_token, &minted);
        Ok(refreshed)
    }

    fn headers(auth: &Auth, token: &str, stream: bool, opts: &Options, payload: &[u8]) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if !token.trim().is_empty()
            && let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}"))
        {
            h.insert(AUTHORIZATION, v);
        }
        h.insert(http::header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        h.insert(HeaderName::from_static("x-client-id"), HeaderValue::from_static("tbh:tui"));
        if stream {
            h.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
            h.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        } else {
            h.insert(ACCEPT, HeaderValue::from_static("application/json"));
        }
        let attrs = auth.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let session = ensure_session_id(None, "", opts, payload);
        cpa_core::util::apply_custom_headers_from_attrs(&mut h, &attrs, Some(&opts.headers), session.as_deref());
        h
    }

    /// POSTs the prepared body to `{base_url}/responses`.
    async fn send(
        &self,
        cfg: &Config,
        enriched: &Auth,
        opts: &Options,
        payload: &[u8],
        body: Vec<u8>,
    ) -> Result<reqwest::Response, ExecError> {
        let (base_url, token) = meta_creds(Some(enriched));
        if base_url.trim().is_empty() {
            return Err(status_err(401, "meta executor: missing provider baseURL"));
        }
        let url = format!("{}/responses", base_url.trim_end_matches('/'));
        let headers = Self::headers(enriched, &token, true, opts, payload);
        tracing::debug!(target: "cpa::upstream", provider = PROVIDER, url = %url, "meta upstream request");
        record_request(&opts.api_log, cfg, PROVIDER, Some(enriched), "POST", &url, &headers, &body);
        let resp = new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(enriched), None)
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

    /// Converts the collected upstream SSE (or a plain response object) of a non-stream call into
    /// the client payload and the source event usage is read from (Go: translateMetaCompleted).
    fn translate_completed(
        req_model: &str,
        prepared: &mut Prepared,
        data: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), ExecError> {
        let mut items = OutputItems::default();
        for line in data.split(|b| *b == b'\n') {
            let Some(rest) = line.strip_prefix(b"data:") else {
                continue;
            };
            let event_data = crate::helps::text::trim_space(rest);
            if let Some(err) = meta_stream_event_error(event_data) {
                return Err(err);
            }
            let (events, err) = prepared.apply_patch.transform(event_data);
            if err.is_some() {
                return Err(gateway_error());
            }
            for event in events {
                match cpa_json::parse(&event).g("type").str().as_str() {
                    "response.output_item.done" => items.collect(&event),
                    "response.completed" | "response.incomplete" => {
                        let completed = items.patch_completed(&event);
                        return Self::translate_terminal(req_model, prepared, completed);
                    }
                    _ => {}
                }
            }
        }

        if let Some(completed) = meta_as_completed_event(data) {
            let completed = items.patch_completed(&completed);
            let completed = prepared.apply_patch.bridge.transform_non_stream(&completed).map_err(|_| gateway_error())?;
            return Self::translate_terminal(req_model, prepared, completed);
        }

        if prepared.apply_patch.finish().is_err() {
            return Err(gateway_error());
        }
        Err(status_err(408, "meta stream error: stream disconnected before response.completed or response.incomplete"))
    }

    fn translate_terminal(
        req_model: &str,
        prepared: &Prepared,
        completed: Vec<u8>,
    ) -> Result<(Vec<u8>, Vec<u8>), ExecError> {
        let mut param = Param::default();
        let out = cpa_translator::translate_non_stream(
            &Ctx::default(),
            TO,
            prepared.response_format,
            req_model,
            &prepared.original_payload,
            &prepared.body,
            &completed,
            &mut param,
        );
        match out {
            Some(out) if !out.is_empty() && apply_patch_translation_error(&param).is_none() => Ok((out, completed)),
            _ => Err(gateway_error()),
        }
    }

    async fn execute_inner(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(status_err(501, "/responses/compact not supported"));
        }
        let cfg = self.config();
        let enriched = self.ensure_auth(auth).await?;
        let mut prepared = self.prepare_responses_request(&cfg, &req, &opts, true)?;
        let reporter = UsageReporter::new(PROVIDER, EXECUTOR_TYPE, &prepared.base_model, Some(&enriched), Some(&opts));
        reporter.set_translated_reasoning_effort(&prepared.body, TO.as_str());

        let result = self.execute_prepared(&cfg, &enriched, &req, &opts, &mut prepared, &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn execute_prepared(
        &self,
        cfg: &Config,
        enriched: &Auth,
        req: &Request,
        opts: &Options,
        prepared: &mut Prepared,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        let resp = self.send(cfg, enriched, opts, &req.payload, prepared.body.clone()).await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let data = resp.bytes().await.map_err(|e| {
            let err = transport_error(&e);
            opts.api_log.record_api_response_error(cfg, &err.message);
            err
        })?;
        opts.api_log.append_api_response_chunk(cfg, &data);
        reporter.observe_response_model(&data);
        if !status.is_success() {
            tracing::debug!(
                target: "cpa::upstream",
                status = status.as_u16(),
                "meta request error: {}",
                crate::helps::logging::summarize_error_body("", &data)
            );
            return Err(wrap_meta_upstream_error(status.as_u16(), &data));
        }

        let (payload, source_event) = Self::translate_completed(&req.model, prepared, &data)?;
        if !source_event.is_empty() {
            reporter.observe_response_model(&source_event);
        }
        let mut metadata = Metadata::new();
        match parse_codex_usage(&source_event) {
            Some(detail) => {
                metadata.insert("usage".to_string(), UsageReporter::usage_metadata(&detail));
                reporter.publish(detail);
            }
            None => reporter.ensure_published(),
        }
        let payload = if prepared.response_format == Format::OpenAIResponse {
            ensure_responses_usage_details(&payload)
        } else {
            payload
        };
        Ok(Response { payload: Bytes::from(payload), metadata, headers })
    }

    async fn execute_stream_inner(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(status_err(501, "/responses/compact not supported"));
        }
        let cfg = self.config();
        let enriched = self.ensure_auth(auth).await?;
        let prepared = self.prepare_responses_request(&cfg, &req, &opts, true)?;
        let reporter = UsageReporter::new(PROVIDER, EXECUTOR_TYPE, &prepared.base_model, Some(&enriched), Some(&opts));
        reporter.set_translated_reasoning_effort(&prepared.body, TO.as_str());

        let result = self.start_stream(&cfg, &enriched, req, &opts, prepared, &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn start_stream(
        &self,
        cfg: &Arc<Config>,
        enriched: &Auth,
        req: Request,
        opts: &Options,
        prepared: Prepared,
        reporter: &UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        let resp = self.send(cfg, enriched, opts, &req.payload, prepared.body.clone()).await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        if !status.is_success() {
            let data = resp.bytes().await.map_err(|e| {
                let err = transport_error(&e);
                opts.api_log.record_api_response_error(cfg, &err.message);
                err
            })?;
            opts.api_log.append_api_response_chunk(cfg, &data);
            tracing::debug!(
                target: "cpa::upstream",
                status = status.as_u16(),
                "meta request error: {}",
                crate::helps::logging::summarize_error_body("", &data)
            );
            return Err(wrap_meta_upstream_error(status.as_u16(), &data));
        }

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (usage_tx, usage_rx) = oneshot::channel();
        let reporter = reporter.clone();
        let model = req.model;
        let log = StreamLog { api_log: opts.api_log.clone(), cfg: cfg.clone() };
        tokio::spawn(run_stream(resp, prepared, model, reporter, log, tx, usage_tx));
        let mut result = StreamResult::new(headers, rx);
        result.usage = Some(usage_rx);
        Ok(result)
    }
}

/// Request-log handle and config snapshot the stream task records with.
struct StreamLog {
    api_log: ApiLogHandle,
    cfg: Arc<Config>,
}

/// Everything the stream task needs to translate and deliver lines.
struct StreamCtx {
    prepared: Prepared,
    model: String,
    reporter: UsageReporter,
    tx: mpsc::Sender<Result<Bytes, ExecError>>,
    param: Param,
    claude_tokens: ClaudeInputTokenState,
}

impl StreamCtx {
    /// Runs `line` through the patch bridge and the translator and delivers the chunks. False
    /// when the stream must stop (failure sent or client gone).
    async fn emit(&mut self, line: &[u8]) -> bool {
        let (lines, bridge_err) = self.prepared.apply_patch.stream(line);
        if bridge_err.is_some() {
            self.reporter.publish_failure(&gateway_error());
        }
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        for l in &lines {
            chunks.extend(self.claude_tokens.translate_stream(
            TO,
                self.prepared.response_format,
                &self.model,
                &self.prepared.original_payload,
                &self.prepared.body,
                l,
                &mut self.param,
        ));
        }
        record_apply_patch_stream_failure(&self.param, &self.reporter, &gateway_error());
        for chunk in chunks {
            if self.tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                return false;
            }
        }
        if let Some(err) = patch_failure(&self.param, &self.reporter) {
            let _ = self.tx.send(Err(err)).await;
            return false;
        }
        if bridge_err.is_some() {
            let err = gateway_error();
            self.reporter.publish_failure(&err);
            let _ = self.tx.send(Err(err)).await;
            return false;
        }
        true
    }
}

fn finish_usage(reporter: &UsageReporter, usage: &StreamUsageBuffer, usage_tx: oneshot::Sender<serde_json::Value>) {
    if !reporter.publish_buffer(usage) {
        reporter.ensure_published();
    }
    if let Some(detail) = usage.detail_ref() {
        let _ = usage_tx.send(UsageReporter::usage_metadata(detail));
    }
}

/// Reads the upstream SSE, collecting output items so a completed event with an empty output is
/// patched, and delivers translated chunks (Go: ExecuteStream goroutine).
async fn run_stream(
    resp: reqwest::Response,
    prepared: Prepared,
    model: String,
    reporter: UsageReporter,
    log: StreamLog,
    tx: mpsc::Sender<Result<Bytes, ExecError>>,
    usage_tx: oneshot::Sender<serde_json::Value>,
) {
    let mut lines = LineReader::from_response(resp, STREAM_SCANNER_BUFFER);
    let claude_tokens = ClaudeInputTokenState::new(prepared.from, TO, prepared.response_format, &prepared.original_payload);
    let mut sc = StreamCtx { prepared, model, reporter, tx, param: Param::default(), claude_tokens };
    let mut usage = StreamUsageBuffer::default();
    let mut items = OutputItems::default();
    let mut scan_err = None;

    while let Some(line) = lines.next_line_or_closed(&sc.tx).await {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                scan_err = Some(e);
                break;
            }
        };
        log.api_log.append_api_response_chunk(&log.cfg, &line);
        let Some(rest) = line.strip_prefix(b"data:") else {
            if !sc.emit(&line).await {
                finish_usage(&sc.reporter, &usage, usage_tx);
                return;
            }
            continue;
        };
        let mut event_data = crate::helps::text::trim_space(rest).to_vec();
        sc.reporter.observe_response_model(&event_data);
        if let Some(err) = meta_stream_event_error(&event_data) {
            log.api_log.record_api_response_error(&log.cfg, &err.message);
            sc.reporter.publish_failure(&err);
            let _ = sc.tx.send(Err(err)).await;
            finish_usage(&sc.reporter, &usage, usage_tx);
            return;
        }
        match cpa_json::parse(&event_data).g("type").str().as_str() {
            "response.output_item.done" => items.collect(&event_data),
            "response.completed" | "response.incomplete" => {
                if let Some(detail) = parse_codex_usage(&event_data) {
                    usage.observe(detail, true);
                }
                event_data = items.patch_completed(&event_data);
            }
            _ => {}
        }
        let mut framed = b"data: ".to_vec();
        framed.extend_from_slice(&event_data);
        if !sc.emit(&framed).await {
            finish_usage(&sc.reporter, &usage, usage_tx);
            return;
        }
    }

    let (finish_events, finish_err) = sc.prepared.apply_patch.finish_stream();
    if finish_err.is_some() {
        sc.reporter.publish_failure(&gateway_error());
    }
    for event in &finish_events {
        let chunks = sc.claude_tokens.translate_stream(
            TO,
            sc.prepared.response_format,
            &sc.model,
            &sc.prepared.original_payload,
            &sc.prepared.body,
            event,
            &mut sc.param,
        );
        for chunk in chunks {
            if sc.tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                finish_usage(&sc.reporter, &usage, usage_tx);
                return;
            }
        }
    }
    if finish_err.is_some() {
        let err = gateway_error();
        sc.reporter.publish_failure(&err);
        let _ = sc.tx.send(Err(err)).await;
        finish_usage(&sc.reporter, &usage, usage_tx);
        return;
    }
    if let Some(e) = scan_err {
        let err = ExecError::from(e);
        log.api_log.record_api_response_error(&log.cfg, &err.message);
        sc.reporter.publish_failure(&err);
        let _ = sc.tx.send(Err(err)).await;
    }
    finish_usage(&sc.reporter, &usage, usage_tx);
}

#[async_trait]
impl Executor for MetaExecutor {
    fn identifier(&self) -> &str {
        PROVIDER
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.execute_inner(auth, req, opts).await
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        self.execute_stream_inner(auth, req, opts).await
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        self.refresh_auth(auth).await
    }

    /// Local O200k estimate of the translated request's input tokens.
    async fn count_tokens(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let cfg = self.config();
        self.ensure_auth(auth).await?;
        let prepared = self.prepare_responses_request(&cfg, &req, &opts, false)?;
        let enc = tokenizer_for_model("gpt-5").map_err(|e| ExecError::new(0, format!("meta executor: tokenizer init failed: {e}")))?;
        let count = count_codex_input_tokens(&enc, &prepared.body);
        let usage_json = format!(
            r#"{{"response":{{"usage":{{"input_tokens":{count},"output_tokens":0,"total_tokens":{count}}}}}}}"#
        );
        let translated =
            cpa_translator::translate_token_count(&Ctx::default(), TO, prepared.response_format, count, usage_json.as_bytes());
        Ok(Response { payload: Bytes::from(translated), ..Default::default() })
    }

    fn for_api_key(&self) -> Option<DynExecutor> {
        Some(Arc::new(MetaExecutor { cfg: self.cfg.clone(), api_key_scope: true, mint_url: self.mint_url.clone() }))
    }

    /// A DCA token without a usable API key must be exchanged before the request is built.
    fn should_prepare_request_auth(&self, auth: &Auth) -> bool {
        if auth.is_config_api_key() {
            return false;
        }
        meta_creds(Some(auth)).1.is_empty() && !extract_dca_token(auth).is_empty()
    }

    async fn prepare_request_auth(&self, auth: &Auth) -> Result<Option<Auth>, ExecError> {
        if !self.should_prepare_request_auth(auth) {
            return Ok(None);
        }
        self.refresh_auth(auth).await.map(Some)
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }

    /// Go: MetaExecutor.PrepareRequest.
    async fn prepare_request(&self, req: &mut reqwest::Request, auth: &Auth) -> Result<(), ExecError> {
        let (_, token) = meta_creds(Some(auth));
        http_request::set_bearer_or_clear(req, &token);
        http_request::set_header(req, "User-Agent", USER_AGENT);
        http_request::set_header(req, "X-Client-Id", "tbh:tui");
        http_request::apply_attr_headers(req, auth);
        Ok(())
    }

    /// Go: MetaExecutor.HttpRequest (mints the API key from a DCA token first).
    async fn http_request(&self, auth: &Auth, mut req: reqwest::Request) -> Result<reqwest::Response, ExecError> {
        let enriched = self.ensure_auth(auth).await?;
        self.prepare_request(&mut req, &enriched).await?;
        let client = crate::helps::proxy::new_proxy_aware_http_client("", Some(&self.config()), Some(&enriched), None);
        http_request::execute(&client, req).await
    }
}

#[cfg(test)]
mod tests;

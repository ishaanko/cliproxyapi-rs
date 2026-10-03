//! Devin executor: the Codeium/Windsurf Connect-RPC chat backend (Go: devin_executor.go and
//! helps/devin_*).
//!
//! The executor consumes the internal `interactions` format: any other client format is
//! translated to it first, the chat request is encoded as protobuf in a Connect envelope, and the
//! server-streaming response frames are turned back into interactions events and translated to
//! the client format.
//!
//! Upstream note: Devin injects roughly 390-580 hidden system prompt tokens server side, so the
//! reported prompt usage is higher than the request content alone.
//!
//! - [`wire`]: protobuf encoding/decoding, Connect framing, trailer errors.
//! - [`models`]: chat model uid resolution.
//! - [`request`]: interactions payload to prompts/tools/session ids.
//! - [`stream`]: response frames to interactions events (stream and non-stream).

// ExecError is a large shared error type; every executor returns it by value.
#![allow(clippy::result_large_err)]

mod log;
pub mod models;
mod pb;
pub mod request;
pub mod stream;
pub mod wire;

#[cfg(test)]
mod test_support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use cpa_auth::Auth;
use cpa_auth::devin::DevinAuthService;
use cpa_auth::util::format_rfc3339_utc;
use cpa_core::registry::lookup_model_info;
use cpa_core::thinking::parse_suffix;
use cpa_runtime::conductor::session::canonical_session_id;
use cpa_runtime::executor::{
    DynExecutor, ExecError, Executor, Options, Request, Response, StreamResult,
};
use cpa_translator::Format;
use futures_util::{StreamExt, TryStreamExt};
use http::{HeaderMap, HeaderName, HeaderValue};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::helps::cloak_obfuscate::SensitiveWordMatcher;
use crate::helps::logging::UpstreamRequestLog;
use self::stream::{StreamParams, consume_frames_to_interactions, stream_frames};
use self::wire::{
    CHAT_PATH, ChatRequest, ConnectFrameReader, DEFAULT_BASE_URL, build_get_chat_message_request,
    generate_sentry_trace, wrap_connect_envelope,
};
use crate::ConfigRx;
use crate::helps::apply_patch::{
    APPLY_PATCH_UPSTREAM_ERROR_MESSAGE, apply_patch_original_request, apply_patch_requested,
    apply_patch_translation_error,
};
use crate::helps::proxy::new_devin_http_client;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::session::ensure_session_id;
use crate::helps::status::{status_err, transport_error, transport_message};
use crate::helps::usage::{UsageReporter, parse_interactions_usage};

/// Provider key and usage executor type.
const PROVIDER: &str = "devin";
const EXECUTOR_TYPE: &str = "DevinExecutor";
/// Error bodies are read up to this size.
const MAX_ERROR_BODY: usize = 1 << 20;
/// Chunk channel depth between the frame task and the conductor.
const STREAM_CHANNEL_DEPTH: usize = 16;

/// Registers the Devin executor with the live config handle.
pub fn new(cfg: ConfigRx) -> DynExecutor {
    Arc::new(DevinExecutor::new(cfg))
}

/// Executor for the Codeium/Devin Connect-RPC backend.
pub struct DevinExecutor {
    cfg: ConfigRx,
    /// Matcher compiled for the last seen word list, keyed by the joined words.
    matcher: Mutex<Option<(String, Option<Arc<SensitiveWordMatcher>>)>>,
    /// Tests send through their own client so they do not populate the shared client cache.
    #[cfg(test)]
    test_client: Option<reqwest::Client>,
}

/// Credential fields the executor reads from an auth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub api_key: String,
    pub base_url: String,
    pub device_seed: String,
}

/// Session token, upstream base URL and fingerprint seed of `auth`: attributes first, then
/// metadata (metadata `base_url` only applies while the default is in use).
pub fn credentials(auth: Option<&Auth>) -> Credentials {
    let mut c = Credentials {
        api_key: String::new(),
        base_url: DEFAULT_BASE_URL.to_string(),
        device_seed: String::new(),
    };
    let Some(auth) = auth else { return c };
    for key in ["api_key", "session_token", "token"] {
        let v = auth.attr(key);
        if !v.is_empty() && c.api_key.is_empty() {
            c.api_key = v;
        }
    }
    let v = auth.attr("base_url");
    if !v.is_empty() {
        c.base_url = v;
    }
    let v = auth.attr("device_seed");
    if !v.is_empty() {
        c.device_seed = v;
    }
    for key in ["api_key", "session_token"] {
        let v = auth.meta_str(key);
        if !v.is_empty() && c.api_key.is_empty() {
            c.api_key = v;
        }
    }
    let v = auth.meta_str("base_url");
    if !v.is_empty() && c.base_url == DEFAULT_BASE_URL {
        c.base_url = v;
    }
    let v = auth.meta_str("device_seed");
    if !v.is_empty() && c.device_seed.is_empty() {
        c.device_seed = v;
    }
    c
}

/// Headers of an upstream call (Go: PrepareRequest). `path` selects the unary variant, which has
/// no Sentry-Trace. `User-Agent` is set to the empty string: the native client sends none, and
/// the transport drops an empty value (see [`DevinExecutor::send`]). Custom `header:<Name>`
/// attributes are applied last.
pub fn prepare_headers(
    auth: Option<&Auth>,
    path: &str,
    client_headers: Option<&HeaderMap>,
    session_id: Option<&str>,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let api_key = credentials(auth).api_key;
    if !api_key.is_empty()
        && let Ok(v) = HeaderValue::from_str(&format!("Basic {api_key}-{api_key}"))
    {
        headers.insert(http::header::AUTHORIZATION, v);
    }
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/connect+proto"),
    );
    headers.insert(
        HeaderName::from_static("connect-protocol-version"),
        HeaderValue::from_static("1"),
    );
    headers.insert(http::header::ACCEPT, HeaderValue::from_static("*/*"));
    // Sentry-Trace is only attached to chat streaming, never to unary status/catalog calls.
    let is_unary = [
        "GetUserStatus",
        "GetCliModelConfigs",
        "SeatManagementService",
    ]
    .iter()
    .any(|m| path.contains(m));
    if !is_unary
        && !headers.contains_key("sentry-trace")
        && let Ok(v) = HeaderValue::from_str(&generate_sentry_trace())
    {
        headers.insert(HeaderName::from_static("sentry-trace"), v);
    }
    headers.insert(http::header::USER_AGENT, HeaderValue::from_static(""));

    if let Some(auth) = auth {
        let attrs: HashMap<String, String> = auth
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        cpa_core::util::apply_custom_headers_from_attrs(
            &mut headers,
            &attrs,
            client_headers,
            session_id,
        );
    }
    headers
}

/// Non-2xx upstream response as an error; a 429 carries its `Retry-After` hint (delta seconds, or
/// an HTTP date still in the future).
pub fn new_status_error(status: u16, headers: &HeaderMap, body: &[u8]) -> ExecError {
    let mut err = status_err(status, String::from_utf8_lossy(body).into_owned());
    if status == 429
        && let Some(raw) = headers
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
    {
        let raw = raw.trim();
        if !raw.is_empty() {
            if let Ok(seconds) = raw.parse::<i64>()
                && seconds >= 0
            {
                err.retry_after = Some(Duration::from_secs(seconds as u64));
            } else if let Ok(deadline) = httpdate::parse_http_date(raw)
                && let Ok(delay) = deadline.duration_since(SystemTime::now())
                && !delay.is_zero()
            {
                err.retry_after = Some(delay);
            }
        }
    }
    err
}

/// Everything needed to send one chat request.
struct Prepared {
    url: String,
    headers: HeaderMap,
    body: Vec<u8>,
    chat_model_uid: String,
    /// Readable rendition of the request for the request log (Go: `logBody`).
    log_body: Vec<u8>,
}

impl DevinExecutor {
    pub fn new(cfg: ConfigRx) -> Self {
        Self {
            cfg,
            matcher: Mutex::new(None),
            #[cfg(test)]
            test_client: None,
        }
    }

    /// Cached HTTP client for the proxy settings of this call (no response decompression).
    fn http_client(
        &self,
        opts_proxy: &str,
        auth: &Auth,
        timeout: Option<Duration>,
    ) -> reqwest::Client {
        #[cfg(test)]
        if let Some(client) = &self.test_client {
            return client.clone();
        }
        let cfg = self.cfg.borrow().clone();
        new_devin_http_client(opts_proxy, Some(&cfg), Some(auth), timeout)
    }

    /// Matcher for the configured sensitive words, rebuilt only when the list changes.
    fn sensitive_word_matcher(&self, words: &[String]) -> Option<Arc<SensitiveWordMatcher>> {
        if words.is_empty() {
            return None;
        }
        let key = words.join("\0");
        let mut cache = self.matcher.lock();
        if let Some((cached_key, matcher)) = cache.as_ref()
            && *cached_key == key
        {
            return matcher.clone();
        }
        let matcher = SensitiveWordMatcher::new(words).map(Arc::new);
        *cache = Some((key, matcher.clone()));
        matcher
    }

    /// Builds the framed chat request: payload translation to interactions, prompt extraction,
    /// model uid resolution, protobuf encoding and headers.
    fn prepare_request(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
    ) -> Result<Prepared, ExecError> {
        let creds = credentials(Some(auth));
        if creds.api_key.is_empty() {
            let mut err = ExecError::new(
                0,
                "devin credentials missing: api_key or session_token required",
            );
            err.upstream_attempted = false;
            return Err(err);
        }
        let cfg = self.cfg.borrow().clone();

        let session = ensure_session_id(None, "", opts, &req.payload);
        let mut payload = req.payload.to_vec();
        if opts.source_format != Format::Interactions {
            payload = cpa_translator::translate_request(
                opts.source_format,
                Format::Interactions,
                &req.model,
                &payload,
                opts.stream,
            );
        }
        let mut parsed = request::parse_interactions_payload(&payload, &opts.original_request);
        let (session_id, cascade_id) = request::resolve_session_and_cascade_ids(
            &parsed.session_id,
            &parsed.cascade_id,
            session.as_deref().unwrap_or(""),
            || canonical_session_id(&opts.headers, &opts.original_request, &opts.metadata),
        );

        let base_model = parse_suffix(&req.model).model_name;
        if let Some(info) = lookup_model_info(&base_model, Some("devin"))
            && info.max_completion_tokens > 0
            && (parsed.max_tokens > info.max_completion_tokens || parsed.max_tokens <= 0)
        {
            parsed.max_tokens = info.max_completion_tokens;
        }
        let chat_model_uid = models::resolve_chat_model_uid(
            &req.model,
            &parsed.thinking_level,
            parsed.budget_tokens,
        );

        let matcher = self.sensitive_word_matcher(&cfg.devin.sensitive_words);
        let proto = build_get_chat_message_request(&ChatRequest {
            session_token: &creds.api_key,
            device_seed: &creds.device_seed,
            chat_model_uid: &chat_model_uid,
            system_prompt: &parsed.system_prompt,
            prompts: &parsed.prompts,
            tools: &parsed.tools,
            temperature: parsed.temperature,
            max_tokens: parsed.max_tokens,
            session_id: &session_id,
            cascade_id: &cascade_id,
            matcher: matcher.as_deref(),
        });

        let sanitized_system_prompt = if parsed.system_prompt.is_empty() {
            String::new()
        } else {
            wire::sanitize_system_prompt(&parsed.system_prompt, matcher.as_deref())
        };
        let log_body = log::request_body(
            &payload,
            opts.source_format == Format::Interactions,
            &chat_model_uid,
            &sanitized_system_prompt,
            &parsed.prompts,
            &parsed.tools,
            parsed.temperature,
            parsed.max_tokens,
            &session_id,
            &cascade_id,
        );

        let url = format!("{}{CHAT_PATH}", creds.base_url.trim_end_matches('/'));
        let headers = prepare_headers(
            Some(auth),
            CHAT_PATH,
            Some(&opts.headers),
            session.as_deref(),
        );
        Ok(Prepared {
            url,
            headers,
            body: wrap_connect_envelope(&proto),
            chat_model_uid,
            log_body,
        })
    }

    /// Sends the prepared chat request. An empty `User-Agent` is removed first: net/http (and the
    /// native client) send no User-Agent at all rather than an empty one.
    async fn send(
        &self,
        auth: &Auth,
        opts: &Options,
        prepared: Prepared,
    ) -> Result<reqwest::Response, ExecError> {
        let cfg = self.cfg.borrow().clone();
        let client = self.http_client(&opts.proxy_url, auth, None);
        // The log keeps the headers as prepared, including the empty User-Agent.
        let (auth_type, auth_value) = auth_log_fields(auth);
        opts.api_log.record_api_request(
            &cfg,
            UpstreamRequestLog {
                url: prepared.url.clone(),
                method: "POST".to_string(),
                headers: prepared.headers.clone(),
                body: prepared.log_body.clone(),
                provider: PROVIDER.to_string(),
                auth_id: auth.id.clone(),
                auth_label: auth.label.clone(),
                auth_type,
                auth_value,
            },
        );
        let mut headers = prepared.headers;
        if headers
            .get(http::header::USER_AGENT)
            .is_some_and(|v| v.is_empty())
        {
            headers.remove(http::header::USER_AGENT);
        }
        let resp = match client.post(&prepared.url).headers(headers).body(prepared.body).send().await {
            Ok(resp) => resp,
            Err(e) => {
                let err = transport_error(&e);
                opts.api_log.record_api_response_error(&cfg, &err.message);
                return Err(err);
            }
        };
        opts.api_log.record_api_response_metadata(&cfg, resp.status().as_u16(), resp.headers());
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = read_limited(resp, MAX_ERROR_BODY).await;
        opts.api_log.append_api_response_chunk(&cfg, &body);
        Err(new_status_error(status, &headers, &body))
    }
}

/// `devinAuthLogFields`: the request log's auth type and masked session token.
fn auth_log_fields(auth: &Auth) -> (String, String) {
    let api_key = credentials(Some(auth)).api_key;
    // Byte-wise like Go's slicing, which may cut inside a multi-byte character.
    let bytes = api_key.as_bytes();
    let value = match bytes.len() {
        0 => String::new(),
        n if n > 8 => format!("{}...{}", String::from_utf8_lossy(&bytes[..4]), String::from_utf8_lossy(&bytes[n - 4..])),
        _ => "***".to_string(),
    };
    ("devin".to_string(), value)
}

/// Reads at most `limit` bytes of a response body (errors end the read early).
async fn read_limited(resp: reqwest::Response, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        let room = limit - out.len();
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= limit {
            break;
        }
    }
    out
}

#[async_trait]
impl Executor for DevinExecutor {
    fn identifier(&self) -> &str {
        PROVIDER
    }

    async fn execute(
        &self,
        auth: &Auth,
        req: Request,
        opts: Options,
    ) -> Result<Response, ExecError> {
        let target_model = parse_suffix(&req.model).model_name;
        let reporter = UsageReporter::new(
            PROVIDER,
            EXECUTOR_TYPE,
            &target_model,
            Some(auth),
            Some(&opts),
        );
        let result = self.execute_inner(auth, req, opts, &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn execute_stream(
        &self,
        auth: &Auth,
        req: Request,
        opts: Options,
    ) -> Result<StreamResult, ExecError> {
        let target_model = parse_suffix(&req.model).model_name;
        let reporter = UsageReporter::new(
            PROVIDER,
            EXECUTOR_TYPE,
            &target_model,
            Some(auth),
            Some(&opts),
        );
        let result = self.execute_stream_inner(auth, req, opts, &reporter).await;
        reporter.track_failure(&result);
        result
    }

    /// Re-reads the user status (plan, quota signals); the session token is never rotated.
    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        let creds = credentials(Some(auth));
        if creds.api_key.is_empty() {
            return Ok(auth.clone());
        }
        let client = self.http_client("", auth, Some(Duration::from_secs(30)));
        let mut service = DevinAuthService::with_client(client);
        service.set_server_base_url(&creds.base_url);
        let status = match service
            .fetch_user_status(&creds.api_key, &creds.device_seed)
            .await
        {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    "devin executor: failed to refresh user status for {}: {err}",
                    auth.id
                );
                return Err(ExecError::new(0, err.to_string()));
            }
        };

        let mut updated = auth.clone();
        for (key, value) in [
            ("email", &status.email),
            ("user_name", &status.user_name),
            ("user_id", &status.user_id),
            ("team_id", &status.team_id),
            ("plan", &status.plan),
            ("org_id", &status.org_id),
            ("org_name", &status.org_name),
        ] {
            if !value.is_empty() {
                updated
                    .metadata
                    .insert(key.to_string(), value.clone().into());
                updated.attributes.insert(key.to_string(), value.clone());
            }
        }

        // Quota signals for the management UI and the conductor.
        let signals = &mut updated.quota.signals;
        if !status.plan.is_empty() {
            signals.insert("plan".into(), status.plan.clone());
        }
        signals.insert(
            "daily_quota_remaining_percent".into(),
            format!("{}%", status.daily_quota_remaining_percent),
        );
        signals.insert(
            "weekly_quota_remaining_percent".into(),
            format!("{}%", status.weekly_quota_remaining_percent),
        );
        for (key, t) in [
            ("daily_quota_reset_at", status.daily_quota_reset_at),
            ("weekly_quota_reset_at", status.weekly_quota_reset_at),
            ("plan_start", status.plan_start),
            ("plan_end", status.plan_end),
        ] {
            if let Some(t) = t {
                signals.insert(key.into(), format_rfc3339_utc(t));
            }
        }
        let now = Utc::now();
        updated.quota.observed_at = Some(now);
        updated.last_refreshed_at = Some(now);
        Ok(updated)
    }

    /// No upstream count endpoint exists; estimates `len(payload) / 4`.
    async fn count_tokens(
        &self,
        _auth: &Auth,
        req: Request,
        _opts: Options,
    ) -> Result<Response, ExecError> {
        let prompt_tokens = req.payload.len() / 4;
        Ok(Response {
            payload: Bytes::from(format!(
                r#"{{"total_tokens":{prompt_tokens},"input_tokens":{prompt_tokens}}}"#
            )),
            ..Default::default()
        })
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }

    /// Go: DevinExecutor.PrepareRequest. The Connect-RPC headers replace the request's own;
    /// an existing `Sentry-Trace` is kept.
    async fn prepare_request(&self, req: &mut reqwest::Request, auth: &Auth) -> Result<(), ExecError> {
        let prepared = prepare_headers(Some(auth), req.url().path(), None, None);
        for (name, value) in &prepared {
            if name.as_str() == "sentry-trace" && req.headers().contains_key(name) {
                continue;
            }
            req.headers_mut().insert(name.clone(), value.clone());
        }
        Ok(())
    }

    /// Go: DevinExecutor.HttpRequest.
    async fn http_request(&self, auth: &Auth, mut req: reqwest::Request) -> Result<reqwest::Response, ExecError> {
        Executor::prepare_request(self, &mut req, auth).await?;
        let client = self.http_client("", auth, None);
        crate::helps::http_request::execute(&client, req).await
    }
}

impl DevinExecutor {
    async fn execute_inner(
        &self,
        auth: &Auth,
        req: Request,
        opts: Options,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        let prepared = self.prepare_request(auth, &req, &opts)?;
        reporter.set_upstream_model(&prepared.chat_model_uid);
        let resp = self.send(auth, &opts, prepared).await?;
        let headers = resp.headers().clone();

        let original = apply_patch_original_request(&req, &opts);
        let reader = ConnectFrameReader::new(resp.bytes_stream().map_err(|e| transport_message(&e)).boxed());
        let cfg = self.cfg.borrow().clone();
        let (outcome, response_log) = consume_frames_to_interactions(reader, &req.model, &original).await;
        let interactions_raw = outcome.as_ref().map(|c| cpa_json::to_vec(&c.interactions)).unwrap_or_default();
        if response_log.is_some() || !interactions_raw.is_empty() {
            let body = log::response_body(response_log.as_ref(), &interactions_raw);
            opts.api_log.append_api_response_chunk(&cfg, &body);
        }
        let consumed = match outcome {
            Ok(c) => c,
            // A declared apply_patch tool hides every upstream decoding failure behind the
            // sanitized gateway error.
            Err(_) if apply_patch_requested(&original) => {
                return Err(status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE));
            }
            Err(err) => {
                opts.api_log.record_api_response_error(&cfg, &err.message);
                return Err(err);
            }
        };
        if let Some(model) = consumed
            .usage
            .as_ref()
            .map(|u| u.model_name.as_str())
            .filter(|m| !m.is_empty())
        {
            reporter.set_response_model(model);
        }

        let interactions = cpa_json::to_vec(&consumed.interactions);
        let target_format = opts.response_format_or_source();
        let mut param = cpa_translator::Param::default();
        let out = cpa_translator::translate_non_stream(
            &cpa_translator::Ctx::default(),
            Format::Interactions,
            target_format,
            &req.model,
            &original,
            &req.payload,
            &interactions,
            &mut param,
        );
        let out = match out {
            Some(out) if apply_patch_translation_error(&param).is_none() && !out.is_empty() => out,
            _ => return Err(status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)),
        };
        let detail = parse_interactions_usage(&interactions);
        let usage = UsageReporter::usage_metadata(&detail);
        reporter.publish(detail);
        let out = if target_format == Format::OpenAIResponse {
            ensure_responses_usage_details(&out)
        } else {
            out
        };

        let mut response = Response {
            payload: Bytes::from(out),
            headers,
            ..Default::default()
        };
        response.metadata.insert("usage".into(), usage);
        Ok(response)
    }

    async fn execute_stream_inner(
        &self,
        auth: &Auth,
        req: Request,
        opts: Options,
        reporter: &UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        let prepared = self.prepare_request(auth, &req, &opts)?;
        reporter.set_upstream_model(&prepared.chat_model_uid);
        let chat_model_uid = prepared.chat_model_uid.clone();
        let resp = self.send(auth, &opts, prepared).await?;
        let headers = resp.headers().clone();

        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_DEPTH);
        let (usage_tx, usage_rx) = oneshot::channel();
        let params = StreamParams {
            model: req.model.clone(),
            request: req.payload.clone(),
            original: apply_patch_original_request(&req, &opts),
            client_original: opts.original_request.clone(),
            source_format: opts.source_format,
            response_format: opts.response_format_or_source(),
            chat_model_uid,
            reporter: reporter.clone(),
            log: crate::helps::gemini_log::UpstreamLog::new(&opts, &self.cfg.borrow().clone()),
        };
        let reader = ConnectFrameReader::new(resp.bytes_stream().map_err(|e| transport_message(&e)).boxed());
        tokio::spawn(async move {
            stream_frames(reader, params, tx, usage_tx).await;
        });

        let mut result = StreamResult::new(headers, rx);
        result.usage = Some(usage_rx);
        Ok(result)
    }
}

#[cfg(test)]
mod tests;

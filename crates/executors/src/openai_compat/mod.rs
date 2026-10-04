//! OpenAI-compatible upstreams: port of openai_compat_executor.go.
//!
//! One [`OpenAiCompatExecutor`] is bound to one provider key (`openai-compatible-<name>` for
//! config entries, `openai-compatibility` for the bare provider). The conductor registers the
//! bare one at startup via [`new`]; per-entry keys are created on demand by [`factory`], which
//! the service installs as its `ExecutorFactory`.

mod compat_config;
pub(crate) mod images;
pub(crate) mod log;
pub(crate) mod translate;
mod stream;

/// Metadata key naming a handler-level source type (`openai-image`, `openai-video`).
pub use translate::META_HANDLER_TYPE;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::thinking::parse_suffix;
use cpa_json::J;
use cpa_runtime::conductor::usage::META_USAGE;
use cpa_runtime::executor::{
    DynExecutor, ExecError, Executor, Metadata, Options, Request, Response, StreamResult,
};
use cpa_runtime::service::ExecutorFactory;
use cpa_translator::{Ctx, Format, Param};
use http::header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, USER_AGENT};
use http::{HeaderMap, HeaderValue};

use crate::helps::http_request;
use crate::ConfigRx;
use crate::helps::home_refresh::refresh_auth_via_home;
use crate::helps::apply_patch::{
    APPLY_PATCH_UPSTREAM_ERROR_MESSAGE, apply_patch_original_request, apply_patch_translation_error,
};
use crate::helps::oauth_scope::config_for_api_key;
use crate::helps::openai_compat::{
    normalize_openai_max_tokens, normalize_openai_tool_results_text_only, should_normalize_openai_tool_results_for_model,
    should_use_max_completion_tokens_for_model,
};
use crate::helps::openai_responses_signature::sanitize_openai_responses_reasoning_encrypted_content;
use crate::helps::payload::{
    PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model, set_bool_if_different,
    set_string_if_different,
};
use crate::helps::proxy::new_proxy_aware_http_client;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::session::{ensure_session_id, provider_session_uuid};
use crate::helps::status::{openai_compat_status_error, status_err};
use crate::helps::status::transport_error;
use crate::helps::thinking::{api_key_model_is_compat, apply_request_thinking};
use crate::helps::translate::{RequestTranslation, translate_request, translate_request_pair};
use crate::helps::token_count::{build_openai_usage_json, count_openai_chat_tokens, tokenizer_for_model};
use crate::helps::usage::{UsageReporter, parse_openai_usage};

use compat_config::resolve_compat_config;

const EXECUTOR_TYPE: &str = "OpenAICompatExecutor";
const DEFAULT_PROVIDER: &str = "openai-compatibility";
const USER_AGENT_VALUE: &str = "cli-proxy-openai-compat";
const IMAGE_HANDLER_TYPE: &str = "openai-image";
const IMAGES_GENERATIONS_PATH: &str = "/images/generations";
const IMAGES_EDITS_PATH: &str = "/images/edits";

/// Executor for one OpenAI-compatible provider key.
pub struct OpenAiCompatExecutor {
    provider: String,
    cfg: ConfigRx,
    /// Reads go through `Config::for_api_key` (Go: `ForAPIKey`).
    api_key_view: bool,
}

/// Executor for the bare `openai-compatibility` provider.
pub fn new(cfg: ConfigRx) -> DynExecutor {
    Arc::new(OpenAiCompatExecutor::with_provider(DEFAULT_PROVIDER, cfg))
}

/// Executor factory for per-config-entry provider keys (`openai-compatible-<name>`) and any
/// other unhandled key, matching Go's default branch of `registerExecutorForAuth`. Install it
/// with `ServiceBuilder::executor_factory(openai_compat::factory(cfg_rx))`.
pub fn factory(cfg: ConfigRx) -> ExecutorFactory {
    let slot = ConfigSlot::default();
    slot.set(cfg);
    slot.factory()
}

/// A factory whose config handle is supplied after the service is built, for callers that must
/// pass the factory to `ServiceBuilder` before `subscribe_config()` exists. Call
/// [`ConfigSlot::set`] before `Service::start` so the first auth synthesis can bind executors.
pub fn lazy_factory() -> (ExecutorFactory, ConfigSlot) {
    let slot = ConfigSlot::default();
    (slot.factory(), slot)
}

/// Late-bound config handle of [`lazy_factory`].
#[derive(Clone, Default)]
pub struct ConfigSlot(Arc<OnceLock<ConfigRx>>);

impl ConfigSlot {
    /// Binds the live config; later calls are ignored.
    pub fn set(&self, cfg: ConfigRx) {
        let _ = self.0.set(cfg);
    }

    fn factory(&self) -> ExecutorFactory {
        let slot = self.0.clone();
        Arc::new(move |key: &str| {
            let cfg = slot.get()?.clone();
            let key = key.trim().to_lowercase();
            let key = if key.is_empty() { DEFAULT_PROVIDER.to_string() } else { key };
            Some(Arc::new(OpenAiCompatExecutor::with_provider(&key, cfg)) as DynExecutor)
        })
    }
}

impl OpenAiCompatExecutor {
    /// Executor bound to `provider` (Go: NewOpenAICompatExecutor).
    pub fn with_provider(provider: &str, cfg: ConfigRx) -> Self {
        Self { provider: provider.to_string(), cfg, api_key_view: false }
    }

    fn config(&self) -> Arc<Config> {
        let cfg = self.cfg.borrow().clone();
        if self.api_key_view { config_for_api_key(&cfg) } else { cfg }
    }

    /// `(base_url, api_key)` from the auth attributes.
    fn resolve_credentials(auth: &Auth) -> (String, String) {
        (auth.attr("base_url"), auth.attr("api_key"))
    }

    /// Go: applyPromptCacheKey. `prompt_cache_key` from the request, the Claude Code cache id,
    /// or a UUIDv5 over provider, model, source format and session.
    #[allow(clippy::too_many_arguments)]
    fn apply_prompt_cache_key(
        &self,
        cfg: &Config,
        auth: &Auth,
        from: Format,
        base_model: &str,
        req: &Request,
        opts: &Options,
        translated: Vec<u8>,
    ) -> Vec<u8> {
        let Some(compat) = resolve_compat_config(cfg, auth, req) else { return translated };
        if !compat.support_prompt_cache_key {
            return translated;
        }
        let set = |translated: &[u8], key: &str| -> Vec<u8> {
            let mut v = cpa_json::parse(translated);
            set_string_if_different(&mut v, "prompt_cache_key", key);
            cpa_json::to_vec(&v)
        };
        for payload in [req.payload.as_ref(), opts.original_request.as_ref(), translated.as_slice()] {
            let key = cpa_json::parse(payload).g("prompt_cache_key").str();
            let key = key.trim();
            if !key.is_empty() {
                return set(&translated, key);
            }
        }
        let mut model_name = cpa_json::parse(&translated).g("model").str().trim().to_string();
        if model_name.is_empty() {
            model_name = base_model.to_string();
        }
        if from == Format::Claude
            && let Some(id) = crate::helps::session::claude_code_prompt_cache_id(&model_name, &req.payload, &opts.headers)
        {
            return set(&translated, &id);
        }
        let session_id = provider_session_uuid(&self.provider, &[&opts.metadata, &req.metadata]);
        if session_id.is_empty() {
            return translated;
        }
        let mut provider = self.provider.trim().to_string();
        if provider.is_empty() {
            provider = compat.name.trim().to_string();
        }
        let identity = [
            "cli-proxy-api:openai-compat:prompt-cache",
            &provider.to_lowercase(),
            &model_name.to_lowercase(),
            &from.as_str().trim().to_lowercase(),
            &session_id,
        ]
        .join("\x00");
        set(&translated, &crate::helps::session::uuid_sha1_oid(identity.as_bytes()))
    }

    /// Request translation shared by `execute` and `execute_stream` (everything before the
    /// HTTP call): translate, thinking, payload rules, tool-result and max-token
    /// normalization, prompt cache key.
    #[allow(clippy::too_many_arguments)]
    async fn prepare_chat(
        &self,
        cfg: &Config,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        base_model: &str,
        stream: bool,
        allow_compact: bool,
    ) -> Result<Prepared, ExecError> {
        let from = opts.source_format;
        let mut to = Format::OpenAI;
        let mut endpoint = "/chat/completions";
        let compact = allow_compact && opts.alt == "responses/compact";
        if compact {
            to = Format::OpenAIResponse;
            endpoint = "/responses/compact";
        }
        let original_source: &[u8] = if opts.original_request.is_empty() { &req.payload } else { &opts.original_request };
        let is_compat = api_key_model_is_compat(req);
        let translation = RequestTranslation::new(&opts.headers, Some(cfg), from, to, base_model, stream).compat(is_compat);
        let (original_translated, translated, updates_changed) = translate_request_pair(&translation, original_source, &req.payload);
        let mut translated =
            apply_request_thinking(&translated, req, opts, from.as_str(), to.as_str(), &self.provider, updates_changed)
                .map_err(thinking_error)?;

        let requested_model = payload_requested_model(opts, &req.model);
        let request_path = payload_request_path(opts);
        translated = apply_payload_config(
            &PayloadRequest {
                cfg: Some(cfg),
                target_executor: "",
                model: base_model,
                protocol: to.as_str(),
                from_protocol: from.as_str(),
                root: "",
                requested_model: &requested_model,
                request_path: &request_path,
                headers: Some(&opts.headers),
            },
            &translated,
            &original_translated,
        );
        let compat = resolve_compat_config(cfg, auth, req);
        if should_normalize_openai_tool_results_for_model(compat.as_deref(), base_model, &requested_model) {
            translated = normalize_openai_tool_results_text_only(&translated);
        }
        if !compact {
            let use_mct = should_use_max_completion_tokens_for_model(compat.as_deref(), base_model, &requested_model);
            translated = normalize_openai_max_tokens(&translated, use_mct);
            translated = self.apply_prompt_cache_key(cfg, auth, from, base_model, req, opts, translated);
        } else {
            let mut v = cpa_json::parse(&translated);
            cpa_json::delete(&mut v, "stream");
            translated = cpa_json::to_vec(&v);
            translated = sanitize_openai_responses_reasoning_encrypted_content("openai compat executor", &translated);
        }
        Ok(Prepared { translated, to, endpoint })
    }

    /// Headers common to every upstream call: content type, bearer key, user agent.
    fn base_headers(content_type: &str, api_key: &str) -> Result<HeaderMap, ExecError> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, header_value(content_type)?);
        if !api_key.is_empty() {
            h.insert(AUTHORIZATION, header_value(&format!("Bearer {api_key}"))?);
        }
        h.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        Ok(h)
    }

    fn apply_custom_headers(headers: &mut HeaderMap, auth: &Auth, opts: &Options, session: Option<&str>) {
        let attrs: HashMap<String, String> = auth.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        cpa_core::util::apply_custom_headers_from_attrs(headers, &attrs, Some(&opts.headers), session);
    }

    fn http_client(&self, cfg: &Config, auth: &Auth, opts: &Options) -> reqwest::Client {
        new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(auth), None)
    }

    async fn execute_inner(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        let cfg = self.config();
        let session = ensure_session_id(None, "", opts, &req.payload);
        let base_model = parse_suffix(&req.model).model_name;
        let (base_url, api_key) = Self::resolve_credentials(auth);
        if base_url.is_empty() {
            return Err(status_err(401, "missing provider baseURL"));
        }
        let response_format = opts.response_format_or_source();
        let prepared = self.prepare_chat(&cfg, auth, req, opts, &base_model, opts.stream, true).await?;
        let Prepared { translated, to, endpoint } = prepared;
        reporter.set_translated_reasoning_effort(&translated, to.as_str());

        let url = format!("{}{endpoint}", base_url.trim_end_matches('/'));
        let mut headers = Self::base_headers("application/json", &api_key)?;
        Self::apply_custom_headers(&mut headers, auth, opts, session.as_deref());
        tracing::debug!(target: "cpa::upstream", provider = %self.provider, "POST {url}");
        log::record_request(&opts.api_log, &cfg, &self.provider, Some(auth), "POST", &url, &headers, &translated);

        reporter.start_response_ttft();
        let resp = self
            .http_client(&cfg, auth, opts)
            .post(&url)
            .headers(headers)
            .body(translated.clone())
            .send()
            .await
            .map_err(|e| {
                let err = transport_error(&e);
                opts.api_log.record_api_response_error(&cfg, &err.message);
                err
            })?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        opts.api_log.record_api_response_metadata(&cfg, status, &resp_headers);
        if !(200..300).contains(&status) {
            let body = read_body(reporter, resp).await.unwrap_or_default();
            opts.api_log.append_api_response_chunk(&cfg, &body);
            tracing::debug!(
                "request error, error status: {status}, error message: {}",
                crate::helps::logging::summarize_error_body(
                    resp_headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default(),
                    &body
                )
            );
            return Err(openai_compat_status_error(status, &resp_headers, &body));
        }
        let body = read_body(reporter, resp).await.inspect_err(|e| opts.api_log.record_api_response_error(&cfg, &e.message))?;
        opts.api_log.append_api_response_chunk(&cfg, &body);
        reporter.observe_response_model(&body);
        let mut param = Param::default();
        let original = apply_patch_original_request(req, opts);
        let out = cpa_translator::translate_non_stream(
            &Ctx::default(),
            to,
            response_format,
            &req.model,
            &original,
            &translated,
            &body,
            &mut param,
        );
        let out = match out {
            Some(out) if !out.is_empty() && apply_patch_translation_error(&param).is_none() => out,
            _ => return Err(status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)),
        };
        let detail = parse_openai_usage(&body);
        let mut metadata = Metadata::new();
        metadata.insert(META_USAGE.to_string(), UsageReporter::usage_metadata(&detail));
        reporter.publish(detail);
        let out = if response_format == Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out };
        Ok(Response { payload: Bytes::from(out), metadata, headers: resp_headers })
    }

    async fn execute_images_inner(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        endpoint_path: &str,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        let cfg = self.config();
        let session = ensure_session_id(None, "", opts, &req.payload);
        let base_model = parse_suffix(&req.model).model_name;
        let (base_url, api_key) = Self::resolve_credentials(auth);
        if base_url.is_empty() {
            return Err(status_err(401, "missing provider baseURL"));
        }
        let content_type = header_str(&opts.headers, "content-type");
        let (payload, content_type) = images::prepare_images_payload(&req.payload, &base_model, &content_type, false)?;
        let content_type = if content_type.is_empty() { "application/json".to_string() } else { content_type };
        reporter.set_translated_reasoning_effort(&payload, "openai");

        let url = format!("{}{endpoint_path}", base_url.trim_end_matches('/'));
        let mut headers = Self::base_headers(&content_type, &api_key)?;
        Self::apply_custom_headers(&mut headers, auth, opts, session.as_deref());
        log::record_request(&opts.api_log, &cfg, &self.provider, Some(auth), "POST", &url, &headers, &payload);
        reporter.start_response_ttft();
        let resp = self
            .http_client(&cfg, auth, opts)
            .post(&url)
            .headers(headers)
            .body(payload)
            .send()
            .await
            .map_err(|e| {
                let err = transport_error(&e);
                opts.api_log.record_api_response_error(&cfg, &err.message);
                err
            })?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        opts.api_log.record_api_response_metadata(&cfg, status, &resp_headers);
        let body = read_body(reporter, resp).await.inspect_err(|e| opts.api_log.record_api_response_error(&cfg, &e.message))?;
        opts.api_log.append_api_response_chunk(&cfg, &body);
        if !(200..300).contains(&status) {
            tracing::debug!(
                "request error, error status: {status}, error message: {}",
                crate::helps::logging::summarize_error_body(
                    resp_headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default(),
                    &body
                )
            );
            return Err(openai_compat_status_error(status, &resp_headers, &body));
        }
        reporter.observe_response_model(&body);
        let detail = parse_openai_usage(&body);
        let mut metadata = Metadata::new();
        metadata.insert(META_USAGE.to_string(), UsageReporter::usage_metadata(&detail));
        reporter.publish(detail);
        reporter.ensure_published();
        Ok(Response { payload: body, metadata, headers: resp_headers })
    }

    async fn execute_stream_inner(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        reporter: &UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        let cfg = self.config();
        let session = ensure_session_id(None, "", opts, &req.payload);
        let base_model = parse_suffix(&req.model).model_name;
        let (base_url, api_key) = Self::resolve_credentials(auth);
        if base_url.is_empty() {
            return Err(status_err(401, "missing provider baseURL"));
        }
        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let prepared = self.prepare_chat(&cfg, auth, req, opts, &base_model, true, false).await?;
        let Prepared { translated, to, .. } = prepared;
        // Ask for usage in the final chunk so token statistics exist for any compatible upstream.
        let translated = {
            let mut v = cpa_json::parse(&translated);
            set_bool_if_different(&mut v, "stream_options.include_usage", true);
            cpa_json::to_vec(&v)
        };
        reporter.set_translated_reasoning_effort(&translated, to.as_str());

        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let mut headers = Self::base_headers("application/json", &api_key)?;
        Self::apply_custom_headers(&mut headers, auth, opts, session.as_deref());
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        tracing::debug!(target: "cpa::upstream", provider = %self.provider, "POST {url}");
        log::record_request(&opts.api_log, &cfg, &self.provider, Some(auth), "POST", &url, &headers, &translated);

        reporter.start_response_ttft();
        let resp = self
            .http_client(&cfg, auth, opts)
            .post(&url)
            .headers(headers)
            .body(translated.clone())
            .send()
            .await
            .map_err(|e| {
                let err = transport_error(&e);
                opts.api_log.record_api_response_error(&cfg, &err.message);
                err
            })?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        opts.api_log.record_api_response_metadata(&cfg, status, &resp_headers);
        if !(200..300).contains(&status) {
            let body = read_body(reporter, resp).await.unwrap_or_default();
            opts.api_log.append_api_response_chunk(&cfg, &body);
            tracing::debug!(
                "request error, error status: {status}, error message: {}",
                crate::helps::logging::summarize_error_body(
                    resp_headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default(),
                    &body
                )
            );
            return Err(openai_compat_status_error(status, &resp_headers, &body));
        }
        let original_payload: Bytes =
            if opts.original_request.is_empty() { req.payload.clone() } else { opts.original_request.clone() };
        Ok(stream::spawn_chat_stream(
            resp,
            resp_headers,
            stream::ChatStreamParams {
                reporter: reporter.clone(),
                api_log: opts.api_log.clone(),
                cfg: cfg.clone(),
                from,
                to,
                response_format,
                model: req.model.clone(),
                original_payload,
                patch_original: apply_patch_original_request(req, opts),
                translated: Bytes::from(translated),
            },
        ))
    }

    async fn execute_images_stream_inner(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        endpoint_path: &str,
        reporter: &UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        let cfg = self.config();
        let session = ensure_session_id(None, "", opts, &req.payload);
        let base_model = parse_suffix(&req.model).model_name;
        let (base_url, api_key) = Self::resolve_credentials(auth);
        if base_url.is_empty() {
            return Err(status_err(401, "missing provider baseURL"));
        }
        let content_type = header_str(&opts.headers, "content-type");
        let (payload, content_type) = images::prepare_images_payload(&req.payload, &base_model, &content_type, true)?;
        let content_type = if content_type.is_empty() { "application/json".to_string() } else { content_type };
        reporter.set_translated_reasoning_effort(&payload, "openai");

        let url = format!("{}{endpoint_path}", base_url.trim_end_matches('/'));
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, header_value(&content_type)?);
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        if !api_key.is_empty() {
            headers.insert(AUTHORIZATION, header_value(&format!("Bearer {api_key}"))?);
        }
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        Self::apply_custom_headers(&mut headers, auth, opts, session.as_deref());
        log::record_request(&opts.api_log, &cfg, &self.provider, Some(auth), "POST", &url, &headers, &payload);
        reporter.start_response_ttft();
        let resp = self
            .http_client(&cfg, auth, opts)
            .post(&url)
            .headers(headers)
            .body(payload)
            .send()
            .await
            .map_err(|e| {
                let err = transport_error(&e);
                opts.api_log.record_api_response_error(&cfg, &err.message);
                err
            })?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        opts.api_log.record_api_response_metadata(&cfg, status, &resp_headers);
        if !(200..300).contains(&status) {
            let body = read_body(reporter, resp).await.inspect_err(|e| opts.api_log.record_api_response_error(&cfg, &e.message))?;
            opts.api_log.append_api_response_chunk(&cfg, &body);
            tracing::debug!(
                "request error, error status: {status}, error message: {}",
                crate::helps::logging::summarize_error_body(
                    resp_headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default(),
                    &body
                )
            );
            return Err(status_err(status, String::from_utf8_lossy(&body).into_owned()));
        }
        Ok(stream::spawn_image_stream(resp, resp_headers, reporter.clone(), opts.api_log.clone(), cfg))
    }
}

struct Prepared {
    translated: Vec<u8>,
    to: Format,
    endpoint: &'static str,
}

fn thinking_error(err: cpa_core::thinking::ThinkingError) -> ExecError {
    let status = err.status_code();
    ExecError::new(status, err.to_string())
}

fn header_value(v: &str) -> Result<HeaderValue, ExecError> {
    HeaderValue::from_str(v).map_err(|e| ExecError::new(0, format!("invalid header value: {e}")))
}

fn header_str(headers: &HeaderMap, name: &str) -> String {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string()
}

/// Reads a whole upstream body, marking the first byte for TTFT.
async fn read_body(reporter: &UsageReporter, resp: reqwest::Response) -> Result<Bytes, ExecError> {
    reporter.read_body_tracked(resp, false).await.map_err(|e| transport_error(&e))
}

/// Go: openAICompatImageEndpointPath, "" when the request is not an image call.
fn image_endpoint_path(opts: &Options) -> &'static str {
    if translate::source_handler_type(opts) != IMAGE_HANDLER_TYPE {
        return "";
    }
    let path = payload_request_path(opts);
    if path.ends_with("/images/edits") {
        IMAGES_EDITS_PATH
    } else {
        // Anything else (including `/images/generations`) defaults to generations, as in Go.
        IMAGES_GENERATIONS_PATH
    }
}

fn has_refresh_token(auth: &Auth) -> bool {
    ["refresh_token", "refreshToken"].iter().any(|k| !auth.meta_str(k).is_empty())
}

#[async_trait]
impl Executor for OpenAiCompatExecutor {
    fn identifier(&self) -> &str {
        &self.provider
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let reporter = UsageReporter::new(&self.provider, EXECUTOR_TYPE, &base_model, Some(auth), Some(&opts));
        let endpoint = image_endpoint_path(&opts);
        let result = if endpoint.is_empty() {
            self.execute_inner(auth, &req, &opts, &reporter).await
        } else {
            self.execute_images_inner(auth, &req, &opts, endpoint, &reporter).await
        };
        reporter.track_failure(&result);
        result
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let reporter = UsageReporter::new(&self.provider, EXECUTOR_TYPE, &base_model, Some(auth), Some(&opts));
        let endpoint = image_endpoint_path(&opts);
        let result = if endpoint.is_empty() {
            self.execute_stream_inner(auth, &req, &opts, &reporter).await
        } else {
            self.execute_images_stream_inner(auth, &req, &opts, endpoint, &reporter).await
        };
        reporter.track_failure(&result);
        result
    }

    /// Credentials are static API keys; OAuth-style refresh tokens cannot be rotated here.
    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        let cfg = self.cfg.borrow().clone();
        if let Some(result) = refresh_auth_via_home(&cfg, auth).await {
            return result;
        }
        if has_refresh_token(auth) {
            let provider = if self.provider.is_empty() { auth.provider.trim() } else { self.provider.as_str() };
            return Err(ExecError::new(
                0,
                format!("openai compat executor cannot refresh oauth credentials for provider {provider}"),
            ));
        }
        Ok(auth.clone())
    }

    /// Local tokenizer estimate, no upstream call.
    async fn count_tokens(&self, _auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let to = Format::OpenAI;
        let cfg = self.config();
        let translation = RequestTranslation::new(&opts.headers, Some(&cfg), from, to, &base_model, false).compat(api_key_model_is_compat(&req));
        let (translated, updates_changed) = translate_request(&translation, &req.payload);
        let translated =
            apply_request_thinking(&translated, &req, &opts, from.as_str(), to.as_str(), &self.provider, updates_changed)
                .map_err(thinking_error)?;
        let enc = tokenizer_for_model(&base_model)
            .map_err(|e| ExecError::new(0, format!("openai compat executor: tokenizer init failed: {e}")))?;
        let count = count_openai_chat_tokens(&enc, &translated);
        let usage_json = build_openai_usage_json(count);
        let out = cpa_translator::translate_token_count(&Ctx::default(), to, response_format, count, &usage_json);
        Ok(Response { payload: Bytes::from(out), ..Default::default() })
    }

    fn for_api_key(&self) -> Option<DynExecutor> {
        Some(Arc::new(OpenAiCompatExecutor { provider: self.provider.clone(), cfg: self.cfg.clone(), api_key_view: true }))
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }

    /// Go: OpenAICompatExecutor.PrepareRequest (a blank key leaves `Authorization` as is).
    async fn prepare_request(&self, req: &mut reqwest::Request, auth: &Auth) -> Result<(), ExecError> {
        let (_, api_key) = Self::resolve_credentials(auth);
        if !api_key.trim().is_empty() {
            http_request::set_header(req, "Authorization", &format!("Bearer {api_key}"));
        }
        http_request::apply_attr_headers(req, auth);
        Ok(())
    }

    /// Go: OpenAICompatExecutor.HttpRequest.
    async fn http_request(&self, auth: &Auth, mut req: reqwest::Request) -> Result<reqwest::Response, ExecError> {
        self.prepare_request(&mut req, auth).await?;
        let client = crate::helps::proxy::new_proxy_aware_http_client("", Some(&self.config()), Some(auth), None);
        http_request::execute(&client, req).await
    }
}

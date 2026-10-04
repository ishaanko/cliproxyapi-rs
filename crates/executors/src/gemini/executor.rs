//! Gemini API-key executor (Go: gemini_executor.go): `generativelanguage.googleapis.com` with an
//! `x-goog-api-key` credential, plus the native Interactions provider that shares the struct.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::signature::sanitize_gemini_request_thought_signatures;
use cpa_core::thinking::parse_suffix;
use cpa_json::J;
use cpa_runtime::executor::{ExecError, Executor, Options, Request, Response, StreamResult};
use cpa_translator::{Ctx, Format, Param};
use http::HeaderMap;

use super::common::{
    GL_API_VERSION, GL_ENDPOINT, PumpSetup, StreamPump, apply_custom_headers,
    cap_gemini_max_output_tokens, compact_unsupported, error_body, fix_gemini_image_aspect_ratio,
    is_count_tokens_action, json_headers, observed_lines, original_payload, post_json, read_body, set_header,
    set_model, thinking_error, translate_request_pair, upstream_error, usage_metadata,
};
use crate::helps::http_request;
use crate::helps::home_refresh::refresh_auth_via_home;
use crate::helps::gemini_content_turns::{ensure_leading_user_content_value, ensure_trailing_user_content_value};
use super::interactions;
use crate::ConfigRx;
use crate::helps::apply_patch::{apply_patch_original_request, apply_patch_translation_error, gateway_error};
use crate::helps::gemini_log::UpstreamLog;
use crate::helps::payload::{PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model};
use crate::helps::proxy::new_proxy_aware_http_client;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::session::ensure_session_id;
use crate::helps::text::json_payload;
use crate::helps::thinking::{api_key_model_is_compat, apply_request_thinking};
use crate::helps::usage::{UsageReporter, filter_sse_usage_metadata_cow, parse_gemini_stream_usage, parse_gemini_usage};

const PROVIDER: &str = "gemini";
const INTERACTIONS_PROVIDER: &str = "gemini-interactions";

/// Stateless executor for the official Gemini API (and, as `gemini-interactions`, the native
/// Interactions API) using API keys.
pub struct GeminiExecutor {
    pub(super) cfg: ConfigRx,
    pub(super) identifier: &'static str,
}

impl GeminiExecutor {
    pub fn new(cfg: ConfigRx) -> Self {
        Self { cfg, identifier: PROVIDER }
    }

    /// The executor bound to the native Interactions provider.
    pub fn new_interactions(cfg: ConfigRx) -> Self {
        Self { cfg, identifier: INTERACTIONS_PROVIDER }
    }

    pub(super) fn config(&self) -> Arc<Config> {
        self.cfg.borrow().clone()
    }

    pub(super) fn reporter(&self, model: &str, auth: &Auth, opts: &Options) -> UsageReporter {
        UsageReporter::new(self.identifier, "GeminiExecutor", model, Some(auth), Some(opts))
    }
}

/// `attributes.api_key`, "" when absent.
pub(super) fn gemini_api_key(auth: &Auth) -> String {
    auth.attributes.get("api_key").cloned().unwrap_or_default()
}

/// `attributes.base_url` (trailing `/` trimmed) or the public endpoint.
pub(super) fn resolve_base_url(auth: &Auth) -> String {
    let custom = auth.attributes.get("base_url").map(|v| v.trim()).unwrap_or_default();
    if custom.is_empty() {
        return GL_ENDPOINT.to_string();
    }
    let base = custom.trim_end_matches('/');
    if base.is_empty() { GL_ENDPOINT.to_string() } else { base.to_string() }
}

/// Request headers: JSON content type, the API key when present, then custom headers.
pub(super) fn request_headers(auth: &Auth, opts: &Options, session_id: Option<&str>) -> Result<HeaderMap, ExecError> {
    let mut headers = json_headers();
    let api_key = gemini_api_key(auth);
    if !api_key.is_empty() {
        set_header(&mut headers, "x-goog-api-key", &api_key)?;
    }
    apply_custom_headers(&mut headers, auth, opts, session_id);
    Ok(headers)
}

/// Whether the request runs on the native Interactions API: an Interactions-capable client
/// protocol on a `gemini-interactions` credential.
pub(super) fn should_execute_native_interactions(auth: &Auth, opts: &Options) -> bool {
    native_interactions_source_format(opts.source_format)
        && auth.provider.trim().eq_ignore_ascii_case(INTERACTIONS_PROVIDER)
}

pub(super) fn native_interactions_source_format(format: Format) -> bool {
    matches!(format, Format::Interactions | Format::OpenAI | Format::OpenAIResponse | Format::Claude | Format::Gemini)
}

/// Everything that goes into the upstream generate/stream body.
struct GenerateBody {
    body: Vec<u8>,
    from: Format,
}

impl GeminiExecutor {
    /// Translation, thinking, payload rules, signature sanitizing and turn-shape fixes for a
    /// `generateContent` style request (`count` keeps a trailing model turn).
    fn build_body(
        &self,
        cfg: &Config,
        req: &Request,
        opts: &Options,
        base_model: &str,
        stream: bool,
        trailing_user: bool,
    ) -> Result<GenerateBody, ExecError> {
        let from = opts.source_format;
        let to = Format::Gemini;
        let is_compat = api_key_model_is_compat(req);
        let original_source = original_payload(req, opts);
        let (original_translated, body) = translate_request_pair(
            cfg,
            &opts.headers,
            from,
            to,
            base_model,
            original_source,
            &req.payload,
            stream,
            is_compat,
        );

        let body = apply_request_thinking(&body, req, opts, from.as_str(), to.as_str(), self.identifier, false)
            .map_err(thinking_error)?;
        let body = fix_gemini_image_aspect_ratio(base_model, body);
        let requested_model = payload_requested_model(opts, &req.model);
        let request_path = payload_request_path(opts);
        let payload_req = PayloadRequest {
            cfg: Some(cfg),
            target_executor: "",
            model: base_model,
            protocol: to.as_str(),
            from_protocol: from.as_str(),
            root: "",
            requested_model: &requested_model,
            request_path: &request_path,
            headers: Some(&opts.headers),
        };
        let body = apply_payload_config(&payload_req, &body, &original_translated);

        let mut v = cpa_json::parse(&body);
        set_model(&mut v, base_model);
        cap_gemini_max_output_tokens(&mut v, base_model);
        let body = sanitize_gemini_request_thought_signatures(&cpa_json::to_vec(&v), "contents");

        let mut v = cpa_json::parse(&body);
        ensure_leading_user_content_value(&mut v, "contents");
        if trailing_user {
            ensure_trailing_user_content_value(&mut v, "contents");
        }
        cpa_json::delete(&mut v, "session_id");
        Ok(GenerateBody { body: cpa_json::to_vec(&v), from })
    }
}

#[async_trait]
impl Executor for GeminiExecutor {
    fn identifier(&self) -> &str {
        self.identifier
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let cfg = self.config();
        let session_id = ensure_session_id(None, "", &opts, &req.payload);
        if opts.alt == "responses/compact" {
            return Err(compact_unsupported());
        }
        if should_execute_native_interactions(auth, &opts) {
            return interactions::execute(self, &cfg, auth, req, opts, session_id).await;
        }
        let base_model = parse_suffix(&req.model).model_name;
        let reporter = self.reporter(&base_model, auth, &opts);
        let result = self.execute_inner(&cfg, auth, &req, &opts, &base_model, session_id.as_deref(), &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let cfg = self.config();
        let session_id = ensure_session_id(None, "", &opts, &req.payload);
        if opts.alt == "responses/compact" {
            return Err(compact_unsupported());
        }
        if should_execute_native_interactions(auth, &opts) {
            return interactions::execute_stream(self, &cfg, auth, req, opts, session_id).await;
        }
        let base_model = parse_suffix(&req.model).model_name;
        let reporter = self.reporter(&base_model, auth, &opts);
        let result = self.stream_inner(&cfg, auth, req, opts, &base_model, session_id.as_deref(), &reporter).await;
        reporter.track_failure(&result);
        result
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        if let Some(result) = refresh_auth_via_home(&self.config(), auth).await {
            return result;
        }
        Ok(auth.clone())
    }

    async fn count_tokens(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let cfg = self.config();
        let session_id = ensure_session_id(None, "", &opts, &req.payload);
        let base_model = parse_suffix(&req.model).model_name;
        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let to = Format::Gemini;

        let (_, translated) = translate_request_pair(
            &cfg,
            &opts.headers,
            from,
            to,
            &base_model,
            &req.payload,
            &req.payload,
            false,
            api_key_model_is_compat(&req),
        );
        let translated =
            apply_request_thinking(&translated, &req, &opts, from.as_str(), to.as_str(), self.identifier, false)
                .map_err(thinking_error)?;
        let translated = fix_gemini_image_aspect_ratio(&base_model, translated);
        let mut v = cpa_json::parse(&translated);
        for key in ["tools", "generationConfig", "safetySettings"] {
            cpa_json::delete(&mut v, key);
        }
        set_model(&mut v, &base_model);
        let translated = sanitize_gemini_request_thought_signatures(&cpa_json::to_vec(&v), "contents");
        let mut v = cpa_json::parse(&translated);
        ensure_leading_user_content_value(&mut v, "contents");
        let translated = cpa_json::to_vec(&v);

        let url = format!("{}/{GL_API_VERSION}/models/{base_model}:countTokens", resolve_base_url(auth));
        let headers = request_headers(auth, &opts, session_id.as_deref())?;
        let log = UpstreamLog::new(&opts, &cfg);
        log.request(auth, self.identifier, &url, &headers, &translated);
        let client = new_proxy_aware_http_client(&opts.proxy_url, Some(&cfg), Some(auth), None);
        let resp = log.tap_err(post_json(&client, &url, headers, translated).await)?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        log.metadata(status, &resp_headers);
        let data = log.tap_err(read_body(resp).await)?;
        log.chunk(&data);
        if !(200..300).contains(&status) {
            return Err(upstream_error(status, &data));
        }
        let count = cpa_json::parse(&data).g("totalTokens").int();
        let ctx = Ctx { alt: Some(opts.alt.clone()) };
        let payload = cpa_translator::translate_token_count(&ctx, to, response_format, count, &data);
        Ok(Response { payload: Bytes::from(payload), headers: resp_headers, ..Default::default() })
    }

    fn supports_apply_patch(&self, _model: &str) -> bool {
        true
    }

    /// Go: GeminiExecutor.PrepareRequest.
    async fn prepare_request(&self, req: &mut reqwest::Request, auth: &Auth) -> Result<(), ExecError> {
        let api_key = gemini_api_key(auth);
        if !api_key.is_empty() {
            http_request::set_header(req, "x-goog-api-key", &api_key);
        } else {
            http_request::del_header(req, "x-goog-api-key");
        }
        http_request::del_header(req, "Authorization");
        http_request::apply_attr_headers(req, auth);
        Ok(())
    }

    /// Go: GeminiExecutor.HttpRequest.
    async fn http_request(&self, auth: &Auth, mut req: reqwest::Request) -> Result<reqwest::Response, ExecError> {
        self.prepare_request(&mut req, auth).await?;
        let client = new_proxy_aware_http_client("", Some(&self.config()), Some(auth), None);
        http_request::execute(&client, req).await
    }
}

impl GeminiExecutor {
    #[allow(clippy::too_many_arguments)]
    async fn execute_inner(
        &self,
        cfg: &Arc<Config>,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        base_model: &str,
        session_id: Option<&str>,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        let response_format = opts.response_format_or_source();
        let count_tokens = is_count_tokens_action(req);
        let built = self.build_body(cfg, req, opts, base_model, false, !count_tokens)?;
        let action = if count_tokens { "countTokens" } else { "generateContent" };
        let mut url = format!("{}/{GL_API_VERSION}/models/{base_model}:{action}", resolve_base_url(auth));
        if !opts.alt.is_empty() && !count_tokens {
            url.push_str(&format!("?$alt={}", opts.alt));
        }
        reporter.set_translated_reasoning_effort(&built.body, Format::Gemini.as_str());

        let headers = request_headers(auth, opts, session_id)?;
        let log = UpstreamLog::new(opts, cfg);
        log.request(auth, self.identifier, &url, &headers, &built.body);
        let client = new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(auth), None);
        reporter.start_response_ttft();
        let resp = log.tap_err(post_json(&client, &url, headers, built.body.clone()).await)?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        log.metadata(status, &resp_headers);
        if !(200..300).contains(&status) {
            let body = error_body(resp).await;
            log.chunk(&body);
            return Err(upstream_error(status, &body));
        }
        let data = log.tap_err(read_body(resp).await)?;
        log.chunk(&data);
        reporter.mark_first_response_byte();
        reporter.observe_response_model(&data);
        let mut param = Param::default();
        let original = apply_patch_original_request(req, opts);
        let out = cpa_translator::translate_non_stream(
            &Ctx::default(),
            Format::Gemini,
            response_format,
            &req.model,
            &original,
            &built.body,
            &data,
            &mut param,
        );
        let out = match out {
            Some(out) if apply_patch_translation_error(&param).is_none() && !out.is_empty() => out,
            _ => return Err(gateway_error()),
        };
        let detail = parse_gemini_usage(&data);
        reporter.publish(detail.clone());
        let out = if response_format == Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out };
        Ok(Response { payload: Bytes::from(out), metadata: usage_metadata(&detail), headers: resp_headers })
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream_inner(
        &self,
        cfg: &Arc<Config>,
        auth: &Auth,
        req: Request,
        opts: Options,
        base_model: &str,
        session_id: Option<&str>,
        reporter: &UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        let response_format = opts.response_format_or_source();
        let built = self.build_body(cfg, &req, &opts, base_model, true, true)?;
        let mut url = format!("{}/{GL_API_VERSION}/models/{base_model}:streamGenerateContent", resolve_base_url(auth));
        if opts.alt.is_empty() {
            url.push_str("?alt=sse");
        } else {
            url.push_str(&format!("?$alt={}", opts.alt));
        }
        reporter.set_translated_reasoning_effort(&built.body, Format::Gemini.as_str());

        let headers = request_headers(auth, &opts, session_id)?;
        let log = UpstreamLog::new(&opts, cfg);
        log.request(auth, self.identifier, &url, &headers, &built.body);
        let client = new_proxy_aware_http_client(&opts.proxy_url, Some(cfg), Some(auth), None);
        reporter.start_response_ttft();
        let resp = log.tap_err(post_json(&client, &url, headers, built.body.clone()).await)?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        log.metadata(status, &resp_headers);
        if !(200..300).contains(&status) {
            let body = error_body(resp).await;
            log.chunk(&body);
            return Err(upstream_error(status, &body));
        }

        let (mut pump, rx, usage_rx) = StreamPump::new(PumpSetup {
            reporter: reporter.clone(),
            from: built.from,
            upstream: Format::Gemini,
            response: response_format,
            req: &req,
            opts: &opts,
            body: built.body,
            ctx: Ctx::default(),
        });
        let reporter = reporter.clone();
        tokio::spawn(async move {
            let mut lines = observed_lines(reporter.clone(), resp);
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
                log.chunk(&line);
                // The observers below read the same frame: share one parse (dropped before the await).
                let filtered = {
                    let _parse_scope = crate::helps::parse_cache::scope();
                    reporter.observe_response_model(&line);
                    filter_sse_usage_metadata_cow(&line)
                };
                let Some(payload) = json_payload(&filtered) else { continue };
                if let Some(detail) = parse_gemini_stream_usage(payload) {
                    pump.usage.observe(detail, true);
                }
                if !pump.feed(payload).await {
                    return;
                }
            }
            if pump.end_apply_patch().await {
                return;
            }
            // A read error is reported without a synthetic terminal event, and a gone client
            // needs none.
            if let Some(err) = scan_err {
                log.error(&err.to_string());
                pump.fail(err.into()).await;
                pump.finish();
                return;
            }
            if pump.tx.is_closed() {
                return;
            }
            if !pump.feed(b"[DONE]").await {
                return;
            }
            pump.finish();
        });
        let mut result = StreamResult::new(resp_headers, rx);
        result.usage = Some(usage_rx);
        Ok(result)
    }
}

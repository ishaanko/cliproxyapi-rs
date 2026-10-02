//! `countTokens` (Go: antigravity_executor_tokens.go).

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_json::J;
use cpa_runtime::conductor::resolved_model_info;
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, Response};
use cpa_translator::{Ctx, Format, RequestEnvelope, translate_request_envelope, translate_token_count};

use super::AntigravityExecutor;
use super::pipeline::{base_model_of, pre_send};
use super::replay::prepare_gemini_reasoning_replay_payload;
use super::request::{COUNT_TOKENS_PATH, base_headers, resolve_request_base_url};
use super::signature::{ensure_leading_user_content, sanitize_gemini_request_signatures, validate_request_signatures};
use super::transport::close_auth_idle_transports;
use crate::helps::json_retry::parse_retry_delay;
use crate::helps::payload::delete_json_field;
use crate::helps::thinking::apply_request_thinking;

impl AntigravityExecutor {
    pub(crate) async fn count_tokens_impl(&self, auth: &Auth, mut req: Request, opts: Options) -> Result<Response, ExecError> {
        let cfg = self.cfg();
        let base_model = base_model_of(&req.model);
        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let to = Format::Antigravity;

        let original_source: &[u8] = if opts.original_request.is_empty() { &req.payload } else { &opts.original_request };
        let validated = validate_request_signatures(&base_model, from, original_source.to_vec());
        req.payload = validated.into();

        let (token, updated) = self.ensure_access_token(&cfg, auth).await.map_err(pre_send)?;
        let auth = updated.unwrap_or_else(|| auth.clone());
        if token.trim().is_empty() {
            return Err(pre_send(ExecError::new(401, "missing access token")));
        }

        let model_info = resolved_model_info(&req).map(|r| r.info);
        let envelope = RequestEnvelope {
            model: base_model.clone(),
            body: req.payload.to_vec(),
            model_info,
            ..Default::default()
        };
        let ctx = Ctx { alt: Some(opts.alt.clone()) };
        let payload = translate_request_envelope(&ctx, from, to, envelope).body;
        let payload = apply_request_thinking(&payload, &req, &opts, from.as_str(), to.as_str(), "antigravity", false)
            .map_err(|e| pre_send(ExecError::new(e.status_code(), e.message)))?;
        let payload = Self::obfuscate_sensitive_words(&cfg, payload);
        let payload = sanitize_gemini_request_signatures(&base_model, payload);
        let (prepared, _scope) =
            prepare_gemini_reasoning_replay_payload(&base_model, &req, &opts, payload).map_err(pre_send)?;
        let mut payload = ensure_leading_user_content(&base_model, prepared);

        for field in ["project", "model", "request.safetySettings", "request.toolConfig", "request.labels", "request.sessionId"] {
            payload = delete_json_field(&payload, field);
        }

        let base = resolve_request_base_url(&auth);
        let client = self.client(&cfg, &auth, &opts.proxy_url);
        let mut url = format!("{}{COUNT_TOKENS_PATH}", base.trim_end_matches('/'));
        if !opts.alt.is_empty() {
            url.push_str("?$alt=");
            url.extend(url::form_urlencoded::byte_serialize(opts.alt.as_bytes()));
        }

        let resp = client
            .post(&url)
            .headers(base_headers(&auth, &token))
            .body(payload)
            .send()
            .await
            .map_err(|e| ExecError::new(0, e.without_url().to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(|e| ExecError::new(0, e.without_url().to_string()))?;

        if (200..300).contains(&status) {
            let count = cpa_json::parse(&body).g("totalTokens").int();
            let translated = translate_token_count(&ctx, to, response_format, count, &body);
            return Ok(Response { payload: Bytes::from(translated), metadata: Metadata::new(), headers });
        }

        let mut err = ExecError::new(status, String::from_utf8_lossy(&body).into_owned());
        if err.message.is_empty() {
            err.message = format!("status {status}");
        }
        if status == 429 {
            close_auth_idle_transports(&auth);
            if let Some(d) = parse_retry_delay(&body) {
                err.retry_after = Some(d);
            }
        }
        Err(err)
    }
}

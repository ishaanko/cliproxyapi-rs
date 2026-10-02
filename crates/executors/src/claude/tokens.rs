//! count_tokens (Go: claude_executor_tokens.go): the upstream endpoint on first-party Anthropic,
//! a local tokenizer estimate everywhere else.

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::thinking::parse_suffix;
use cpa_json::J;
use cpa_runtime::executor::{ErrorCode, ExecError, Options, Request, Response};
use cpa_translator::{Ctx, Format};
use http::HeaderMap;

use super::body::{extract_and_remove_betas, rebuild_mid_system_messages_to_top_level};
use super::cache_control::{enforce_cache_control_limit, normalize_cache_control_ttl};
use super::cloaking::{
    detect_incoming_claude_code_request, relocate_claude_system_prompt_for_count_tokens, validate_claude_caller_system_blocks,
    validate_claude_mid_system_message_model,
};
use super::execute::sanitize_claude_messages_for_claude_upstream_with_debug;
use super::helps::cloak_obfuscate::{build_sensitive_word_matcher, obfuscate_sensitive_words};
use super::helps::credential_identity::claude_agent_session_uuid_for_request;
use super::helps::input_tokens::count_claude_input_tokens;
use super::helps::upstream::is_anthropic_upstream_base;
use super::policy::{resolve_claude_fingerprint_policy, resolve_claude_wire_policy};
use super::request::{
    ClaudeHeaderInput, CLAUDE_TOKEN_COUNTING_BETA, set_string_if_different_bytes, apply_claude_headers_with_native_profile,
    classify_claude_upstream_error_with_cooling, claude_creds,
};
use super::signing::rebuild_mid_system_message_enabled;
use super::tool_remap::{prepare_claude_oauth_tool_names_for_upstream, resolve_claude_mcp_alias_options};
use super::{ClaudeExecutor, DEFAULT_BASE_URL};
use crate::helps::status::status_err;
use crate::helps::translate::{RequestTranslation, translate_request};
use crate::helps::thinking::{api_key_model_is_compat, apply_request_thinking};

/// Only Anthropic's first-party origin has the measured native count_tokens contract
/// (Go: shouldUseClaudeUpstreamTokenCount).
pub fn should_use_claude_upstream_token_count(api_key: &str, base_url: &str) -> bool {
    !api_key.trim().is_empty() && is_anthropic_upstream_base(base_url)
}

fn token_count_validation_error(message: &str) -> ExecError {
    status_err(400, message).with_code(ErrorCode::RequestScoped)
}

/// Shape check for the local estimator (Go: validateClaudeTokenCountRequest); errors are 400 and
/// request scoped.
pub fn validate_claude_token_count_request(body: &[u8]) -> Result<(), ExecError> {
    if !cpa_json::valid(body) {
        return Err(token_count_validation_error("invalid Claude token count request JSON"));
    }
    let root = cpa_json::parse(body);
    if !root.is_object() {
        return Err(token_count_validation_error("Claude token count request must be a JSON object"));
    }
    let messages = root.g("messages");
    if !messages.is_array() || messages.array().is_empty() {
        return Err(token_count_validation_error("Claude token count request messages must be a non-empty array"));
    }
    for message in messages.array() {
        if !message.is_object() {
            return Err(token_count_validation_error("Claude token count request messages must contain objects"));
        }
        let role = message.g("role").str();
        if role != "user" && role != "assistant" {
            return Err(token_count_validation_error("Claude token count request message role must be user or assistant"));
        }
        let content = message.g("content");
        if content.is_string() {
            continue;
        }
        if !content.is_array() {
            return Err(token_count_validation_error("Claude token count request message content must be a string or array"));
        }
        for block in content.array() {
            let block_type = block.g("type");
            if !block.is_object() || !block_type.is_string() || block_type.str().is_empty() {
                return Err(token_count_validation_error("Claude token count request content blocks must be typed objects"));
            }
        }
    }
    Ok(())
}

impl ClaudeExecutor {
    /// Go: ClaudeExecutor.CountTokens.
    pub(super) async fn count_tokens_impl(
        &self,
        cfg: &Config,
        auth: &Auth,
        req: Request,
        opts: Options,
    ) -> Result<Response, ExecError> {
        let (api_key, mut base_url) = claude_creds(auth);
        if base_url.is_empty() {
            base_url = DEFAULT_BASE_URL.to_string();
        }
        // Every custom or third-party base URL keeps local estimation, OAuth or API key alike.
        if should_use_claude_upstream_token_count(&api_key, &base_url) {
            return self.count_tokens_upstream(cfg, auth, req, opts).await;
        }

        let base_model = parse_suffix(&req.model).model_name;
        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let to = Format::Claude;

        // Streaming translation preserves function calling, except for claude.
        let stream = from != to;
        let translation = RequestTranslation::new(&opts.headers, Some(cfg), from, to, &base_model, stream).compat(api_key_model_is_compat(&req));
        let mut body = translate_request(&translation, &req.payload).0;
        body = apply_request_thinking(&body, &req, &opts, from.as_str(), to.as_str(), "claude", false)
            .map_err(|e| ExecError::new(e.status_code(), e.to_string()))?;
        if rebuild_mid_system_message_enabled(cfg, auth) {
            body = rebuild_mid_system_messages_to_top_level(&body);
        }
        body = sanitize_claude_messages_for_claude_upstream_with_debug(&body, &base_model, api_key_model_is_compat(&req));
        validate_claude_token_count_request(&body)?;

        // Gateways without a native count_tokens contract use the local estimator without
        // generation-only CLI instructions.
        let count = count_claude_input_tokens(&body)
            .map_err(|e| ExecError::new(0, format!("claude executor: token counting failed: {e}")))?;
        let usage_json = format!(r#"{{"input_tokens":{count}}}"#);
        let out = cpa_translator::translate_token_count(&Ctx::default(), to, response_format, count, usage_json.as_bytes());
        Ok(Response { payload: Bytes::from(out), ..Default::default() })
    }

    /// Anthropic's native token-counting contract (Go: countTokensUpstream).
    async fn count_tokens_upstream(
        &self,
        cfg: &Config,
        auth: &Auth,
        req: Request,
        opts: Options,
    ) -> Result<Response, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let upstream_model = base_model.clone();

        let (api_key, mut base_url) = claude_creds(auth);
        if base_url.is_empty() {
            base_url = DEFAULT_BASE_URL.to_string();
        }
        let url = format!("{base_url}/v1/messages/count_tokens?beta=true");
        let fp = resolve_claude_fingerprint_policy(cfg, auth, &api_key);

        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let to = Format::Claude;
        let original_payload: &[u8] = if opts.original_request.is_empty() { &req.payload } else { &opts.original_request };
        let (incoming_headers, detection) = detect_incoming_claude_code_request(&opts.headers, original_payload, true, cfg);
        let confirmed_claude_code = detection.confirmed;
        let mut claude_session_id = String::new();
        if fp.profile_claude_code_cli {
            claude_session_id = claude_agent_session_uuid_for_request(
                &incoming_headers,
                original_payload,
                &req.payload,
                confirmed_claude_code,
                &[&opts.metadata, &req.metadata],
            );
        }
        let stream = from != to;
        let translation = RequestTranslation::new(&opts.headers, Some(cfg), from, to, &base_model, stream).compat(api_key_model_is_compat(&req));
        let mut body = translate_request(&translation, &req.payload).0;
        body = set_string_if_different_bytes(&body, "model", &upstream_model);
        body = apply_request_thinking(&body, &req, &opts, from.as_str(), to.as_str(), "claude", false)
            .map_err(|e| ExecError::new(e.status_code(), e.to_string()))?;
        if rebuild_mid_system_message_enabled(cfg, auth) {
            body = rebuild_mid_system_messages_to_top_level(&body);
        }

        let direct_anthropic = is_anthropic_upstream_base(&base_url);
        // Claude Code's count_tokens carries only model, messages and tools, so Messages cloaking
        // must not run. Relocate the caller system prompt into messages so its tokens stay counted,
        // and obfuscate sensitive words like the Messages path.
        let (policy, settings) = resolve_claude_wire_policy(cfg, auth, &api_key, confirmed_claude_code);
        let cloaked = policy.cloak;
        if cloaked {
            if !settings.strict_mode {
                validate_claude_caller_system_blocks(&cpa_json::parse(&body).g("system").value())?;
            }
            body = relocate_claude_system_prompt_for_count_tokens(&body, settings.strict_mode);
            if !settings.sensitive_words.is_empty() {
                body = obfuscate_sensitive_words(&body, build_sensitive_word_matcher(&settings.sensitive_words).as_ref());
            }
        }

        body = enforce_cache_control_limit(&body, 4);
        body = normalize_cache_control_ttl(&body);

        let (mut extra_betas, mut body) = extract_and_remove_betas(&body);
        // Claude Code's beta.messages.countTokens() always appends this beta.
        extra_betas.push(CLAUDE_TOKEN_COUNTING_BETA.to_string());
        if fp.mcp_alias && cloaked {
            let secret = resolve_claude_mcp_alias_options(opts.metadata.get("client_api_key").and_then(|v| v.as_str()).unwrap_or(""));
            body = prepare_claude_oauth_tool_names_for_upstream(&body, &secret).0;
        }
        body = sanitize_claude_messages_for_claude_upstream_with_debug(&body, &base_model, api_key_model_is_compat(&req));
        // api.anthropic.com rejects these fields on count_tokens outright, so they go for every
        // credential that lands there; elsewhere only an explicit claude-code-cli profile aligns
        // the shape to the measured one (model, messages, tools).
        let align_cli_count_tokens_shape = fp.profile_claude_code_cli;
        if direct_anthropic || align_cli_count_tokens_shape {
            let mut root = cpa_json::parse(&body);
            cpa_json::delete(&mut root, "metadata");
            cpa_json::delete(&mut root, "context_management");
            cpa_json::delete(&mut root, "diagnostics");
            body = cpa_json::to_vec(&root);
        }
        if align_cli_count_tokens_shape {
            body = cpa_core::util::strip_claude_code_attribution_system(&body);
        }
        validate_claude_mid_system_message_model(&body, confirmed_claude_code, direct_anthropic)?;

        let parsed_url = url::Url::parse(&url).map_err(|e| ExecError::new(0, e.to_string()))?;
        let cpa_session_id = crate::helps::session::ensure_session_id(None, "", &opts, &req.payload);
        let mut headers = HeaderMap::new();
        apply_claude_headers_with_native_profile(
            &mut headers,
            &ClaudeHeaderInput {
                auth,
                api_key: &api_key,
                stream: false,
                extra_betas: &extra_betas,
                body: &body,
                cfg,
                incoming_headers: &incoming_headers,
                confirmed_claude_code: confirmed_claude_code && !cloaked,
                helper_profile: false,
                session_ids: &[claude_session_id.as_str()],
                url: &parsed_url,
                cpa_session_id: cpa_session_id.as_deref(),
            },
        )?;

        let client = super::http::claude_http_client(&opts.proxy_url, cfg, auth);
        let resp = super::http::send_messages(&client, &url, &headers, &body).await?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        if !(200..300).contains(&status) {
            let data = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => Bytes::from(format!("failed to read error response body: {}", crate::helps::status::transport_message(&e))),
            };
            return Err(classify_claude_upstream_error_with_cooling(status, &resp_headers, &data, cfg.claude.model_level_cooling));
        }
        let data = resp.bytes().await.map_err(|e| ExecError::new(0, crate::helps::status::transport_message(&e)))?;
        let count = cpa_json::parse(&data).g("input_tokens").int();
        let out = cpa_translator::translate_token_count(&Ctx::default(), to, response_format, count, &data);
        Ok(Response { payload: Bytes::from(out), headers: resp_headers, ..Default::default() })
    }
}

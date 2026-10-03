//! Messages request pipeline shared by `Execute` and `ExecuteStream`, plus the non-stream
//! `Execute` response handling (Go: claude_executor_execute.go and the pipeline half of
//! claude_executor_stream.go, which are line-for-line the same).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::thinking::parse_suffix;
use cpa_json::J;
use cpa_runtime::executor::{ExecError, Options, Request, Response};
use cpa_translator::{Ctx, Format, Param};
use http::HeaderMap;
use parking_lot::Mutex;

use super::body::{
    extract_and_remove_betas, disable_thinking_if_tool_choice_forced, normalize_claude_sampling_for_upstream, rebuild_mid_system_messages_to_top_level,
    sanitize_claude_web_search_domains,
};
use super::cache_control::{
    CLAUDE_CACHE_CONTROL_TTL_1H, enforce_cache_control_limit, ensure_cache_control, ensure_model_max_tokens, normalize_cache_control_ttl,
    should_ensure_cache_control, strip_claude_cache_control_ttl, upgrade_claude_cache_control_ttl,
};
use super::cloaking::{
    ClaudeCodeContextManagementState, apply_cloaking_internal, capture_claude_code_fable_state, capture_claude_code_system_placement,
    claude_cch_fallback_billing_header, detect_incoming_claude_code_request, inject_claude_code_context_management,
    reconcile_claude_code_context_management, reconcile_claude_code_fable_model_after_payload,
    reconcile_claude_code_system_placement_after_payload, resolve_claude_continuity_tags, validate_claude_mid_system_message_model,
};
use super::diagnostics::{
    ClaudeDiagnosticsRequestState, claude_message_id_from_response, claude_message_id_from_sse, commit_claude_continuity_state,
    inject_claude_diagnostics, inject_claude_diagnostics_with_state,
};
use super::fast_error::{
    claude_request_is_fast, new_claude_fast_direct_response_error, wrap_claude_fast_request_error,
};
use crate::helps::cloak_obfuscate::{build_sensitive_word_matcher, obfuscate_sensitive_words};
use super::helps::credential_identity::{apply_claude_credential_metadata, claude_agent_session_uuid_for_request, claude_request_has_execution_metadata};
use super::helps::diagnostics::{
    claude_subagent_requests_1h, extract_claude_billing_tags, inject_claude_billing_tags, is_claude_probe_or_helper_request,
    is_claude_subagent_request, strip_claude_billing_tags,
};
use super::helps::upstream::is_anthropic_upstream_base;
use super::helps::{ClaudeContinuityContext, ClaudeCtx};
use super::policy::{resolve_claude_fingerprint_policy, resolve_claude_wire_policy};
use super::request::{
    ClaudeHeaderInput, apply_claude_headers_with_native_profile, claude_creds, classify_claude_upstream_error_with_cooling, header_value,
    set_bool_if_different_bytes, set_string_if_different_bytes,
};
use super::signing::{
    ClaudeCchUpstreamKind, claude_body_needs_billing_fallback, claude_cch_signing_enabled, finalize_anthropic_messages_body_cch,
    is_kimi_messages_upstream, rebuild_mid_system_message_enabled, strip_default_kimi_claude_code_attribution,
};
use super::thinking_replay::{
    ClaudeThinkingReplayScope, cache_claude_thinking_replay_response, claude_thinking_replay_enabled,
    clear_claude_thinking_replay_content, prepare_claude_thinking_replay_request, should_clear_kimi_thinking_replay_after_error,
};
use super::tool_remap::{
    prepare_claude_oauth_tool_names_for_upstream, resolve_claude_mcp_alias_options, restore_claude_oauth_tool_names_from_response,
    restore_claude_oauth_tool_names_from_stream_line,
};
use super::{ClaudeExecutor, DEFAULT_BASE_URL};
use crate::helps::apply_patch::{apply_patch_original_request, apply_patch_translation_error, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE};
use crate::helps::payload::{PayloadRequest, apply_payload_config_tracked, payload_request_path, payload_requested_model};
use crate::helps::status::status_err;
use crate::helps::translate::{RequestTranslation, translate_request_pair};
use crate::helps::thinking::{api_key_model_is_compat, apply_request_thinking};
use crate::helps::usage::{Detail, StreamUsageBuffer, UsageReporter, parse_claude_usage};

/// Everything the response half needs after the shared request pipeline ran.
pub(super) struct Prepared {
    pub url: String,
    pub upstream_stream: bool,
    pub body_for_translation: Vec<u8>,
    pub body_for_upstream: Vec<u8>,
    pub headers: HeaderMap,
    /// Forward tool-name alias map inverted for the response (alias to original).
    pub tool_reverse_map: HashMap<String, String>,
    pub diagnostics_state: ClaudeDiagnosticsRequestState,
    pub fast_request: bool,
    pub replay_scope: ClaudeThinkingReplayScope,
    /// Client model to write back into responses (embedded executors only; see
    /// `Prepared::restore_response_model`).
    pub restore_model: Option<String>,
    /// OAuth credentials record client cancellations as stream failures.
    pub oauth_cancellation: bool,
    pub req: Request,
}

impl Prepared {
    /// Go: ClaudeExecutor.restoreResponseModel.
    pub fn restore_response_model(&self, payload: Vec<u8>) -> Vec<u8> {
        match &self.restore_model {
            Some(model) => super::body::restore_claude_response_model(&payload, model),
            None => payload,
        }
    }
}

/// Signature sanitizer for thinking blocks coming from other providers, then the web-search
/// domain fix (Go: sanitizeClaudeMessagesForClaudeUpstreamWithDebug).
pub fn sanitize_claude_messages_for_claude_upstream_with_debug(
    body: &[u8],
    base_model: &str,
    preserve_empty_thinking_blocks: bool,
) -> Vec<u8> {
    use cpa_core::signature::{SignatureProvider, sanitize_claude_messages_for_claude_upstream, signature_provider_from_model_name};
    let mut sanitized = body.to_vec();
    if signature_provider_from_model_name(base_model) == SignatureProvider::Claude || preserve_empty_thinking_blocks {
        let (out, report) = sanitize_claude_messages_for_claude_upstream(body, base_model, preserve_empty_thinking_blocks);
        sanitized = out;
        if report.dropped_blocks != 0 || report.dropped_signatures != 0 || report.replaced_signatures != 0 {
            tracing::debug!(
                component = "signature_sanitizer",
                executor = "claude",
                target_model = base_model,
                preserved = report.preserved,
                dropped_blocks = report.dropped_blocks,
                dropped_signatures = report.dropped_signatures,
                replaced_signatures = report.replaced_signatures,
                "claude executor: sanitized signature history before upstream"
            );
        }
    }
    sanitize_claude_web_search_domains(&sanitized)
}

impl ClaudeExecutor {
    /// The shared request pipeline: translate, thinking, cloaking, payload rules, cache control,
    /// tool aliasing, identity, CCH signing and headers (Go: the first half of Execute and
    /// ExecuteStream). `stream` is true for ExecuteStream, which always streams upstream.
    pub(super) fn prepare_messages_request(
        &self,
        cfg: &Config,
        auth: &Auth,
        req: Request,
        opts: &Options,
        stream: bool,
        reporter: &UsageReporter,
    ) -> Result<Prepared, ExecError> {
        // Stages and helpers below re-read the body many times; memoize its parse (see parse_cache).
        let _parse_scope = crate::helps::parse_cache::scope();
        let mut req = req;
        let mut replay_scope = ClaudeThinkingReplayScope::default();
        if claude_thinking_replay_enabled(auth, &req, opts) {
            let replay_ctx = ClaudeCtx { incoming_headers: Some(opts.headers.clone()), ..Default::default() };
            let caller_api_key = opts.metadata.get("client_api_key").and_then(|v| v.as_str()).unwrap_or("");
            let (new_req, scope) = prepare_claude_thinking_replay_request(&replay_ctx, auth, req, opts, caller_api_key);
            req = new_req;
            replay_scope = scope;
        }
        // Any later failure drops the replayed content, like the Go deferred clear.
        let outcome = self.prepare_messages_request_inner(cfg, auth, req, replay_scope.clone(), opts, stream, reporter);
        if let Err(err) = &outcome
            && replay_scope.replay_applied
            && should_clear_kimi_thinking_replay_after_error(Some(err))
        {
            clear_claude_thinking_replay_content(&replay_scope);
        }
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_messages_request_inner(
        &self,
        cfg: &Config,
        auth: &Auth,
        req: Request,
        replay_scope: ClaudeThinkingReplayScope,
        opts: &Options,
        stream: bool,
        reporter: &UsageReporter,
    ) -> Result<Prepared, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let upstream_model = self.upstream_model(&base_model);
        if upstream_model != base_model {
            reporter.set_upstream_model(&upstream_model);
        }

        let (api_key, mut base_url) = claude_creds(auth);
        if base_url.is_empty() {
            base_url = DEFAULT_BASE_URL.to_string();
        }
        let url = format!("{base_url}/v1/messages?beta=true");
        let fp = resolve_claude_fingerprint_policy(cfg, auth, &api_key);
        // Real OAuth signs everywhere; an opted-in API key signs only on first-party.
        let cch_signing = claude_cch_signing_enabled(&api_key, ClaudeCchUpstreamKind::Anthropic, fp.profile_claude_code_cli, &url);

        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let to = Format::Claude;
        // Use an upstream stream whenever the downstream response needs translation from Claude
        // events. Native Claude responses use the JSON response path.
        let upstream_stream = if stream { true } else { response_format != to };
        let original_payload: Bytes =
            if opts.original_request.is_empty() { req.payload.clone() } else { opts.original_request.clone() };
        let (incoming_headers, detection) = detect_incoming_claude_code_request(&opts.headers, &original_payload, false, cfg);
        let confirmed_claude_code = detection.confirmed;
        let mut claude_session_id = String::new();
        if fp.profile_claude_code_cli {
            claude_session_id = claude_agent_session_uuid_for_request(
                &incoming_headers,
                &original_payload,
                &req.payload,
                confirmed_claude_code,
                &[&opts.metadata, &req.metadata],
            );
        }

        let continuity = Arc::new(Mutex::new(ClaudeContinuityContext::default()));
        let ctx = ClaudeCtx {
            incoming_headers: Some(incoming_headers.clone()),
            session_id: claude_session_id.clone(),
            execution_metadata: claude_request_has_execution_metadata(&[&opts.metadata, &req.metadata]),
            continuity: Some(Arc::clone(&continuity)),
        };

        let is_compat = api_key_model_is_compat(&req);
        let translation = RequestTranslation::new(&opts.headers, Some(cfg), from, to, &base_model, upstream_stream).compat(is_compat);
        let (original_translated, mut body, _) = translate_request_pair(&translation, &original_payload, &req.payload);
        body = set_string_if_different_bytes(&body, "model", &upstream_model);

        body = apply_request_thinking(&body, &req, opts, from.as_str(), to.as_str(), "claude", false)
            .map_err(|e| ExecError::new(e.status_code(), e.to_string()))?;
        if rebuild_mid_system_message_enabled(cfg, auth) {
            body = rebuild_mid_system_messages_to_top_level(&body);
        }

        // Cloaking: system prompt injection, fake user id, sensitive word obfuscation.
        let (_, wire_settings) = resolve_claude_wire_policy(cfg, auth, &api_key, confirmed_claude_code);
        let body_before_cloaking = body.clone();
        let mut is_probe_or_helper = is_claude_probe_or_helper_request(&body_before_cloaking);
        let (cloaked_body, cloaked) =
            apply_cloaking_internal(&ctx, cfg, auth, body, &api_key, confirmed_claude_code, cch_signing, false)?;
        body = cloaked_body;
        let system_placement_state = capture_claude_code_system_placement(&body_before_cloaking, &body, cloaked);
        let fable_state = capture_claude_code_fable_state(&body_before_cloaking, &body, cloaked);
        // Only the Messages endpoint on Anthropic itself was captured.
        let mut diagnostics_state = ClaudeDiagnosticsRequestState::default();
        if !is_probe_or_helper {
            is_probe_or_helper = is_claude_probe_or_helper_request(&body);
        }
        {
            let c = continuity.lock();
            if c.initialized {
                diagnostics_state = ClaudeDiagnosticsRequestState {
                    key: c.key.clone(),
                    sequence: c.sequence,
                    prompt_id: c.prompt_id.clone(),
                };
            }
        }
        let mut context_management_state = ClaudeCodeContextManagementState {
            eligible: cloaked && is_anthropic_upstream_base(&base_url),
            caller_owned: crate::helps::parse_cache::parse(&body).g("context_management").exists(),
            ..Default::default()
        };
        let mut diagnostics_injected_by_cpa = false;
        if context_management_state.eligible {
            let (updated, injected) = inject_claude_code_context_management(&body);
            body = updated;
            context_management_state.automatically_injected = injected;
            if fp.inject_diagnostics && !is_probe_or_helper {
                diagnostics_injected_by_cpa = true;
                let c = continuity.lock().clone();
                if c.initialized {
                    let (updated, state) = inject_claude_diagnostics_with_state(
                        &body,
                        &c.key,
                        c.sequence,
                        &c.previous_message_id,
                        &c.prompt_id,
                    );
                    body = updated;
                    diagnostics_state = state;
                } else {
                    let (updated, state) = inject_claude_diagnostics(&body, auth, &claude_session_id);
                    body = updated;
                    diagnostics_state = state;
                }
            }
        }

        let requested_model = payload_requested_model(opts, &req.model);
        let request_path = payload_request_path(opts);
        let (payload_body, touched_payload_paths): (Vec<u8>, HashSet<String>) = apply_payload_config_tracked(
            &PayloadRequest {
                cfg: Some(cfg),
                target_executor: "",
                model: &base_model,
                protocol: to.as_str(),
                from_protocol: from.as_str(),
                root: "",
                requested_model: &requested_model,
                request_path: &request_path,
                headers: Some(&opts.headers),
            },
            &body,
            &original_translated,
            &["context_management", "fallbacks", "thinking.display", "diagnostics"],
        );
        body = payload_body;
        context_management_state.payload_rule_touched = touched_payload_paths.contains("context_management");
        body = reconcile_claude_code_system_placement_after_payload(&body, &system_placement_state);
        let was_probe_or_helper = is_probe_or_helper;
        is_probe_or_helper = is_claude_probe_or_helper_request(&body);
        if is_probe_or_helper {
            diagnostics_state = ClaudeDiagnosticsRequestState::default();
            if diagnostics_injected_by_cpa && !touched_payload_paths.contains("diagnostics") {
                let mut root = cpa_json::parse(&body);
                cpa_json::delete(&mut root, "diagnostics");
                body = cpa_json::to_vec(&root);
            }
            if cloaked {
                body = strip_claude_billing_tags(&body);
            }
            *continuity.lock() = ClaudeContinuityContext::default();
        } else if was_probe_or_helper && cloaked {
            // Declassified as probe (a payload override changed max_tokens: 1 to a normal request):
            // initialize continuity and diagnostics if cloaked and eligible.
            let (existing_prev_req, existing_prompt_id) = extract_claude_billing_tags(&body);
            if let Some(tags) = resolve_claude_continuity_tags(
                &ctx,
                auth,
                &incoming_headers,
                &body,
                confirmed_claude_code,
                &existing_prev_req,
                &existing_prompt_id,
            ) {
                *continuity.lock() = tags.ctx.clone();
                body = inject_claude_billing_tags(&body, &tags.prev_req, &tags.prompt_id);
                if fp.inject_diagnostics && is_anthropic_upstream_base(&base_url) {
                    let (updated, state) = inject_claude_diagnostics_with_state(
                        &body,
                        &tags.ctx.key,
                        tags.ctx.sequence,
                        &tags.ctx.previous_message_id,
                        &tags.prompt_id,
                    );
                    body = updated;
                    diagnostics_state = state;
                }
            }
        }
        body = reconcile_claude_code_fable_model_after_payload(
            body,
            fable_state,
            touched_payload_paths.contains("fallbacks"),
            touched_payload_paths.contains("thinking.display"),
            cloaked,
            is_probe_or_helper,
        );
        body = ensure_model_max_tokens(&body, &base_model);

        // Disable thinking if tool_choice forces tool use (Anthropic API constraint).
        body = disable_thinking_if_tool_choice_forced(&body);
        body = reconcile_claude_code_context_management(&body, context_management_state);
        body = normalize_claude_sampling_for_upstream(&body, confirmed_claude_code);

        // Default cache_control for translated entrypoints and other non-native callers.
        // Confirmed native Claude Code owns its marker placement. Cloaked requests always run the
        // section-independent ensure so cloaking's first-user marker cannot suppress the system and
        // latest-user breakpoints.
        let cpa_owns_cache_control = should_ensure_cache_control(&body, cloaked, confirmed_claude_code);
        if cpa_owns_cache_control {
            body = ensure_cache_control(&body);
        }

        // Anthropic allows at most 4 cache_control breakpoints per request.
        body = enforce_cache_control_limit(&body, 4);

        // Native selects the 1h cache pool for OAuth credentials and pairs it with
        // extended-cache-ttl, which the beta assembly emits on the same condition. Upgrade only
        // while CPA owns placement; subagents default to 5m unless 1h is requested, probes omit both.
        let is_subagent = is_claude_subagent_request(&incoming_headers, &body);
        let subagent_1h = is_subagent && claude_subagent_requests_1h(&incoming_headers, &body);
        if cpa_owns_cache_control && fp.profile_claude_code_cli && (!is_subagent || subagent_1h) && !is_probe_or_helper {
            body = upgrade_claude_cache_control_ttl(&body, CLAUDE_CACHE_CONTROL_TTL_1H);
        } else if is_probe_or_helper || (is_subagent && !subagent_1h) {
            body = strip_claude_cache_control_ttl(&body);
        }

        // A 1h block must not appear after a 5m block (tools, system, messages order).
        body = normalize_cache_control_ttl(&body);
        if !stream {
            // Payload rules may rewrite `stream`; keep body, headers and response parser on one
            // authority. Native non-stream Haiku helpers omit `stream` rather than send false.
            let stream_field_exists = crate::helps::parse_cache::parse(&body).g("stream").exists();
            if !detection.helper_profile || stream_field_exists || upstream_stream {
                body = set_bool_if_different_bytes(&body, "stream", upstream_stream);
            }
        }

        // Extract betas from the body into the header list.
        let (extra_betas, body) = extract_and_remove_betas(&body);
        let body_for_translation = body.clone();
        let mut body_for_upstream = body;
        let mut tool_reverse_map = HashMap::new();
        if fp.mcp_alias && cloaked {
            let alias_options = resolve_claude_mcp_alias_options(opts.metadata.get("client_api_key").and_then(|v| v.as_str()).unwrap_or(""));
            let (updated, reverse) = prepare_claude_oauth_tool_names_for_upstream(&body_for_upstream, &alias_options);
            body_for_upstream = updated;
            tool_reverse_map = reverse;
        }
        body_for_upstream = sanitize_claude_messages_for_claude_upstream_with_debug(&body_for_upstream, &base_model, is_compat);
        if fp.apply_cli_identity {
            body_for_upstream = apply_claude_cli_identity(
                &body_for_upstream,
                auth,
                &api_key,
                &url,
                &claude_session_id,
                fp.synthesize_identity,
            )?;
        }
        if cloaked && !wire_settings.sensitive_words.is_empty() {
            let matcher = build_sensitive_word_matcher(&wire_settings.sensitive_words);
            body_for_upstream = obfuscate_sensitive_words(&body_for_upstream, matcher.as_ref());
        }
        if cch_signing {
            let mut cch_billing = String::new();
            if !detection.helper_profile || claude_body_needs_billing_fallback(&body_for_upstream) {
                cch_billing = claude_cch_fallback_billing_header(&ctx, cfg, &body_for_upstream, &detection.entrypoint);
            }
            body_for_upstream = finalize_anthropic_messages_body_cch(&body_for_upstream, &cch_billing)
                .map_err(|e| ExecError::new(0, format!("finalize Claude CCH: {e}")))?;
        }
        body_for_upstream = strip_default_kimi_claude_code_attribution(Some(auth), &url, fp.profile_claude_code_cli, &body_for_upstream);
        // Runs on the finished body: payload rules can rewrite model and messages long after
        // translation.
        validate_claude_mid_system_message_model(&body_for_upstream, confirmed_claude_code, is_anthropic_upstream_base(&base_url))?;
        reporter.set_translated_reasoning_effort(&body_for_upstream, to.as_str());

        let parsed_url = url::Url::parse(&url).map_err(|e| ExecError::new(0, e.to_string()))?;
        let cpa_session_id = crate::helps::session::ensure_session_id(None, "", opts, &req.payload);
        let mut headers = HeaderMap::new();
        apply_claude_headers_with_native_profile(
            &mut headers,
            &ClaudeHeaderInput {
                auth,
                api_key: &api_key,
                stream: upstream_stream,
                extra_betas: &extra_betas,
                body: &body_for_upstream,
                cfg,
                incoming_headers: &incoming_headers,
                confirmed_claude_code: confirmed_claude_code && !cloaked,
                helper_profile: detection.helper_profile,
                session_ids: &[claude_session_id.as_str()],
                url: &parsed_url,
                cpa_session_id: cpa_session_id.as_deref(),
            },
        )?;
        let fast_request = is_anthropic_upstream_base(&base_url) && claude_request_is_fast(&headers, &body_for_upstream);

        Ok(Prepared {
            url,
            upstream_stream,
            body_for_translation,
            body_for_upstream,
            headers,
            tool_reverse_map,
            diagnostics_state,
            fast_request,
            replay_scope,
            oauth_cancellation: fp.oauth_cancellation,
            restore_model: (self.embedding.is_some() && !req.model.trim().is_empty()).then(|| req.model.clone()),
            req,
        })
    }

    /// Non-stream Messages call (Go: ClaudeExecutor.Execute).
    pub(super) async fn execute_impl(
        &self,
        cfg: &Config,
        auth: &Auth,
        req: Request,
        opts: Options,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(status_err(501, "/responses/compact not supported"));
        }
        let response_format = opts.response_format_or_source();
        let prepared = match self.prepare_messages_request(cfg, auth, req, &opts, false, reporter) {
            Ok(p) => p,
            Err(err) => return Err(err),
        };
        let replay_scope = prepared.replay_scope.clone();
        let result = self.send_non_stream(cfg, auth, &opts, response_format, &prepared, reporter).await;
        if let Err(err) = &result
            && replay_scope.replay_applied
            && should_clear_kimi_thinking_replay_after_error(Some(err))
        {
            clear_claude_thinking_replay_content(&replay_scope);
        }
        result
    }

    /// Sends the prepared request and maps a non-2xx response to its classified error
    /// (Go: the shared send + error half of Execute/ExecuteStream/CountTokens).
    pub(super) async fn send_upstream(
        &self,
        cfg: &Config,
        auth: &Auth,
        opts: &Options,
        p: &Prepared,
    ) -> Result<reqwest::Response, ExecError> {
        self.record_upstream_request(cfg, auth, opts, &p.url, &p.headers, &p.body_for_upstream);
        let client = super::http::claude_http_client(&opts.proxy_url, cfg, auth);
        let model_level_cooling = cfg.claude.model_level_cooling;
        let resp = match super::http::send_messages(&client, &p.url, &p.headers, &p.body_for_upstream).await {
            Ok(r) => r,
            Err(err) => {
                tracing::debug!("claude upstream request failed: {}", err.message);
                opts.api_log.record_api_response_error(cfg, &err.message);
                return Err(wrap_claude_fast_request_error(p.fast_request, 0, err));
            }
        };
        let status = resp.status().as_u16();
        opts.api_log.record_api_response_metadata(cfg, status, resp.headers());
        if (200..300).contains(&status) {
            return Ok(resp);
        }
        let resp_headers = resp.headers().clone();
        let body = match resp.bytes().await {
            Ok(b) => match super::decode::decode_body(b) {
                Ok(b) => b,
                Err(e) => {
                    opts.api_log.record_api_response_error(cfg, &e);
                    let msg = format!("failed to decode error response body: {e}");
                    let err = classify_claude_upstream_error_with_cooling(status, &resp_headers, msg.as_bytes(), model_level_cooling);
                    return Err(wrap_claude_fast_request_error(p.fast_request, status, err));
                }
            },
            Err(e) => {
                let msg = crate::helps::status::transport_message(&e);
                opts.api_log.record_api_response_error(cfg, &msg);
                Bytes::from(format!("failed to read error response body: {msg}"))
            }
        };
        opts.api_log.append_api_response_chunk(cfg, &body);
        tracing::debug!(
            "request error, error status: {status}, error message: {}",
            crate::helps::logging::summarize_error_body(
                resp_headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or(""),
                &body
            )
        );
        if p.fast_request {
            return Err(new_claude_fast_direct_response_error(status, &resp_headers, &body));
        }
        Err(classify_claude_upstream_error_with_cooling(status, &resp_headers, &body, model_level_cooling))
    }

    async fn send_non_stream(
        &self,
        cfg: &Config,
        auth: &Auth,
        opts: &Options,
        response_format: Format,
        p: &Prepared,
        reporter: &UsageReporter,
    ) -> Result<Response, ExecError> {
        let to = Format::Claude;
        let resp = self.send_upstream(cfg, auth, opts, p).await?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        let data = match resp.bytes().await.map_err(|e| crate::helps::status::transport_message(&e)).and_then(super::decode::decode_body) {
            Ok(b) => b,
            Err(msg) => {
                opts.api_log.record_api_response_error(cfg, &msg);
                return Err(wrap_claude_fast_request_error(p.fast_request, status, ExecError::new(0, msg)));
            }
        };
        opts.api_log.append_api_response_chunk(cfg, &data);
        let mut data = data.to_vec();
        let mut stream_usage = StreamUsageBuffer::default();
        if p.upstream_stream {
            if let Err(err) = validate_claude_streaming_response(&data) {
                opts.api_log.record_api_response_error(cfg, &err.message);
                return Err(wrap_claude_fast_request_error(p.fast_request, status, err));
            }
            let msg_id = claude_message_id_from_sse(&data);
            if !msg_id.is_empty() {
                commit_claude_continuity_state(&p.diagnostics_state, &msg_id, &header_value(&resp_headers, "request-id"));
            }
            let mut lines: Vec<Vec<u8>> = data.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
            for line in &mut lines {
                reporter.observe_response_model(line);
                stream_usage.observe_claude_stream(line);
                match restore_claude_oauth_tool_names_from_stream_line(line, &p.tool_reverse_map) {
                    Ok(restored) => *line = restored,
                    Err(err) => {
                        let mut err = err.into_exec_error();
                        err.message = format!("restore Claude OAuth tool name from streaming response: {}", err.message);
                        opts.api_log.record_api_response_error(cfg, &err.message);
                        reporter.publish_buffer_failure(&stream_usage, &err);
                        return Err(wrap_claude_fast_request_error(p.fast_request, status, err));
                    }
                }
            }
            data = lines.join(&b'\n');
        } else {
            commit_claude_continuity_state(
                &p.diagnostics_state,
                &claude_message_id_from_response(&data),
                &header_value(&resp_headers, "request-id"),
            );
            reporter.observe_response_model(&data);
            data = restore_claude_oauth_tool_names_from_response(&data, &p.tool_reverse_map).map_err(|err| {
                let mut err = err.into_exec_error();
                err.message = format!("restore Claude OAuth tool name from response: {}", err.message);
                opts.api_log.record_api_response_error(cfg, &err.message);
                wrap_claude_fast_request_error(p.fast_request, status, err)
            })?;
        }
        data = p.restore_response_model(data);
        cache_claude_thinking_replay_response(&p.replay_scope, &data);
        let mut param = Param::default();
        let original_request = apply_patch_original_request(&p.req, opts);
        let out = cpa_translator::translate_non_stream(
            &Ctx::default(),
            to,
            response_format,
            &p.req.model,
            &original_request,
            &p.body_for_translation,
            &data,
            &mut param,
        );
        let out = match out {
            Some(out) if apply_patch_translation_error(&param).is_none() && !out.is_empty() => out,
            _ => return Err(status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)),
        };
        let detail: Detail = if p.upstream_stream {
            stream_usage.detail().unwrap_or_default()
        } else {
            parse_claude_usage(&data)
        };
        if p.upstream_stream {
            reporter.publish_buffer(&stream_usage);
        } else {
            reporter.publish(detail.clone());
        }
        let out = if response_format == Format::OpenAIResponse {
            crate::helps::responses_usage::ensure_responses_usage_details(&out)
        } else {
            out
        };
        let mut response = Response { payload: Bytes::from(out), headers: resp_headers, ..Default::default() };
        response.metadata.insert("usage".to_string(), UsageReporter::usage_metadata(&detail));
        Ok(response)
    }
}

/// Applies the Claude CLI credential identity to the upstream body (Go: applyClaudeCLIIdentity).
/// API keys seed the synthesized identity from the key itself; Kimi seeds from the auth identity.
pub fn apply_claude_cli_identity(
    body: &[u8],
    auth: &Auth,
    api_key: &str,
    upstream_url: &str,
    session_id: &str,
    synthesize: bool,
) -> Result<Vec<u8>, ExecError> {
    use super::helps::cli_identity_seed::{claude_cli_auth_identity_seed, prepare_claude_cli_fingerprint_auth};
    let identity_seed = if is_kimi_messages_upstream(Some(auth), upstream_url) {
        claude_cli_auth_identity_seed(auth)
    } else {
        api_key.to_string()
    };
    let mut identity_auth = prepare_claude_cli_fingerprint_auth(auth, &identity_seed, synthesize).into_owned();
    let (updated, _) = apply_claude_credential_metadata(body, &mut identity_auth, session_id).map_err(|e| {
        let mut err = e.into_exec_error();
        err.message = format!("apply Claude credential metadata: {}", err.message);
        err
    })?;
    Ok(updated)
}

/// Validates a fully buffered upstream SSE body before it is translated (Go:
/// validateClaudeStreamingResponse): malformed data, error events, empty streams and streams
/// missing `message_start` or `message_delta` are 502s.
pub fn validate_claude_streaming_response(data: &[u8]) -> Result<(), ExecError> {
    let (mut has_data, mut has_message_start, mut has_message_delta) = (false, false, false);
    for line in data.split(|b| *b == b'\n') {
        let line = crate::helps::text::trim_space(line);
        if line.is_empty() || !line.starts_with(b"data:") {
            continue;
        }
        let payload = crate::helps::text::trim_space(&line[5..]);
        if payload.is_empty() || payload == b"[DONE]" {
            continue;
        }
        has_data = true;
        if !cpa_json::valid(payload) {
            return Err(status_err(502, "claude executor: upstream returned malformed stream data"));
        }
        let root = crate::helps::parse_cache::parse(payload);
        match root.g("type").str().as_str() {
            "error" => {
                let mut message = root.g("error.message").str().trim().to_string();
                if message.is_empty() {
                    message = root.g("error.type").str().trim().to_string();
                }
                if message.is_empty() {
                    message = "unknown upstream error".to_string();
                }
                return Err(status_err(502, format!("claude executor: upstream returned error event: {message}")));
            }
            "message_start" => {
                let message = root.g("message");
                if message.g("id").str().trim().is_empty() || message.g("model").str().trim().is_empty() {
                    return Err(status_err(502, "claude executor: upstream stream message_start is missing id or model"));
                }
                has_message_start = true;
            }
            "message_delta" => has_message_delta = true,
            _ => {}
        }
    }
    if !has_data {
        return Err(status_err(502, "claude executor: upstream returned empty stream response"));
    }
    if !has_message_start {
        return Err(status_err(502, "claude executor: upstream stream response is missing message_start"));
    }
    if !has_message_delta {
        return Err(status_err(502, "claude executor: upstream stream response ended before message completion"));
    }
    Ok(())
}

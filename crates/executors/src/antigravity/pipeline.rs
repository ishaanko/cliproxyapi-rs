//! The request pipeline shared by `Execute`, the Claude/Gemini-3 non-stream path and
//! `ExecuteStream` (Go: the identical prefix of antigravity_executor_execute.go and
//! antigravity_executor_stream.go), plus the shared error handling for upstream failures.

use std::sync::Arc;

use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::thinking::parse_suffix;
use cpa_runtime::conductor::{ANTIGRAVITY_CREDITS_METADATA_KEY, resolved_model_info};
use cpa_runtime::executor::{ExecError, Options, Request};
use cpa_translator::{Ctx, Format};
use serde_json::Value;

use super::AntigravityExecutor;
use super::credits::{
    Decision429Kind, cooling_disabled, credits_retry_enabled, decide_429, has_explicit_credits_balance_exhausted_reason,
    home_kv_unavailable_status_err, inject_enabled_credit_types, is_in_short_cooldown_required,
    mark_credits_permanently_disabled, mark_short_cooldown_required, new_status_err, should_bypass_short_cooldown,
};
use crate::helps::cloak_obfuscate::{SensitiveWordMatcher, obfuscate_sensitive_words_in_system_instruction};
use super::replay::{
    ReplayScope, clear_reasoning_replay_on_invalid_signature, prepare_gemini_reasoning_replay_payload,
};
use super::request::{BuiltRequest, build_request, resolve_request_base_url};
use super::signature::{ensure_boundary_user_content, sanitize_gemini_request_signatures, validate_request_signatures};
use super::transport::close_auth_idle_transports;
use crate::helps::payload::{PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model};
use crate::helps::session::derived_antigravity_session_id;
use crate::helps::thinking::apply_request_thinking;
use crate::helps::translate::{RequestTranslation, translate_request};
use crate::helps::usage::UsageReporter;

/// Which of the three upstream call shapes is being prepared.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Real `generateContent`.
    NonStream,
    /// Non-stream result built by aggregating `streamGenerateContent` (Claude, gemini-3-pro, image).
    AggregatedStream,
    /// Client-facing stream.
    Stream,
}

impl Mode {
    fn upstream_stream(self) -> bool {
        self != Mode::NonStream
    }
}

/// Everything the send/parse half needs.
pub(crate) struct Prepared {
    pub cfg: Arc<Config>,
    pub base_model: String,
    pub auth: Auth,
    pub from: Format,
    pub response_format: Format,
    /// Validated original (pre-translation) payload.
    pub original_payload: Vec<u8>,
    /// Translated envelope before credits, replay and boundary turns (what translators read back).
    pub translated: Vec<u8>,
    /// Payload the replay accumulator records against.
    pub request_payload: Vec<u8>,
    pub replay_scope: ReplayScope,
    pub use_credits: bool,
    pub reporter: UsageReporter,
    pub client: reqwest::Client,
    pub built: BuiltRequest,
}

pub(crate) fn credits_requested(opts: &Options) -> bool {
    matches!(opts.metadata.get(ANTIGRAVITY_CREDITS_METADATA_KEY), Some(Value::Bool(true)))
}

/// Not-sent failures do not count as upstream attempts for the conductor.
pub(crate) fn pre_send(mut err: ExecError) -> ExecError {
    err.upstream_attempted = false;
    err
}

/// Base model without the `(budget)` thinking suffix.
pub(crate) fn base_model_of(model: &str) -> String {
    parse_suffix(model).model_name
}

impl AntigravityExecutor {
    /// Short-cooldown precheck: an auth that hit a short rate limit answers 429 with the
    /// remaining time so the conductor switches credentials (skipped for the credits fallback).
    /// A Home KV failure reads as `503 home kv store unavailable`.
    pub(crate) async fn check_short_cooldown(
        &self,
        cfg: &Config,
        auth: &Auth,
        base_model: &str,
        opts: &Options,
    ) -> Result<(), ExecError> {
        if cooling_disabled(auth, Some(cfg)) {
            return Ok(());
        }
        let remaining = is_in_short_cooldown_required(auth, base_model)
            .await
            .map_err(|e| pre_send(home_kv_unavailable_status_err(Some(&e))))?;
        if let Some(remaining) = remaining
            && !should_bypass_short_cooldown(credits_requested(opts), cfg)
        {
            tracing::debug!(
                "antigravity executor: auth {} in short cooldown for model {base_model} ({remaining:?} remaining), returning 429 to switch auth",
                auth.id
            );
            let mut err = ExecError::new(429, format!("auth in short cooldown, {remaining:?} remaining"));
            err.retry_after = Some(remaining);
            return Err(pre_send(err));
        }
        Ok(())
    }

    pub(crate) fn obfuscate_sensitive_words(cfg: &Config, payload: Vec<u8>) -> Vec<u8> {
        if cfg.antigravity.sensitive_words.is_empty() {
            return payload;
        }
        match SensitiveWordMatcher::new(&cfg.antigravity.sensitive_words) {
            Some(matcher) => obfuscate_sensitive_words_in_system_instruction(&payload, Some(&matcher)),
            None => payload,
        }
    }

    /// Validates, translates and shapes the request, then builds the upstream call.
    pub(crate) async fn prepare(
        &self,
        cfg: Arc<Config>,
        auth: &Auth,
        req: &mut Request,
        opts: &Options,
        mode: Mode,
    ) -> Result<Prepared, ExecError> {
        let base_model = base_model_of(&req.model);
        let reporter = UsageReporter::new("antigravity", "AntigravityExecutor", &base_model, Some(auth), Some(opts));
        let result = self.prepare_inner(cfg, auth, req, opts, mode, &base_model, &reporter).await;
        match result {
            Ok(p) => Ok(p),
            Err(err) => {
                let err = pre_send(err);
                reporter.publish_failure(&err);
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_inner(
        &self,
        cfg: Arc<Config>,
        auth: &Auth,
        req: &mut Request,
        opts: &Options,
        mode: Mode,
        base_model: &str,
        reporter: &UsageReporter,
    ) -> Result<Prepared, ExecError> {
        let from = opts.source_format;
        let response_format = opts.response_format_or_source();
        let to = Format::Antigravity;

        let original_source: &[u8] = if opts.original_request.is_empty() { &req.payload } else { &opts.original_request };
        let original_payload = validate_request_signatures(base_model, from, original_source.to_vec());
        req.payload = original_payload.clone().into();

        let (token, updated_auth) = self.ensure_access_token(&cfg, auth).await?;
        let auth = match updated_auth {
            Some(a) => {
                reporter.update_access_token_fingerprint(&a);
                a
            }
            None => auth.clone(),
        };

        let model_info = resolved_model_info(req).map(|r| r.info);
        let mut translation = RequestTranslation::new(&opts.headers, Some(&cfg), from, to, base_model, mode.upstream_stream());
        translation.ctx = Ctx { alt: Some(opts.alt.clone()) };
        translation.envelope.model_info = model_info;
        let original_translated = translate_request(&translation, &original_payload).0;
        let mut translated = original_translated.clone();

        translated = apply_request_thinking(&translated, req, opts, from.as_str(), to.as_str(), "antigravity", false)
            .map_err(|e| ExecError::new(e.status_code(), e.message))?;

        let requested_model = payload_requested_model(opts, &req.model);
        let request_path = payload_request_path(opts);
        translated = apply_payload_config(
            &PayloadRequest {
                cfg: Some(&cfg),
                target_executor: "",
                model: base_model,
                protocol: "antigravity",
                from_protocol: from.as_str(),
                root: "request",
                requested_model: &requested_model,
                request_path: &request_path,
                headers: Some(&opts.headers),
            },
            &translated,
            &original_translated,
        );
        translated = Self::obfuscate_sensitive_words(&cfg, translated);
        translated = sanitize_gemini_request_signatures(base_model, translated);
        if mode == Mode::Stream {
            let mut v = cpa_json::parse(&translated);
            cpa_json::delete(&mut v, "request.stream");
            translated = cpa_json::to_vec(&v);
        }
        reporter.set_translated_reasoning_effort(&translated, to.as_str());

        let use_credits = credits_requested(opts) && credits_retry_enabled(&cfg);
        let base_url = resolve_request_base_url(&auth);
        let client = self.client(&cfg, &auth, &opts.proxy_url);

        // Credential retry rounds belong to the conductor: one upstream request per credential.
        let mut request_payload = translated.clone();
        if use_credits && let Some(cp) = inject_enabled_credit_types(&translated) {
            request_payload = cp;
        }
        let mut replay_scope = ReplayScope::default();
        if super::signature::uses_reasoning_replay_cache(base_model) {
            let (payload, scope) = prepare_gemini_reasoning_replay_payload(base_model, req, opts, request_payload)?;
            request_payload = payload;
            replay_scope = scope;
        }
        request_payload = ensure_boundary_user_content(base_model, request_payload);

        let derived = derived_antigravity_session_id(&[&opts.metadata, &req.metadata]);
        let derived: Vec<String> = if derived.is_empty() { Vec::new() } else { vec![derived] };
        let built = build_request(
            &auth,
            &token,
            base_model,
            &request_payload,
            mode.upstream_stream(),
            &opts.alt,
            &base_url,
            &derived,
        )?;

        Ok(Prepared {
            cfg,
            base_model: base_model.to_string(),
            auth,
            from,
            response_format,
            original_payload,
            translated,
            request_payload,
            replay_scope,
            use_credits,
            reporter: reporter.clone(),
            client,
            built,
        })
    }

    /// Sends the built request. Transport failures carry no status.
    pub(crate) async fn send(&self, p: &Prepared) -> Result<reqwest::Response, ExecError> {
        p.reporter.start_response_ttft();
        p.client
            .post(&p.built.url)
            .headers(p.built.headers.clone())
            .body(p.built.body.clone())
            .send()
            .await
            .map_err(|e| crate::helps::status::transport_error(&e))
    }

    /// Non-2xx handling common to every path: 429 cooldown and credits bookkeeping, replay
    /// invalidation on signature errors, then the typed status error.
    pub(crate) async fn handle_upstream_error(&self, p: &Prepared, status: u16, body: &[u8]) -> ExecError {
        if status == 429 {
            let decision = decide_429(body);
            match decision.kind {
                Decision429Kind::ShortCooldownSwitchAuth => {
                    close_auth_idle_transports(&p.auth);
                    if let Some(d) = decision.retry_after
                        && !d.is_zero()
                        && !cooling_disabled(&p.auth, Some(&p.cfg))
                    {
                        if let Err(e) = mark_short_cooldown_required(&p.auth, &p.base_model, d).await {
                            return home_kv_unavailable_status_err(Some(&e));
                        }
                        tracing::debug!(
                            "antigravity executor: short quota cooldown ({d:?}) for model {}, recorded cooldown",
                            p.base_model
                        );
                    }
                }
                Decision429Kind::FullQuotaExhausted => {
                    close_auth_idle_transports(&p.auth);
                    if p.use_credits
                        && has_explicit_credits_balance_exhausted_reason(body)
                        && !cooling_disabled(&p.auth, Some(&p.cfg))
                    {
                        mark_credits_permanently_disabled(&p.auth).await;
                    }
                }
                _ => {}
            }
        }
        tracing::debug!(
            "antigravity executor: upstream error status: {status}, body: {}",
            crate::helps::logging::summarize_error_body("", body)
        );
        clear_reasoning_replay_on_invalid_signature(&p.replay_scope, status, body);
        new_status_err(status, body)
    }
}

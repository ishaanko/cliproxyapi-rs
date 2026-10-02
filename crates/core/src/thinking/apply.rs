//! `apply_thinking`: the unified entry point (Go: internal/thinking/apply.go).
//!
//! Pipeline: suffix parse and model lookup -> provider applier lookup -> capability check ->
//! config extraction (suffix wins over body) -> validation -> provider-specific application ->
//! summary visibility restore.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use parking_lot::RwLock;
use tracing::{debug, warn};

use super::configuration_update::{
    extract_configuration_update_config, is_responses_format, strip_configuration_updates,
    strip_responses_effort,
};
use super::convert::convert_level_to_budget;
use super::extract::{
    extract_codex_usage_config, extract_source_thinking_config, extract_thinking_config,
    parse_suffix_to_config,
};
use super::json::parse_valid;
use super::parse_suffix;
use super::provider::{
    AntigravityApplier, ClaudeApplier, CodexApplier, GeminiApplier, InteractionsApplier,
    KimiApplier, OpenAIApplier, XaiApplier,
};
use super::strip::strip_thinking_config;
use super::summary::{
    SummaryConfig, SummaryMode, apply_summary_config_for_provider, extract_summary_config,
    strip_inferred_claude_summary_activation,
};
use super::types::{
    ProviderApplier, SuffixResult, ThinkingConfig, ThinkingError, ThinkingMode, level,
};
use super::validate::{
    is_budget_capable_provider, is_level_supported, is_same_provider_family, validate_config,
};
use crate::registry::{ModelInfo, lookup_model_info};

type Appliers = HashMap<String, Arc<dyn ProviderApplier>>;

/// Built-in appliers by provider name; the Go port registers these from package `init`s, here they
/// are present from the start.
static APPLIERS: LazyLock<RwLock<Appliers>> = LazyLock::new(|| {
    let kimi: Arc<dyn ProviderApplier> = Arc::new(KimiApplier);
    let mut m: Appliers = HashMap::new();
    m.insert("gemini".into(), Arc::new(GeminiApplier));
    m.insert("claude".into(), Arc::new(ClaudeApplier));
    m.insert("openai".into(), Arc::new(OpenAIApplier));
    m.insert("codex".into(), Arc::new(CodexApplier));
    m.insert("antigravity".into(), Arc::new(AntigravityApplier));
    m.insert("xai".into(), Arc::new(XaiApplier::default()));
    m.insert("interactions".into(), Arc::new(InteractionsApplier));
    for name in ["kimi", "kimi-ai", "kimi.ai", "kimi.com"] {
        m.insert(name.into(), kimi.clone());
    }
    RwLock::new(m)
});

fn normalized_provider_name(provider: &str) -> String {
    provider.trim().to_lowercase()
}

/// The applier registered for `provider` (trimmed, lowercased), if any.
pub fn get_provider_applier(provider: &str) -> Option<Arc<dyn ProviderApplier>> {
    let name = normalized_provider_name(provider);
    if name.is_empty() {
        return None;
    }
    APPLIERS.read().get(&name).cloned()
}

/// Registers (or replaces) a provider applier by name.
pub fn register_provider(name: &str, applier: Arc<dyn ProviderApplier>) {
    let name = normalized_provider_name(name);
    if name.is_empty() {
        return;
    }
    APPLIERS.write().insert(name, applier);
}

/// Whether the model's thinking config passes through without validation. User-defined models come
/// from the config file's `models[]` arrays (marked `user_defined` at registration); unknown models
/// (no model info) are treated the same so upstream can validate.
pub fn is_user_defined_model(model_info: Option<&ModelInfo>) -> bool {
    model_info.is_none_or(|m| m.user_defined)
}

/// Everything one run of the pipeline needs.
struct Request<'a> {
    body: &'a [u8],
    /// Original source request when translation already changed the target protocol.
    source_body: &'a [u8],
    model: &'a str,
    from: &'a str,
    to: &'a str,
    provider_key: &'a str,
    /// Exact model definition bound to an API-key attempt (only meaningful when
    /// `model_info_resolved`).
    resolved: Option<&'a ModelInfo>,
    model_info_resolved: bool,
    summary: SummaryConfig,
    updates_changed: bool,
}

/// Applies thinking configuration to a request body.
///
/// `model` may carry a suffix (`gemini-2.5-pro(8192)`) that overrides body settings. `from` is the
/// source request format, `to` the target provider format (gemini, antigravity, claude, openai,
/// codex, kimi, xai, interactions), `provider_key` the provider used for registry lookups (may
/// differ from `to`, e.g. openrouter -> openai).
///
/// Passes the body through unchanged for unknown providers and for models without thinking
/// support; unknown models count as user-defined. Validation failures return the `ThinkingError`
/// (HTTP 400).
pub fn apply_thinking(
    body: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider_key: &str,
) -> Result<Vec<u8>, ThinkingError> {
    let summary = extract_summary_config(body, to);
    apply_inner(Request {
        body,
        source_body: &[],
        model,
        from,
        to,
        provider_key,
        resolved: None,
        model_info_resolved: false,
        summary,
        updates_changed: false,
    })
}

/// [`apply_thinking`] preserving summary visibility extracted from the original source request.
/// Callers that translate before applying thinking must pass the source config explicitly: a target
/// Claude body can temporarily lack display while disabled thinking is rewritten by a suffix.
pub fn apply_thinking_with_summary(
    body: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider_key: &str,
    summary: &SummaryConfig,
) -> Result<Vec<u8>, ThinkingError> {
    apply_inner(Request {
        body,
        source_body: &[],
        model,
        from,
        to,
        provider_key,
        resolved: None,
        model_info_resolved: false,
        summary: summary.clone(),
        updates_changed: false,
    })
}

/// Applies thinking using the original source request when translation has already changed the
/// target protocol. Without a bound model definition, an unknown model has no configuration
/// update support. `normalized_updates_changed` says a plugin normalizer rewrote the target's
/// `configuration_update` items.
#[allow(clippy::too_many_arguments)]
pub fn apply_thinking_with_source_and_summary(
    body: &[u8],
    source_body: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider_key: &str,
    summary: &SummaryConfig,
    normalized_updates_changed: bool,
) -> Result<Vec<u8>, ThinkingError> {
    apply_inner(Request {
        body,
        source_body,
        model,
        from,
        to,
        provider_key,
        resolved: None,
        model_info_resolved: false,
        summary: summary.clone(),
        updates_changed: normalized_updates_changed,
    })
}

/// Applies thinking with the exact configured model definition selected for an API-key execution
/// attempt, preserving summary visibility from the original source body. `model_info == None`
/// means the bound model is unknown (treated as user-defined).
pub fn apply_thinking_with_model_info(
    body: &[u8],
    source_body: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider_key: &str,
    model_info: Option<&ModelInfo>,
) -> Result<Vec<u8>, ThinkingError> {
    let summary = if source_body.is_empty() {
        extract_summary_config(body, to)
    } else {
        extract_summary_config(source_body, from)
    };
    apply_thinking_with_model_info_and_summary(
        body,
        source_body,
        model,
        from,
        to,
        provider_key,
        model_info,
        &summary,
        false,
    )
}

/// [`apply_thinking_with_model_info`] with a summary intent already resolved across source
/// translation and plugin normalization.
#[allow(clippy::too_many_arguments)]
pub fn apply_thinking_with_model_info_and_summary(
    body: &[u8],
    source_body: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider_key: &str,
    model_info: Option<&ModelInfo>,
    summary: &SummaryConfig,
    normalized_updates_changed: bool,
) -> Result<Vec<u8>, ThinkingError> {
    apply_inner(Request {
        body,
        source_body,
        model,
        from,
        to,
        provider_key,
        resolved: model_info,
        model_info_resolved: true,
        summary: summary.clone(),
        updates_changed: normalized_updates_changed,
    })
}

fn apply_inner(req: Request<'_>) -> Result<Vec<u8>, ThinkingError> {
    let mut provider_format = req.to.trim().to_lowercase();
    if provider_format == "openai-response" {
        provider_format = "codex".into();
    }
    let mut provider_key = req.provider_key.trim().to_lowercase();
    if provider_key.is_empty() {
        provider_key = provider_format.clone();
    }
    let mut from = req.from.trim().to_lowercase();
    if from.is_empty() {
        from = provider_format.clone();
    }
    let provider_format = provider_format.as_str();
    let provider_key = provider_key.as_str();
    let from = from.as_str();

    // 1. Suffix and model info (provider-specific lookup handles per-provider capability
    // differences).
    let suffix = parse_suffix(req.model);
    let base_model = suffix.model_name.as_str();
    let looked_up;
    let model_info: Option<&ModelInfo> = if req.model_info_resolved {
        req.resolved
    } else {
        looked_up = lookup_model_info(base_model, Some(provider_key));
        looked_up.as_ref()
    };

    // Resolve source intent before stripping unsupported target input items.
    let mut body = req.body.to_vec();
    let response_target = provider_format == "codex" || provider_format == "xai";
    let mut source_config = ThinkingConfig::default();
    if is_responses_format(from) {
        let source_request: &[u8] = if !req.updates_changed && !req.source_body.is_empty() {
            req.source_body
        } else {
            &body
        };
        if !req.updates_changed || response_target {
            source_config = extract_codex_usage_config(source_request);
        }
    }
    let supports_updates = model_info.is_some_and(|m| m.support_configuration_update);
    if response_target && !supports_updates {
        body = strip_configuration_updates(&body);
    }
    let native_responses = response_target && is_responses_format(from) && supports_updates;

    // 2. Route check, after target cleanup.
    let Some(applier) = get_provider_applier(provider_format) else {
        debug!(
            provider = provider_format,
            model = req.model,
            "thinking: unknown provider, passthrough |"
        );
        return Ok(body);
    };

    // 3. Model capability check. Unknown models count as user-defined so the config is still
    // applied and the upstream validates it.
    if !suffix.has_suffix
        && !req.source_body.is_empty()
        && is_responses_format(from)
        && parse_valid(&body).is_none()
        && extract_configuration_update_config(req.source_body).has_config()
    {
        // Do not rebuild a malformed target from a separate source update.
        return Ok(body);
    }
    if native_responses && !suffix.has_suffix {
        // Native Responses keeps the top-level baseline and in-turn updates as-is.
        return Ok(body);
    }
    let info = match model_info {
        Some(info) if !info.user_defined => info,
        _ => {
            return apply_user_defined_model(
                body,
                model_info,
                from,
                provider_format,
                provider_key,
                &suffix,
                source_config,
                native_responses,
                &req.summary,
                applier.as_ref(),
            );
        }
    };
    if info.thinking.is_none() {
        let config = extract_thinking_config(&body, provider_format);
        if config.has_config() || req.summary.mode != SummaryMode::Unspecified {
            debug!(
                model = base_model,
                provider = provider_format,
                "thinking: model does not support thinking, stripping config |"
            );
            return Ok(if response_target {
                strip_responses_effort(&body)
            } else {
                strip_thinking_config(&body, provider_format)
            });
        }
        debug!(
            model = base_model,
            provider = provider_format,
            "thinking: model does not support thinking, passthrough |"
        );
        return Ok(body);
    }

    // 4. Config: suffix priority over body.
    let mut config;
    if suffix.has_suffix {
        config = parse_suffix_to_config(&suffix.raw_suffix);
    } else {
        config = source_config;
        if !config.has_config()
            && !req.updates_changed
            && req.model_info_resolved
            && !req.source_body.is_empty()
        {
            config = extract_source_thinking_config(req.source_body, from);
        }
        if !config.has_config() {
            config = extract_thinking_config(&body, provider_format);
        }
    }

    if !config.has_config() {
        debug!(
            provider = provider_format,
            model = info.id.as_str(),
            "thinking: no config found, passthrough |"
        );
        if native_responses {
            return Ok(body);
        }
        if req.model_info_resolved
            && provider_format == "claude"
            && from != provider_format
            && extract_summary_config(req.source_body, from).mode == SummaryMode::Enabled
        {
            // Registry translation only sees aggregate model capabilities. For a cross-protocol
            // summary-only request it may have activated adaptive thinking solely to make display
            // valid. The selected API-key model is authoritative at execution time, so discard that
            // inferred activation when the exact model supports only manual extended thinking. The
            // source intent is used even if a target normalizer removed display; the inferred
            // amount must then disappear with it.
            body = strip_inferred_claude_summary_activation(body, Some(info));
        }
        return Ok(apply_summary_config_for_provider(
            body,
            provider_format,
            base_model,
            provider_key,
            Some(info),
            &req.summary,
        ));
    }
    if req.model_info_resolved
        && config.mode == ThinkingMode::Level
        && info.thinking.is_some()
        && should_map_configured_high_intent(from, provider_format, Some(info))
    {
        config.level = map_configured_high_intent(&config.level, Some(info));
    }

    // 5. Validate and normalize. Unsupported update items were already removed above.
    let validated = validate_config(&config, Some(info), from, provider_format, suffix.has_suffix).inspect_err(|e| {
        warn!(provider = provider_format, model = info.id.as_str(), error = %e, "thinking: validation failed |");
    })?;
    debug!(provider = provider_format, model = info.id.as_str(), mode = %validated.mode, budget = validated.budget, level = validated.level.as_str(), "thinking: processed config to apply |");

    // 6. Apply with the provider applier, then restore the target summary intent that was explicit
    // before suffix processing.
    let applied = applier.apply(&body, &validated, Some(info))?;
    // A fully disabled amount takes precedence over visibility: re-applying a summary-only field
    // could recreate a removed provider config and make a default-on model think again.
    if validated.is_fully_disabled() || native_responses {
        return Ok(applied);
    }
    Ok(apply_summary_config_for_provider(
        applied,
        provider_format,
        base_model,
        provider_key,
        Some(info),
        &req.summary,
    ))
}

/// Whether a configured xhigh/max intent should be remapped onto the exact model's levels.
fn should_map_configured_high_intent(from: &str, to: &str, model_info: Option<&ModelInfo>) -> bool {
    let from = from.trim().to_lowercase();
    let to = to.trim().to_lowercase();
    if from != to {
        return true;
    }
    let Some(info) = model_info else {
        return false;
    };
    let model_type = info.r#type.trim().to_lowercase();
    !model_type.is_empty() && !is_same_provider_family(&to, &model_type)
}

/// `xhigh` picks the first supported of [xhigh, max, high], `max` of [max, xhigh, high].
fn map_configured_high_intent(lvl: &str, model_info: Option<&ModelInfo>) -> String {
    let Some(support) = model_info
        .and_then(|m| m.thinking.as_ref())
        .filter(|t| !t.levels.is_empty())
    else {
        return lvl.to_owned();
    };
    let lvl = lvl.trim().to_lowercase();
    let candidates: [&str; 3] = match lvl.as_str() {
        level::XHIGH => [level::XHIGH, level::MAX, level::HIGH],
        level::MAX => [level::MAX, level::XHIGH, level::HIGH],
        _ => return lvl,
    };
    for candidate in candidates {
        if is_level_supported(candidate, &support.levels) {
            return candidate.to_owned();
        }
    }
    lvl
}

/// User-defined (or unknown) models: no capability validation; the config is applied directly and
/// the upstream validates it.
#[allow(clippy::too_many_arguments)]
fn apply_user_defined_model(
    body: Vec<u8>,
    model_info: Option<&ModelInfo>,
    from: &str,
    to: &str,
    provider_key: &str,
    suffix: &SuffixResult,
    source_config: ThinkingConfig,
    native_responses: bool,
    summary: &SummaryConfig,
    applier: &dyn ProviderApplier,
) -> Result<Vec<u8>, ThinkingError> {
    let model_id = model_info.map_or(suffix.model_name.as_str(), |m| m.id.as_str());

    // Config: suffix priority over body.
    let mut config;
    if suffix.has_suffix {
        config = parse_suffix_to_config(&suffix.raw_suffix);
    } else {
        config = source_config;
        if !config.has_config() {
            config = extract_thinking_config(&body, from);
        }
        if !config.has_config() && from != to {
            config = extract_thinking_config(&body, to);
        }
    }

    if !config.has_config() {
        debug!(
            model = model_id,
            provider = to,
            "thinking: user-defined model, passthrough (no config) |"
        );
        return Ok(apply_summary_config_for_provider(
            body,
            to,
            model_id,
            provider_key,
            model_info,
            summary,
        ));
    }

    let config = normalize_user_defined_config(config, to);
    debug!(provider = to, model = model_id, mode = %config.mode, budget = config.budget, level = config.level.as_str(), "thinking: processed config to apply |");
    let applied = applier.apply(&body, &config, model_info)?;
    if config.is_fully_disabled() || native_responses {
        return Ok(applied);
    }
    Ok(apply_summary_config_for_provider(
        applied,
        to,
        model_id,
        provider_key,
        model_info,
        summary,
    ))
}

/// Level -> budget for budget-capable targets other than Claude (Claude keeps adaptive levels).
fn normalize_user_defined_config(mut config: ThinkingConfig, to: &str) -> ThinkingConfig {
    if config.mode != ThinkingMode::Level || to == "claude" || !is_budget_capable_provider(to) {
        return config;
    }
    let Some(budget) = convert_level_to_budget(&config.level) else {
        return config;
    };
    config.mode = ThinkingMode::Budget;
    config.budget = budget;
    config.level.clear();
    config
}

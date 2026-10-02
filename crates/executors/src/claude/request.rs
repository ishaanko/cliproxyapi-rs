//! Request-side helpers (Go: claude_executor_request.go): Anthropic-Beta assembly, model
//! predicates, upstream error classification and the Messages/count_tokens header builder.
//!
//! The body edits, tool-name remap and response decoding that live in the same Go file are in
//! sibling modules (`body`, `tool_remap`) or handled by reqwest.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::SystemTime;

use cpa_auth::Auth;
use cpa_auth::types::AUTH_KIND_API_KEY;
use cpa_config::Config;
use cpa_json::J;
use cpa_runtime::executor::{ErrorCode, ExecError};
use http::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use url::Url;

use super::helps::client_detection::*;
use super::helps::device_profile::*;
use super::helps::diagnostics::*;
use super::helps::ratelimit::*;
use super::helps::upstream::is_anthropic_upstream_url;
use super::policy::{resolve_claude_fingerprint_policy, resolve_claude_wire_policy};
use crate::helps::status::status_err;

pub const CLAUDE_TOKEN_COUNTING_BETA: &str = "token-counting-2024-11-01";
pub const CLAUDE_FAST_MODE_BETA: &str = "fast-mode-2026-02-01";
pub const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
pub const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
pub const CLAUDE_CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";
pub const CLAUDE_MID_CONV_SYSTEM_BETA: &str = "mid-conversation-system-2026-04-07";
pub const CLAUDE_PER_TURN_CONTROL_BETA: &str = "per-turn-control-2026-07-01";
pub const CLAUDE_PER_TURN_TIMING_BETA: &str = "timing-2026-09-09";
pub const CLAUDE_MID_CONV_TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-2026-07-01";
pub const CLAUDE_INLINE_TOOLS_BETA: &str = "inline-tools-2026-09-15";
pub const CLAUDE_MID_CONV_SYSTEM_CLEAR_AT_BETA: &str = "mid-conversation-system-clear-at-2026-08-21";
pub const CLAUDE_DANGEROUS_TOOL_USE_BETA: &str = "dangerous-tool-use-2026-09-03";
pub const CLAUDE_ADVISOR_TOOL_BETA: &str = "advisor-tool-2026-03-01";
pub const CLAUDE_ADVANCED_TOOL_USE_BETA: &str = "advanced-tool-use-2025-11-20";
pub const CLAUDE_EFFORT_BETA: &str = "effort-2025-11-24";
pub const CLAUDE_SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-06-01";
pub const CLAUDE_FALLBACK_CREDIT_BETA: &str = "fallback-credit-2026-06-01";
pub const CLAUDE_STRUCTURED_OUTPUTS_BETA: &str = "structured-outputs-2025-12-15";
pub const CLAUDE_THINKING_DISPLAY_UPDATES_BETA: &str = "thinking-display-updates-2026-08-18";
pub const CLAUDE_THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
pub const CLAUDE_THINKING_RESUMPTION_BETA: &str = "thinking-resumption-2026-07-17";
pub const CLAUDE_EXTENDED_CACHE_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";
pub const CLAUDE_PROMPT_CACHING_EVICT_BETA: &str = "prompt-caching-evict-2026-05-12";
pub const CLAUDE_CACHE_DIAGNOSIS_BETA: &str = "cache-diagnosis-2026-04-07";
pub const CLAUDE_REDACT_THINKING_BETA: &str = "redact-thinking-2026-02-12";
pub const CLAUDE_AFK_MODE_BETA: &str = "afk-mode-2026-01-31";

/// Betas Claude Code sends on every `cli` Messages request after `claude-code-20250219`.
const CLAUDE_CODE_CLI_CONSTANT_BETAS: [&str; 5] = [
    "interleaved-thinking-2025-05-14",
    CLAUDE_REDACT_THINKING_BETA,
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
];

/// Caller-supplied betas real Claude Code emits after effort, in that relative order.
const CLAUDE_CODE_TRAILING_BETAS: [&str; 3] = [
    CLAUDE_SERVER_SIDE_FALLBACK_BETA,
    CLAUDE_FALLBACK_CREDIT_BETA,
    CLAUDE_STRUCTURED_OUTPUTS_BETA,
];

/// Every beta the proxy assembles or gates; caller betas outside this set are forwarded verbatim.
static CLAUDE_MANAGED_BETA_SET: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut set: HashSet<&'static str> = [
        CLAUDE_TOKEN_COUNTING_BETA,
        CLAUDE_FAST_MODE_BETA,
        CLAUDE_OAUTH_BETA,
        CLAUDE_CODE_BETA,
        CLAUDE_CONTEXT_1M_BETA,
        CLAUDE_MID_CONV_SYSTEM_BETA,
        CLAUDE_PER_TURN_CONTROL_BETA,
        CLAUDE_PER_TURN_TIMING_BETA,
        CLAUDE_MID_CONV_TOOL_CHANGES_BETA,
        CLAUDE_INLINE_TOOLS_BETA,
        CLAUDE_MID_CONV_SYSTEM_CLEAR_AT_BETA,
        CLAUDE_DANGEROUS_TOOL_USE_BETA,
        CLAUDE_ADVISOR_TOOL_BETA,
        CLAUDE_ADVANCED_TOOL_USE_BETA,
        CLAUDE_EFFORT_BETA,
        CLAUDE_SERVER_SIDE_FALLBACK_BETA,
        CLAUDE_FALLBACK_CREDIT_BETA,
        CLAUDE_STRUCTURED_OUTPUTS_BETA,
        CLAUDE_THINKING_DISPLAY_UPDATES_BETA,
        CLAUDE_THINKING_BINDING_BETA,
        CLAUDE_THINKING_RESUMPTION_BETA,
        CLAUDE_EXTENDED_CACHE_TTL_BETA,
        CLAUDE_PROMPT_CACHING_EVICT_BETA,
        CLAUDE_CACHE_DIAGNOSIS_BETA,
        CLAUDE_REDACT_THINKING_BETA,
        CLAUDE_AFK_MODE_BETA,
    ]
    .into_iter()
    .collect();
    set.extend(CLAUDE_CODE_CLI_CONSTANT_BETAS);
    set.extend(CLAUDE_CODE_TRAILING_BETAS);
    set
});

/// Go: isManagedClaudeBeta.
pub fn is_managed_claude_beta(beta: &str) -> bool {
    CLAUDE_MANAGED_BETA_SET.contains(beta.trim())
}

/// The set of betas the caller asked for (Go: `map[string]bool`).
pub type RequestedBetas = HashSet<String>;

fn requested(set: &RequestedBetas, beta: &str) -> bool {
    set.contains(beta)
}

/// Assembles the Anthropic-Beta baseline the way Claude Code 2.1.280 does (Go: claudeCodeCLIBetas).
/// The order of the 29 slots is the wire order; see the Go spec comment.
pub fn claude_code_cli_betas(body: &[u8], requested_set: &RequestedBetas, oauth_token: bool) -> String {
    let root = cpa_json::parse(body);
    let mut betas: Vec<&str> = Vec::with_capacity(20);
    betas.push(CLAUDE_CODE_BETA);
    if oauth_token {
        betas.push(CLAUDE_OAUTH_BETA);
    }
    if requested(requested_set, CLAUDE_CONTEXT_1M_BETA) {
        betas.push(CLAUDE_CONTEXT_1M_BETA);
    }
    let redact_thinking = !claude_thinking_display_set(&root);
    for beta in CLAUDE_CODE_CLI_CONSTANT_BETAS {
        if beta == CLAUDE_REDACT_THINKING_BETA && !redact_thinking {
            continue;
        }
        betas.push(beta);
    }
    let model = root.g("model").str();
    let legacy = super::cloaking::claude_uses_legacy_system_reminder(&root);
    if !legacy {
        betas.push(CLAUDE_MID_CONV_SYSTEM_BETA);
        if claude_include_per_turn_control(&root, requested_set) {
            betas.push(CLAUDE_PER_TURN_CONTROL_BETA);
        }
        if claude_include_per_turn_timing(&root, requested_set) {
            betas.push(CLAUDE_PER_TURN_TIMING_BETA);
        }
        if !is_claude_sonnet5_model(&model) {
            betas.push(CLAUDE_MID_CONV_TOOL_CHANGES_BETA);
        }
        if claude_include_inline_tools(&root, requested_set) {
            betas.push(CLAUDE_INLINE_TOOLS_BETA);
        }
    } else {
        // Legacy models have no mid-conversation slot; a caller still naming these betas keeps them.
        if claude_include_per_turn_control(&root, requested_set) {
            betas.push(CLAUDE_PER_TURN_CONTROL_BETA);
        }
        if claude_include_per_turn_timing(&root, requested_set) {
            betas.push(CLAUDE_PER_TURN_TIMING_BETA);
        }
    }
    if requested(requested_set, CLAUDE_ADVISOR_TOOL_BETA) || claude_body_has_advisor_tool(&root) {
        betas.push(CLAUDE_ADVISOR_TOOL_BETA);
    }
    if requested(requested_set, CLAUDE_ADVANCED_TOOL_USE_BETA) || claude_body_uses_advanced_tool_use(&root) {
        betas.push(CLAUDE_ADVANCED_TOOL_USE_BETA);
    }
    if !legacy && claude_include_mid_conv_clear_at(&root, requested_set) {
        betas.push(CLAUDE_MID_CONV_SYSTEM_CLEAR_AT_BETA);
    }
    if requested(requested_set, CLAUDE_DANGEROUS_TOOL_USE_BETA) || root.g("safeguards").exists() {
        betas.push(CLAUDE_DANGEROUS_TOOL_USE_BETA);
    }
    if claude_request_supports_effort_value(&root, body.is_empty(), requested_set) {
        betas.push(CLAUDE_EFFORT_BETA);
    }
    let is_probe_or_helper = is_claude_probe_or_helper_request(body);
    if !is_probe_or_helper
        && (requested(requested_set, CLAUDE_SERVER_SIDE_FALLBACK_BETA) || root.g("fallbacks").exists())
    {
        betas.push(CLAUDE_SERVER_SIDE_FALLBACK_BETA);
    }
    let include_fallback_credit = requested(requested_set, CLAUDE_FALLBACK_CREDIT_BETA)
        || root.g("fallback_credit_token").exists()
        || (oauth_token && root.g("fallbacks").exists());
    if include_fallback_credit {
        betas.push(CLAUDE_FALLBACK_CREDIT_BETA);
    }
    for beta in CLAUDE_CODE_TRAILING_BETAS {
        if beta == CLAUDE_SERVER_SIDE_FALLBACK_BETA || beta == CLAUDE_FALLBACK_CREDIT_BETA {
            continue;
        }
        if requested(requested_set, beta) {
            betas.push(beta);
        }
    }
    let thinking_type = root.g("thinking.type").str();
    if requested(requested_set, CLAUDE_THINKING_BINDING_BETA)
        || root.g("thinking.block_binding").exists()
        || (claude_model_uses_progress_display(&model) && thinking_type == "adaptive")
    {
        betas.push(CLAUDE_THINKING_BINDING_BETA);
    }
    if !is_probe_or_helper
        && thinking_type != "disabled"
        && (requested(requested_set, CLAUDE_THINKING_DISPLAY_UPDATES_BETA) || claude_thinking_display_updates(&root))
    {
        betas.push(CLAUDE_THINKING_DISPLAY_UPDATES_BETA);
    }
    if requested(requested_set, CLAUDE_THINKING_RESUMPTION_BETA) {
        betas.push(CLAUDE_THINKING_RESUMPTION_BETA);
    }
    if claude_request_uses_fast_mode_value(&root, requested_set) {
        betas.push(CLAUDE_FAST_MODE_BETA);
    }
    if requested(requested_set, CLAUDE_AFK_MODE_BETA) {
        betas.push(CLAUDE_AFK_MODE_BETA);
    }
    if !is_probe_or_helper {
        let include_extended = (oauth_token && !is_claude_subagent_request(None, body))
            || requested(requested_set, CLAUDE_EXTENDED_CACHE_TTL_BETA)
            || claude_payload_has_1h_ttl(body);
        if include_extended {
            betas.push(CLAUDE_EXTENDED_CACHE_TTL_BETA);
        }
    }
    if requested(requested_set, CLAUDE_PROMPT_CACHING_EVICT_BETA)
        || crate::helps::text::contains(body, br#""evict_on_complete""#)
    {
        betas.push(CLAUDE_PROMPT_CACHING_EVICT_BETA);
    }
    if root.g("diagnostics").is_object() {
        betas.push(CLAUDE_CACHE_DIAGNOSIS_BETA);
    }
    betas.join(",")
}

/// Go: isClaudeHaikuModel.
pub fn is_claude_haiku_model(model: &str) -> bool {
    model.to_lowercase().contains("haiku")
}

/// Lowercased model name after the last `/` (Go: claudeCanonicalModel).
pub fn claude_canonical_model(model: &str) -> String {
    let model = model.trim().to_lowercase();
    match model.rfind('/') {
        Some(slash) => model[slash + 1..].to_string(),
        None => model,
    }
}

/// Go: isClaudeOpus55Model.
pub fn is_claude_opus55_model(model: &str) -> bool {
    let model = claude_canonical_model(model);
    model == "claude-opus-5-5" || model.starts_with("claude-opus-5-5[")
}

/// Go: isClaudeSonnet55Model.
pub fn is_claude_sonnet55_model(model: &str) -> bool {
    let model = claude_canonical_model(model);
    model == "claude-sonnet-5-5" || model.starts_with("claude-sonnet-5-5-") || model.starts_with("claude-sonnet-5-5[")
}

/// Go: isClaudeSonnet5Model.
pub fn is_claude_sonnet5_model(model: &str) -> bool {
    let model = claude_canonical_model(model);
    if is_claude_sonnet55_model(&model) {
        return false;
    }
    model == "claude-sonnet-5" || model.starts_with("claude-sonnet-5-") || model.starts_with("claude-sonnet-5[")
}

/// Go: isClaudeFable51Model (native family/major/minor check).
pub fn is_claude_fable51_model(model: &str) -> bool {
    let m = model.trim().to_lowercase();
    for target in ["fable-5-1", "fable-5.1", "mythos-5-1", "mythos-5.1"] {
        if let Some(idx) = m.find(target) {
            let next = idx + target.len();
            if next >= m.len() || !m.as_bytes()[next].is_ascii_digit() {
                return true;
            }
        }
    }
    false
}

/// Interactive CLI models that send `thinking.display=updates` unless the caller chose a display
/// (Go: claudeModelUsesProgressDisplay).
pub fn claude_model_uses_progress_display(model: &str) -> bool {
    is_claude_opus55_model(model)
        || is_claude_fable51_model(model)
        || is_claude_sonnet5_model(model)
        || is_claude_sonnet55_model(model)
}

/// Fills the latest CLI display only when the translated caller did not choose one
/// (Go: applyClaudeCloakThinkingDisplay).
pub fn apply_claude_cloak_thinking_display(body: Vec<u8>, caller_owned: bool) -> Vec<u8> {
    if caller_owned || body.is_empty() {
        return body;
    }
    let mut root = cpa_json::parse(&body);
    if root.g("thinking.display").exists() {
        return body;
    }
    if !claude_model_uses_progress_display(&root.g("model").str()) {
        return body;
    }
    match root.g("thinking.type").str().trim().to_lowercase().as_str() {
        "adaptive" | "enabled" => {}
        _ => return body,
    }
    cpa_json::set(&mut root, "thinking.display", "updates");
    cpa_json::to_vec(&root)
}

fn claude_model_has_per_turn_effort(model: &str) -> bool {
    is_claude_opus55_model(model) || claude_canonical_model(model).starts_with("claude-fable-5-1")
}

fn claude_model_has_per_turn_timing(model: &str) -> bool {
    let model = claude_canonical_model(model);
    claude_model_has_per_turn_effort(&model) || model.starts_with("claude-mythos-5-1")
}

fn claude_include_per_turn_control(root: &Value, requested_set: &RequestedBetas) -> bool {
    requested(requested_set, CLAUDE_PER_TURN_CONTROL_BETA) || claude_model_has_per_turn_effort(&root.g("model").str())
}

fn claude_include_per_turn_timing(root: &Value, requested_set: &RequestedBetas) -> bool {
    if requested(requested_set, CLAUDE_PER_TURN_TIMING_BETA) {
        return true;
    }
    if !claude_model_has_per_turn_timing(&root.g("model").str()) {
        return false;
    }
    if root.g("output_config.timing").exists() {
        return true;
    }
    root.g("messages").array().iter().any(|msg| msg.g("output_config.timing").exists())
}

fn claude_include_inline_tools(root: &Value, requested_set: &RequestedBetas) -> bool {
    if requested(requested_set, CLAUDE_INLINE_TOOLS_BETA) {
        return true;
    }
    for msg in root.g("messages").array() {
        for block in msg.g("content").array() {
            if block.g("type").str().trim().eq_ignore_ascii_case("tool_addition") && block.g("tool.definition").exists()
            {
                return true;
            }
        }
    }
    false
}

fn claude_include_mid_conv_clear_at(root: &Value, requested_set: &RequestedBetas) -> bool {
    if requested(requested_set, CLAUDE_MID_CONV_SYSTEM_CLEAR_AT_BETA)
        || claude_model_uses_progress_display(&root.g("model").str())
    {
        return true;
    }
    root.g("messages").array().iter().any(|msg| msg.g("clear_at").exists())
}

/// Whether the effort beta applies (Go: claudeRequestSupportsEffort); `requested` never turns it off.
pub fn claude_request_supports_effort(body: &[u8]) -> bool {
    if body.is_empty() {
        return true;
    }
    claude_request_supports_effort_value(&cpa_json::parse(body), false, &RequestedBetas::new())
}

fn claude_request_supports_effort_value(root: &Value, body_empty: bool, _requested: &RequestedBetas) -> bool {
    if !body_empty {
        let body = cpa_json::to_vec(root);
        if is_claude_probe_or_helper_request(&body) {
            return false;
        }
        let model = root.g("model").str().trim().to_lowercase();
        if is_claude_haiku_model(&model) {
            return false;
        }
        if root.g("thinking.type").str().trim().to_lowercase() == "disabled" {
            return false;
        }
    }
    true
}

fn claude_thinking_display_updates(root: &Value) -> bool {
    let display = root.g("thinking.display");
    display.is_string() && display.str().trim().eq_ignore_ascii_case("updates")
}

/// advanced-tool-use is needed for tool search, deferred loading, tool examples and programmatic
/// calling (Go: claudeBodyUsesAdvancedToolUse).
fn claude_body_uses_advanced_tool_use(root: &Value) -> bool {
    let tools = root.g("tools");
    if !tools.is_array() {
        return false;
    }
    for tool in tools.array() {
        let tool_type = tool.g("type").str().trim().to_lowercase();
        if tool_type.starts_with("tool_search_tool_") {
            return true;
        }
        if tool.g("defer_loading").bool() || tool.g("input_examples").exists() || tool.g("allowed_callers").exists() {
            return true;
        }
    }
    false
}

/// Whether the body declares an advisor server tool (Go: claudeBodyHasAdvisorTool).
pub fn claude_body_has_advisor_tool_bytes(body: &[u8]) -> bool {
    claude_body_has_advisor_tool(&cpa_json::parse(body))
}

fn claude_body_has_advisor_tool(root: &Value) -> bool {
    let tools = root.g("tools");
    if !tools.is_array() {
        return false;
    }
    tools.array().iter().any(|tool| tool.g("type").str().trim().to_lowercase().starts_with("advisor_"))
}

/// Whether the request carries a non-empty `thinking.display` (Go: claudeThinkingDisplaySet);
/// the redact-thinking beta must not accompany it.
fn claude_thinking_display_set(root: &Value) -> bool {
    let display = root.g("thinking.display");
    display.is_string() && !display.str().trim().is_empty()
}

/// Whether the request selects the fast service tier (Go: claudeRequestUsesFastMode).
pub fn claude_request_uses_fast_mode(body: &[u8], requested_set: &RequestedBetas) -> bool {
    claude_request_uses_fast_mode_value(&cpa_json::parse(body), requested_set)
}

fn claude_request_uses_fast_mode_value(root: &Value, requested_set: &RequestedBetas) -> bool {
    if requested(requested_set, CLAUDE_FAST_MODE_BETA) {
        return true;
    }
    let speed = root.g("speed");
    speed.is_string() && speed.str().trim().eq_ignore_ascii_case("fast")
}

/// Fixed count_tokens beta profile (Go: claudeCountTokensBetas).
const CLAUDE_COUNT_TOKENS_BETAS: [&str; 4] = [
    CLAUDE_CODE_BETA,
    "interleaved-thinking-2025-05-14",
    "context-management-2025-06-27",
    CLAUDE_TOKEN_COUNTING_BETA,
];

/// Go: claudeCountTokensBetasForCredential.
pub fn claude_count_tokens_betas_for_credential(oauth_token: bool) -> String {
    let mut betas = vec![CLAUDE_CODE_BETA];
    if oauth_token {
        betas.push(CLAUDE_OAUTH_BETA);
    }
    betas.extend_from_slice(&CLAUDE_COUNT_TOKENS_BETAS[1..]);
    betas.join(",")
}

/// Splits a comma list into trimmed, deduplicated entries, first occurrence wins.
fn dedupe_betas(betas: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    for beta in betas.split(',') {
        let beta = beta.trim();
        if !beta.is_empty() && seen.insert(beta.to_string()) {
            parts.push(beta.to_string());
        }
    }
    parts
}

fn insert_oauth_beta(parts: &mut Vec<String>) {
    let insert_at = usize::from(parts.first().is_some_and(|p| p == CLAUDE_CODE_BETA));
    parts.insert(insert_at, CLAUDE_OAUTH_BETA.to_string());
}

/// Go: withClaudeCountTokensOAuthBeta.
pub fn with_claude_count_tokens_oauth_beta(betas: &str) -> String {
    let mut parts = dedupe_betas(betas);
    if parts.iter().any(|p| p == CLAUDE_OAUTH_BETA) {
        return parts.join(",");
    }
    insert_oauth_beta(&mut parts);
    parts.join(",")
}

/// Restores the credential-scoped betas describing the selected OAuth account
/// (Go: withClaudeOAuthCredentialBetas). Betas already present stay where the caller put them.
pub fn with_claude_oauth_credential_betas(betas: &str, include_extended_cache_ttl: bool) -> String {
    let mut parts = dedupe_betas(betas);
    if !parts.iter().any(|p| p == CLAUDE_OAUTH_BETA) {
        insert_oauth_beta(&mut parts);
    }
    if include_extended_cache_ttl && !parts.iter().any(|p| p == CLAUDE_EXTENDED_CACHE_TTL_BETA) {
        parts.push(CLAUDE_EXTENDED_CACHE_TTL_BETA.to_string());
    }
    parts.join(",")
}

/// Removes one beta, trimming and dropping empty entries (Go: withoutClaudeBeta; no dedupe).
pub fn without_claude_beta(betas: &str, remove_beta: &str) -> String {
    betas
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != remove_beta)
        .collect::<Vec<_>>()
        .join(",")
}

/// Go: withClaudeExtendedCacheTTLBeta.
pub fn with_claude_extended_cache_ttl_beta(betas: &str) -> String {
    let mut parts = dedupe_betas(betas);
    if !parts.iter().any(|p| p == CLAUDE_EXTENDED_CACHE_TTL_BETA) {
        parts.push(CLAUDE_EXTENDED_CACHE_TTL_BETA.to_string());
    }
    parts.join(",")
}

/// Ensures advisor-tool is present at its observed wire position, before every beta that follows
/// it on the wire (Go: withClaudeAdvisorToolBeta).
pub fn with_claude_advisor_tool_beta(betas: &str) -> String {
    if betas.trim().is_empty() {
        return CLAUDE_ADVISOR_TOOL_BETA.to_string();
    }
    let mut seen = HashSet::new();
    let mut parts: Vec<String> = Vec::new();
    for beta in betas.split(',') {
        let beta = beta.trim();
        if !beta.is_empty() && beta != CLAUDE_ADVISOR_TOOL_BETA && seen.insert(beta.to_string()) {
            parts.push(beta.to_string());
        }
    }
    const BOUNDARIES: [&str; 9] = [
        CLAUDE_ADVANCED_TOOL_USE_BETA,
        CLAUDE_EFFORT_BETA,
        CLAUDE_SERVER_SIDE_FALLBACK_BETA,
        CLAUDE_FALLBACK_CREDIT_BETA,
        CLAUDE_STRUCTURED_OUTPUTS_BETA,
        CLAUDE_FAST_MODE_BETA,
        CLAUDE_AFK_MODE_BETA,
        CLAUDE_EXTENDED_CACHE_TTL_BETA,
        CLAUDE_CACHE_DIAGNOSIS_BETA,
    ];
    let insert_at = parts.iter().position(|b| BOUNDARIES.contains(&b.as_str())).unwrap_or(parts.len());
    parts.insert(insert_at, CLAUDE_ADVISOR_TOOL_BETA.to_string());
    parts.join(",")
}

/// Classifies a non-2xx upstream response (Go: classifyClaudeUpstreamErrorWithCooling).
///
/// The returned error carries the body as its message and a rate-limit derived retry delay. A
/// unified 5h/7d rejection is credential scoped (cool down and rotate), a fast-mode entitlement
/// refusal is request scoped (no rotation, no cooldown), any other 429 is a model-level limit.
pub fn classify_claude_upstream_error_with_cooling(
    status_code: u16,
    headers: &HeaderMap,
    body: &[u8],
    model_level_cooling: bool,
) -> ExecError {
    let mut err = status_err(status_code, String::from_utf8_lossy(body).into_owned());
    if status_code == 429 || (400..600).contains(&status_code) {
        err.retry_after = parse_claude_rate_limit_reset(headers, SystemTime::now());
    }
    if status_code == 429 {
        if !model_level_cooling && claude_headers_indicate_unified_rate_limit_rejection(headers) {
            err.credential_scoped = true;
            return err;
        }
        if claude_body_indicates_fast_mode_credits(body) {
            return err.with_code(ErrorCode::RequestScoped);
        }
    }
    err
}

/// Matches Anthropic's fast-mode entitlement refusal without matching a genuine rate limit
/// (Go: claudeBodyIndicatesFastModeCredits).
pub fn claude_body_indicates_fast_mode_credits(body: &[u8]) -> bool {
    let mut message = cpa_json::parse(body).g("error.message").str().to_lowercase();
    if message.is_empty() {
        message = String::from_utf8_lossy(body).to_lowercase();
    }
    message.contains("fast request rejected")
        || (message.contains("fast") && (message.contains("usage credits") || message.contains("credits are required")))
}

/// Every beta the caller asked for, from the header and from betas lifted out of the body
/// (Go: claudeRequestedBetas).
pub fn claude_requested_betas(incoming_betas: &str, extra_betas: &[String]) -> RequestedBetas {
    let mut set = RequestedBetas::new();
    for beta in incoming_betas.split(',').chain(extra_betas.iter().map(String::as_str)) {
        let beta = beta.trim();
        if !beta.is_empty() {
            set.insert(beta.to_string());
        }
    }
    set
}

/// `(api_key, base_url)` of a credential (Go: claudeCreds).
pub fn claude_creds(auth: &Auth) -> (String, String) {
    let mut api_key = auth.attributes.get("api_key").cloned().unwrap_or_default();
    let base_url = auth.attributes.get("base_url").cloned().unwrap_or_default();
    if api_key.is_empty() {
        api_key = auth.meta_str("access_token");
    }
    (api_key, base_url)
}

/// The sole OAuth discriminator for the wire policy (Go: isClaudeOAuthToken).
pub fn is_claude_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

/// Whether the credential authenticates with Bearer rather than `x-api-key`
/// (Go: claudeCredentialUsesOAuth).
pub fn claude_credential_uses_oauth(auth: &Auth, api_key: &str) -> bool {
    if is_claude_oauth_token(api_key) {
        return true;
    }
    if auth.auth_kind() == AUTH_KIND_API_KEY {
        return false;
    }
    auth.attributes.get("api_key").map(|k| k.trim()).unwrap_or("").is_empty()
}

// ---------------------------------------------------------------------------- headers

/// First non-empty value of `name` (Go: helps.HeaderValueCaseInsensitive; HeaderMap is already
/// case-insensitive).
pub fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

fn header_values_joined(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(",")
        .trim()
        .to_string()
}

/// `Header.Set`: replaces every value; invalid names or values are skipped.
pub fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
        headers.insert(n, v);
    }
}

fn del_header(headers: &mut HeaderMap, name: &str) {
    headers.remove(name);
}

/// Copies the caller's fingerprint headers verbatim (Go: copyClaudeCallerFingerprintHeaders).
pub fn copy_claude_caller_fingerprint_headers(dst: &mut HeaderMap, src: &HeaderMap, confirmed_claude_code: bool) {
    let names: Vec<HeaderName> = src.keys().cloned().collect();
    for name in names {
        let lower = name.as_str().trim().to_lowercase();
        let wanted = lower == "accept"
            || lower == "accept-encoding"
            || lower == "user-agent"
            || lower == "x-app"
            || lower == "x-client-request-id"
            || lower.starts_with("anthropic-")
            || lower.starts_with("x-stainless-")
            || lower.starts_with("x-claude-code-")
            || lower.starts_with("x-claude-remote-")
            || lower == "x-client-app"
            || lower == "x-anthropic-additional-protection";
        if !wanted {
            continue;
        }
        if !confirmed_claude_code && (lower.starts_with("x-claude-code-") || lower.starts_with("x-claude-remote-")) {
            continue;
        }
        dst.remove(&name);
        for value in src.get_all(&name) {
            dst.append(name.clone(), value.clone());
        }
    }
}

/// Everything the header builder reads besides the outgoing header map.
pub struct ClaudeHeaderInput<'a> {
    pub auth: &'a Auth,
    pub api_key: &'a str,
    pub stream: bool,
    pub extra_betas: &'a [String],
    pub body: &'a [u8],
    pub cfg: &'a Config,
    pub incoming_headers: &'a HeaderMap,
    pub confirmed_claude_code: bool,
    pub helper_profile: bool,
    /// Candidates for `X-Claude-Code-Session-Id`; the first non-blank wins.
    pub session_ids: &'a [&'a str],
    /// Target URL (first-party check and the `/count_tokens` suffix).
    pub url: &'a Url,
    /// The internal CPA session id expanded into `$CPA-SESSION-ID` custom headers.
    pub cpa_session_id: Option<&'a str>,
}

struct BetaCtx<'a> {
    body: &'a [u8],
    incoming_headers: &'a HeaderMap,
    count_tokens: bool,
    helper_profile: bool,
}

/// Post-assembly beta fixups and the `Anthropic-Beta` header write (Go: the `applyBetaHeader`
/// closure). Mutates `base_betas` so later steps see the fixed list.
fn apply_beta_header(headers: &mut HeaderMap, base_betas: &mut String, ctx: &BetaCtx<'_>) {
    let body = ctx.body;
    let root = cpa_json::parse(body);
    // Enforce native model and turn beta gating.
    if !claude_request_supports_effort(body) {
        *base_betas = without_claude_beta(base_betas, CLAUDE_EFFORT_BETA);
    }
    let probe_or_helper = is_claude_probe_or_helper_request(body);
    if probe_or_helper && !ctx.helper_profile {
        *base_betas = without_claude_beta(base_betas, CLAUDE_SERVER_SIDE_FALLBACK_BETA);
        *base_betas = without_claude_beta(base_betas, CLAUDE_THINKING_DISPLAY_UPDATES_BETA);
        *base_betas = without_claude_beta(base_betas, CLAUDE_EXTENDED_CACHE_TTL_BETA);
    }
    if root.g("thinking.type").str() == "disabled" {
        *base_betas = without_claude_beta(base_betas, CLAUDE_THINKING_DISPLAY_UPDATES_BETA);
    }
    if is_claude_subagent_request(Some(ctx.incoming_headers), body)
        && !claude_subagent_requests_1h(Some(ctx.incoming_headers), body)
    {
        *base_betas = without_claude_beta(base_betas, CLAUDE_EXTENDED_CACHE_TTL_BETA);
    }
    if !probe_or_helper && !ctx.count_tokens && claude_payload_has_1h_ttl(body) {
        *base_betas = with_claude_extended_cache_ttl_beta(base_betas);
    }
    let model = root.g("model").str().trim().to_lowercase();
    if is_claude_haiku_model(&model) && !root.g("fallbacks").exists() && !ctx.helper_profile {
        *base_betas = without_claude_beta(base_betas, CLAUDE_SERVER_SIDE_FALLBACK_BETA);
    }
    if base_betas.trim().is_empty() {
        del_header(headers, "anthropic-beta");
        return;
    }
    set_header(headers, "anthropic-beta", base_betas);
}

fn identity_header(
    headers: &mut HeaderMap,
    confirmed: bool,
    incoming: &HeaderMap,
    name: &str,
    fallback: &str,
) {
    if confirmed {
        cpa_core::misc::ensure_header(headers, Some(incoming), name, fallback);
        return;
    }
    set_header(headers, name, fallback);
}

fn apply_custom_headers(headers: &mut HeaderMap, input: &ClaudeHeaderInput<'_>) {
    let attrs: HashMap<String, String> =
        input.auth.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    cpa_core::util::apply_custom_headers_from_attrs(headers, &attrs, Some(input.incoming_headers), input.cpa_session_id);
}

/// Builds every Messages/count_tokens request header (Go: applyClaudeHeadersWithNativeProfile;
/// `applyClaudeHeaders` is this with `helper_profile = false`).
///
/// Two modes: caller-owned passthrough (plain API key, not cloaked, not a confirmed native client)
/// and the Claude Code CLI profile. Wire casing and header order are not reproduced (reqwest sends
/// lowercase names).
pub fn apply_claude_headers_with_native_profile(
    headers: &mut HeaderMap,
    input: &ClaudeHeaderInput<'_>,
) -> Result<(), ExecError> {
    let ClaudeHeaderInput {
        auth,
        api_key,
        stream,
        extra_betas,
        body,
        cfg,
        incoming_headers,
        confirmed_claude_code,
        helper_profile,
        ..
    } = *input;
    let (stream, confirmed, helper) = (stream, confirmed_claude_code, helper_profile);
    let hd = &cfg.claude_header_defaults;

    // Authentication and wire fingerprint are separate authorities.
    let credential_uses_bearer = claude_credential_uses_oauth(auth, api_key);
    let use_api_key = !credential_uses_bearer;
    let fp = resolve_claude_fingerprint_policy(cfg, auth, api_key);
    let (wire_policy, _) = resolve_claude_wire_policy(cfg, auth, api_key, confirmed);
    let apply_cli_fingerprint = fp.profile_claude_code_cli || wire_policy.cloak;
    let preserve_caller_fingerprint = !apply_cli_fingerprint && !confirmed;
    let use_oauth_betas = fp.use_oauth_betas;
    let is_anthropic_base = is_anthropic_upstream_url(input.url);
    if !api_key.trim().is_empty() {
        if is_anthropic_base && use_api_key {
            del_header(headers, "authorization");
            set_header(headers, "x-api-key", api_key);
        } else {
            del_header(headers, "x-api-key");
            set_header(headers, "authorization", &format!("Bearer {api_key}"));
        }
    } else {
        del_header(headers, "authorization");
        del_header(headers, "x-api-key");
    }
    set_header(headers, "content-type", "application/json");

    let stabilize_device_profile = claude_device_profile_stabilization_enabled(Some(cfg));
    let mut device_profile = None;
    if stabilize_device_profile && confirmed {
        device_profile = Some(resolve_claude_device_profile_required(Some(auth), api_key, Some(incoming_headers), Some(cfg))?);
    }

    let incoming_betas = header_values_joined(incoming_headers, "anthropic-beta");
    let count_tokens = input.url.path().ends_with("/count_tokens");
    let requested_map = claude_requested_betas(&incoming_betas, extra_betas);
    let advisor_needed = requested(&requested_map, CLAUDE_ADVISOR_TOOL_BETA) || claude_body_has_advisor_tool_bytes(body);

    let mut base_betas = incoming_betas.clone();
    if !preserve_caller_fingerprint {
        base_betas = claude_code_cli_betas(body, &requested_map, use_oauth_betas);
        if count_tokens {
            base_betas = claude_count_tokens_betas_for_credential(use_oauth_betas);
            if advisor_needed {
                base_betas = with_claude_advisor_tool_beta(&base_betas);
            }
        }
    }
    if confirmed && !incoming_betas.is_empty() {
        base_betas = incoming_betas.clone();
        if advisor_needed {
            base_betas = with_claude_advisor_tool_beta(&base_betas);
        }
        // Measured helper requests carry the exact credential beta profile; native subagents and
        // probes omit extended-cache-ttl.
        if use_oauth_betas && !helper {
            if count_tokens {
                base_betas = with_claude_count_tokens_oauth_beta(&base_betas);
            } else {
                let is_subagent = is_claude_subagent_request(Some(incoming_headers), body);
                let is_probe = is_claude_probe_or_helper_request(body);
                let subagent_1h = is_subagent && claude_subagent_requests_1h(Some(incoming_headers), body);
                let include_extended = (!is_subagent || subagent_1h) && !is_probe;
                base_betas = with_claude_oauth_credential_betas(&base_betas, include_extended);
            }
        }
    } else if preserve_caller_fingerprint && use_oauth_betas {
        if count_tokens {
            base_betas = with_claude_count_tokens_oauth_beta(&base_betas);
        } else {
            base_betas = with_claude_oauth_credential_betas(&base_betas, false);
        }
    }
    if preserve_caller_fingerprint && advisor_needed {
        base_betas = with_claude_advisor_tool_beta(&base_betas);
    }
    if !claude_request_supports_effort(body) {
        base_betas = without_claude_beta(&base_betas, CLAUDE_EFFORT_BETA);
    }
    let mut existing: HashSet<String> =
        base_betas.split(',').map(str::trim).filter(|b| !b.is_empty()).map(str::to_string).collect();
    let mut append_beta = |base: &mut String, beta: &str| {
        let beta = beta.trim();
        if beta.is_empty() || existing.contains(beta) {
            return;
        }
        if base.trim().is_empty() {
            *base = beta.to_string();
        } else {
            base.push(',');
            base.push_str(beta);
        }
        existing.insert(beta.to_string());
    };
    if preserve_caller_fingerprint {
        // Caller-owned mode preserves header and body-lifted betas verbatim; an explicit
        // speed=fast request still needs its protocol beta.
        if cpa_json::parse(body).g("speed").str().trim().eq_ignore_ascii_case("fast") {
            append_beta(&mut base_betas, CLAUDE_FAST_MODE_BETA);
        }
        for beta in extra_betas {
            append_beta(&mut base_betas, beta);
        }
    } else {
        // On direct Anthropic an unconfirmed CLI-profile caller's managed betas are dropped;
        // unmanaged ones are newer-client features and are forwarded. Custom gateways keep all.
        if !confirmed && !incoming_betas.is_empty() {
            for beta in incoming_betas.split(',') {
                let beta = beta.trim();
                if beta.is_empty() {
                    continue;
                }
                if is_managed_claude_beta(beta) && is_anthropic_base {
                    continue;
                }
                append_beta(&mut base_betas, beta);
            }
        }
        if !is_anthropic_base {
            for beta in extra_betas {
                append_beta(&mut base_betas, beta);
            }
        }
    }
    let beta_ctx = BetaCtx { body, incoming_headers, count_tokens, helper_profile: helper };
    apply_beta_header(headers, &mut base_betas, &beta_ctx);

    if preserve_caller_fingerprint {
        let mut default_accept = "application/json";
        let mut default_accept_encoding = "gzip, deflate, br, zstd";
        if stream && !is_anthropic_base {
            default_accept = "text/event-stream";
            default_accept_encoding = "identity";
        }
        copy_claude_caller_fingerprint_headers(headers, incoming_headers, confirmed);
        cpa_core::misc::ensure_header(headers, Some(incoming_headers), "Anthropic-Version", "2023-06-01");
        cpa_core::misc::ensure_header(headers, Some(incoming_headers), "Accept", default_accept);
        cpa_core::misc::ensure_header(headers, Some(incoming_headers), "Accept-Encoding", default_accept_encoding);
        // A caller that sent no User-Agent must not look like a bot: identify as CPA.
        cpa_core::misc::ensure_header(
            headers,
            Some(incoming_headers),
            "User-Agent",
            &format!("CLIProxyAPI/{}", option_env!("CPA_VERSION").unwrap_or("dev")),
        );
        apply_beta_header(headers, &mut base_betas, &beta_ctx);
        let session_id = input.session_ids.iter().map(|c| c.trim()).find(|c| !c.is_empty()).unwrap_or("");
        if !session_id.is_empty() {
            set_header(headers, "x-claude-code-session-id", session_id);
        }
        apply_custom_headers(headers, input);
        // Scope the custom-header escape hatch: first-party claws Anthropic-Beta, Accept and
        // Accept-Encoding back; streaming requests keep their Accept negotiation.
        let restore_caller_transport = |headers: &mut HeaderMap| {
            for (name, fallback) in [("Accept", default_accept), ("Accept-Encoding", default_accept_encoding)] {
                let value = header_value(incoming_headers, name);
                let value = value.trim();
                if !value.is_empty() {
                    set_header(headers, name, value);
                } else {
                    set_header(headers, name, fallback);
                }
            }
        };
        if is_anthropic_base {
            apply_beta_header(headers, &mut base_betas, &beta_ctx);
            restore_caller_transport(headers);
        } else if stream {
            restore_caller_transport(headers);
        }
        return Ok(());
    }

    identity_header(headers, confirmed, incoming_headers, "Anthropic-Version", "2023-06-01");
    identity_header(headers, confirmed, incoming_headers, "Anthropic-Dangerous-Direct-Browser-Access", "true");
    identity_header(headers, confirmed, incoming_headers, "X-App", "cli");
    // Values match Claude Code 2.1.280 / @anthropic-ai/sdk 0.112.1.
    identity_header(headers, confirmed, incoming_headers, "X-Stainless-Retry-Count", "0");
    identity_header(headers, confirmed, incoming_headers, "X-Stainless-Runtime", "node");
    identity_header(headers, confirmed, incoming_headers, "X-Stainless-Lang", "js");
    // The async SDK helper header survives only after the complete native-client detector succeeds.
    if confirmed && header_value(incoming_headers, "X-Stainless-Async") == "async" {
        set_header(headers, "X-Stainless-Async", "async");
    }
    // Claude Code omits X-Stainless-Timeout on count_tokens.
    if !count_tokens {
        let timeout = if hd.timeout.is_empty() { "600" } else { hd.timeout.as_str() };
        identity_header(headers, confirmed, incoming_headers, "X-Stainless-Timeout", timeout);
    } else if confirmed {
        let incoming_timeout = header_value(incoming_headers, "X-Stainless-Timeout");
        if !incoming_timeout.is_empty() {
            set_header(headers, "X-Stainless-Timeout", &incoming_timeout);
        }
    }
    let mut session_id = input.session_ids.iter().map(|c| c.trim()).find(|c| !c.is_empty()).unwrap_or("").to_string();
    if !session_id.is_empty() {
        set_header(headers, "X-Claude-Code-Session-Id", &session_id);
    } else {
        session_id = crate::helps::id_cache::cached_session_id(api_key);
        identity_header(headers, confirmed, incoming_headers, "X-Claude-Code-Session-Id", &session_id);
    }
    // Native subagent and environment headers pass through when present.
    for name in [
        "X-Claude-Code-Agent-Id",
        "X-Claude-Code-Parent-Agent-Id",
        "X-Claude-Remote-Container-Id",
        "X-Claude-Remote-Session-Id",
        "X-Client-App",
        "X-Anthropic-Additional-Protection",
    ] {
        let val = header_value(incoming_headers, name);
        if !val.is_empty() {
            set_header(headers, name, &val);
        }
    }
    // Gateway hints are caller-owned software state, preserved only for a confirmed native client.
    if confirmed {
        for name in [
            "X-Claude-Code-Request-Class",
            "X-Claude-Code-Agent-Type",
            "X-Claude-Code-Prev-Tool-Durations",
            "X-Claude-Code-Compaction",
            "X-Claude-Code-Context-Compacted",
        ] {
            let val = header_value(incoming_headers, name);
            if !val.is_empty() {
                set_header(headers, name, &val);
            }
        }
    }
    // Per-request UUID like Claude Code's x-client-request-id, first-party only; a helper on a
    // gateway only carries the id it already sent.
    if is_anthropic_base || (helper && !header_value(incoming_headers, "x-client-request-id").is_empty()) {
        identity_header(headers, confirmed, incoming_headers, "x-client-request-id", &uuid::Uuid::new_v4().to_string());
    }
    set_header(headers, "connection", "keep-alive");
    let apply_transport_negotiation = |headers: &mut HeaderMap| {
        if helper {
            identity_header(headers, confirmed, incoming_headers, "Accept", "application/json");
            identity_header(headers, confirmed, incoming_headers, "Accept-Encoding", "gzip, deflate, br, zstd");
            return;
        }
        if stream && !is_anthropic_base {
            // Other Anthropic-compatible upstreams keep the conservative contract.
            set_header(headers, "accept", "text/event-stream");
            set_header(headers, "accept-encoding", "identity");
            return;
        }
        set_header(headers, "accept", "application/json");
        set_header(headers, "accept-encoding", "gzip, deflate, br, zstd");
    };
    apply_transport_negotiation(headers);
    // Confirmed requests contribute their real software profile; unconfirmed callers always
    // receive the CLI baseline.
    if stabilize_device_profile {
        match (&device_profile, confirmed) {
            (Some(profile), true) => apply_claude_device_profile_headers(headers, profile),
            _ => apply_claude_default_device_profile_headers(headers, Some(cfg)),
        }
    } else {
        apply_claude_legacy_device_headers(headers, Some(incoming_headers), Some(cfg), confirmed);
    }
    apply_custom_headers(headers, input);
    // Custom credential headers are an escape hatch for third-party gateways. On first-party they
    // must not rewrite the reconstructed identity; elsewhere only streaming is protected.
    if is_anthropic_base {
        if base_betas.trim().is_empty() {
            del_header(headers, "anthropic-beta");
        } else {
            set_header(headers, "anthropic-beta", &base_betas);
        }
        apply_transport_negotiation(headers);
    } else if stream {
        apply_transport_negotiation(headers);
    }
    Ok(())
}

/// Sets a top-level string field unless it already holds that value (Go: helps.SetStringIfDifferent).
pub fn set_string_if_different_bytes(body: &[u8], path: &str, value: &str) -> Vec<u8> {
    let mut root = cpa_json::parse(body);
    if root.g(path).as_str() == Some(value) {
        return body.to_vec();
    }
    cpa_json::set(&mut root, path, value);
    cpa_json::to_vec(&root)
}

/// Sets a boolean field unless it already holds that value (Go: helps.SetBoolIfDifferent).
pub fn set_bool_if_different_bytes(body: &[u8], path: &str, value: bool) -> Vec<u8> {
    let mut root = cpa_json::parse(body);
    if root.g(path).v() == Some(&Value::Bool(value)) {
        return body.to_vec();
    }
    cpa_json::set(&mut root, path, value);
    cpa_json::to_vec(&root)
}

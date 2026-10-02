//! Native Claude Code client detection (Go: helps/claude_client_detection.go): strong signals,
//! measured Haiku helper shapes, user agent plausibility and entrypoint parsing.

use std::collections::HashMap;
use std::sync::LazyLock;

use cpa_config::Config;
use cpa_json::{J, Kind};
use http::HeaderMap;
use regex::Regex;

use super::code_session::CLAUDE_CODE_SESSION_HEADER;
use super::credential_identity::{go_json_valid, skip_claude_json_value, skip_claude_json_whitespace};
use super::device_profile::{
    ClaudeDeviceProfile, default_claude_device_profile, meets_claude_device_profile_baseline,
    parse_claude_cli_version, plausible_claude_cli_version,
};
use crate::helps::id_cache::is_valid_user_id;

/// Go: `claudeAnthropicVersion`, the only Anthropic-Version Claude Code sends.
const CLAUDE_ANTHROPIC_VERSION: &str = "2023-06-01";
/// Go: `claudeDefaultStainlessTimeout`. Deliberately not the configured timeout (see Go comment).
const CLAUDE_DEFAULT_STAINLESS_TIMEOUT: &str = "600";
const CLAUDE_CODE_HELPER_MODEL: &str = "claude-haiku-4-5-20251001";

// Go's `\s`/`\S` are ASCII only, so the classes are spelled out.
static USER_AGENT_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^claude-cli/").expect("static regex"));
static USER_AGENT_DETAILS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^claude-cli/[^\t\n\f\r ]+[\t\n\f\r ]+\(external,[\t\n\f\r ]*([^,)]+)(?:,[\t\n\f\r ]*agent-sdk/([^,)]+))?")
        .expect("static regex")
});
static NATIVE_USER_AGENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^claude-cli/[0-9]+\.[0-9]+\.[0-9]+[\t\n\f\r ]+\(external,[\t\n\f\r ]*[^,)]+(?:,[\t\n\f\r ]*agent-sdk/[0-9]+\.[0-9]+\.[0-9]+)?\)$")
        .expect("static regex")
});

/// Go: `claudeCodeSubclientByEntrypoint` lookup; "" for unknown entrypoints.
pub fn claude_code_subclient_by_entrypoint(entrypoint: &str) -> &'static str {
    match entrypoint {
        "cli" => "claude-code-cli",
        "mcp" => "claude-code-mcp",
        "bench" => "claude-code-bench",
        "sdk-cli" => "claude-code-cli-sdk",
        "sdk-ts" => "claude-code-sdk-ts",
        "sdk-py" => "claude-code-sdk-py",
        "claude-vscode" => "claude-code-vscode",
        "claude-code-github-action" => "claude-code-gh-action",
        "local-agent" | "local_agent" => "claude-local-agent",
        "claude-desktop" => "claude-desktop",
        "claude-desktop-3p" => "claude-desktop-3p",
        "remote" => "claude-remote",
        "remote_baku" => "claude-remote-baku",
        "remote_cowork" => "claude-remote-cowork",
        "remote_trigger" => "claude-remote-trigger",
        "remote_desktop" => "claude-remote-desktop",
        "remote_mobile" => "claude-remote-mobile",
        "claude_in_slack" | "claude-in-slack" => "claude-in-slack",
        "claude-in-teams" => "claude-in-teams",
        "claude-security" => "claude-security",
        "ssh-remote" => "claude-ssh-remote",
        "claude-coworker" => "claude-coworker",
        "claude-coworker-terminal" => "claude-coworker-terminal",
        _ => "",
    }
}

/// Go: `nativeClaudeEntrypoints[entrypoint]`: entrypoints with verified pass-through wire behavior.
pub fn native_claude_entrypoint(entrypoint: &str) -> bool {
    matches!(entrypoint, "cli" | "sdk-cli" | "claude-vscode")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperShape {
    Minimal,
    Structured,
    Title280,
}

/// Go: `claudeCodeHelperBetaProfile`.
fn helper_beta_profile(redact_thinking: bool, trailing: &[&str]) -> String {
    let mut betas = vec!["oauth-2025-04-20", "interleaved-thinking-2025-05-14"];
    if redact_thinking {
        betas.push("redact-thinking-2026-02-12");
    }
    betas.extend([
        "thinking-token-count-2026-05-13",
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
    ]);
    betas.extend_from_slice(trailing);
    betas.join(",")
}

/// Go: `measuredClaudeCodeHelperBetaProfiles`, the exact beta sequences seen on markerless native
/// Haiku helper requests.
static MEASURED_HELPER_BETA_PROFILES: LazyLock<HashMap<String, HelperShape>> = LazyLock::new(|| {
    HashMap::from([
        (helper_beta_profile(true, &[]), HelperShape::Minimal),
        (helper_beta_profile(false, &[]), HelperShape::Minimal),
        (
            helper_beta_profile(true, &["advisor-tool-2026-03-01", "structured-outputs-2025-12-15", "cache-diagnosis-2026-04-07"]),
            HelperShape::Structured,
        ),
        (
            helper_beta_profile(true, &["structured-outputs-2025-12-15", "fallback-credit-2026-06-01"]),
            HelperShape::Structured,
        ),
        (helper_beta_profile(true, &["structured-outputs-2025-12-15"]), HelperShape::Structured),
        (helper_beta_profile(false, &["structured-outputs-2025-12-15"]), HelperShape::Structured),
        (
            helper_beta_profile(
                true,
                &[
                    "structured-outputs-2025-12-15",
                    "server-side-fallback-2026-06-01",
                    "fallback-credit-2026-06-01",
                    "cache-diagnosis-2026-04-07",
                ],
            ),
            HelperShape::Title280,
        ),
    ])
});

/// Go: `ClaudeCodeRequestDetection`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCodeRequestDetection {
    pub confirmed: bool,
    pub strong_signals: bool,
    pub native_client: bool,
    pub x_app_cli: bool,
    pub user_agent: bool,
    pub betas_present: bool,
    pub metadata_user_id: bool,
    pub helper_profile: bool,
    pub entrypoint: String,
    pub subclient: String,
    pub agent_sdk_version: String,
}

/// Go: `DetectClaudeCodeRequest`. Standard Messages requests need all four strong signals
/// (count_tokens omits `metadata.user_id`); a narrow profile also accepts measured native Haiku
/// helper requests that omit `claude-code-20250219`.
pub fn detect_claude_code_request(
    headers: &HeaderMap,
    payload: &[u8],
    count_tokens: bool,
    cfg: Option<&Config>,
) -> ClaudeCodeRequestDetection {
    let user_agent = header_value(headers, "User-Agent");
    let (entrypoint, agent_sdk_version) = parse_claude_code_user_agent_details(&user_agent);
    let mut detection = ClaudeCodeRequestDetection {
        x_app_cli: header_value(headers, "X-App") == "cli",
        user_agent: plausible_claude_code_user_agent(&user_agent, cfg),
        betas_present: header_contains_claude_code_beta(headers),
        subclient: claude_code_subclient_by_entrypoint(&entrypoint).to_string(),
        entrypoint,
        agent_sdk_version,
        ..Default::default()
    };

    let root = cpa_json::parse(payload);
    let metadata_user_id = root.g("metadata.user_id");
    detection.metadata_user_id =
        metadata_user_id.exists() && metadata_user_id.is_string() && is_valid_user_id(&metadata_user_id.str());
    detection.native_client = native_claude_entrypoint(&detection.entrypoint);
    let standard_signals = detection.x_app_cli
        && detection.user_agent
        && detection.betas_present
        && (count_tokens || detection.metadata_user_id);
    detection.helper_profile = detection.native_client
        && matches_measured_helper_profile(headers, payload, &root, count_tokens, &detection, cfg);
    detection.strong_signals = standard_signals || detection.helper_profile;
    detection.confirmed = detection.strong_signals && detection.native_client;
    detection
}

/// Go: `matchesMeasuredClaudeCodeHelperProfile`.
fn matches_measured_helper_profile(
    headers: &HeaderMap,
    payload: &[u8],
    root: &serde_json::Value,
    count_tokens: bool,
    detection: &ClaudeCodeRequestDetection,
    cfg: Option<&Config>,
) -> bool {
    if count_tokens
        || detection.entrypoint != "cli"
        || detection.betas_present
        || !detection.x_app_cli
        || !detection.user_agent
        || !detection.metadata_user_id
    {
        return false;
    }
    let Some(&shape) = MEASURED_HELPER_BETA_PROFILES.get(&normalized_claude_beta_header(headers)) else {
        return false;
    };
    if measured_helper_body_shape(payload, root) != Some(shape) {
        return false;
    }
    if !measured_helper_headers_match(headers, cfg) {
        return false;
    }
    measured_helper_session_matches(headers, payload, root)
}

/// Go: `normalizedClaudeBetaHeader`: every Anthropic-Beta value in wire order, comma joined.
fn normalized_claude_beta_header(headers: &HeaderMap) -> String {
    let mut betas: Vec<String> = Vec::new();
    for value in headers.get_all("anthropic-beta") {
        let value = String::from_utf8_lossy(value.as_bytes());
        for beta in value.split(',') {
            let beta = beta.trim();
            if !beta.is_empty() {
                betas.push(beta.to_string());
            }
        }
    }
    betas.join(",")
}

/// Go: `measuredClaudeCodeHelperHeadersMatch`: the helper transport envelope. Platform and
/// software-version headers are only required to be present, then checked against the baseline.
fn measured_helper_headers_match(headers: &HeaderMap, cfg: Option<&Config>) -> bool {
    let profile = default_claude_device_profile(cfg);
    let expected = [
        ("Accept", "application/json"),
        ("Content-Type", "application/json"),
        ("X-Stainless-Lang", "js"),
        ("X-Stainless-Runtime", "node"),
        ("X-Stainless-Retry-Count", "0"),
        ("X-Stainless-Timeout", CLAUDE_DEFAULT_STAINLESS_TIMEOUT),
        ("Anthropic-Version", CLAUDE_ANTHROPIC_VERSION),
        ("Anthropic-Dangerous-Direct-Browser-Access", "true"),
    ];
    if expected.iter().any(|(name, want)| header_value(headers, name) != *want) {
        return false;
    }
    let required =
        ["X-Stainless-Package-Version", "X-Stainless-Runtime-Version", "X-Stainless-OS", "X-Stainless-Arch"];
    if required.iter().any(|name| header_value(headers, name).is_empty()) {
        return false;
    }
    let user_agent = header_value(headers, "User-Agent");
    let candidate = ClaudeDeviceProfile {
        version: parse_claude_cli_version(&user_agent),
        user_agent,
        package_version: header_value(headers, "X-Stainless-Package-Version"),
        runtime_version: header_value(headers, "X-Stainless-Runtime-Version"),
        ..Default::default()
    };
    if !meets_claude_device_profile_baseline(&candidate, &profile) {
        return false;
    }
    // Claude Code 2.1.258+: no X-Stainless-Async, full compression set, optional UUID request id.
    if !header_value(headers, "X-Stainless-Async").is_empty() {
        return false;
    }
    if header_value(headers, "Accept-Encoding") != "gzip, deflate, br, zstd" {
        return false;
    }
    let request_id = header_value(headers, "X-Client-Request-Id");
    if !request_id.is_empty() && uuid::Uuid::parse_str(&request_id).is_err() {
        return false;
    }
    true
}

/// Go: `measuredClaudeCodeHelperSessionMatches`: the session header must equal the
/// `metadata.user_id` session and the identity must be in native key order.
fn measured_helper_session_matches(headers: &HeaderMap, payload: &[u8], root: &serde_json::Value) -> bool {
    let metadata = root.g("metadata");
    if !metadata.is_object() || !raw_has_keys(payload, "metadata", &["user_id"]) {
        return false;
    }
    let user_id = metadata.g("user_id");
    if !user_id.is_string() || !is_valid_user_id(&user_id.str()) {
        return false;
    }
    // parent_session_id is a legitimate optional trailing key for sub-agent and forked sessions.
    let identity = user_id.str();
    let identity_raw = identity.as_bytes();
    if !claude_json_object_has_keys(identity_raw, &["device_id", "account_uuid", "session_id"])
        && !claude_json_object_has_keys(identity_raw, &["device_id", "account_uuid", "session_id", "parent_session_id"])
    {
        return false;
    }
    let session_id = cpa_json::parse(identity_raw).g("session_id").str();
    header_value(headers, CLAUDE_CODE_SESSION_HEADER) == session_id
}

/// `claudeJSONObjectHasKeys([]byte(<gjson result at path>.Raw), want)`, scanning the original text.
fn raw_has_keys(payload: &[u8], path: &str, want: &[&str]) -> bool {
    cpa_json::raw_at(payload, path).is_some_and(|raw| claude_json_object_has_keys(raw.as_bytes(), want))
}

/// Go: `measuredClaudeCodeHelperBodyShape`; `None` is Go's `claudeCodeHelperShapeNone`.
fn measured_helper_body_shape(payload: &[u8], root: &serde_json::Value) -> Option<HelperShape> {
    let minimal_keys = ["model", "max_tokens", "messages", "metadata"];
    let structured_keys =
        ["model", "messages", "system", "tools", "metadata", "max_tokens", "thinking", "temperature", "output_config", "stream"];
    let title280_keys = ["model", "max_tokens", "messages", "metadata", "output_config"];
    let (shape, is_title280) = if claude_json_object_has_keys(payload, &minimal_keys) {
        (HelperShape::Minimal, false)
    } else if claude_json_object_has_keys(payload, &structured_keys) {
        (HelperShape::Structured, false)
    } else if claude_json_object_has_keys(payload, &title280_keys) {
        (HelperShape::Title280, true)
    } else {
        return None;
    };

    let max_tokens = root.g("max_tokens");
    if root.g("model").str() != CLAUDE_CODE_HELPER_MODEL || max_tokens.kind() != Kind::Number {
        return None;
    }
    let messages = root.g("messages");
    if !messages.is_array() || messages.array().len() != 1 {
        return None;
    }
    let message = messages.g("0");
    if !raw_has_keys(payload, "messages.0", &["role", "content"]) || message.g("role").str() != "user" {
        return None;
    }

    if shape == HelperShape::Minimal {
        if max_tokens.raw() != "1" || message.g("content").kind() != Kind::String {
            return None;
        }
        return Some(shape);
    }

    if is_title280 {
        let format = root.g("output_config.format");
        let schema = format.g("schema");
        if max_tokens.raw() != "80"
            || message.g("content").kind() != Kind::String
            || !raw_has_keys(payload, "output_config", &["format"])
            || !raw_has_keys(payload, "output_config.format", &["type", "schema"])
            || format.g("type").str() != "json_schema"
            || !raw_has_keys(payload, "output_config.format.schema", &["type"])
            || schema.g("type").str() != "object"
        {
            return None;
        }
        return Some(shape);
    }

    let content = message.g("content");
    if !content.is_array() || content.array().len() != 1 {
        return None;
    }
    let content_block = content.g("0");
    if !raw_has_keys(payload, "messages.0.content.0", &["type", "text"]) || content_block.g("type").str() != "text" {
        return None;
    }
    if !measured_helper_system_matches(payload, root) {
        return None;
    }
    let tools = root.g("tools");
    if !tools.is_array() || !tools.array().is_empty() {
        return None;
    }
    let thinking = root.g("thinking");
    if !raw_has_keys(payload, "thinking", &["type"]) || thinking.g("type").str() != "disabled" {
        return None;
    }
    let format = root.g("output_config.format");
    let schema = format.g("schema");
    let properties = schema.g("properties");
    let title_property = properties.g("title");
    let required = schema.g("required");
    let additional_properties = schema.g("additionalProperties");
    if !raw_has_keys(payload, "output_config", &["format"])
        || !raw_has_keys(payload, "output_config.format", &["type", "schema"])
        || format.g("type").str() != "json_schema"
        || !raw_has_keys(
            payload,
            "output_config.format.schema",
            &["type", "properties", "required", "additionalProperties"],
        )
        || schema.g("type").str() != "object"
        || !raw_has_keys(payload, "output_config.format.schema.properties", &["title"])
        || !raw_has_keys(payload, "output_config.format.schema.properties.title", &["type"])
        || title_property.g("type").str() != "string"
        || !required.is_array()
        || required.array().len() != 1
        || required.g("0").str() != "title"
        || additional_properties.kind() != Kind::False
    {
        return None;
    }
    if max_tokens.raw() != "32000" || root.g("temperature").raw() != "1" || root.g("stream").kind() != Kind::True {
        return None;
    }
    Some(shape)
}

/// Go: `measuredClaudeCodeHelperSystemMatches`.
fn measured_helper_system_matches(payload: &[u8], root: &serde_json::Value) -> bool {
    let system = root.g("system");
    if !system.is_array() || system.array().len() != 3 {
        return false;
    }
    let blocks = cpa_json::raw_children(payload, "system");
    if blocks.len() != 3 {
        return false;
    }
    for raw in &blocks {
        let block = cpa_json::parse(raw.as_bytes());
        if !claude_json_object_has_keys(raw.as_bytes(), &["type", "text"]) || block.g("type").str() != "text" {
            return false;
        }
    }
    let billing = system.g("0.text").str();
    let identity = system.g("1.text").str();
    billing.starts_with("x-anthropic-billing-header:")
        && measured_claude_billing_cch(&billing)
        && identity.starts_with("You are Claude Code")
}

/// Go: `measuredClaudeBillingCCH`: five lowercase hex characters after ` cch=` and a `;`.
fn measured_claude_billing_cch(billing: &str) -> bool {
    const MARKER: &str = " cch=";
    let Some(marker) = billing.find(MARKER) else { return false };
    let value_start = marker + MARKER.len();
    let value_end = value_start + 5;
    let bytes = billing.as_bytes();
    if value_end >= bytes.len() || bytes[value_end] != b';' {
        return false;
    }
    // Go ranges over runes; any non-hex character (including multibyte) rejects.
    billing
        .get(value_start..value_end)
        .is_some_and(|s| s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Go: `claudeJSONObjectHasKeys`: valid JSON object whose top-level keys are exactly `want`, in
/// order (duplicates count as separate keys).
pub fn claude_json_object_has_keys(raw: &[u8], want: &[&str]) -> bool {
    if !go_json_valid(raw) {
        return false;
    }
    let mut pos = skip_claude_json_whitespace(raw, 0);
    if raw.get(pos) != Some(&b'{') {
        return false;
    }
    pos += 1;
    let mut key_index = 0;
    loop {
        pos = skip_claude_json_whitespace(raw, pos);
        match raw.get(pos) {
            Some(b'}') => break,
            Some(b'"') => {}
            _ => return false,
        }
        let key_end = skip_claude_json_value(raw, pos);
        let Ok(key) = serde_json::from_slice::<String>(&raw[pos..key_end]) else { return false };
        if key_index >= want.len() || key != want[key_index] {
            return false;
        }
        key_index += 1;
        pos = skip_claude_json_whitespace(raw, key_end);
        if raw.get(pos) != Some(&b':') {
            return false;
        }
        pos = skip_claude_json_whitespace(raw, pos + 1);
        pos = skip_claude_json_value(raw, pos);
        pos = skip_claude_json_whitespace(raw, pos);
        if raw.get(pos) == Some(&b',') {
            pos += 1;
        }
    }
    key_index == want.len()
}

/// Go: `plausibleClaudeCodeUserAgent`: native `claude-cli/x.y.z (external, ...)` agent whose
/// version is within the baseline release line (patch >= baseline).
pub fn plausible_claude_code_user_agent(user_agent: &str, cfg: Option<&Config>) -> bool {
    let user_agent = user_agent.trim();
    if !USER_AGENT_PREFIX_RE.is_match(user_agent) || !NATIVE_USER_AGENT_RE.is_match(user_agent) {
        return false;
    }
    let candidate = parse_claude_cli_version(user_agent);
    let baseline = parse_claude_cli_version(&default_claude_device_profile(cfg).user_agent);
    match (candidate, baseline) {
        (Some(c), Some(b)) => plausible_claude_cli_version(c, b),
        _ => false,
    }
}

/// Go: `claudeCodeNativeUserAgentPattern.MatchString`.
pub(crate) fn claude_code_native_user_agent_matches(user_agent: &str) -> bool {
    NATIVE_USER_AGENT_RE.is_match(user_agent)
}

/// Go: `parseClaudeCodeUserAgentDetails`: lowercased entrypoint and agent-sdk version ("" if none).
pub fn parse_claude_code_user_agent_details(user_agent: &str) -> (String, String) {
    let Some(caps) = USER_AGENT_DETAILS_RE.captures(user_agent.trim()) else {
        return (String::new(), String::new());
    };
    let entrypoint = caps.get(1).map(|m| m.as_str().trim().to_lowercase()).unwrap_or_default();
    let agent_sdk_version = caps.get(2).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
    (entrypoint, agent_sdk_version)
}

/// Go: `headerValue`: first value of the header, untrimmed ("" when absent).
pub fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default()
}

/// Go: `headerContainsClaudeCodeBeta`.
fn header_contains_claude_code_beta(headers: &HeaderMap) -> bool {
    headers.get_all("anthropic-beta").iter().any(|value| {
        String::from_utf8_lossy(value.as_bytes()).split(',').any(|beta| beta.trim() == "claude-code-20250219")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};

    const USER_ID: &str = r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","account_uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","session_id":"11111111-2222-4333-8444-555555555555"}"#;

    fn set(h: &mut HeaderMap, name: &str, value: &str) {
        h.insert(HeaderName::from_bytes(name.as_bytes()).expect("name"), HeaderValue::from_str(value).expect("value"));
    }

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            set(&mut h, k, v);
        }
        h
    }

    fn encoded(user_id: &str) -> String {
        serde_json::to_string(user_id).expect("encode")
    }

    fn detection_payload(user_id: &str) -> Vec<u8> {
        format!(r#"{{"metadata":{{"user_id":{}}}}}"#, encoded(user_id)).into_bytes()
    }

    fn confirmed_headers() -> HeaderMap {
        header_map(&[
            ("User-Agent", "claude-cli/2.1.280 (external, cli)"),
            ("X-App", "cli"),
            ("Anthropic-Beta", "claude-code-20250219,interleaved-thinking-2025-05-14"),
        ])
    }

    fn helper_headers(beta_profile: &str) -> HeaderMap {
        let p = default_claude_device_profile(None);
        header_map(&[
            ("Accept", "application/json"),
            ("Accept-Encoding", "gzip, deflate, br, zstd"),
            ("Content-Type", "application/json"),
            ("User-Agent", &p.user_agent),
            ("X-App", "cli"),
            ("Anthropic-Beta", beta_profile),
            ("Anthropic-Version", "2023-06-01"),
            ("Anthropic-Dangerous-Direct-Browser-Access", "true"),
            ("X-Claude-Code-Session-Id", "11111111-2222-4333-8444-555555555555"),
            ("X-Client-Request-Id", "66666666-7777-4888-8999-aaaaaaaaaaaa"),
            ("X-Stainless-Lang", "js"),
            ("X-Stainless-Runtime", "node"),
            ("X-Stainless-Package-Version", &p.package_version),
            ("X-Stainless-Runtime-Version", &p.runtime_version),
            ("X-Stainless-OS", &p.os),
            ("X-Stainless-Arch", &p.arch),
            ("X-Stainless-Retry-Count", "0"),
            ("X-Stainless-Timeout", "600"),
        ])
    }

    fn minimal_payload() -> String {
        format!(
            r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{{"role":"user","content":"helper probe"}}],"metadata":{{"user_id":{}}}}}"#,
            encoded(USER_ID)
        )
    }

    fn structured_payload() -> String {
        format!(
            r#"{{"model":"claude-haiku-4-5-20251001","messages":[{{"role":"user","content":[{{"type":"text","text":"helper probe"}}]}}],"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.258; cc_entrypoint=cli; cch=00000;"}},{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}},{{"type":"text","text":"Return a short title."}}],"tools":[],"metadata":{{"user_id":{}}},"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"output_config":{{"format":{{"type":"json_schema","schema":{{"type":"object","properties":{{"title":{{"type":"string"}}}},"required":["title"],"additionalProperties":false}}}}}},"stream":true}}"#,
            encoded(USER_ID)
        )
    }

    fn title280_payload() -> String {
        format!(
            r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":80,"messages":[{{"role":"user","content":"generate title"}}],"metadata":{{"user_id":{}}},"output_config":{{"format":{{"type":"json_schema","schema":{{"type":"object"}}}}}}}}"#,
            encoded(USER_ID)
        )
    }

    fn detect(h: &HeaderMap, payload: &[u8]) -> ClaudeCodeRequestDetection {
        detect_claude_code_request(h, payload, false, None)
    }

    #[test]
    fn requires_all_four_message_signals() {
        let payload = detection_payload(USER_ID);
        let d = detect(&confirmed_headers(), &payload);
        assert!(d.confirmed && d.strong_signals && d.native_client);
        assert!(d.x_app_cli && d.user_agent && d.betas_present && d.metadata_user_id);

        let ua = ("User-Agent", "claude-cli/2.1.280 (external, cli)");
        let beta = ("Anthropic-Beta", "claude-code-20250219");
        assert!(!detect(&header_map(&[ua, beta]), &payload).confirmed, "x-app");
        assert!(!detect(&header_map(&[("User-Agent", "curl/8.7.1"), ("X-App", "cli"), beta]), &payload).confirmed, "ua");
        assert!(!detect(&header_map(&[ua, ("X-App", "cli")]), &payload).confirmed, "betas");
        assert!(!detect(&confirmed_headers(), br#"{"messages":[]}"#).confirmed, "metadata");
    }

    #[test]
    fn user_agent_plausibility_follows_baseline_release_line() {
        let payload = detection_payload(USER_ID);
        let mut h = confirmed_headers();
        set(&mut h, "User-Agent", "claude-cli/2.1.281 (external, cli)");
        assert!(detect(&h, &payload).confirmed);

        set(&mut h, "User-Agent", "claude-cli/2.2.0 (external, cli)");
        assert!(!detect(&h, &payload).confirmed);
        let mut cfg = Config::default();
        cfg.claude_header_defaults.user_agent = "claude-cli/2.2.0 (external, cli)".into();
        assert!(detect_claude_code_request(&h, &payload, false, Some(&cfg)).confirmed);

        let beta = ("Anthropic-Beta", "claude-code-20250219");
        for ua in [
            "claude-cli/not-a-version (external, cli)",
            "claude-cli/2.1.257 (external, cli)",
            "claude-cli/999.0.0 (external, cli)",
        ] {
            let h = header_map(&[("User-Agent", ua), ("X-App", "cli"), beta]);
            assert!(!detect(&h, &payload).confirmed, "{ua}");
        }
        let h = header_map(&[("User-Agent", "claude-cli/2.1.280 (external, cli)"), ("X-App", "cli"), ("Anthropic-Beta", "anything")]);
        assert!(!detect(&h, &payload).confirmed);
    }

    #[test]
    fn rejects_malformed_metadata_user_ids() {
        for user_id in [
            "user_abc_account__session_session",
            r#"{"device_id":"abc","account_uuid":"","session_id":"11111111-2222-4333-8444-555555555555"}"#,
            r#"{"device_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","account_uuid":"","session_id":"11111111-2222-4333-8444-555555555555"}"#,
            r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","account_uuid":"","session_id":"session"}"#,
        ] {
            assert!(!detect(&confirmed_headers(), &detection_payload(user_id)).confirmed, "{user_id}");
        }
    }

    #[test]
    fn classifies_entrypoints() {
        let payload = detection_payload(USER_ID);
        let cases = [
            ("claude-cli/2.1.280 (external, cli)", "cli", "claude-code-cli", "", true),
            ("claude-cli/2.1.280 (external, claude-vscode, agent-sdk/0.3.220)", "claude-vscode", "claude-code-vscode", "0.3.220", true),
            ("claude-cli/2.1.280 (external, sdk-cli)", "sdk-cli", "claude-code-cli-sdk", "", true),
            ("claude-cli/2.1.280 (external, sdk-ts, agent-sdk/0.3.220)", "sdk-ts", "claude-code-sdk-ts", "0.3.220", false),
            ("claude-cli/2.1.280 (external, sdk-py, agent-sdk/0.1.0)", "sdk-py", "claude-code-sdk-py", "0.1.0", false),
            ("claude-cli/2.1.280 (external, claude-desktop-3p)", "claude-desktop-3p", "claude-desktop-3p", "", false),
            ("claude-cli/2.1.280 (external, claude-code-github-action)", "claude-code-github-action", "claude-code-gh-action", "", false),
            ("claude-cli/2.1.280 (external, copied-client)", "copied-client", "", "", false),
        ];
        for (ua, entrypoint, subclient, sdk, native) in cases {
            let mut h = confirmed_headers();
            set(&mut h, "User-Agent", ua);
            let d = detect(&h, &payload);
            assert!(d.strong_signals, "{ua}");
            assert_eq!((d.confirmed, d.native_client), (native, native), "{ua}");
            assert_eq!(
                (d.entrypoint.as_str(), d.subclient.as_str(), d.agent_sdk_version.as_str()),
                (entrypoint, subclient, sdk),
                "{ua}"
            );
        }
    }

    #[test]
    fn count_tokens_allows_missing_metadata() {
        let mut h = confirmed_headers();
        set(&mut h, "User-Agent", "claude-cli/2.1.280 (external, claude-vscode, agent-sdk/0.3.220)");
        let d = detect_claude_code_request(&h, br#"{"messages":[]}"#, true, None);
        assert!(d.confirmed && !d.metadata_user_id);
        assert_eq!((d.subclient.as_str(), d.agent_sdk_version.as_str()), ("claude-code-vscode", "0.3.220"));
    }

    #[test]
    fn recognizes_measured_haiku_helpers() {
        let title280_beta = helper_beta_profile(
            true,
            &["structured-outputs-2025-12-15", "server-side-fallback-2026-06-01", "fallback-credit-2026-06-01", "cache-diagnosis-2026-04-07"],
        );
        let cases = [
            (helper_beta_profile(true, &[]), minimal_payload()),
            (helper_beta_profile(false, &[]), minimal_payload()),
            (
                helper_beta_profile(true, &["advisor-tool-2026-03-01", "structured-outputs-2025-12-15", "cache-diagnosis-2026-04-07"]),
                structured_payload(),
            ),
            (helper_beta_profile(true, &["structured-outputs-2025-12-15", "fallback-credit-2026-06-01"]), structured_payload()),
            (
                helper_beta_profile(true, &["structured-outputs-2025-12-15"]),
                structured_payload().replacen("cch=00000", "cch=7ee87", 1),
            ),
            (helper_beta_profile(false, &["structured-outputs-2025-12-15"]), structured_payload()),
            (title280_beta, title280_payload()),
        ];
        for (beta, payload) in cases {
            let d = detect(&helper_headers(&beta), payload.as_bytes());
            assert!(d.confirmed && d.strong_signals && d.native_client && d.helper_profile, "{beta}");
            assert!(!d.betas_present);
        }
    }

    #[test]
    fn rejects_mismatched_or_malformed_helper_betas_and_bodies() {
        let title280_trailing =
            ["structured-outputs-2025-12-15", "server-side-fallback-2026-06-01", "fallback-credit-2026-06-01", "cache-diagnosis-2026-04-07"];
        let mut extended = title280_trailing.to_vec();
        extended.push("advisor-tool-2026-03-01");
        let legacy_structured = helper_beta_profile(true, &["structured-outputs-2025-12-15"]);
        let cases = [
            ("extended betas", helper_beta_profile(true, &extended), structured_payload()),
            ("title280 beta, structured body", helper_beta_profile(true, &title280_trailing), structured_payload()),
            ("structured beta, title280 body", legacy_structured.clone(), title280_payload()),
            ("non-hex cch", legacy_structured.clone(), structured_payload().replacen("cch=00000", "cch=ghijk", 1)),
            ("uppercase cch", legacy_structured.clone(), structured_payload().replacen("cch=00000", "cch=7EE87", 1)),
            ("token cap", legacy_structured.clone(), structured_payload().replacen("\"max_tokens\":32000", "\"max_tokens\":32001", 1)),
            ("open schema", legacy_structured, structured_payload().replacen("\"additionalProperties\":false", "\"additionalProperties\":true", 1)),
        ];
        for (name, beta, payload) in cases {
            let d = detect(&helper_headers(&beta), payload.as_bytes());
            assert!(!d.confirmed && !d.helper_profile, "{name}");
        }
    }

    #[test]
    fn rejects_near_miss_helpers() {
        let beta = helper_beta_profile(true, &[]);
        let minimal = minimal_payload();
        type Mutate = Box<dyn Fn(&mut HeaderMap)>;
        let hdr = |name: &'static str, value: &'static str| -> Mutate {
            Box::new(move |h: &mut HeaderMap| set(h, name, value))
        };
        let beta_with_unknown = format!("{beta},unknown-beta");
        let cases: Vec<(&str, Mutate, String, bool)> = vec![
            ("unexpected beta", Box::new(move |h| set(h, "Anthropic-Beta", &beta_with_unknown)), minimal.clone(), false),
            ("missing package", Box::new(|h| { h.remove("x-stainless-package-version"); }), minimal.clone(), false),
            ("compression", hdr("Accept-Encoding", "gzip"), minimal.clone(), false),
            ("session header", hdr("X-Claude-Code-Session-Id", "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"), minimal.clone(), false),
            ("request id", hdr("X-Client-Request-Id", "not-a-uuid"), minimal.clone(), false),
            ("async", hdr("X-Stainless-Async", "async"), minimal.clone(), false),
            ("model", Box::new(|_| {}), minimal.replacen(CLAUDE_CODE_HELPER_MODEL, "claude-sonnet-4-6", 1), false),
            ("token cap", Box::new(|_| {}), minimal.replacen("\"max_tokens\":1", "\"max_tokens\":2", 1), false),
            ("extra root key", Box::new(|_| {}), format!("{},\"tools\":[]}}", minimal.trim_end_matches('}')), false),
            (
                "cache marker content",
                Box::new(|_| {}),
                minimal.replacen(
                    "\"content\":\"helper probe\"",
                    "\"content\":[{\"type\":\"text\",\"text\":\"helper probe\",\"cache_control\":{\"type\":\"ephemeral\",\"ttl\":\"1h\"}}]",
                    1,
                ),
                false,
            ),
            ("count tokens", Box::new(|_| {}), minimal.clone(), true),
        ];
        for (name, mutate, payload, count_tokens) in cases {
            let mut h = helper_headers(&beta);
            mutate(&mut h);
            let d = detect_claude_code_request(&h, payload.as_bytes(), count_tokens, None);
            assert!(!d.confirmed && !d.helper_profile, "{name}");
        }
    }

    #[test]
    fn helper_identity_keys() {
        let beta = helper_beta_profile(true, &[]);
        let payload_for = |identity: &str| {
            format!(
                r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{{"role":"user","content":"helper probe"}}],"metadata":{{"user_id":{}}}}}"#,
                encoded(identity)
            )
        };
        // parent_session_id is a legitimate optional trailing key.
        let with_parent = USER_ID.replacen('}', r#","parent_session_id":"99999999-8888-4777-8666-555555555555"}"#, 1);
        let d = detect(&helper_headers(&beta), payload_for(&with_parent).as_bytes());
        assert!(d.confirmed && d.helper_profile);
        let with_unknown = USER_ID.replacen('}', r#","spoofed":"x"}"#, 1);
        assert!(!detect(&helper_headers(&beta), payload_for(&with_unknown).as_bytes()).helper_profile);
    }

    #[test]
    fn helper_platform_and_software_headers() {
        let beta = helper_beta_profile(true, &[]);
        let payload = minimal_payload();
        for (os, arch) in [("Windows", "x64"), ("Linux", "x64"), ("MacOS", "x64")] {
            let mut h = helper_headers(&beta);
            set(&mut h, "X-Stainless-OS", os);
            set(&mut h, "X-Stainless-Arch", arch);
            let d = detect(&h, payload.as_bytes());
            assert!(d.confirmed && d.helper_profile, "{os}/{arch}");
        }
        for name in ["X-Stainless-OS", "X-Stainless-Arch", "X-Stainless-Package-Version", "X-Stainless-Runtime-Version"] {
            let mut h = helper_headers(&beta);
            h.remove(name);
            assert!(!detect(&h, payload.as_bytes()).helper_profile, "missing {name}");
        }
        for (name, value) in [("X-Stainless-Package-Version", "0.0.1"), ("X-Stainless-Runtime-Version", "v0.0.1")] {
            let mut h = helper_headers(&beta);
            set(&mut h, name, value);
            assert!(!detect(&h, payload.as_bytes()).helper_profile, "foreign {name}");
        }
    }

    #[test]
    fn helper_profile_ignores_configured_stainless_timeout() {
        let beta = helper_beta_profile(true, &[]);
        let payload = minimal_payload();
        let h = helper_headers(&beta);
        for timeout in [None, Some("600"), Some("300"), Some("900")] {
            let mut cfg = Config::default();
            if let Some(t) = timeout {
                cfg.claude_header_defaults.timeout = t.into();
            }
            let d = detect_claude_code_request(&h, payload.as_bytes(), false, Some(&cfg));
            assert!(d.helper_profile && d.confirmed, "{timeout:?}");
        }
        let mut foreign = helper_headers(&beta);
        set(&mut foreign, "X-Stainless-Timeout", "900");
        let mut cfg = Config::default();
        cfg.claude_header_defaults.timeout = "900".into();
        assert!(!detect_claude_code_request(&foreign, payload.as_bytes(), false, Some(&cfg)).helper_profile);
    }

    #[test]
    fn beta_header_joins_values_in_wire_order() {
        let mut h = HeaderMap::new();
        h.append("anthropic-beta", HeaderValue::from_static("oauth-2025-04-20"));
        h.append("Anthropic-Beta", HeaderValue::from_static("interleaved-thinking-2025-05-14, x"));
        assert_eq!(normalized_claude_beta_header(&h), "oauth-2025-04-20,interleaved-thinking-2025-05-14,x");
        assert_eq!(normalized_claude_beta_header(&HeaderMap::new()), "");
    }
}

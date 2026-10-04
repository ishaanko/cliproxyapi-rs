//! Configuration schema. Field names, order and `omitempty` behaviour mirror the Go structs in
//! `internal/config` so that serialising a [`Config`] yields the same (legacy-layout) YAML.
//!
//! A `Config` always uses the legacy field names at runtime; the v8 layout exists only at the YAML
//! boundary (see [`crate::layout`]).
//!
//! Deserialisation notes:
//! - Unknown fields are ignored (Go decodes non-strictly).
//! - Absent fields keep the "pre-set" defaults Go applies before decoding (`parse_defaults`), which
//!   differ from the zero value used by `Default`.
//! - YAML `null` is stripped by the loader before decoding, matching yaml.v3 where a null leaves a
//!   scalar/struct untouched.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use crate::duration::GoDuration;

pub const DEFAULT_PANEL_GITHUB_REPOSITORY: &str =
    "https://github.com/router-for-me/Cli-Proxy-API-Management-Center";
pub const DEFAULT_PPROF_ADDR: &str = "127.0.0.1:8316";
pub const DEFAULT_AUTH_DIR: &str = "~/.cli-proxy-api";
pub const DEFAULT_DISCOVERY_SERVICE_TYPE: &str = "_ai-gateway._tcp";
pub const DEFAULT_PLUGINS_DIR: &str = "plugins";
pub const DEFAULT_PORT: i64 = 8317;
/// Default DNS-SD subtypes advertised by discovery.
pub const DEFAULT_DISCOVERY_SUBTYPES: [&str; 5] = [
    "_chat-completions",
    "_responses",
    "_messages",
    "_generate-content",
    "_interactions",
];

fn default_discovery_subtypes() -> Vec<String> {
    DEFAULT_DISCOVERY_SUBTYPES
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

// serde `skip_serializing_if` helpers (Go `omitempty`).
fn is_zero(v: &i64) -> bool {
    *v == 0
}
fn is_false(v: &bool) -> bool {
    !*v
}

/// Top-level configuration in its effective (legacy-named) runtime form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default = "Config::parse_defaults")]
pub struct Config {
    // --- SDKConfig (inline) ---
    pub client: ClientConfig,
    /// Legacy field names of provider settings that were set under v8 `oauth.providers.*`. They
    /// are zeroed for API-key executions by [`Config::for_api_key`].
    #[serde(skip)]
    pub oauth_only_fields: BTreeSet<String>,
    /// Runtime mirror of the provider-wide Codex setting for API handlers.
    #[serde(skip)]
    pub codex_response_steering: bool,
    #[serde(rename = "proxy-url")]
    pub proxy_url: String,
    /// false | true | "chat" | "passthrough".
    #[serde(rename = "disable-image-generation")]
    pub disable_image_generation: DisableImageGenerationMode,
    /// Must start with "gpt-" (case-insensitive); otherwise the runtime default is used.
    #[serde(
        rename = "gpt-image-2-base-model",
        skip_serializing_if = "String::is_empty"
    )]
    pub gpt_image_2_base_model: String,
    /// Go duration string ("3h"); empty or invalid means the runtime default.
    #[serde(
        rename = "video-result-auth-cache-ttl",
        skip_serializing_if = "String::is_empty"
    )]
    pub video_result_auth_cache_ttl: String,
    #[serde(rename = "force-model-prefix")]
    pub force_model_prefix: bool,
    #[serde(rename = "request-log")]
    pub request_log: bool,
    /// Runtime mirror of the provider-wide Codex setting for API handlers.
    #[serde(skip)]
    pub codex_orphan_delegation_compatibility: bool,
    #[serde(rename = "claude-code")]
    pub claude_code: ClaudeCodeConfig,
    /// Keys clients must present to this proxy.
    #[serde(rename = "api-keys")]
    pub api_keys: Vec<String>,
    #[serde(rename = "passthrough-headers")]
    pub passthrough_headers: bool,
    pub streaming: StreamingConfig,
    #[serde(
        rename = "nonstream-keepalive-interval",
        skip_serializing_if = "is_zero"
    )]
    pub nonstream_keepalive_interval: i64,

    // --- Config ---
    /// Empty binds all interfaces.
    pub host: String,
    /// Zero means "not set"; the server falls back to 8317.
    pub port: i64,
    #[serde(rename = "trusted-proxies")]
    pub trusted_proxies: Vec<String>,
    pub tls: TlsConfig,
    /// Runtime-only (populated from the Home JWT); never read from or written to YAML.
    #[serde(skip)]
    pub home: HomeConfig,
    #[serde(rename = "credential-concurrency")]
    pub credential_concurrency: CredentialConcurrencyConfig,
    #[serde(rename = "credential-in-flight")]
    pub credential_in_flight: CredentialInFlightConfig,
    #[serde(rename = "remote-management")]
    pub remote_management: RemoteManagement,
    pub plugins: PluginsConfig,
    /// Empty means `~/.cli-proxy-api` (see [`crate::paths::resolve_auth_dir`]).
    #[serde(rename = "auth-dir")]
    pub auth_dir: String,
    pub debug: bool,
    pub pprof: PprofConfig,
    pub discovery: DiscoveryConfig,
    #[serde(rename = "commercial-mode")]
    pub commercial_mode: bool,
    #[serde(rename = "logging-to-file")]
    pub logging_to_file: bool,
    #[serde(rename = "logs-max-total-size-mb")]
    pub logs_max_total_size_mb: i64,
    #[serde(rename = "error-logs-max-files")]
    pub error_logs_max_files: i64,
    #[serde(rename = "usage-statistics-enabled")]
    pub usage_statistics_enabled: bool,
    #[serde(rename = "redis-usage-queue-retention-seconds")]
    pub redis_usage_queue_retention_seconds: i64,
    #[serde(rename = "disable-cooling")]
    pub disable_cooling: bool,
    #[serde(rename = "save-cooldown-status")]
    pub save_cooldown_status: bool,
    /// 0 keeps the legacy 60s default; negative disables transient cooldowns.
    #[serde(rename = "transient-error-cooldown-seconds")]
    pub transient_error_cooldown_seconds: i64,
    #[serde(rename = "auth-auto-refresh-workers")]
    pub auth_auto_refresh_workers: i64,
    #[serde(rename = "request-retry")]
    pub request_retry: i64,
    #[serde(rename = "max-retry-credentials")]
    pub max_retry_credentials: i64,
    #[serde(rename = "max-retry-interval")]
    pub max_retry_interval: i64,
    #[serde(rename = "quota-exceeded")]
    pub quota_exceeded: QuotaExceeded,
    pub routing: RoutingConfig,
    #[serde(rename = "ws-auth")]
    pub websocket_auth: bool,
    #[serde(
        rename = "antigravity-signature-cache-enabled",
        skip_serializing_if = "Option::is_none"
    )]
    pub antigravity_signature_cache_enabled: Option<bool>,
    #[serde(
        rename = "antigravity-signature-bypass-strict",
        skip_serializing_if = "Option::is_none"
    )]
    pub antigravity_signature_bypass_strict: Option<bool>,
    pub antigravity: AntigravityConfig,
    pub devin: DevinConfig,
    #[serde(rename = "gemini-api-key")]
    pub gemini_key: Vec<GeminiKey>,
    #[serde(rename = "interactions-api-key")]
    pub interactions_key: Vec<GeminiKey>,
    #[serde(rename = "codex-api-key")]
    pub codex_key: Vec<CodexKey>,
    #[serde(rename = "xai-api-key")]
    pub xai_key: Vec<XaiKey>,
    #[serde(rename = "meta-api-key")]
    pub meta_key: Vec<MetaKey>,
    pub xai: XaiConfig,
    pub codex: CodexConfig,
    #[serde(rename = "codex-header-defaults")]
    pub codex_header_defaults: CodexHeaderDefaults,
    pub claude: ClaudeConfig,
    #[serde(rename = "claude-api-key")]
    pub claude_key: Vec<ClaudeKey>,
    #[serde(rename = "claude-header-defaults")]
    pub claude_header_defaults: ClaudeHeaderDefaults,
    /// Globally disables Claude request cloaking unless a credential overrides it.
    #[serde(rename = "disable-claude-cloak-mode")]
    pub disable_claude_cloak_mode: bool,
    #[serde(rename = "openai-compatibility")]
    pub openai_compatibility: Vec<OpenAiCompatibility>,
    #[serde(rename = "vertex-api-key")]
    pub vertex_compat_api_key: Vec<VertexCompatKey>,
    #[serde(
        rename = "oauth-excluded-models",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub oauth_excluded_models: BTreeMap<String, Vec<String>>,
    #[serde(
        rename = "oauth-model-alias",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub oauth_model_alias: BTreeMap<String, Vec<OAuthModelAlias>>,
    #[serde(
        rename = "oauth-request-scoped-errors",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub oauth_request_scoped_errors: BTreeMap<String, Vec<RequestScopedErrorRule>>,
    #[serde(rename = "oauth-settings", skip_serializing_if = "BTreeMap::is_empty")]
    pub oauth_settings: BTreeMap<String, Vec<OAuthModelSetting>>,
    pub payload: PayloadConfig,
}

/// The Go zero value (`Config{}`): nothing pre-set, no defaults applied.
impl Default for Config {
    fn default() -> Self {
        Self {
            client: ClientConfig::default(),
            oauth_only_fields: BTreeSet::new(),
            codex_response_steering: false,
            proxy_url: String::new(),
            disable_image_generation: DisableImageGenerationMode::Off,
            gpt_image_2_base_model: String::new(),
            video_result_auth_cache_ttl: String::new(),
            force_model_prefix: false,
            request_log: false,
            codex_orphan_delegation_compatibility: false,
            claude_code: ClaudeCodeConfig::default(),
            api_keys: Vec::new(),
            passthrough_headers: false,
            streaming: StreamingConfig::default(),
            nonstream_keepalive_interval: 0,
            host: String::new(),
            port: 0,
            trusted_proxies: Vec::new(),
            tls: TlsConfig::default(),
            home: HomeConfig::default(),
            credential_concurrency: CredentialConcurrencyConfig::default(),
            credential_in_flight: CredentialInFlightConfig::default(),
            remote_management: RemoteManagement::default(),
            plugins: PluginsConfig::default(),
            auth_dir: String::new(),
            debug: false,
            pprof: PprofConfig::default(),
            discovery: DiscoveryConfig::default(),
            commercial_mode: false,
            logging_to_file: false,
            logs_max_total_size_mb: 0,
            error_logs_max_files: 0,
            usage_statistics_enabled: false,
            redis_usage_queue_retention_seconds: 0,
            disable_cooling: false,
            save_cooldown_status: false,
            transient_error_cooldown_seconds: 0,
            auth_auto_refresh_workers: 0,
            request_retry: 0,
            max_retry_credentials: 0,
            max_retry_interval: 0,
            quota_exceeded: QuotaExceeded::default(),
            routing: RoutingConfig::default(),
            websocket_auth: false,
            antigravity_signature_cache_enabled: None,
            antigravity_signature_bypass_strict: None,
            antigravity: AntigravityConfig::default(),
            devin: DevinConfig::default(),
            gemini_key: Vec::new(),
            interactions_key: Vec::new(),
            codex_key: Vec::new(),
            xai_key: Vec::new(),
            meta_key: Vec::new(),
            xai: XaiConfig::default(),
            codex: CodexConfig::default(),
            codex_header_defaults: CodexHeaderDefaults::default(),
            claude: ClaudeConfig::default(),
            claude_key: Vec::new(),
            claude_header_defaults: ClaudeHeaderDefaults::default(),
            disable_claude_cloak_mode: false,
            openai_compatibility: Vec::new(),
            vertex_compat_api_key: Vec::new(),
            oauth_excluded_models: BTreeMap::new(),
            oauth_model_alias: BTreeMap::new(),
            oauth_request_scoped_errors: BTreeMap::new(),
            oauth_settings: BTreeMap::new(),
            payload: PayloadConfig::default(),
        }
    }
}

impl Config {
    /// The values Go pre-sets before decoding YAML (`LoadConfigOptional` / `ParseConfigBytes`),
    /// so absent keys keep them. See docs/survey/conductor-config.md section 15.1.
    pub fn parse_defaults() -> Self {
        Self {
            error_logs_max_files: 10,
            redis_usage_queue_retention_seconds: 60,
            websocket_auth: true,
            pprof: PprofConfig::parse_defaults(),
            discovery: DiscoveryConfig::parse_defaults(),
            remote_management: RemoteManagement::parse_defaults(),
            credential_in_flight: CredentialInFlightConfig::parse_defaults(),
            ..Self::default()
        }
    }

    /// The config returned when the file is missing/empty/invalid in optional (cloud standby)
    /// mode: zero values except the in-flight defaults, with plugins normalised.
    pub fn empty_optional() -> Self {
        let mut cfg = Self {
            credential_in_flight: CredentialInFlightConfig::parse_defaults(),
            ..Self::default()
        };
        cfg.normalize_plugins_config();
        cfg
    }
}

// ---------------------------------------------------------------------------------------------
// Small sections
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    pub codex: CodexClientConfig,
}

/// Codex client compatibility (`client.codex.*`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexClientConfig {
    /// Optimizes official Codex multi-agent requests across providers.
    #[serde(rename = "optimize-multi-agent-v2")]
    pub optimize_multi_agent_v2: bool,
    /// Advertises freeform apply_patch only for supported models.
    #[serde(rename = "enable-apply-patch")]
    pub enable_apply_patch: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeCodeConfig {
    /// Disables model ID cloaking in Anthropic model list responses.
    #[serde(rename = "disable-cloaking-model-list")]
    pub disable_cloaking_model_list: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamingConfig {
    /// SSE/WebSocket keep-alive interval; <= 0 disables.
    #[serde(rename = "keepalive-seconds", skip_serializing_if = "is_zero")]
    pub keepalive_seconds: i64,
    /// Bootstrap retries before the first byte; <= 0 disables.
    #[serde(rename = "bootstrap-retries", skip_serializing_if = "is_zero")]
    pub bootstrap_retries: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    pub enable: bool,
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default = "PprofConfig::parse_defaults")]
pub struct PprofConfig {
    pub enable: bool,
    pub addr: String,
}

impl PprofConfig {
    pub fn parse_defaults() -> Self {
        Self {
            enable: false,
            addr: DEFAULT_PPROF_ADDR.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryInterfacesConfig {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// mDNS / DNS-SD advertising (see `cpa-discovery` and the runtime `service::discovery` manager).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default = "DiscoveryConfig::parse_defaults")]
pub struct DiscoveryConfig {
    pub enabled: bool,
    /// Empty means `CPA-<ShortID>`.
    #[serde(rename = "service-name")]
    pub service_name: String,
    #[serde(rename = "service-type")]
    pub service_type: String,
    pub subtypes: Vec<String>,
    pub interfaces: DiscoveryInterfacesConfig,
    /// Defaults to true when unset.
    #[serde(rename = "auth-required")]
    pub auth_required: Option<bool>,
    #[serde(rename = "advertise-management")]
    pub advertise_management: bool,
}

impl DiscoveryConfig {
    pub fn parse_defaults() -> Self {
        Self {
            service_type: DEFAULT_DISCOVERY_SERVICE_TYPE.to_string(),
            subtypes: default_discovery_subtypes(),
            ..Self::default()
        }
    }
}

/// Management API settings (`remote-management`, v8 `management`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default = "RemoteManagement::parse_defaults")]
pub struct RemoteManagement {
    #[serde(rename = "allow-remote")]
    pub allow_remote: bool,
    /// Plaintext or bcrypt hash. Plaintext is hashed at load time.
    #[serde(rename = "secret-key")]
    pub secret_key: String,
    #[serde(rename = "disable-control-panel")]
    pub disable_control_panel: bool,
    #[serde(rename = "disable-auto-update-panel")]
    pub disable_auto_update_panel: bool,
    #[serde(rename = "panel-github-repository")]
    pub panel_github_repository: String,
    /// Remote management API base URL for TUI client mode.
    #[serde(rename = "base-url", skip_serializing_if = "String::is_empty")]
    pub base_url: String,
}

impl RemoteManagement {
    pub fn parse_defaults() -> Self {
        Self {
            panel_github_repository: DEFAULT_PANEL_GITHUB_REPOSITORY.to_string(),
            ..Self::default()
        }
    }
}

/// Runtime-only Home control plane settings. Never read from YAML.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HomeConfig {
    pub enabled: bool,
    pub node_id: String,
    pub host: String,
    pub port: i64,
    pub disable_cluster_discovery: bool,
    pub tls: HomeTlsConfig,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HomeTlsConfig {
    pub enable: bool,
    pub server_name: String,
    pub insecure_skip_verify: bool,
    pub ca_cert: String,
    pub client_cert: String,
    pub client_key: String,
    pub use_target_server_name: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaExceeded {
    #[serde(rename = "switch-project")]
    pub switch_project: bool,
    #[serde(rename = "switch-preview-model")]
    pub switch_preview_model: bool,
    /// Credits-based last-resort fallback for Claude models on Antigravity.
    #[serde(rename = "antigravity-credits")]
    pub antigravity_credits: bool,
}

/// Credential selection behaviour (same path in both YAML layouts).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// "round-robin" (default), "weighted-round-robin", "fill-first", and the Rust-only
    /// "smart-quota" (usage-aware selection from passive quota observations).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub strategy: String,
    #[serde(rename = "session-affinity", skip_serializing_if = "is_false")]
    pub session_affinity: bool,
    /// Go duration string; default 1h.
    #[serde(
        rename = "session-affinity-ttl",
        skip_serializing_if = "String::is_empty"
    )]
    pub session_affinity_ttl: String,
    /// Default true; ignored when session affinity is off.
    #[serde(
        rename = "session-affinity-subagents",
        skip_serializing_if = "Option::is_none"
    )]
    pub session_affinity_subagents: Option<bool>,
    /// Rust-only. `smart-quota` keeps this percent of the 5h window free by preferring credentials
    /// with at least that much headroom. Default 30, clamped to 0..=100; ignored by other strategies.
    #[serde(
        rename = "smart-quota-reserve-percent",
        skip_serializing_if = "Option::is_none"
    )]
    pub smart_quota_reserve_percent: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// Request-scoped errors, plugins, provider-wide settings
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestScopedErrorRule {
    #[serde(skip_serializing_if = "is_zero")]
    pub status: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub r#match: Vec<String>,
    #[serde(rename = "match-regexr", skip_serializing_if = "Vec::is_empty")]
    pub match_regexr: Vec<String>,
    /// "stop", "stop-and-cooldown", "continue", "continue-and-cooldown".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub action: String,
}

/// Dynamic plugin settings. Parsed and preserved; the plugin host itself is not implemented.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    pub enabled: bool,
    pub dir: String,
    #[serde(rename = "store-sources", skip_serializing_if = "Vec::is_empty")]
    pub store_sources: Vec<String>,
    #[serde(rename = "store-auth", skip_serializing_if = "Vec::is_empty")]
    pub store_auth: Vec<PluginStoreAuth>,
    /// Changes when Home-managed plugin credentials change.
    #[serde(rename = "auth-revision", skip_serializing_if = "is_zero")]
    pub auth_revision: i64,
    pub configs: BTreeMap<String, PluginInstanceConfig>,
}

/// Auth rule for plugin store registry, metadata and artifact requests
/// (`internal/pluginstore.AuthConfig`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginStoreAuth {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub r#match: String,
    #[serde(rename = "apply-to", skip_serializing_if = "Vec::is_empty")]
    pub apply_to: Vec<String>,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(rename = "token-env", skip_serializing_if = "String::is_empty")]
    pub token_env: String,
    #[serde(rename = "username-env", skip_serializing_if = "String::is_empty")]
    pub username_env: String,
    #[serde(rename = "password-env", skip_serializing_if = "String::is_empty")]
    pub password_env: String,
    #[serde(rename = "header-name", skip_serializing_if = "String::is_empty")]
    pub header_name: String,
    #[serde(rename = "header-value-env", skip_serializing_if = "String::is_empty")]
    pub header_value_env: String,
    #[serde(rename = "allow-insecure", skip_serializing_if = "is_false")]
    pub allow_insecure: bool,
}

/// One plugin's settings: the host-owned fields plus the full original YAML subtree, which is
/// what gets written back (plugins own the rest of their configuration).
#[derive(Debug, Clone, PartialEq)]
pub struct PluginInstanceConfig {
    /// `Some(false)` when absent from a mapping; `None` only for a null entry.
    pub enabled: Option<bool>,
    pub priority: i64,
    pub raw: Value,
}

impl Default for PluginInstanceConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            priority: 0,
            raw: Value::Null,
        }
    }
}

impl PluginInstanceConfig {
    /// Builds an instance from a YAML subtree (port of `PluginInstanceConfig.UnmarshalYAML`).
    pub fn from_yaml(value: Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let mut out = Self {
            enabled: Some(false),
            priority: 0,
            raw: value,
        };
        if let Value::Mapping(map) = &out.raw {
            // Null keeps the default (yaml.v3 skips null scalars); other scalars decode leniently.
            if let Some(v) = map.get("enabled").filter(|v| !v.is_null()) {
                out.enabled = Some(
                    crate::lenient::from_value::<bool>(v.clone())
                        .map_err(|e| format!("parse plugin enabled: {e}"))?,
                );
            }
            if let Some(v) = map.get("priority").filter(|v| !v.is_null()) {
                out.priority = crate::lenient::from_value::<i64>(v.clone())
                    .map_err(|e| format!("parse plugin priority: {e}"))?;
            }
        }
        Ok(out)
    }
}

impl<'de> Deserialize<'de> for PluginInstanceConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        Self::from_yaml(raw).map_err(serde::de::Error::custom)
    }
}

impl Serialize for PluginInstanceConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.raw {
            Value::Null => Value::Mapping(serde_yaml_ng::Mapping::new()).serialize(serializer),
            raw => raw.serialize(serializer),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    /// Scopes Claude quota cooldowns to the requested model instead of the whole credential.
    #[serde(rename = "model-level-cooling")]
    pub model_level_cooling: bool,
}

/// Measured Claude Code software baseline used as fingerprint fallback.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeHeaderDefaults {
    #[serde(rename = "user-agent")]
    pub user_agent: String,
    #[serde(rename = "package-version")]
    pub package_version: String,
    #[serde(rename = "runtime-version")]
    pub runtime_version: String,
    pub os: String,
    pub arch: String,
    pub timeout: String,
    pub timezone: String,
    #[serde(
        rename = "stabilize-device-profile",
        skip_serializing_if = "Option::is_none"
    )]
    pub stabilize_device_profile: Option<bool>,
}

/// Fallback headers injected into Codex OAuth requests when the client omits them.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexHeaderDefaults {
    #[serde(rename = "user-agent")]
    pub user_agent: String,
    #[serde(rename = "beta-features")]
    pub beta_features: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct XaiConfig {
    #[serde(rename = "inject-x-search")]
    pub inject_x_search: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DevinConfig {
    #[serde(rename = "sensitive-words", skip_serializing_if = "Vec::is_empty")]
    pub sensitive_words: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AntigravityConfig {
    #[serde(rename = "sensitive-words", skip_serializing_if = "Vec::is_empty")]
    pub sensitive_words: Vec<String>,
    #[serde(
        rename = "connection-pool",
        skip_serializing_if = "AntigravityConnectionPoolConfig::is_empty"
    )]
    pub connection_pool: AntigravityConnectionPoolConfig,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AntigravityConnectionPoolConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Default "30s", capped at 210s.
    #[serde(rename = "idle-conn-timeout", skip_serializing_if = "String::is_empty")]
    pub idle_conn_timeout: String,
    /// Default 2.
    #[serde(
        rename = "max-idle-conns-per-host",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_idle_conns_per_host: Option<i64>,
}

impl AntigravityConnectionPoolConfig {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.idle_conn_timeout.is_empty()
            && self.max_idle_conns_per_host.is_none()
    }
}

/// Provider-wide Codex behaviour (`codex`, v8 `oauth.providers.codex`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    #[serde(rename = "disable-codex-cloaking")]
    pub disable_codex_cloaking: bool,
    #[serde(rename = "stream-bootstrap-buffering")]
    pub stream_bootstrap_buffering: bool,
    /// "0" | "20s" | seconds | none/off/unlimited/disabled/never.
    #[serde(
        rename = "stream-bootstrap-timeout",
        skip_serializing_if = "String::is_empty"
    )]
    pub stream_bootstrap_timeout: String,
    #[serde(rename = "orphan-delegation-compatibility")]
    pub orphan_delegation_compatibility: bool,
    #[serde(rename = "model-level-cooling")]
    pub model_level_cooling: bool,
    #[serde(rename = "live-media-relay")]
    pub live_media_relay: CodexLiveMediaRelayConfig,
    #[serde(rename = "response-steering")]
    pub response_steering: bool,
}

impl CodexConfig {
    /// Maximum time to hold bootstrap frames; zero means unlimited (port of
    /// `StreamBootstrapTimeoutDuration`).
    pub fn stream_bootstrap_timeout_duration(&self) -> GoDuration {
        let raw = self.stream_bootstrap_timeout.trim();
        let lowered = raw.to_ascii_lowercase();
        if raw.is_empty()
            || raw == "0"
            || matches!(
                lowered.as_str(),
                "none" | "unlimited" | "disabled" | "off" | "never"
            )
        {
            return GoDuration(0);
        }
        if let Ok(d) = GoDuration::parse(raw)
            && d.0 >= 0
        {
            return d;
        }
        if let Ok(secs) = raw.parse::<i64>()
            && (0..=i64::MAX / GoDuration::SECOND).contains(&secs)
        {
            return GoDuration::from_secs(secs);
        }
        GoDuration(0)
    }
}

/// In-process Codex Live WebRTC gateway. Parsed and preserved; not implemented.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct CodexLiveMediaRelayConfig {
    pub enabled: bool,
    #[serde(rename = "max-sessions")]
    pub max_sessions: i64,
    #[serde(rename = "disable-private-remote-ips")]
    pub disable_private_remote_ips: bool,
    #[serde(rename = "public-ip")]
    pub public_ip: String,
    #[serde(rename = "udp-port-min")]
    pub udp_port_min: u16,
    #[serde(rename = "udp-port-max")]
    pub udp_port_max: u16,
    #[serde(rename = "ice-servers")]
    pub ice_servers: Vec<CodexLiveIceServer>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexLiveIceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CodexLiveMediaRelayPlain {
    enabled: bool,
    #[serde(rename = "max-sessions")]
    max_sessions: i64,
    #[serde(rename = "disable-private-remote-ips")]
    disable_private_remote_ips: bool,
    #[serde(rename = "public-ip")]
    public_ip: String,
    #[serde(rename = "udp-port-min")]
    udp_port_min: u16,
    #[serde(rename = "udp-port-max")]
    udp_port_max: u16,
    #[serde(rename = "ice-servers")]
    ice_servers: Vec<CodexLiveIceServer>,
}

/// Supports the deprecated `allow-private-remote-ips` (inverse of `disable-private-remote-ips`).
impl<'de> Deserialize<'de> for CodexLiveMediaRelayConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = Value::deserialize(deserializer)?;
        // Same order as Go: decode the plain fields first, then the two private-IP spellings.
        let mut flags = Vec::new();
        if let Value::Mapping(map) = &value {
            for key in ["allow-private-remote-ips", "disable-private-remote-ips"] {
                // Presence counts even for null, which decodes to false.
                let flag = map
                    .get(key)
                    .map(|v| {
                        crate::lenient::from_value::<bool>(v.clone()).map_err(|e| {
                            D::Error::custom(format!("decode codex.live-media-relay.{key}: {e}"))
                        })
                    })
                    .transpose();
                flags.push(flag);
            }
        }
        let plain: CodexLiveMediaRelayPlain =
            crate::lenient::from_value(value).map_err(D::Error::custom)?;
        let mut flags = flags.into_iter();
        let allow = flags.next().transpose()?.flatten();
        let disable = flags.next().transpose()?.flatten();
        if allow.is_some() && disable.is_some() {
            return Err(D::Error::custom(
                "codex.live-media-relay cannot set both allow-private-remote-ips and disable-private-remote-ips",
            ));
        }
        let mut out = Self {
            enabled: plain.enabled,
            max_sessions: plain.max_sessions,
            disable_private_remote_ips: plain.disable_private_remote_ips,
            public_ip: plain.public_ip,
            udp_port_min: plain.udp_port_min,
            udp_port_max: plain.udp_port_max,
            ice_servers: plain.ice_servers,
        };
        if let Some(allow) = allow {
            out.disable_private_remote_ips = !allow;
            tracing::warn!(
                "codex.live-media-relay.allow-private-remote-ips is deprecated; use disable-private-remote-ips with the inverse value"
            );
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------
// Credential concurrency / in-flight (Home-owned)
// ---------------------------------------------------------------------------------------------

/// Lifecycle settings owned by Home. Field presence is tracked so that only absent values get the
/// legacy defaults (an explicit 0 stays 0 and fails validation).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct CredentialConcurrencyConfig {
    #[serde(rename = "lifecycle-config-revision")]
    pub lifecycle_config_revision: i64,
    #[serde(rename = "observation-barrier-revision")]
    pub observation_barrier_revision: i64,
    #[serde(rename = "cpa-heartbeat-timeout")]
    pub cpa_heartbeat_timeout: GoDuration,
    #[serde(rename = "cpa-cancel-bound")]
    pub cpa_cancel_bound: GoDuration,
    #[serde(rename = "reclaim-grace")]
    pub reclaim_grace: GoDuration,
    #[serde(rename = "cleanup-interval")]
    pub cleanup_interval: GoDuration,
    #[serde(rename = "release-flush-interval")]
    pub release_flush_interval: GoDuration,
    #[serde(rename = "release-max-backoff")]
    pub release_max_backoff: GoDuration,
    #[serde(rename = "busy-retry-min")]
    pub busy_retry_min: GoDuration,
    #[serde(rename = "busy-retry-max")]
    pub busy_retry_max: GoDuration,
    #[serde(rename = "max-limit")]
    pub max_limit: i64,

    #[serde(skip)]
    pub(crate) present: ConcurrencyPresence,
}

/// Which `credential-concurrency` fields were present in the YAML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ConcurrencyPresence {
    pub lifecycle_config_revision: bool,
    pub cpa_heartbeat_timeout: bool,
    pub cpa_cancel_bound: bool,
    pub reclaim_grace: bool,
    pub cleanup_interval: bool,
    pub release_flush_interval: bool,
    pub release_max_backoff: bool,
    pub busy_retry_min: bool,
    pub busy_retry_max: bool,
    pub max_limit: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CredentialConcurrencyRaw {
    #[serde(rename = "lifecycle-config-revision")]
    lifecycle_config_revision: Option<i64>,
    #[serde(rename = "observation-barrier-revision")]
    observation_barrier_revision: Option<i64>,
    #[serde(rename = "cpa-heartbeat-timeout")]
    cpa_heartbeat_timeout: Option<GoDuration>,
    #[serde(rename = "cpa-cancel-bound")]
    cpa_cancel_bound: Option<GoDuration>,
    #[serde(rename = "reclaim-grace")]
    reclaim_grace: Option<GoDuration>,
    #[serde(rename = "cleanup-interval")]
    cleanup_interval: Option<GoDuration>,
    #[serde(rename = "release-flush-interval")]
    release_flush_interval: Option<GoDuration>,
    #[serde(rename = "release-max-backoff")]
    release_max_backoff: Option<GoDuration>,
    #[serde(rename = "busy-retry-min")]
    busy_retry_min: Option<GoDuration>,
    #[serde(rename = "busy-retry-max")]
    busy_retry_max: Option<GoDuration>,
    #[serde(rename = "max-limit")]
    max_limit: Option<i64>,
}

impl<'de> Deserialize<'de> for CredentialConcurrencyConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if value.is_null() {
            return Ok(Self::default());
        }
        // Presence counts even when the value is null (the field then keeps its zero value).
        let has = |key: &str| value.as_mapping().is_some_and(|m| m.contains_key(key));
        let present = ConcurrencyPresence {
            lifecycle_config_revision: has("lifecycle-config-revision"),
            cpa_heartbeat_timeout: has("cpa-heartbeat-timeout"),
            cpa_cancel_bound: has("cpa-cancel-bound"),
            reclaim_grace: has("reclaim-grace"),
            cleanup_interval: has("cleanup-interval"),
            release_flush_interval: has("release-flush-interval"),
            release_max_backoff: has("release-max-backoff"),
            busy_retry_min: has("busy-retry-min"),
            busy_retry_max: has("busy-retry-max"),
            max_limit: has("max-limit"),
        };
        let raw: CredentialConcurrencyRaw =
            crate::lenient::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            lifecycle_config_revision: raw.lifecycle_config_revision.unwrap_or(0),
            observation_barrier_revision: raw.observation_barrier_revision.unwrap_or(0),
            cpa_heartbeat_timeout: raw.cpa_heartbeat_timeout.unwrap_or_default(),
            cpa_cancel_bound: raw.cpa_cancel_bound.unwrap_or_default(),
            reclaim_grace: raw.reclaim_grace.unwrap_or_default(),
            cleanup_interval: raw.cleanup_interval.unwrap_or_default(),
            release_flush_interval: raw.release_flush_interval.unwrap_or_default(),
            release_max_backoff: raw.release_max_backoff.unwrap_or_default(),
            busy_retry_min: raw.busy_retry_min.unwrap_or_default(),
            busy_retry_max: raw.busy_retry_max.unwrap_or_default(),
            max_limit: raw.max_limit.unwrap_or(0),
            present,
        })
    }
}

pub const DEFAULT_IN_FLIGHT_MAX_PART_BYTES: i64 = 256 * 1024;
pub const DEFAULT_IN_FLIGHT_MAX_PART_COUNT: i64 = 64;
pub const DEFAULT_IN_FLIGHT_MAX_REVISION_BYTES: i64 = 16 * 1024 * 1024;
pub const DEFAULT_IN_FLIGHT_MAX_AGGREGATE_GROUPS: i64 = 100_000;
pub const DEFAULT_IN_FLIGHT_MAX_DETAILS: i64 = 10_000;
pub const DEFAULT_IN_FLIGHT_MAX_STRING_BYTES: i64 = 256;

/// In-flight credential observation snapshots.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default = "CredentialInFlightConfig::parse_defaults")]
pub struct CredentialInFlightConfig {
    #[serde(rename = "snapshot-interval")]
    pub snapshot_interval: String,
    #[serde(rename = "stale-after")]
    pub stale_after: String,
    #[serde(rename = "max-part-bytes")]
    pub max_part_bytes: i64,
    #[serde(rename = "max-part-count")]
    pub max_part_count: i64,
    #[serde(rename = "max-revision-bytes")]
    pub max_revision_bytes: i64,
    #[serde(rename = "max-aggregate-groups")]
    pub max_aggregate_groups: i64,
    #[serde(rename = "max-details")]
    pub max_details: i64,
    #[serde(rename = "max-string-bytes")]
    pub max_string_bytes: i64,
    #[serde(rename = "staging-retention")]
    pub staging_retention: String,
}

impl CredentialInFlightConfig {
    /// `DefaultCredentialInFlightConfig`.
    pub fn parse_defaults() -> Self {
        Self {
            snapshot_interval: "2s".to_string(),
            stale_after: "10s".to_string(),
            max_part_bytes: DEFAULT_IN_FLIGHT_MAX_PART_BYTES,
            max_part_count: DEFAULT_IN_FLIGHT_MAX_PART_COUNT,
            max_revision_bytes: DEFAULT_IN_FLIGHT_MAX_REVISION_BYTES,
            max_aggregate_groups: DEFAULT_IN_FLIGHT_MAX_AGGREGATE_GROUPS,
            max_details: DEFAULT_IN_FLIGHT_MAX_DETAILS,
            max_string_bytes: DEFAULT_IN_FLIGHT_MAX_STRING_BYTES,
            staging_retention: "1m".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// disable-image-generation
// ---------------------------------------------------------------------------------------------

/// Four-state `disable-image-generation`: false | true | "chat" | "passthrough".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisableImageGenerationMode {
    #[default]
    Off,
    All,
    Chat,
    Passthrough,
}

impl DisableImageGenerationMode {
    /// Port of `parseDisableImageGenerationString`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "" | "false" | "0" | "off" | "no" => Ok(Self::Off),
            "true" | "1" | "on" | "yes" => Ok(Self::All),
            "chat" => Ok(Self::Chat),
            "passthrough" => Ok(Self::Passthrough),
            other => Err(format!(
                "invalid disable-image-generation value {other:?} (allowed: true, false, chat, passthrough)"
            )),
        }
    }
}

impl std::fmt::Display for DisableImageGenerationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "false",
            Self::All => "true",
            Self::Chat => "chat",
            Self::Passthrough => "passthrough",
        })
    }
}

impl Serialize for DisableImageGenerationMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => serializer.serialize_bool(false),
            Self::All => serializer.serialize_bool(true),
            Self::Chat => serializer.serialize_str("chat"),
            Self::Passthrough => serializer.serialize_str("passthrough"),
        }
    }
}

impl<'de> Deserialize<'de> for DisableImageGenerationMode {
    /// Parses the scalar's source text, like Go (a typed bool first, then the text of any other
    /// scalar), so `true`, `chat`, `01`, `on` all go through [`Self::parse`].
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let text = String::deserialize(deserializer)
            .map_err(|_| D::Error::custom("invalid disable-image-generation value"))?;
        Self::parse(&text).map_err(D::Error::custom)
    }
}

// ---------------------------------------------------------------------------------------------
// OAuth channel settings, payload rules
// ---------------------------------------------------------------------------------------------

/// Maps an upstream model name to a client-visible alias for a channel. With `fork` the alias is
/// listed in addition to the original model ID.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OAuthModelAlias {
    pub name: String,
    pub alias: String,
    #[serde(skip_serializing_if = "is_false")]
    pub fork: bool,
    #[serde(rename = "display-name", skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(rename = "force-mapping", skip_serializing_if = "is_false")]
    pub force_mapping: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OAuthModelSetting {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub alias: String,
    #[serde(rename = "max-context-length", skip_serializing_if = "is_zero")]
    pub max_context_length: i64,
}

/// Finds the best matching setting: an exact alias match on `model_id` beats a name match, and
/// later entries override earlier ones within the same specificity.
pub fn resolve_oauth_model_setting<'a>(
    settings: &'a [OAuthModelSetting],
    model_id: &str,
    metadata_model_id: &str,
    model_name: &str,
) -> Option<&'a OAuthModelSetting> {
    let norm = |s: &str| s.trim().to_lowercase();
    let (id, meta_id, name) = (norm(model_id), norm(metadata_model_id), norm(model_name));
    let mut alias_match = None;
    let mut name_match = None;
    for entry in settings {
        let entry_name = norm(&entry.name);
        if entry_name.is_empty() {
            continue;
        }
        let entry_alias = norm(&entry.alias);
        if !entry_alias.is_empty() && !id.is_empty() && id == entry_alias {
            alias_match = Some(entry);
        } else if (entry_alias.is_empty() || entry_alias == id)
            && (id == entry_name
                || (!meta_id.is_empty() && meta_id == entry_name)
                || (!name.is_empty() && name == entry_name))
        {
            name_match = Some(entry);
        }
    }
    alias_match.or(name_match)
}

/// Default/override/filter rules applied to provider payloads.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PayloadConfig {
    pub default: Vec<PayloadRule>,
    #[serde(rename = "default-raw")]
    pub default_raw: Vec<PayloadRule>,
    #[serde(rename = "override")]
    pub r#override: Vec<PayloadRule>,
    #[serde(rename = "override-raw")]
    pub override_raw: Vec<PayloadRule>,
    pub filter: Vec<PayloadFilterRule>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PayloadFilterRule {
    pub models: Vec<PayloadModelRule>,
    /// JSON paths (gjson/sjson syntax) to remove.
    pub params: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PayloadRule {
    pub models: Vec<PayloadModelRule>,
    /// JSON path -> value. For `*-raw` rules, string values are raw JSON fragments.
    pub params: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PayloadModelRule {
    /// Model name or wildcard pattern ("gpt-*", "*-5").
    pub name: String,
    pub protocol: String,
    pub headers: BTreeMap<String, String>,
    #[serde(rename = "from-protocol")]
    pub from_protocol: String,
    pub r#match: Vec<BTreeMap<String, Value>>,
    #[serde(rename = "not-match")]
    pub not_match: Vec<BTreeMap<String, Value>>,
    pub exist: Vec<String>,
    #[serde(rename = "not-exist")]
    pub not_exist: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Upstream API-key entries
// ---------------------------------------------------------------------------------------------

/// Thinking/reasoning capability of a configured model (the registry type). In YAML the fields
/// are kebab-case (`zero-allowed`); the registry type reads both spellings.
pub use cpa_core::registry::ThinkingSupport;

/// serde adapter writing `Option<ThinkingSupport>` with kebab-case YAML field names.
mod thinking_yaml {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{ThinkingSupport, is_false, is_zero};

    #[derive(Serialize)]
    struct Kebab<'a> {
        #[serde(skip_serializing_if = "is_zero")]
        min: i64,
        #[serde(skip_serializing_if = "is_zero")]
        max: i64,
        #[serde(rename = "zero-allowed", skip_serializing_if = "is_false")]
        zero_allowed: bool,
        #[serde(rename = "dynamic-allowed", skip_serializing_if = "is_false")]
        dynamic_allowed: bool,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        levels: &'a Vec<String>,
    }

    pub fn serialize<S: Serializer>(
        value: &Option<ThinkingSupport>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(t) => Kebab {
                min: t.min,
                max: t.max,
                zero_allowed: t.zero_allowed,
                dynamic_allowed: t.dynamic_allowed,
                levels: &t.levels,
            }
            .serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<ThinkingSupport>, D::Error> {
        Option::deserialize(deserializer)
    }
}

/// Cloaking for non-Claude-Code clients ("auto" | "always" | "never").
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CloakConfig {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub mode: String,
    #[serde(rename = "strict-mode", skip_serializing_if = "is_false")]
    pub strict_mode: bool,
    #[serde(rename = "sensitive-words", skip_serializing_if = "Vec::is_empty")]
    pub sensitive_words: Vec<String>,
    #[serde(rename = "cache-user-id", skip_serializing_if = "Option::is_none")]
    pub cache_user_id: Option<bool>,
}

/// Claude API key entry.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeKey {
    #[serde(rename = "api-key")]
    pub api_key: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: i64,
    /// Omitted means 1; non-positive excludes the credential under WRR; max 1,000,000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    #[serde(rename = "base-url")]
    pub base_url: String,
    #[serde(rename = "proxy-url")]
    pub proxy_url: String,
    pub models: Vec<ClaudeModel>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(rename = "excluded-models", skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
    #[serde(
        rename = "rebuild-mid-system-message",
        skip_serializing_if = "is_false"
    )]
    pub rebuild_mid_system_message: bool,
    #[serde(rename = "disable-cooling", skip_serializing_if = "Option::is_none")]
    pub disable_cooling: Option<bool>,
    /// Nil or negative means "use the global request-retry".
    #[serde(rename = "request-retry", skip_serializing_if = "Option::is_none")]
    pub request_retry: Option<i64>,
    #[serde(
        rename = "request-scoped-errors",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloak: Option<CloakConfig>,
    /// "" | "claude-code-cli" (legacy alias "oauth-cli").
    #[serde(
        rename = "fingerprint-profile",
        skip_serializing_if = "String::is_empty"
    )]
    pub fingerprint_profile: String,
    /// Retained for configuration compatibility.
    #[serde(rename = "experimental-cch-signing", skip_serializing_if = "is_false")]
    pub experimental_cch_signing: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeModel {
    pub name: String,
    pub alias: String,
    #[serde(rename = "display-name", skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(rename = "max-context-length", skip_serializing_if = "is_zero")]
    pub max_context_length: i64,
    #[serde(rename = "force-mapping", skip_serializing_if = "is_false")]
    pub force_mapping: bool,
    #[serde(rename = "is-compat", skip_serializing_if = "is_false")]
    pub is_compat: bool,
    #[serde(
        default,
        with = "thinking_yaml",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking: Option<ThinkingSupport>,
}

/// Codex API key entry; xAI and Meta entries share this structure.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexKey {
    #[serde(rename = "api-key")]
    pub api_key: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    #[serde(rename = "base-url")]
    pub base_url: String,
    /// Responses API websocket transport.
    #[serde(skip_serializing_if = "is_false")]
    pub websockets: bool,
    #[serde(rename = "alpha-search", skip_serializing_if = "is_false")]
    pub alpha_search: bool,
    #[serde(rename = "proxy-url")]
    pub proxy_url: String,
    pub models: Vec<CodexModel>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(rename = "excluded-models", skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
    #[serde(rename = "disable-cooling", skip_serializing_if = "Option::is_none")]
    pub disable_cooling: Option<bool>,
    /// Overrides `codex.disable-codex-cloaking` for this credential.
    #[serde(
        rename = "disable-codex-cloaking",
        skip_serializing_if = "Option::is_none"
    )]
    pub disable_codex_cloaking: Option<bool>,
    #[serde(rename = "request-retry", skip_serializing_if = "Option::is_none")]
    pub request_retry: Option<i64>,
    #[serde(
        rename = "request-scoped-errors",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
}

pub type XaiKey = CodexKey;
pub type MetaKey = CodexKey;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexModel {
    pub name: String,
    pub alias: String,
    #[serde(rename = "display-name", skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(rename = "max-context-length", skip_serializing_if = "is_zero")]
    pub max_context_length: i64,
    #[serde(rename = "force-mapping", skip_serializing_if = "is_false")]
    pub force_mapping: bool,
    #[serde(rename = "is-compat", skip_serializing_if = "is_false")]
    pub is_compat: bool,
    #[serde(
        rename = "support-configuration-update",
        skip_serializing_if = "is_false"
    )]
    pub support_configuration_update: bool,
    #[serde(
        default,
        with = "thinking_yaml",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking: Option<ThinkingSupport>,
}

pub type XaiModel = CodexModel;
pub type MetaModel = CodexModel;

/// Gemini API key entry; Interactions entries share this structure.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiKey {
    #[serde(rename = "api-key")]
    pub api_key: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    #[serde(rename = "base-url", skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(rename = "proxy-url", skip_serializing_if = "String::is_empty")]
    pub proxy_url: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<GeminiModel>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(rename = "excluded-models", skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
    #[serde(rename = "disable-cooling", skip_serializing_if = "Option::is_none")]
    pub disable_cooling: Option<bool>,
    #[serde(rename = "request-retry", skip_serializing_if = "Option::is_none")]
    pub request_retry: Option<i64>,
    #[serde(
        rename = "request-scoped-errors",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiModel {
    pub name: String,
    pub alias: String,
    #[serde(rename = "display-name", skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(rename = "max-context-length", skip_serializing_if = "is_zero")]
    pub max_context_length: i64,
    #[serde(rename = "force-mapping", skip_serializing_if = "is_false")]
    pub force_mapping: bool,
    #[serde(rename = "is-compat", skip_serializing_if = "is_false")]
    pub is_compat: bool,
    #[serde(
        default,
        with = "thinking_yaml",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking: Option<ThinkingSupport>,
}

/// OpenAI-compatible provider (`openai-compatibility`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiCompatibility {
    pub name: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: i64,
    #[serde(skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    #[serde(rename = "base-url")]
    pub base_url: String,
    #[serde(rename = "api-key-entries", skip_serializing_if = "Vec::is_empty")]
    pub api_key_entries: Vec<OpenAiCompatibilityApiKey>,
    pub models: Vec<OpenAiCompatibilityModel>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(rename = "support-prompt-cache-key", skip_serializing_if = "is_false")]
    pub support_prompt_cache_key: bool,
    #[serde(rename = "disable-cooling", skip_serializing_if = "Option::is_none")]
    pub disable_cooling: Option<bool>,
    #[serde(rename = "request-retry", skip_serializing_if = "Option::is_none")]
    pub request_retry: Option<i64>,
    #[serde(
        rename = "request-scoped-errors",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiCompatibilityApiKey {
    #[serde(rename = "api-key")]
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<i64>,
    #[serde(rename = "proxy-url", skip_serializing_if = "String::is_empty")]
    pub proxy_url: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiCompatibilityModel {
    pub name: String,
    pub alias: String,
    #[serde(rename = "display-name", skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(rename = "max-context-length", skip_serializing_if = "is_zero")]
    pub max_context_length: i64,
    #[serde(rename = "force-mapping", skip_serializing_if = "is_false")]
    pub force_mapping: bool,
    /// Callable through /v1/images/generations and /v1/images/edits.
    #[serde(skip_serializing_if = "is_false")]
    pub image: bool,
    #[serde(rename = "input-modalities", skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    #[serde(rename = "output-modalities", skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<String>,
    #[serde(rename = "is-compat", skip_serializing_if = "is_false")]
    pub is_compat: bool,
    #[serde(rename = "use-max-completion-tokens", skip_serializing_if = "is_false")]
    pub use_max_completion_tokens: bool,
    #[serde(
        default,
        with = "thinking_yaml",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking: Option<ThinkingSupport>,
}

/// Vertex-compatible API key (third-party services with Vertex-style paths).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VertexCompatKey {
    #[serde(rename = "api-key")]
    pub api_key: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    #[serde(rename = "base-url", skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(rename = "proxy-url", skip_serializing_if = "String::is_empty")]
    pub proxy_url: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<VertexCompatModel>,
    #[serde(rename = "excluded-models", skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
    #[serde(rename = "disable-cooling", skip_serializing_if = "Option::is_none")]
    pub disable_cooling: Option<bool>,
    #[serde(rename = "request-retry", skip_serializing_if = "Option::is_none")]
    pub request_retry: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VertexCompatModel {
    pub name: String,
    pub alias: String,
    #[serde(rename = "display-name", skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(rename = "force-mapping", skip_serializing_if = "is_false")]
    pub force_mapping: bool,
    #[serde(
        default,
        with = "thinking_yaml",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking: Option<ThinkingSupport>,
}

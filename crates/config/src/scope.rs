//! OAuth-only provider settings (port of `oauth_scope.go`).
//!
//! Settings written under v8 `oauth.providers.*` are recorded in
//! [`Config::oauth_only_fields`](crate::Config::oauth_only_fields) and must not influence API-key
//! executions; [`Config::for_api_key`] returns a view with them zeroed.

use std::borrow::Cow;

use serde_yaml_ng::Value;

use crate::error::Result;
use crate::layout::v8_paths;
use crate::types::*;
use crate::yamlpath::{delete_yaml_path, set_yaml_path, yaml_path};

impl Config {
    /// A request-local view without v8 OAuth-only overrides. Legacy global settings and explicit
    /// API-key settings keep their semantics. The shared config is never modified.
    pub fn for_api_key(&self) -> Cow<'_, Config> {
        if self.oauth_only_fields.is_empty() {
            return Cow::Borrowed(self);
        }
        let mut filtered = self.clone();
        for path in &self.oauth_only_fields {
            zero_oauth_field(&mut filtered, path);
        }
        if self
            .oauth_only_fields
            .contains("codex.orphan-delegation-compatibility")
        {
            filtered.codex_orphan_delegation_compatibility = false;
        }
        if self.oauth_only_fields.contains("codex.response-steering") {
            filtered.codex_response_steering = false;
        }
        filtered.oauth_only_fields.clear();
        Cow::Owned(filtered)
    }

    /// Serialises the config for snapshots (legacy layout) while keeping OAuth scope: fields that
    /// were set under `oauth.providers.*` are emitted at their v8 paths.
    pub fn to_yaml_value(&self) -> Result<Value> {
        let mut root = serde_yaml_ng::to_value(self)?;
        for (old, current) in v8_paths() {
            if !self.oauth_only_fields.contains(*old) {
                continue;
            }
            // Fields skipped by `omitempty` are emitted as null.
            let value = yaml_path(&root, old).cloned().unwrap_or(Value::Null);
            delete_yaml_path(&mut root, old);
            set_yaml_path(&mut root, current, value);
        }
        // Plugin options may carry the parser's raw-text wrappers; snapshots use plain scalars.
        crate::rawparse::untag_raw(&mut root);
        Ok(root)
    }
}

/// Resets one legacy leaf field (identified by its legacy YAML path) to its zero value. Returns
/// false for a path it does not know (a test asserts every OAuth-only v8 path is handled).
fn zero_oauth_field(cfg: &mut Config, path: &str) -> bool {
    let relay = &mut cfg.codex.live_media_relay;
    match path {
        "ws-auth" => cfg.websocket_auth = false,
        "claude-code.disable-cloaking-model-list" => {
            cfg.claude_code.disable_cloaking_model_list = false
        }
        "codex.disable-codex-cloaking" => cfg.codex.disable_codex_cloaking = false,
        "codex.stream-bootstrap-buffering" => cfg.codex.stream_bootstrap_buffering = false,
        "codex.stream-bootstrap-timeout" => cfg.codex.stream_bootstrap_timeout.clear(),
        "codex.orphan-delegation-compatibility" => {
            cfg.codex.orphan_delegation_compatibility = false
        }
        "codex.model-level-cooling" => cfg.codex.model_level_cooling = false,
        "codex.response-steering" => cfg.codex.response_steering = false,
        "codex.live-media-relay.enabled" => relay.enabled = false,
        "codex.live-media-relay.max-sessions" => relay.max_sessions = 0,
        "codex.live-media-relay.disable-private-remote-ips" => {
            relay.disable_private_remote_ips = false
        }
        "codex.live-media-relay.public-ip" => relay.public_ip.clear(),
        "codex.live-media-relay.udp-port-min" => relay.udp_port_min = 0,
        "codex.live-media-relay.udp-port-max" => relay.udp_port_max = 0,
        "codex.live-media-relay.ice-servers" => relay.ice_servers.clear(),
        "codex-header-defaults.user-agent" => cfg.codex_header_defaults.user_agent.clear(),
        "codex-header-defaults.beta-features" => cfg.codex_header_defaults.beta_features.clear(),
        "claude.model-level-cooling" => cfg.claude.model_level_cooling = false,
        "claude-header-defaults.user-agent" => cfg.claude_header_defaults.user_agent.clear(),
        "claude-header-defaults.package-version" => {
            cfg.claude_header_defaults.package_version.clear()
        }
        "claude-header-defaults.runtime-version" => {
            cfg.claude_header_defaults.runtime_version.clear()
        }
        "claude-header-defaults.os" => cfg.claude_header_defaults.os.clear(),
        "claude-header-defaults.arch" => cfg.claude_header_defaults.arch.clear(),
        "claude-header-defaults.timeout" => cfg.claude_header_defaults.timeout.clear(),
        "claude-header-defaults.timezone" => cfg.claude_header_defaults.timezone.clear(),
        "claude-header-defaults.stabilize-device-profile" => {
            cfg.claude_header_defaults.stabilize_device_profile = None
        }
        "disable-claude-cloak-mode" => cfg.disable_claude_cloak_mode = false,
        "antigravity-signature-cache-enabled" => cfg.antigravity_signature_cache_enabled = None,
        "antigravity-signature-bypass-strict" => cfg.antigravity_signature_bypass_strict = None,
        "antigravity.sensitive-words" => cfg.antigravity.sensitive_words.clear(),
        "antigravity.connection-pool.enabled" => cfg.antigravity.connection_pool.enabled = None,
        "antigravity.connection-pool.idle-conn-timeout" => {
            cfg.antigravity.connection_pool.idle_conn_timeout.clear()
        }
        "antigravity.connection-pool.max-idle-conns-per-host" => {
            cfg.antigravity.connection_pool.max_idle_conns_per_host = None
        }
        "quota-exceeded.antigravity-credits" => cfg.quota_exceeded.antigravity_credits = false,
        "xai.inject-x-search" => cfg.xai.inject_x_search = false,
        "devin.sensitive-words" => cfg.devin.sensitive_words.clear(),
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::is_oauth_only_path;

    #[test]
    fn every_oauth_only_v8_path_is_zeroed_for_api_keys() {
        let mut cfg = Config::default();
        let oauth_only: Vec<&str> = v8_paths()
            .filter(|(_, cur)| is_oauth_only_path(cur))
            .map(|(old, _)| *old)
            .collect();
        assert!(!oauth_only.is_empty());
        for old in oauth_only {
            assert!(
                zero_oauth_field(&mut cfg, old),
                "{old} is OAuth-only in v8 but not handled by for_api_key"
            );
        }
    }
}

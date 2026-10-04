//! Config scoping for API-key executions (Go: executor/oauth_scope_executor.go).
//!
//! Go binds `cfg.ForAPIKey()` into a value copy of each executor so OAuth-only provider settings
//! (written under `oauth.providers.*`) cannot influence API-key credentials. In Rust the
//! executor's `for_api_key()` returns an executor built over the view this returns.

use std::sync::Arc;

use cpa_config::Config;

/// A request-local config view without OAuth-only overrides. Shares the original `Arc` when the
/// config has no OAuth-only fields (the common case); the shared config is never modified.
pub fn config_for_api_key(cfg: &Arc<Config>) -> Arc<Config> {
    if cfg.oauth_only_fields.is_empty() {
        return Arc::clone(cfg);
    }
    Arc::new(cfg.for_api_key().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_arc_without_oauth_only_fields_and_filters_otherwise() {
        let plain = Arc::new(Config::default());
        assert!(Arc::ptr_eq(&config_for_api_key(&plain), &plain));

        let mut scoped = Config::default();
        scoped.codex_header_defaults.user_agent = "oauth-agent".into();
        scoped.oauth_only_fields.insert("codex-header-defaults.user-agent".into());
        let scoped = Arc::new(scoped);
        let view = config_for_api_key(&scoped);
        assert!(!Arc::ptr_eq(&view, &scoped));
        assert!(view.codex_header_defaults.user_agent.is_empty());
        assert_eq!(scoped.codex_header_defaults.user_agent, "oauth-agent");
    }
}

//! Public embedder surface (Go `sdk/pluginstore`): client constructors and re-exports of
//! the store types and constants.

use std::sync::Arc;

use chrono::{DateTime, Utc};

pub use crate::auth::{
    AUTH_TYPE_BASIC, AUTH_TYPE_BEARER, AUTH_TYPE_GITHUB_TOKEN, AUTH_TYPE_HEADER, AUTH_TYPE_NONE, AuthConfig,
    REQUEST_KIND_ARTIFACT, REQUEST_KIND_METADATA, REQUEST_KIND_REGISTRY, ResolvedAuthConfig, Secret,
    auth_configured, clear_resolved_auth_configs, normalize_auth_configs, plugin_auth_configured,
    resolved_auth_for_request, validate_resolved_auth_config,
};
pub use crate::direct::select_artifact;
pub use crate::github::{Client, Release, ReleaseAsset, release_version};
pub use crate::home_sync::{PLUGIN_SYNC_SCHEMA_VERSION, PluginSyncItem, PluginSyncRequest, PluginSyncResponse};
pub use crate::http::HttpDoer;
pub use crate::install::{InstallOptions, InstallResult};
pub use crate::manifest::Manifest;
pub use crate::registry::{
    Artifact, DEFAULT_REGISTRY_URL, DEFAULT_SOURCE_ID, DEFAULT_SOURCE_NAME, INSTALL_TYPE_DIRECT,
    INSTALL_TYPE_GITHUB_RELEASE, InstallPlan, Platform, Plugin, Registry, SCHEMA_VERSION, SCHEMA_VERSION_V2,
    Source, Version, default_source, github_repository_parts, normalize_sources, plugin_artifacts,
    plugin_install_type, plugin_platforms, source_id, validate_plugin,
};
pub use crate::version::update_available;

/// `NewClient`: registry client without credentials.
pub fn new_client(http_client: Option<Arc<dyn HttpDoer>>, registry_url: &str) -> Client {
    Client { http_client, registry_url: registry_url.trim().to_string(), ..Default::default() }
}

/// `NewClientWithAuth`: environment-backed auth rules (normalized).
pub fn new_client_with_auth(
    http_client: Option<Arc<dyn HttpDoer>>,
    registry_url: &str,
    auth: &[AuthConfig],
) -> Client {
    Client {
        http_client,
        registry_url: registry_url.trim().to_string(),
        auth: normalize_auth_configs(auth),
        ..Default::default()
    }
}

/// `NewClientWithResolvedAuth`: credentials supplied by Home, no expiry.
pub fn new_client_with_resolved_auth(
    http_client: Option<Arc<dyn HttpDoer>>,
    registry_url: &str,
    auth: Vec<ResolvedAuthConfig>,
) -> Client {
    new_client_with_resolved_auth_expiry(http_client, registry_url, auth, None)
}

/// `NewClientWithResolvedAuthExpiry`: resolved credentials that stop working at `expires_at`.
pub fn new_client_with_resolved_auth_expiry(
    http_client: Option<Arc<dyn HttpDoer>>,
    registry_url: &str,
    auth: Vec<ResolvedAuthConfig>,
    expires_at: Option<DateTime<Utc>>,
) -> Client {
    Client {
        http_client,
        registry_url: registry_url.trim().to_string(),
        resolved_auth: auth,
        resolved_auth_expires_at: expires_at,
        ..Default::default()
    }
}

impl Client {
    /// Copy with the given proxy/egress identity for shared GitHub API cooldowns. Use the
    /// same scope for clients with the same egress and credentials; an empty scope denotes
    /// direct connections. This does not configure the HTTP transport.
    pub fn with_network_scope(&self, network_scope: &str) -> Client {
        let mut client = self.clone();
        client.network_scope = network_scope.trim().to_string();
        client
    }

    /// Overwrites resolved credentials held by the client.
    pub fn clear_auth(&mut self) {
        clear_resolved_auth_configs(&mut self.resolved_auth);
        self.resolved_auth = Vec::new();
        self.resolved_auth_expires_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Context;
    use crate::http::{DoError, Headers, HttpRequest, HttpResponse};
    use crate::ratelimit::GitHubRateLimiter;
    use async_trait::async_trait;
    use parking_lot::Mutex;

    #[test]
    fn network_scope_is_normalized_without_modifying_original() {
        let clients = [
            new_client(None, ""),
            new_client_with_auth(None, "", &[]),
            new_client_with_resolved_auth(None, "", Vec::new()),
            new_client_with_resolved_auth_expiry(None, "", Vec::new(), None),
        ];
        for client in clients {
            let scoped = client.with_network_scope("  http://proxy.example:8080  ");
            assert_eq!(scoped.network_scope, "http://proxy.example:8080");
            assert_eq!(client.network_scope, "");
            assert_eq!(scoped.with_network_scope("").network_scope, "");
        }
    }

    struct CountingDoer {
        calls: Mutex<u32>,
    }

    #[async_trait]
    impl HttpDoer for CountingDoer {
        async fn get(&self, _request: HttpRequest) -> Result<HttpResponse, DoError> {
            let mut calls = self.calls.lock();
            *calls += 1;
            if *calls == 1 {
                let mut headers = Headers::new();
                headers.set("Retry-After", "3600");
                return Ok(HttpResponse::from_bytes(429, headers, "limited"));
            }
            Ok(HttpResponse::from_bytes(200, Headers::new(), r#"{"tag_name":"v1.0.0"}"#))
        }
    }

    #[tokio::test]
    async fn network_scope_isolates_and_shares_cooldowns() {
        let doer = Arc::new(CountingDoer { calls: Mutex::new(0) });
        let mut client = new_client(Some(doer.clone()), "");
        client.rate_limiter = Some(Arc::new(GitHubRateLimiter::new()));
        let ctx = Context::background();
        let mut plugin = Plugin { repository: "https://github.com/test/network-scope".into(), ..Default::default() };

        let proxy_a = client.with_network_scope("http://proxy-a.example");
        let err = proxy_a.fetch_latest_release(&ctx, &plugin).await.expect_err("limited");
        assert!(err.rate_limit().is_some());
        assert_eq!(*doer.calls.lock(), 1);
        for scope in ["http://proxy-b.example", ""] {
            client
                .with_network_scope(scope)
                .fetch_latest_release(&ctx, &plugin)
                .await
                .unwrap_or_else(|err| panic!("independent scope {scope:?} was blocked: {err}"));
        }
        assert_eq!(*doer.calls.lock(), 3);
        plugin.repository = "https://github.com/test/another-repository".into();
        let err = client
            .with_network_scope(" http://proxy-a.example ")
            .fetch_latest_release(&ctx, &plugin)
            .await
            .expect_err("same scope shares cooldown");
        assert!(err.rate_limit().is_some());
        assert_eq!(*doer.calls.lock(), 3);
    }
}

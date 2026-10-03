//! Request identity hashing and release cache keys (Go `request_identity.go`).

use sha2::{Digest, Sha256};

use crate::auth::Env;
use crate::error::Result;
use crate::github::Client;
use crate::http::Headers;
use crate::registry::{Plugin, github_repository_parts};
use crate::goturl::path_escape;

/// Credentials and request headers resolved once so a cache key and the request it
/// guards use the same snapshot (Go `preparedPluginStoreAuth`).
#[derive(Debug, Clone)]
pub(crate) struct PreparedAuth {
    pub(crate) request_url: String,
    pub(crate) kind: String,
    pub(crate) headers: Headers,
    pub(crate) authenticated: bool,
}

/// Hash of network scope, whether auth is applied and (when it is) the sorted auth
/// headers, so equivalent credentials share a key without being recoverable from it.
pub(crate) fn request_identity(network_scope: &str, headers: &Headers, authenticated: bool) -> String {
    let mut data = format!("{network_scope}\0{authenticated}\0").into_bytes();
    if authenticated {
        headers.write_to(&mut data);
    }
    hex::encode(Sha256::digest(&data))
}

fn latest_release_url(plugin: &Plugin) -> Result<String> {
    let (owner, repo) = github_repository_parts(&plugin.repository)?;
    Ok(format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        path_escape(&owner),
        path_escape(&repo)
    ))
}

impl Client {
    /// Identifies a repository under the effective credentials and network scope,
    /// without retaining credential material in the key.
    pub fn latest_release_cache_key(&self, plugin: &Plugin) -> Result<String> {
        self.prepare_latest_release(plugin).map(|(_, key)| key)
    }

    /// Binds the cache identity and the initial release request to the same credential
    /// snapshot, even when environment values change while the request is queued.
    pub fn prepare_latest_release(&self, plugin: &Plugin) -> Result<(Client, String)> {
        let request_url = latest_release_url(plugin)?;
        let (headers, authenticated) = self.auth_headers(&request_url, crate::auth::REQUEST_KIND_METADATA)?;
        let key = format!(
            "{}/{}",
            request_url.to_lowercase(),
            request_identity(&self.network_scope, &headers, authenticated)
        );
        let mut prepared = self.clone();
        prepared.prepared_auth = Some(std::sync::Arc::new(PreparedAuth {
            request_url,
            kind: crate::auth::REQUEST_KIND_METADATA.to_string(),
            headers,
            authenticated,
        }));
        Ok((prepared, key))
    }

    pub(crate) fn env(&self) -> Env<'_> {
        Env(self.env.as_ref())
    }
}

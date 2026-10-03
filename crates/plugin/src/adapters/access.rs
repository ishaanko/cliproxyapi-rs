//! Frontend (client request) authentication through plugins (Go: `adapters_auth.go`).

use std::collections::HashMap;
use std::sync::Arc;

use cpa_pluginapi::abi;
use cpa_pluginapi::api::{FrontendAuthRequest, FrontendAuthResponse};
use http::HeaderMap;

use crate::caps::Record;
use crate::convert::{headers_to_go, query_to_go};
use crate::ctx::CallCtx;
use crate::host::Host;

/// Outcome codes of a request authentication provider (Go: `sdkaccess.AuthError` codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessFailure {
    NotHandled,
    NoCredentials,
    InvalidCredential,
    Internal { message: String },
}

/// A successful authentication (Go: `sdkaccess.Result`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessResult {
    pub provider: String,
    pub principal: String,
    pub metadata: std::collections::BTreeMap<String, String>,
}

/// One plugin frontend auth provider registered with the request access chain.
pub struct AccessAdapter {
    host: Arc<Host>,
    record: Arc<Record>,
}

/// Registered providers in registration order plus the exclusive key (Go: the `sdk/access`
/// global registry).
#[derive(Default)]
pub(crate) struct AccessRegistry {
    pub order: Vec<String>,
    pub providers: HashMap<String, Arc<AccessAdapter>>,
    pub exclusive: Option<String>,
}

impl AccessAdapter {
    pub fn plugin_id(&self) -> &str {
        &self.record.id
    }

    /// `plugin:<pluginID>:<providerID>`; empty when the plugin does not answer (Go: `Identifier`).
    pub fn identifier(&self) -> String {
        if self.host.is_plugin_fused(&self.record.id) {
            return String::new();
        }
        let provider = self.record.frontend_identifier();
        let plugin_id = self.record.id.trim();
        if plugin_id.is_empty() || provider.is_empty() {
            return String::new();
        }
        format!("plugin:{plugin_id}:{provider}")
    }

    /// Authenticates one request (Go: `Authenticate`).
    pub async fn authenticate(
        &self,
        ctx: &CallCtx,
        method: &str,
        path: &str,
        headers: &HeaderMap,
        query: &[(String, String)],
        body: &[u8],
    ) -> Result<AccessResult, AccessFailure> {
        if !self.host.usable(&self.record) {
            return Err(AccessFailure::NotHandled);
        }
        let req = FrontendAuthRequest {
            method: method.to_string(),
            path: path.to_string(),
            headers: headers_to_go(headers),
            query: query_to_go(query),
            body: body.to_vec(),
        };
        let resp: FrontendAuthResponse = match self.host.rpc(&self.record, ctx, abi::METHOD_FRONTEND_AUTH_AUTHENTICATE, &req).await {
            Ok(r) => r,
            Err(_) => return Err(AccessFailure::NotHandled),
        };
        if !resp.authenticated {
            return Err(AccessFailure::NotHandled);
        }
        let provider = self.identifier();
        if provider.is_empty() {
            return Err(AccessFailure::NotHandled);
        }
        Ok(AccessResult { provider, principal: resp.principal, metadata: resp.metadata })
    }
}

impl Host {
    /// Rebuilds the frontend auth provider set from the active plugins (Go:
    /// `RegisterFrontendAuthProviders`).
    pub fn register_frontend_auth_providers(self: &Arc<Self>) {
        struct Candidate {
            key: String,
            plugin_id: String,
            priority: i64,
        }
        let mut next_keys: Vec<String> = Vec::new();
        let mut adapters: Vec<(String, Arc<AccessAdapter>)> = Vec::new();
        let mut best: Option<Candidate> = None;
        for rec in self.active_records() {
            if !rec.caps().frontend_auth_provider || self.is_plugin_fused(&rec.id) {
                continue;
            }
            let adapter = Arc::new(AccessAdapter { host: self.clone(), record: rec.clone() });
            let key = adapter.identifier().trim().to_string();
            if key.is_empty() {
                continue;
            }
            adapters.push((key.clone(), adapter));
            next_keys.push(key.clone());
            if rec.caps().frontend_auth_provider_exclusive {
                let cand = Candidate { key, plugin_id: rec.id.clone(), priority: rec.priority };
                let better = match &best {
                    None => true,
                    Some(b) => cand.priority > b.priority || (cand.priority == b.priority && cand.plugin_id < b.plugin_id),
                };
                if better {
                    best = Some(cand);
                }
            }
        }
        let mut reg = self.access.lock();
        for (key, adapter) in adapters {
            if !reg.providers.contains_key(&key) {
                reg.order.push(key.clone());
            }
            reg.providers.insert(key, adapter);
        }
        reg.exclusive = best.map(|b| b.key);
        // Prune providers of plugins that are no longer active.
        let stale: Vec<String> = {
            let mut st = self.state.lock();
            let keys: std::collections::HashSet<&String> = next_keys.iter().collect();
            let stale = st.access_provider_keys.iter().filter(|k| !keys.contains(k)).cloned().collect();
            st.access_provider_keys = next_keys.into_iter().collect();
            stale
        };
        for key in stale {
            if reg.providers.remove(&key).is_some() {
                reg.order.retain(|k| k != &key);
            }
        }
    }

    /// The providers the request access chain should consult after the built-in ones, honoring
    /// the exclusive provider (Go: `sdkaccess.RegisteredProviders`).
    pub fn frontend_auth_providers(&self) -> Vec<Arc<AccessAdapter>> {
        let reg = self.access.lock();
        if reg.order.is_empty() {
            return Vec::new();
        }
        if let Some(ex) = &reg.exclusive
            && let Some(p) = reg.providers.get(ex)
        {
            return vec![p.clone()];
        }
        reg.order.iter().filter_map(|k| reg.providers.get(k).cloned()).collect()
    }

    /// Whether a plugin has taken over client authentication exclusively.
    pub fn exclusive_frontend_auth_provider(&self) -> Option<String> {
        let reg = self.access.lock();
        reg.exclusive.as_ref().filter(|k| reg.providers.contains_key(*k)).cloned()
    }
}

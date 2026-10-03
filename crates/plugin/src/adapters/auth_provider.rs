//! Plugin auth providers: parse, login, poll and refresh (Go: `auth_provider.go`).

use std::sync::Arc;

use cpa_auth::Auth;
use cpa_pluginapi::abi;
use cpa_pluginapi::api::{
    AuthData, AuthLoginPollRequest, AuthLoginPollResponse, AuthLoginStartRequest, AuthLoginStartResponse, AuthParseRequest,
    AuthParseResponse, AuthRefreshRequest, AuthRefreshResponse,
};
use serde_json::{Map, Value};

use crate::caps::Record;
use crate::convert::{
    host_config_summary, normalize_provider_id, preserve_file_auth_priority, storage_json_from_auth,
};
use crate::ctx::CallCtx;
use crate::host::Host;

/// Failure of an auth-provider operation (Go: a returned `error`).
#[derive(Debug, Clone)]
pub struct AuthProviderError {
    pub message: String,
    pub status: i32,
}

impl std::fmt::Display for AuthProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<crate::client::PluginError> for AuthProviderError {
    fn from(e: crate::client::PluginError) -> Self {
        AuthProviderError { message: e.message, status: e.status }
    }
}

fn err(message: impl Into<String>) -> AuthProviderError {
    AuthProviderError { message: message.into(), status: 0 }
}

/// Result of a login/parse/refresh call: `handled` tells whether any plugin took it.
pub type Handled<T> = Result<Option<T>, AuthProviderError>;

impl Host {
    /// Identifiers of the active auth-provider plugins (Go: `AuthProviderIdentifiers`).
    pub fn auth_provider_identifiers(&self) -> Vec<String> {
        self.active_records()
            .iter()
            .filter_map(|r| self.auth_identifier(r))
            .filter(|id| !normalize_provider_id(id).is_empty())
            .collect()
    }

    pub fn has_auth_provider(&self, provider: &str) -> bool {
        self.auth_provider_record(provider).is_some()
    }

    /// The active plugin serving `provider` (Go: `authProviderRecord`).
    pub fn auth_provider_record(&self, provider: &str) -> Option<Arc<Record>> {
        let provider = normalize_provider_id(provider);
        if provider.is_empty() {
            return None;
        }
        self.active_records().into_iter().find(|r| {
            self.auth_identifier(r).is_some_and(|id| normalize_provider_id(&id) == provider)
        })
    }

    /// First parsed auth (Go: `ParseAuth`).
    pub async fn parse_auth(self: &Arc<Self>, ctx: &CallCtx, req: AuthParseRequest) -> Handled<Auth> {
        Ok(self.parse_auths(ctx, req).await?.and_then(|mut v| (!v.is_empty()).then(|| v.remove(0))))
    }

    /// Lets auth providers expand one credential payload into runtime auths (Go: `ParseAuths`).
    pub async fn parse_auths(self: &Arc<Self>, ctx: &CallCtx, req: AuthParseRequest) -> Handled<Vec<Auth>> {
        if !req.provider.trim().is_empty() {
            let Some(rec) = self.auth_provider_record(&req.provider) else { return Ok(None) };
            return self.call_parse_auths(ctx, &rec, req).await;
        }
        for rec in self.active_records() {
            if !rec.caps().auth_provider || self.is_plugin_fused(&rec.id) {
                continue;
            }
            match self.call_parse_auths(ctx, &rec, req.clone()).await {
                Ok(None) => {}
                other => return other,
            }
        }
        Ok(None)
    }

    async fn call_parse_auths(self: &Arc<Self>, ctx: &CallCtx, rec: &Arc<Record>, mut req: AuthParseRequest) -> Handled<Vec<Auth>> {
        if !self.usable(rec) || !rec.caps().auth_provider {
            return Ok(None);
        }
        let summary = host_config_summary(self.runtime_config().as_deref());
        if req.host.auth_dir.is_empty() {
            req.host = summary;
        }
        req.provider = normalize_provider_id(&req.provider);
        let provider_id = self.auth_identifier(rec).unwrap_or_default();
        if req.provider.is_empty() {
            req.provider = normalize_provider_id(&provider_id);
        }
        let resp: AuthParseResponse = self.rpc(rec, ctx, abi::METHOD_AUTH_PARSE, &req).await.map_err(AuthProviderError::from)?;
        if !resp.handled {
            return Ok(None);
        }
        let datas: Vec<AuthData> = if resp.auths.is_empty() { vec![resp.auth] } else { resp.auths };
        let mut auths = Vec::with_capacity(datas.len());
        for mut data in datas {
            if data.provider.trim().is_empty() {
                data.provider = req.provider.clone();
            }
            if data.provider.trim().is_empty() {
                data.provider = normalize_provider_id(&provider_id);
            }
            if normalize_provider_id(&data.provider).is_empty() {
                return Err(err(format!("auth provider {} returned auth without provider", rec.id)));
            }
            let Some(parsed) = self.auth_data_to_core_auth(&data, &req.path, &req.file_name) else {
                return Err(err(format!("auth provider {} returned invalid auth data", rec.id)));
            };
            auths.push(parsed);
        }
        Ok(Some(auths))
    }

    /// Starts a provider login flow (Go: `StartLogin`).
    pub async fn start_login(
        self: &Arc<Self>,
        ctx: &CallCtx,
        provider: &str,
        base_url: &str,
        metadata: Option<Map<String, Value>>,
    ) -> Handled<AuthLoginStartResponse> {
        let Some(rec) = self.auth_provider_record(provider) else { return Ok(None) };
        if !self.usable(&rec) {
            return Ok(None);
        }
        let req = AuthLoginStartRequest {
            provider: normalize_provider_id(provider),
            base_url: base_url.trim().to_string(),
            host: host_config_summary(self.runtime_config().as_deref()),
            metadata: metadata.unwrap_or_default(),
        };
        let resp: AuthLoginStartResponse = self.rpc_cb(&rec, ctx, abi::METHOD_AUTH_LOGIN_START, &req).await.map_err(AuthProviderError::from)?;
        Ok(Some(resp))
    }

    /// Polls a provider login flow (Go: `PollLogin`).
    pub async fn poll_login(
        self: &Arc<Self>,
        ctx: &CallCtx,
        provider: &str,
        state: &str,
        metadata: Option<Map<String, Value>>,
    ) -> Handled<AuthLoginPollResponse> {
        let Some(rec) = self.auth_provider_record(provider) else { return Ok(None) };
        if !self.usable(&rec) {
            return Ok(None);
        }
        let req = AuthLoginPollRequest {
            provider: normalize_provider_id(provider),
            state: state.trim().to_string(),
            host: host_config_summary(self.runtime_config().as_deref()),
            metadata: metadata.unwrap_or_default(),
        };
        let resp: AuthLoginPollResponse = self.rpc_cb(&rec, ctx, abi::METHOD_AUTH_LOGIN_POLL, &req).await.map_err(AuthProviderError::from)?;
        Ok(Some(resp))
    }

    /// Refreshes a plugin-provided credential (Go: `RefreshAuth`).
    pub async fn refresh_auth(self: &Arc<Self>, ctx: &CallCtx, auth: &Auth) -> Handled<Auth> {
        let Some(rec) = self.auth_provider_record(&auth.provider) else { return Ok(None) };
        if !rec.caps().auth_provider || !self.record_current(&rec) {
            return Ok(None);
        }
        let req = AuthRefreshRequest {
            auth_id: auth.id.clone(),
            auth_provider: auth.provider.clone(),
            storage_json: storage_json_from_auth(Some(auth)),
            metadata: auth.metadata.clone(),
            attributes: auth.attributes.clone(),
            host: host_config_summary(self.runtime_config().as_deref()),
        };
        let resp: AuthRefreshResponse = self.rpc_cb(&rec, ctx, abi::METHOD_AUTH_REFRESH, &req).await.map_err(AuthProviderError::from)?;
        let mut data = resp.auth;
        if data.provider.trim().is_empty() {
            data.provider = auth.provider.clone();
        }
        if data.id.trim().is_empty() {
            data.id = auth.id.clone();
        }
        if data.file_name.trim().is_empty() {
            data.file_name = auth.file_name.clone();
        }
        if data.label.trim().is_empty() {
            data.label = auth.label.clone();
        }
        if data.prefix.trim().is_empty() {
            data.prefix = auth.prefix.clone();
        }
        if data.proxy_url.trim().is_empty() {
            data.proxy_url = auth.proxy_url.clone();
        }
        if data.metadata.is_empty() {
            data.metadata = auth.metadata.clone();
        }
        if data.attributes.is_empty() {
            data.attributes = auth.attributes.clone();
        } else {
            for (k, v) in &auth.attributes {
                data.attributes.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        preserve_file_auth_priority(&mut data, auth);
        if data.storage_json.is_empty() {
            data.storage_json = storage_json_from_auth(Some(auth));
        }
        data.next_refresh_after = resp.next_refresh_after.or(auth.next_refresh_after);
        let path = auth.attributes.get(cpa_auth::types::ATTRIBUTE_PATH).cloned().unwrap_or_default();
        let Some(mut next) = self.auth_data_to_core_auth(&data, &path, &data.file_name) else {
            return Err(err("auth provider refresh returned invalid auth data"));
        };
        next.index = auth.index.clone();
        next.created_at = auth.created_at;
        next.updated_at = auth.updated_at;
        Ok(Some(next))
    }
}

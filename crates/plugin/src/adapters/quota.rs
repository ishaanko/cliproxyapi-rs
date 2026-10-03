//! Quota provider plugins (Go: `quota_provider.go`).

use std::collections::HashSet;
use std::sync::Arc;

use cpa_pluginapi::abi;
use cpa_pluginapi::api::{
    QuotaDescribeRequest, QuotaDescribeResponse, QuotaFetchRequest, QuotaFetchResponse, QuotaResetRequest, QuotaResetResponse,
};
use serde::Serialize;

use crate::caps::Record;
use crate::client::PluginError;
use crate::convert::{host_config_summary, normalize_provider_id};
use crate::ctx::CallCtx;
use crate::host::Host;

/// An active quota provider (Go: `RegisteredQuotaProviderInfo`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RegisteredQuotaProviderInfo {
    pub plugin_id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supported_providers: Vec<String>,
    pub supports_reset: bool,
}

/// `Ok(None)` mirrors Go's `handled == false`.
pub type QuotaResult<T> = Result<Option<T>, PluginError>;

impl Host {
    /// Unique quota provider identifiers (Go: `QuotaProviderIdentifiers`).
    pub fn quota_provider_identifiers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for rec in self.active_records() {
            if let Some(id) = self.quota_identifier(&rec)
                && !out.contains(&id)
            {
                out.push(id);
            }
        }
        out
    }

    /// Go `QuotaProviders`.
    pub async fn quota_providers(&self, ctx: &CallCtx) -> Vec<RegisteredQuotaProviderInfo> {
        let mut out = Vec::new();
        for rec in self.active_records() {
            if !rec.caps().quota_provider || self.is_plugin_fused(&rec.id) {
                continue;
            }
            let identifier = self.quota_identifier(&rec).unwrap_or_else(|| normalize_provider_id(&rec.id));
            let desc = self.call_quota_describe(ctx, &rec).await.ok().flatten().unwrap_or_default();
            let mut supported = desc.supported_providers.clone();
            if supported.is_empty() && !identifier.is_empty() {
                supported = vec![identifier.clone()];
            }
            let mut display_name = desc.display_name.clone();
            if display_name.is_empty() {
                display_name = rec.meta.name.clone();
                if display_name.is_empty() {
                    display_name = identifier.clone();
                }
            }
            out.push(RegisteredQuotaProviderInfo {
                plugin_id: rec.id.clone(),
                provider: identifier,
                display_name,
                supported_providers: supported,
                supports_reset: desc.supports_reset,
            });
        }
        out
    }

    pub async fn has_quota_provider(&self, ctx: &CallCtx, provider: &str) -> bool {
        self.quota_provider_record(ctx, provider).await.is_some()
    }

    pub fn has_quota_provider_for_plugin(&self, plugin_id: &str) -> bool {
        self.quota_provider_record_by_plugin(plugin_id).is_some()
    }

    /// Every provider key an active quota provider supports (Go: `QuotaSupportedProvidersSet`).
    pub async fn quota_supported_providers_set(&self, ctx: &CallCtx) -> HashSet<String> {
        let snap = self.snapshot();
        let mut out = HashSet::new();
        for rec in self.active_records_from(&snap) {
            if ctx.is_canceled() {
                return out;
            }
            if !rec.caps().quota_provider || self.is_plugin_fused(&rec.id) {
                continue;
            }
            if let Some(id) = self.quota_identifier(&rec) {
                out.insert(id);
            }
            let norm = normalize_provider_id(&rec.id);
            if !norm.is_empty() {
                out.insert(norm);
            }
            if rec.caps().auth_provider
                && let Some(id) = self.auth_identifier(&rec)
                && !id.is_empty()
            {
                out.insert(id);
            }
            for p in self.cached_quota_supported_providers(ctx, &snap, &rec).await {
                let clean = normalize_provider_id(&p);
                if !clean.is_empty() {
                    out.insert(clean);
                }
            }
        }
        out
    }

    async fn quota_provider_record(&self, ctx: &CallCtx, provider: &str) -> Option<Arc<Record>> {
        let provider = normalize_provider_id(provider);
        if provider.is_empty() {
            return None;
        }
        let snap = self.snapshot();
        let records = self.active_records_from(&snap);
        // First pass: exact quota identifier, plugin id or auth provider identifier.
        for rec in &records {
            if ctx.is_canceled() {
                return None;
            }
            if !rec.caps().quota_provider || self.is_plugin_fused(&rec.id) {
                continue;
            }
            if self.quota_identifier(rec).is_some_and(|id| id == provider) || normalize_provider_id(&rec.id) == provider {
                return Some(rec.clone());
            }
            if rec.caps().auth_provider && self.auth_identifier(rec).is_some_and(|id| id == provider) {
                return Some(rec.clone());
            }
        }
        // Second pass: providers declared by DescribeQuota.
        for rec in &records {
            if ctx.is_canceled() {
                return None;
            }
            if !rec.caps().quota_provider || self.is_plugin_fused(&rec.id) {
                continue;
            }
            let supported = self.cached_quota_supported_providers(ctx, &snap, rec).await;
            if supported.iter().any(|p| normalize_provider_id(p) == provider) {
                return Some(rec.clone());
            }
        }
        None
    }

    async fn cached_quota_supported_providers(&self, ctx: &CallCtx, snap: &Arc<crate::host::Snapshot>, rec: &Arc<Record>) -> Vec<String> {
        if let Some(cached) = snap.quota_supported.lock().get(&rec.id) {
            return cached.clone();
        }
        let Ok(Some(desc)) = self.call_quota_describe(ctx, rec).await else { return Vec::new() };
        let supported = desc.supported_providers.clone();
        snap.quota_supported.lock().insert(rec.id.clone(), supported.clone());
        supported
    }

    fn quota_provider_record_by_plugin(&self, plugin_id: &str) -> Option<Arc<Record>> {
        let plugin_id = plugin_id.trim();
        if plugin_id.is_empty() {
            return None;
        }
        self.active_records().into_iter().find(|r| r.id == plugin_id && r.caps().quota_provider && !self.is_plugin_fused(&r.id))
    }

    /// Go `DescribeQuota`.
    pub async fn describe_quota(&self, ctx: &CallCtx, plugin_id: &str) -> QuotaResult<QuotaDescribeResponse> {
        let mut rec = self.quota_provider_record_by_plugin(plugin_id);
        if rec.is_none() {
            rec = self.quota_provider_record(ctx, plugin_id).await;
        }
        match rec {
            Some(r) => self.call_quota_describe(ctx, &r).await,
            None => Ok(None),
        }
    }

    /// Go `FetchQuota`: by provider key, falling back to a plugin id.
    pub async fn fetch_quota(self: &Arc<Self>, ctx: &CallCtx, req: QuotaFetchRequest) -> QuotaResult<QuotaFetchResponse> {
        let mut rec = self.quota_provider_record(ctx, &req.provider).await;
        if rec.is_none() && !req.provider.is_empty() {
            rec = self.quota_provider_record_by_plugin(&req.provider);
        }
        match rec {
            Some(r) => self.call_quota_fetch(ctx, &r, req).await,
            None => Ok(None),
        }
    }

    pub async fn fetch_quota_by_plugin(self: &Arc<Self>, ctx: &CallCtx, plugin_id: &str, req: QuotaFetchRequest) -> QuotaResult<QuotaFetchResponse> {
        match self.quota_provider_record_by_plugin(plugin_id) {
            Some(r) => self.call_quota_fetch(ctx, &r, req).await,
            None => Ok(None),
        }
    }

    pub async fn reset_quota(self: &Arc<Self>, ctx: &CallCtx, req: QuotaResetRequest) -> QuotaResult<QuotaResetResponse> {
        let mut rec = self.quota_provider_record(ctx, &req.provider).await;
        if rec.is_none() && !req.provider.is_empty() {
            rec = self.quota_provider_record_by_plugin(&req.provider);
        }
        match rec {
            Some(r) => self.call_quota_reset(ctx, &r, req).await,
            None => Ok(None),
        }
    }

    pub async fn reset_quota_by_plugin(self: &Arc<Self>, ctx: &CallCtx, plugin_id: &str, req: QuotaResetRequest) -> QuotaResult<QuotaResetResponse> {
        match self.quota_provider_record_by_plugin(plugin_id) {
            Some(r) => self.call_quota_reset(ctx, &r, req).await,
            None => Ok(None),
        }
    }

    async fn call_quota_describe(&self, ctx: &CallCtx, rec: &Arc<Record>) -> QuotaResult<QuotaDescribeResponse> {
        if !self.usable(rec) {
            return Ok(None);
        }
        let req = QuotaDescribeRequest { plugin: rec.meta.clone() };
        self.rpc::<QuotaDescribeResponse>(rec, ctx, abi::METHOD_QUOTA_DESCRIBE, &req).await.map(Some)
    }

    /// Fills the request from the credential behind `auth_index` (Go: the shared prologue of
    /// `callQuotaFetch`/`callQuotaReset`).
    fn quota_request_fill(&self, auth_index: &str, auth_id: &mut String, provider: &mut String, storage_json: &mut Vec<u8>, metadata: &mut serde_json::Map<String, serde_json::Value>, attributes: &mut std::collections::BTreeMap<String, String>) {
        if auth_index.is_empty() {
            return;
        }
        if let Ok((auth, raw)) = self.auth_physical_json_by_index(auth_index) {
            if auth_id.is_empty() {
                *auth_id = auth.id.clone();
            }
            if provider.is_empty() {
                *provider = auth.provider.clone();
            }
            if storage_json.is_empty() {
                *storage_json = raw;
            }
            if metadata.is_empty() {
                *metadata = auth.metadata.clone();
            }
            if attributes.is_empty() {
                *attributes = auth.attributes.clone();
            }
        }
    }

    async fn call_quota_fetch(self: &Arc<Self>, ctx: &CallCtx, rec: &Arc<Record>, mut req: QuotaFetchRequest) -> QuotaResult<QuotaFetchResponse> {
        if !self.usable(rec) || !rec.caps().quota_provider {
            return Ok(None);
        }
        let idx = req.auth_index.clone();
        self.quota_request_fill(&idx, &mut req.auth_id, &mut req.provider, &mut req.storage_json, &mut req.metadata, &mut req.attributes);
        req.host = host_config_summary(self.runtime_config().as_deref());
        self.rpc_cb::<QuotaFetchResponse>(rec, ctx, abi::METHOD_QUOTA_FETCH, &req).await.map(Some)
    }

    async fn call_quota_reset(self: &Arc<Self>, ctx: &CallCtx, rec: &Arc<Record>, mut req: QuotaResetRequest) -> QuotaResult<QuotaResetResponse> {
        if !self.usable(rec) || !rec.caps().quota_provider {
            return Ok(None);
        }
        let idx = req.auth_index.clone();
        self.quota_request_fill(&idx, &mut req.auth_id, &mut req.provider, &mut req.storage_json, &mut req.metadata, &mut req.attributes);
        req.host = host_config_summary(self.runtime_config().as_deref());
        self.rpc_cb::<QuotaResetResponse>(rec, ctx, abi::METHOD_QUOTA_RESET, &req).await.map(Some)
    }
}

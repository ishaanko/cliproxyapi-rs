//! Usage plugins (Go: `RegisterUsagePlugins` and `usageAdapter.HandleUsage`): every usage record
//! is forwarded to each active plugin with the usage capability.

use std::collections::HashSet;
use std::sync::Arc;

use cpa_pluginapi::abi;
use cpa_pluginapi::api::{UsageDetail, UsageFailure, UsageRecord as PluginUsageRecord};
use cpa_runtime::usage::{UsageListener, UsageRecord, UsageTracker};

use crate::client::Empty;
use crate::convert::headers_to_go;
use crate::ctx::CallCtx;
use crate::host::Host;

/// Tracker key of a plugin's listener (Go: `"plugin:" + id`).
fn listener_key(plugin_id: &str) -> String {
    format!("plugin:{plugin_id}")
}

struct UsageAdapter {
    host: Arc<Host>,
    plugin_id: String,
}

impl UsageListener for UsageAdapter {
    fn handle_usage(&self, record: &UsageRecord) {
        let Some(rec) = self.host.active_records().into_iter().find(|r| r.id == self.plugin_id) else { return };
        if self.host.is_plugin_fused(&rec.id) || !rec.caps().usage_plugin {
            return;
        }
        let payload = plugin_usage_record(record);
        let host = self.host.clone();
        let task = async move {
            let ctx = CallCtx::background();
            if let Err(e) = host.rpc_cb::<Empty>(&rec, &ctx, abi::METHOD_USAGE_HANDLE, &payload).await {
                tracing::warn!("pluginhost: usage plugin {} failed: {e}", rec.id);
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(task);
            }
            Err(_) => {
                if let Some(h) = self.host.runtime_handle() {
                    h.spawn(task);
                }
            }
        }
    }
}

/// The record a plugin sees (Go: the `pluginapi.UsageRecord` literal in `HandleUsage`).
fn plugin_usage_record(r: &UsageRecord) -> PluginUsageRecord {
    let x = &r.extra;
    let nanos = |ms: i64| ms.saturating_mul(1_000_000);
    PluginUsageRecord {
        request_id: r.request_id.trim().to_string(),
        trace_id: x.trace_id.trim().to_string(),
        provider: r.provider.clone(),
        base_url: x.base_url.clone(),
        executor_type: r.executor_type.clone(),
        model: r.model.clone(),
        alias: r.alias.clone(),
        api_key: r.api_key.clone(),
        session_id: x.session_id.trim().to_string(),
        parent_session_id: x.parent_session_id.trim().to_string(),
        auth_id: x.auth_id.clone(),
        auth_index: r.auth_index.clone(),
        auth_type: r.auth_type.clone(),
        source: r.source.clone(),
        reasoning_effort: x.reasoning_effort.clone(),
        service_tier: x.service_tier.clone(),
        response_service_tier: x.response_service_tier.clone(),
        response_model: x.response_model.clone(),
        generate: x.generate.unwrap_or(true),
        stream: r.stream,
        requested_at: Some(r.timestamp),
        latency: nanos(r.latency_ms),
        ttft: nanos(r.ttft_ms),
        failed: r.failed,
        failure: UsageFailure { status_code: i64::from(r.fail.status_code), body: r.fail.body.clone() },
        detail: UsageDetail {
            input_tokens: r.tokens.input_tokens,
            output_tokens: r.tokens.output_tokens,
            reasoning_tokens: r.tokens.reasoning_tokens,
            cached_tokens: r.tokens.cached_tokens,
            cache_read_tokens: x.cache_read_tokens,
            cache_creation_tokens: x.cache_creation_tokens,
            total_tokens: r.tokens.total_tokens,
        },
        response_headers: headers_to_go(&x.response_headers),
    }
}

impl Host {
    /// Registers a listener on `tracker` for every active usage plugin and drops the listeners of
    /// plugins that are gone (Go: `RegisterUsagePlugins`).
    pub fn register_usage_plugins(self: &Arc<Self>, tracker: &UsageTracker) {
        let mut keys = HashSet::new();
        for rec in self.active_records() {
            if !rec.caps().usage_plugin || self.is_plugin_fused(&rec.id) {
                continue;
            }
            let key = listener_key(&rec.id);
            tracker.register_listener(&key, Arc::new(UsageAdapter { host: self.clone(), plugin_id: rec.id.clone() }));
            keys.insert(key);
        }
        let stale: Vec<String> = {
            let mut st = self.state.lock();
            let stale = st.usage_listener_keys.difference(&keys).cloned().collect();
            st.usage_listener_keys = keys;
            stale
        };
        for key in stale {
            tracker.unregister_listener(&key);
        }
    }
}

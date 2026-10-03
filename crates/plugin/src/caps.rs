//! A registered plugin: negotiated capabilities plus its RPC client (Go: `capabilityRecord` and
//! `pluginapi.Capabilities`, whose interface values here are flags on the shared RPC adapter).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use cpa_pluginapi::abi;
use cpa_pluginapi::api::{Header, PluginMetadata};
use crate::client::{GuardedClient, call_plugin_blocking, empty_request};

/// `capabilities` of the registration response (Go: `rpcCapabilities`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub model_registrar: bool,
    pub model_provider: bool,
    pub auth_provider: bool,
    pub frontend_auth_provider: bool,
    pub frontend_auth_provider_exclusive: bool,
    pub scheduler: bool,
    #[serde(skip_serializing_if = "cpa_pluginapi::wire::is_false")]
    pub scheduler_across_priorities: bool,
    pub model_router: bool,
    pub executor: bool,
    pub executor_model_scope: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub executor_input_formats: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub executor_output_formats: Vec<String>,
    pub request_translator: bool,
    pub request_normalizer: bool,
    pub request_interceptor: bool,
    pub request_lifecycle_plugin: bool,
    pub response_translator: bool,
    pub response_before_translator: bool,
    pub response_after_translator: bool,
    pub response_interceptor: bool,
    #[serde(rename = "response_stream_interceptor")]
    pub stream_chunk_interceptor: bool,
    pub websocket_response_observer: bool,
    pub thinking_applier: bool,
    pub usage_plugin: bool,
    pub command_line_plugin: bool,
    pub management_api: bool,
    pub quota_provider: bool,
}

impl Capabilities {
    /// Whether the plugin declares anything the host can use (Go: tail of `validPlugin`).
    pub fn any(&self) -> bool {
        self.model_registrar
            || self.model_provider
            || self.auth_provider
            || self.frontend_auth_provider
            || self.scheduler
            || self.model_router
            || self.executor
            || self.request_translator
            || self.request_normalizer
            || self.request_interceptor
            || self.request_lifecycle_plugin
            || self.response_translator
            || self.response_before_translator
            || self.response_after_translator
            || self.response_interceptor
            || self.stream_chunk_interceptor
            || self.websocket_response_observer
            || self.thinking_applier
            || self.usage_plugin
            || self.command_line_plugin
            || self.management_api
            || self.quota_provider
    }

    /// Declared executor scope, defaulting to `both` (Go: `normalizedExecutorModelScope`).
    pub fn normalized_executor_scope(&self) -> &str {
        if !self.executor {
            return "both";
        }
        match self.executor_model_scope.as_str() {
            s @ ("static" | "oauth" | "both") => s,
            _ => "both",
        }
    }

    pub fn scope_allows_static_models(&self) -> bool {
        !self.executor || matches!(self.normalized_executor_scope(), "static" | "both")
    }

    pub fn scope_allows_oauth_models(&self) -> bool {
        !self.executor || matches!(self.normalized_executor_scope(), "oauth" | "both")
    }
}

/// `plugin.register` response before capabilities are turned into a [`Record`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Registration {
    pub schema_version: u32,
    pub metadata: PluginMetadata,
    pub capabilities: Capabilities,
}

/// Capabilities negotiated at registration (Go: `pluginapi.Plugin`).
#[derive(Debug, Clone, Default)]
pub struct PluginInfo {
    pub metadata: PluginMetadata,
    /// Negotiated contract version; a missing one counts as 1.
    pub schema_version: u32,
    pub caps: Capabilities,
    /// Normalized `auth.identifier`, queried at registration.
    pub auth_identifier: String,
    /// Normalized `quota.identifier`, queried at registration.
    pub quota_identifier: String,
}

impl PluginInfo {
    /// Go: `validPlugin`.
    pub fn is_valid(&self) -> bool {
        let m = &self.metadata;
        !m.name.trim().is_empty()
            && !m.version.trim().is_empty()
            && !m.author.trim().is_empty()
            && !m.git_hub_repository.trim().is_empty()
            && self.caps.any()
    }
}

/// One active plugin in a runtime snapshot (Go: `capabilityRecord`).
pub struct Record {
    pub id: String,
    pub path: PathBuf,
    pub version: String,
    pub priority: i64,
    pub meta: PluginMetadata,
    pub info: PluginInfo,
    pub client: Arc<GuardedClient>,
    identifiers: Mutex<HashMap<&'static str, String>>,
}

impl Record {
    pub fn new(id: String, path: PathBuf, version: String, priority: i64, info: PluginInfo, client: Arc<GuardedClient>) -> Self {
        Record { id, path, version, priority, meta: info.metadata.clone(), info, client, identifiers: Mutex::new(HashMap::new()) }
    }

    pub fn caps(&self) -> &Capabilities {
        &self.info.caps
    }

    pub fn schema_version(&self) -> u32 {
        self.info.schema_version
    }

    /// `*.identifier` of this plugin, asked once and cached (Go asks on every use; the value is
    /// constant for a loaded plugin). Empty when the plugin does not answer.
    pub fn identifier(&self, method: &'static str) -> String {
        if let Some(v) = self.identifiers.lock().get(method) {
            return v.clone();
        }
        let resp: PluginIdentifier = call_plugin_blocking(&self.client, method, &empty_request()).unwrap_or_default();
        let id = resp.identifier.trim().to_string();
        if !id.is_empty() {
            self.identifiers.lock().insert(method, id.clone());
        }
        id
    }

    pub fn frontend_identifier(&self) -> String {
        self.identifier(abi::METHOD_FRONTEND_AUTH_IDENTIFIER)
    }

    pub fn executor_identifier(&self) -> String {
        self.identifier(abi::METHOD_EXECUTOR_IDENTIFIER)
    }

    pub fn thinking_identifier(&self) -> String {
        self.identifier(abi::METHOD_THINKING_IDENTIFIER)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct PluginIdentifier {
    #[serde(default)]
    pub identifier: String,
}

/// Wraps a request with the host callback id (and stream id) the plugin echoes back.
#[derive(Serialize)]
pub struct WithHost<'a, T: Serialize> {
    #[serde(flatten)]
    pub inner: &'a T,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub host_callback_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stream_id: String,
}

impl<'a, T: Serialize> WithHost<'a, T> {
    pub fn new(inner: &'a T, host_callback_id: String) -> Self {
        WithHost { inner, host_callback_id, stream_id: String::new() }
    }
}

/// Clones headers, mapping empty to the empty map (callers serialize empty as null).
pub fn clone_header(h: &Header) -> Header {
    h.clone()
}

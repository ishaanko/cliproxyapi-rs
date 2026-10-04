//! Payloads of plugin-to-host callbacks (`host.*` methods; Go: the `Host*` and `HTTP*` types of
//! sdk/pluginapi). Re-exported from [`crate::api`].

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::Header;
use crate::wire::{self, b64, gotime, nul};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostModelExecutionRequest {
    pub entry_protocol: String,
    pub exit_protocol: String,
    pub model: String,
    pub stream: bool,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "nul")]
    pub query: BTreeMap<String, Vec<String>>,
    pub alt: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub forced_provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub auth_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub proxy_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostModelExecutionResponse {
    pub status_code: i64,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostModelStreamResponse {
    pub status_code: i64,
    #[serde(with = "nul")]
    pub headers: Header,
    pub stream_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostModelStreamReadRequest {
    pub stream_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostModelStreamReadResponse {
    #[serde(with = "b64")]
    pub payload: Vec<u8>,
    pub error: String,
    pub done: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostModelStreamCloseRequest {
    pub stream_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostRecentRequestEntry {
    pub time: String,
    pub success: i64,
    pub failed: i64,
}

/// One credential exposed through `host.auth.*` callbacks. Go's `omitempty` never omits the
/// `time.Time` fields, so those always serialize (zero time when unset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostAuthFileEntry {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub auth_index: String,
    pub name: String,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status_message: String,
    #[serde(skip_serializing_if = "wire::is_false")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "wire::is_false")]
    pub unavailable: bool,
    #[serde(skip_serializing_if = "wire::is_false")]
    pub runtime_only: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(skip_serializing_if = "wire::is_zero_i64")]
    pub size: i64,
    #[serde(rename = "modtime", with = "gotime")]
    pub mod_time: Option<DateTime<Utc>>,
    #[serde(with = "gotime")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(with = "gotime")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(with = "gotime")]
    pub last_refresh: Option<DateTime<Utc>>,
    #[serde(with = "gotime")]
    pub next_retry_after: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub project_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub account_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub account: String,
    #[serde(skip_serializing_if = "wire::is_zero_i64")]
    pub priority: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(skip_serializing_if = "wire::is_false")]
    pub websockets: bool,
    #[serde(skip_serializing_if = "wire::is_zero_i64")]
    pub success: i64,
    #[serde(skip_serializing_if = "wire::is_zero_i64")]
    pub failed: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_requests: Vec<HostRecentRequestEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostAuthGetRequest {
    pub auth_index: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostAuthGetResponse {
    pub auth_index: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
    pub json: Option<Box<serde_json::value::RawValue>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostAuthGetRuntimeResponse {
    pub auth: HostAuthFileEntry,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostAuthSaveRequest {
    pub name: String,
    pub json: Option<Box<serde_json::value::RawValue>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostAuthSaveResponse {
    pub name: String,
    pub path: String,
}

/// Asks the host to clear quota and cooldown routing state for one credential.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostRoutingResetCooldownRequest {
    pub auth_index: String,
}

/// The credential whose quota and cooldown state was cleared and the model keys reset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostRoutingResetCooldownResponse {
    pub auth_index: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

pub const AFFINITY_STATUS_BOUND: &str = "bound";
pub const AFFINITY_STATUS_UNBOUND: &str = "unbound";
pub const AFFINITY_STATUS_AMBIGUOUS: &str = "ambiguous";
pub const AFFINITY_STATUS_UNSUPPORTED: &str = "unsupported";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostAffinityLookupRequest {
    pub provider: String,
    pub model: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostAffinityLookupResponse {
    pub status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub auth_index: String,
    #[serde(with = "gotime")]
    pub observed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "wire::is_false")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "wire::is_false")]
    pub unavailable: bool,
}

/// Transport wire representation requested for one host HTTP request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpWireProfile {
    #[serde(skip_serializing_if = "wire::is_false")]
    pub http1_only: bool,
    #[serde(skip_serializing_if = "wire::is_false")]
    pub disable_auto_compression: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub header_profile: Vec<String>,
}

/// Upstream HTTP request issued through the host (Go `HTTPRequest`, untagged except
/// `wire_profile`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpRequest {
    #[serde(rename = "Method")]
    pub method: String,
    #[serde(rename = "URL")]
    pub url: String,
    #[serde(rename = "Headers", with = "nul")]
    pub headers: Header,
    #[serde(rename = "Body", with = "b64")]
    pub body: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wire_profile: Option<HttpWireProfile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct HttpResponse {
    pub status_code: i64,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
}

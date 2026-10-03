//! Plugin API payloads (Go: sdk/pluginapi). Field names match Go's untagged struct encoding
//! (`PascalCase` with acronyms kept, e.g. `AuthID`, `StorageJSON`).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::wire::{self, b64, b64_list, gotime, nul};

pub use crate::host_api::*;

pub type Header = BTreeMap<String, Vec<String>>;
pub type AnyMap = Map<String, Value>;

// ---- plugin identity ----

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct PluginMetadata {
    pub name: String,
    pub version: String,
    pub author: String,
    #[serde(rename = "GitHubRepository")]
    pub git_hub_repository: String,
    pub logo: String,
    #[serde(with = "nul")]
    pub config_fields: Vec<ConfigField>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ConfigField {
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    #[serde(with = "nul")]
    pub enum_values: Vec<String>,
    pub description: String,
}

// ---- models ----

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ThinkingSupport {
    pub min: i64,
    pub max: i64,
    pub zero_allowed: bool,
    pub dynamic_allowed: bool,
    #[serde(with = "nul")]
    pub levels: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ModelInfo {
    #[serde(rename = "ID")]
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub display_name: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub input_token_limit: i64,
    pub output_token_limit: i64,
    #[serde(with = "nul")]
    pub supported_generation_methods: Vec<String>,
    pub context_length: i64,
    pub max_completion_tokens: i64,
    #[serde(with = "nul")]
    pub supported_parameters: Vec<String>,
    #[serde(with = "nul")]
    pub supported_input_modalities: Vec<String>,
    #[serde(with = "nul")]
    pub supported_output_modalities: Vec<String>,
    pub thinking: Option<ThinkingSupport>,
    pub user_defined: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ModelAlias {
    pub name: String,
    pub alias: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct HostConfigSummary {
    pub auth_dir: String,
    #[serde(rename = "ProxyURL")]
    pub proxy_url: String,
    pub force_model_prefix: bool,
    #[serde(rename = "OAuthModelAlias", with = "nul")]
    pub oauth_model_alias: BTreeMap<String, Vec<ModelAlias>>,
    #[serde(with = "nul")]
    pub excluded_models: BTreeMap<String, Vec<String>>,
}

// ---- auth ----

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthData {
    pub provider: String,
    #[serde(rename = "ID")]
    pub id: String,
    pub file_name: String,
    pub label: String,
    pub prefix: String,
    #[serde(rename = "ProxyURL")]
    pub proxy_url: String,
    pub disabled: bool,
    #[serde(rename = "StorageJSON", with = "b64")]
    pub storage_json: Vec<u8>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
    #[serde(with = "nul")]
    pub attributes: BTreeMap<String, String>,
    #[serde(with = "gotime")]
    pub next_refresh_after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthParseRequest {
    pub provider: String,
    pub path: String,
    pub file_name: String,
    #[serde(rename = "RawJSON", with = "b64")]
    pub raw_json: Vec<u8>,
    pub host: HostConfigSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthParseResponse {
    pub handled: bool,
    pub auth: AuthData,
    #[serde(with = "nul")]
    pub auths: Vec<AuthData>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthLoginStartRequest {
    pub provider: String,
    #[serde(rename = "BaseURL")]
    pub base_url: String,
    pub host: HostConfigSummary,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthLoginStartResponse {
    pub provider: String,
    #[serde(rename = "URL")]
    pub url: String,
    pub state: String,
    #[serde(with = "gotime")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthLoginPollRequest {
    pub provider: String,
    pub state: String,
    pub host: HostConfigSummary,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthLoginPollResponse {
    /// `pending`, `success` or `error`.
    pub status: String,
    pub message: String,
    pub auth: AuthData,
    #[serde(with = "nul")]
    pub auths: Vec<AuthData>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthRefreshRequest {
    #[serde(rename = "AuthID")]
    pub auth_id: String,
    pub auth_provider: String,
    #[serde(rename = "StorageJSON", with = "b64")]
    pub storage_json: Vec<u8>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
    #[serde(with = "nul")]
    pub attributes: BTreeMap<String, String>,
    pub host: HostConfigSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthRefreshResponse {
    pub auth: AuthData,
    #[serde(with = "gotime")]
    pub next_refresh_after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ModelRegistrationRequest {
    pub plugin: PluginMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ModelRegistrationResponse {
    pub provider: String,
    #[serde(with = "nul")]
    pub models: Vec<ModelInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct StaticModelRequest {
    pub plugin: PluginMetadata,
    pub host: HostConfigSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthModelRequest {
    pub plugin: PluginMetadata,
    #[serde(rename = "AuthID")]
    pub auth_id: String,
    pub auth_provider: String,
    #[serde(rename = "StorageJSON", with = "b64")]
    pub storage_json: Vec<u8>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
    #[serde(with = "nul")]
    pub attributes: BTreeMap<String, String>,
    pub host: HostConfigSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ModelResponse {
    pub provider: String,
    #[serde(with = "nul")]
    pub models: Vec<ModelInfo>,
    pub auth_update: AuthData,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct FrontendAuthRequest {
    pub method: String,
    pub path: String,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "nul")]
    pub query: BTreeMap<String, Vec<String>>,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct FrontendAuthResponse {
    pub authenticated: bool,
    pub principal: String,
    #[serde(with = "nul")]
    pub metadata: BTreeMap<String, String>,
}

// ---- scheduler / router ----

pub const SCHEDULER_BUILTIN_ROUND_ROBIN: &str = "round-robin";
pub const SCHEDULER_BUILTIN_FILL_FIRST: &str = "fill-first";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct SchedulerOptions {
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct SchedulerAuthCandidate {
    #[serde(rename = "ID")]
    pub id: String,
    pub provider: String,
    pub priority: i64,
    pub status: String,
    #[serde(with = "nul")]
    pub attributes: BTreeMap<String, String>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct SchedulerPickRequest {
    pub plugin: PluginMetadata,
    pub provider: String,
    #[serde(with = "nul")]
    pub providers: Vec<String>,
    pub model: String,
    pub stream: bool,
    pub options: SchedulerOptions,
    #[serde(with = "nul")]
    pub candidates: Vec<SchedulerAuthCandidate>,
}

/// Accepts both the Go field names and snake_case names (Go: custom `UnmarshalJSON`).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SchedulerPickResponse {
    #[serde(rename = "AuthID")]
    pub auth_id: String,
    pub delegate_builtin: String,
    pub handled: bool,
    pub reject: bool,
    pub reject_reason: String,
    pub reject_code: String,
}

impl<'de> Deserialize<'de> for SchedulerPickResponse {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Raw {
            #[serde(rename = "AuthID")]
            auth_id: Option<String>,
            #[serde(rename = "auth_id")]
            alt_auth_id: Option<String>,
            #[serde(rename = "DelegateBuiltin")]
            delegate_builtin: Option<String>,
            #[serde(rename = "delegate_builtin")]
            alt_delegate: Option<String>,
            #[serde(rename = "Handled")]
            handled: Option<bool>,
            #[serde(rename = "handled")]
            alt_handled: Option<bool>,
            #[serde(rename = "Reject")]
            reject: Option<bool>,
            #[serde(rename = "reject")]
            alt_reject: Option<bool>,
            #[serde(rename = "RejectReason")]
            reject_reason: Option<String>,
            #[serde(rename = "reject_reason")]
            alt_reject_reason: Option<String>,
            #[serde(rename = "RejectCode")]
            reject_code: Option<String>,
            #[serde(rename = "reject_code")]
            alt_reject_code: Option<String>,
        }
        let r = Raw::deserialize(d)?;
        Ok(SchedulerPickResponse {
            auth_id: r.auth_id.or(r.alt_auth_id).unwrap_or_default(),
            delegate_builtin: r.delegate_builtin.or(r.alt_delegate).unwrap_or_default(),
            handled: r.handled.or(r.alt_handled).unwrap_or_default(),
            reject: r.reject.or(r.alt_reject).unwrap_or_default(),
            reject_reason: r.reject_reason.or(r.alt_reject_reason).unwrap_or_default(),
            reject_code: r.reject_code.or(r.alt_reject_code).unwrap_or_default(),
        })
    }
}

pub const ROUTE_TARGET_SELF: &str = "self";
pub const ROUTE_TARGET_EXECUTOR: &str = "executor";
pub const ROUTE_TARGET_PROVIDER: &str = "provider";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ModelRouteRequest {
    pub plugin: PluginMetadata,
    #[serde(rename = "PluginID")]
    pub plugin_id: String,
    pub source_format: String,
    pub requested_model: String,
    pub stream: bool,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "nul")]
    pub query: BTreeMap<String, Vec<String>>,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
    #[serde(with = "nul")]
    pub available_providers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ModelRouteResponse {
    pub handled: bool,
    pub target_kind: String,
    pub target: String,
    pub target_model: String,
    pub reason: String,
}

// ---- executor ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ExecutorRequest {
    #[serde(rename = "AuthID")]
    pub auth_id: String,
    pub auth_provider: String,
    pub model: String,
    pub format: String,
    pub stream: bool,
    pub alt: String,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "nul")]
    pub query: BTreeMap<String, Vec<String>>,
    #[serde(with = "b64")]
    pub original_request: Vec<u8>,
    pub source_format: String,
    #[serde(with = "b64")]
    pub payload: Vec<u8>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
    #[serde(rename = "StorageJSON", with = "b64")]
    pub storage_json: Vec<u8>,
    #[serde(with = "nul")]
    pub auth_metadata: AnyMap,
    #[serde(with = "nul")]
    pub auth_attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ExecutorResponse {
    #[serde(with = "b64")]
    pub payload: Vec<u8>,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ExecutorStreamChunk {
    #[serde(with = "b64")]
    pub payload: Vec<u8>,
    /// Go `error` field; plugins normally leave it unset.
    pub err: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutorStreamResponse {
    #[serde(with = "nul", skip_serializing_if = "Header::is_empty")]
    pub headers: Header,
    #[serde(with = "nul", skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ExecutorStreamChunk>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ExecutorHttpRequest {
    #[serde(rename = "AuthID")]
    pub auth_id: String,
    pub auth_provider: String,
    pub method: String,
    #[serde(rename = "URL")]
    pub url: String,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
    #[serde(rename = "StorageJSON", with = "b64")]
    pub storage_json: Vec<u8>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
    #[serde(with = "nul")]
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ExecutorHttpResponse {
    pub status_code: i64,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
}

// ---- translation ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct RequestTransformRequest {
    pub from_format: String,
    pub to_format: String,
    pub model: String,
    pub stream: bool,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ResponseTransformRequest {
    pub from_format: String,
    pub to_format: String,
    pub model: String,
    pub stream: bool,
    #[serde(with = "b64")]
    pub original_request: Vec<u8>,
    #[serde(with = "b64")]
    pub translated_request: Vec<u8>,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct PayloadResponse {
    #[serde(with = "b64")]
    pub body: Vec<u8>,
}

// ---- interceptors ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct RequestInterceptRequest {
    #[serde(rename = "RequestID")]
    pub request_id: String,
    #[serde(rename = "TraceID")]
    pub trace_id: String,
    pub source_format: String,
    pub to_format: String,
    pub model: String,
    pub requested_model: String,
    pub stream: bool,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct RequestInterceptResponse {
    #[serde(rename = "path", skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
    #[serde(with = "nul")]
    pub clear_headers: Vec<String>,
    pub terminate: bool,
    pub status_code: i64,
    #[serde(with = "nul")]
    pub response_headers: Header,
    #[serde(with = "b64")]
    pub response_body: Vec<u8>,
}

pub const COMPLETION_SUCCEEDED: &str = "succeeded";
pub const COMPLETION_FAILED: &str = "failed";
pub const COMPLETION_REJECTED: &str = "rejected";
pub const COMPLETION_CANCELED: &str = "canceled";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct RequestCompletion {
    #[serde(rename = "RequestID")]
    pub request_id: String,
    #[serde(rename = "TraceID")]
    pub trace_id: String,
    pub source_format: String,
    pub model: String,
    pub requested_model: String,
    pub stream: bool,
    pub outcome: String,
    pub status_code: i64,
    pub error: String,
    #[serde(with = "gotime")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(with = "gotime")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ResponseInterceptRequest {
    #[serde(rename = "RequestID")]
    pub request_id: String,
    pub source_format: String,
    pub model: String,
    pub requested_model: String,
    pub stream: bool,
    #[serde(with = "nul")]
    pub request_headers: Header,
    #[serde(with = "nul")]
    pub response_headers: Header,
    #[serde(with = "b64")]
    pub original_request: Vec<u8>,
    #[serde(with = "b64")]
    pub request_body: Vec<u8>,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
    pub status_code: i64,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ResponseInterceptResponse {
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
    #[serde(with = "nul")]
    pub clear_headers: Vec<String>,
}

/// Marks the header-only stream initialization interceptor call.
pub const STREAM_CHUNK_HEADER_INIT_INDEX: i64 = -1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct StreamChunkInterceptRequest {
    #[serde(rename = "RequestID")]
    pub request_id: String,
    pub source_format: String,
    pub model: String,
    pub requested_model: String,
    #[serde(with = "nul")]
    pub request_headers: Header,
    #[serde(with = "nul")]
    pub response_headers: Header,
    #[serde(with = "b64")]
    pub original_request: Vec<u8>,
    #[serde(with = "b64")]
    pub request_body: Vec<u8>,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
    #[serde(with = "b64_list")]
    pub history_chunks: Vec<Vec<u8>>,
    pub chunk_index: i64,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct StreamChunkInterceptResponse {
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
    #[serde(with = "nul")]
    pub clear_headers: Vec<String>,
    pub drop_chunk: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct WebSocketResponseEvent {
    #[serde(rename = "RequestID")]
    pub request_id: String,
    #[serde(rename = "TraceID")]
    pub trace_id: String,
    pub source_format: String,
    pub model: String,
    pub requested_model: String,
    pub provider: String,
    #[serde(rename = "AuthID")]
    pub auth_id: String,
    pub auth_label: String,
    pub auth_type: String,
    pub event_type: String,
    #[serde(with = "b64")]
    pub payload: Vec<u8>,
    #[serde(with = "nul")]
    pub metadata: AnyMap,
}

// ---- thinking ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ThinkingConfig {
    pub mode: String,
    pub budget: i64,
    pub level: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ThinkingApplyRequest {
    pub provider: String,
    pub model: ModelInfo,
    pub config: ThinkingConfig,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
}

// ---- usage ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UsageFailure {
    pub status_code: i64,
    pub body: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UsageDetail {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub cached_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UsageRecord {
    #[serde(rename = "RequestID")]
    pub request_id: String,
    #[serde(rename = "TraceID")]
    pub trace_id: String,
    pub provider: String,
    #[serde(rename = "BaseURL")]
    pub base_url: String,
    pub executor_type: String,
    pub model: String,
    pub alias: String,
    #[serde(rename = "APIKey")]
    pub api_key: String,
    #[serde(rename = "SessionID")]
    pub session_id: String,
    #[serde(rename = "ParentSessionID")]
    pub parent_session_id: String,
    #[serde(rename = "AuthID")]
    pub auth_id: String,
    pub auth_index: String,
    pub auth_type: String,
    pub source: String,
    pub reasoning_effort: String,
    pub service_tier: String,
    pub response_service_tier: String,
    pub response_model: String,
    pub generate: bool,
    pub stream: bool,
    #[serde(with = "gotime")]
    pub requested_at: Option<DateTime<Utc>>,
    /// Nanoseconds (Go `time.Duration`).
    pub latency: i64,
    #[serde(rename = "TTFT")]
    pub ttft: i64,
    pub failed: bool,
    pub failure: UsageFailure,
    pub detail: UsageDetail,
    #[serde(with = "nul")]
    pub response_headers: Header,
}

// ---- command line ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CommandLineRegistrationRequest {
    pub plugin: PluginMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CommandLineFlag {
    pub name: String,
    pub usage: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub default_value: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CommandLineRegistrationResponse {
    #[serde(with = "nul")]
    pub flags: Vec<CommandLineFlag>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CommandLineFlagValue {
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub value: String,
    pub set: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CommandLineExecutionRequest {
    pub plugin: PluginMetadata,
    pub program: String,
    #[serde(with = "nul")]
    pub args: Vec<String>,
    pub config_path: String,
    pub host: HostConfigSummary,
    #[serde(with = "nul")]
    pub flags: BTreeMap<String, CommandLineFlagValue>,
    #[serde(with = "nul")]
    pub triggered_flags: BTreeMap<String, CommandLineFlagValue>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CommandLineExecutionResponse {
    #[serde(with = "b64")]
    pub stdout: Vec<u8>,
    #[serde(with = "b64")]
    pub stderr: Vec<u8>,
    #[serde(with = "nul")]
    pub auths: Vec<AuthData>,
    pub exit_code: i64,
}

// ---- management ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ManagementRegistrationRequest {
    pub plugin: PluginMetadata,
    pub base_path: String,
    pub resource_base_path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ManagementRoute {
    pub method: String,
    pub path: String,
    pub menu: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ResourceRoute {
    pub path: String,
    pub menu: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ManagementRegistrationResponse {
    #[serde(with = "nul")]
    pub routes: Vec<ManagementRoute>,
    #[serde(with = "nul")]
    pub resources: Vec<ResourceRoute>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ManagementRequest {
    pub method: String,
    pub path: String,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "nul")]
    pub query: BTreeMap<String, Vec<String>>,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ManagementResponse {
    pub status_code: i64,
    #[serde(with = "nul")]
    pub headers: Header,
    #[serde(with = "b64")]
    pub body: Vec<u8>,
}

// ---- quota ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaDescribeRequest {
    pub plugin: PluginMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaDescribeResponse {
    #[serde(skip_serializing_if = "Vec::is_empty", with = "nul")]
    pub supported_providers: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(skip_serializing_if = "wire::is_false")]
    pub supports_reset: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaFetchRequest {
    pub auth_index: String,
    pub auth_id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Vec::is_empty", with = "b64")]
    pub storage_json: Vec<u8>,
    #[serde(skip_serializing_if = "Map::is_empty", with = "nul")]
    pub metadata: AnyMap,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", with = "nul")]
    pub attributes: BTreeMap<String, String>,
    pub host: HostConfigSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaSubscription {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub plan: String,
    #[serde(rename = "tierName", skip_serializing_if = "String::is_empty")]
    pub tier_name: String,
    #[serde(rename = "tierId", skip_serializing_if = "String::is_empty")]
    pub tier_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaBucket {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub window: String,
    #[serde(rename = "remainingFraction")]
    pub remaining_fraction: f64,
    #[serde(rename = "resetTime", skip_serializing_if = "String::is_empty")]
    pub reset_time: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaGroup {
    #[serde(rename = "displayName", skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    #[serde(skip_serializing_if = "Vec::is_empty", with = "nul")]
    pub buckets: Vec<QuotaBucket>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaMetric {
    pub key: String,
    pub label: String,
    pub value: f64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub unit: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub format: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub currency: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct QuotaFetchResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<QuotaSubscription>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<QuotaMetric>,
    #[serde(rename = "serverTimeOffsetMs", skip_serializing_if = "wire::is_zero_i64")]
    pub server_time_offset_ms: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<QuotaGroup>,
}

impl<'de> Deserialize<'de> for QuotaFetchResponse {
    /// Accepts camelCase and snake_case spellings (Go: the `UnmarshalJSON` fallbacks).
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let g = |k: &str| v.get(k);
        let mut out = QuotaFetchResponse::default();
        let sub = |k: &str| -> Option<Value> { g(k).cloned() };
        if let Some(s) = sub("subscription").filter(|s| !s.is_null()) {
            let mut sub: QuotaSubscription = serde_json::from_value(s.clone()).map_err(serde::de::Error::custom)?;
            if sub.tier_name.is_empty() {
                sub.tier_name = s.get("tier_name").and_then(Value::as_str).unwrap_or("").to_string();
            }
            if sub.tier_id.is_empty() {
                sub.tier_id = s.get("tier_id").and_then(Value::as_str).unwrap_or("").to_string();
            }
            out.subscription = Some(sub);
        }
        if let Some(s) = sub("summary").filter(|s| !s.is_null()) {
            out.summary = serde_json::from_value(s).map_err(serde::de::Error::custom)?;
        }
        out.server_time_offset_ms = g("serverTimeOffsetMs").and_then(Value::as_i64).unwrap_or(0);
        if out.server_time_offset_ms == 0 {
            out.server_time_offset_ms = g("server_time_offset_ms").and_then(Value::as_i64).unwrap_or(0);
        }
        if let Some(Value::Array(groups)) = g("groups") {
            for gr in groups {
                let mut group: QuotaGroup = serde_json::from_value(gr.clone()).map_err(serde::de::Error::custom)?;
                if group.display_name.is_empty() {
                    group.display_name = gr.get("display_name").and_then(Value::as_str).unwrap_or("").to_string();
                }
                if let Some(Value::Array(buckets)) = gr.get("buckets") {
                    for (i, b) in buckets.iter().enumerate() {
                        let bucket = &mut group.buckets[i];
                        if b.get("remainingFraction").is_none() {
                            if let Some(f) = b.get("remaining_fraction").and_then(Value::as_f64) {
                                bucket.remaining_fraction = f;
                            }
                        }
                        if bucket.reset_time.is_empty() {
                            bucket.reset_time = b.get("reset_time").and_then(Value::as_str).unwrap_or("").to_string();
                        }
                    }
                }
                out.groups.push(group);
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaResetRequest {
    pub auth_index: String,
    pub auth_id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Vec::is_empty", with = "b64")]
    pub storage_json: Vec<u8>,
    #[serde(skip_serializing_if = "Map::is_empty", with = "nul")]
    pub metadata: AnyMap,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", with = "nul")]
    pub attributes: BTreeMap<String, String>,
    pub host: HostConfigSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaResetResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
}

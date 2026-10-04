//! Plugin ABI constants and the RPC envelope (Go: sdk/pluginabi).

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// Native C ABI shape (plugin exports).
pub const ABI_VERSION: u32 = 1;
/// RPC JSON contract version exchanged at `plugin.register`.
pub const SCHEMA_VERSION: u32 = 6;
pub const SCHEMA_VERSION_STREAM_CHUNK_OMIT_REQUEST_BODY: u32 = 3;
pub const SCHEMA_VERSION_WEBSOCKET_RESPONSE_OBSERVER: u32 = 4;
pub const SCHEMA_VERSION_STREAM_CHUNK_OMIT_HISTORY: u32 = 5;
pub const SCHEMA_VERSION_RAW_MANAGEMENT_RESPONSE: u32 = 6;

pub const METHOD_PLUGIN_REGISTER: &str = "plugin.register";
pub const METHOD_PLUGIN_QUIESCE: &str = "plugin.quiesce";
pub const METHOD_PLUGIN_RECONFIGURE: &str = "plugin.reconfigure";
pub const METHOD_PLUGIN_SHUTDOWN: &str = "plugin.shutdown";

pub const METHOD_MODEL_REGISTER: &str = "model.register";
pub const METHOD_MODEL_STATIC: &str = "model.static";
pub const METHOD_MODEL_FOR_AUTH: &str = "model.for_auth";

pub const METHOD_AUTH_IDENTIFIER: &str = "auth.identifier";
pub const METHOD_AUTH_PARSE: &str = "auth.parse";
pub const METHOD_AUTH_LOGIN_START: &str = "auth.login.start";
pub const METHOD_AUTH_LOGIN_POLL: &str = "auth.login.poll";
pub const METHOD_AUTH_REFRESH: &str = "auth.refresh";

pub const METHOD_FRONTEND_AUTH_IDENTIFIER: &str = "frontend_auth.identifier";
pub const METHOD_FRONTEND_AUTH_AUTHENTICATE: &str = "frontend_auth.authenticate";

pub const METHOD_SCHEDULER_PICK: &str = "scheduler.pick";
pub const METHOD_MODEL_ROUTE: &str = "model.route";

pub const METHOD_EXECUTOR_IDENTIFIER: &str = "executor.identifier";
pub const METHOD_EXECUTOR_EXECUTE: &str = "executor.execute";
pub const METHOD_EXECUTOR_EXECUTE_STREAM: &str = "executor.execute_stream";
pub const METHOD_EXECUTOR_COUNT_TOKENS: &str = "executor.count_tokens";
pub const METHOD_EXECUTOR_HTTP_REQUEST: &str = "executor.http_request";

pub const METHOD_REQUEST_TRANSLATE: &str = "request.translate";
pub const METHOD_REQUEST_NORMALIZE: &str = "request.normalize";
pub const METHOD_REQUEST_INTERCEPT_BEFORE: &str = "request.intercept_before";
pub const METHOD_REQUEST_INTERCEPT_AFTER: &str = "request.intercept_after";
pub const METHOD_REQUEST_COMPLETE: &str = "request.complete";

pub const METHOD_RESPONSE_TRANSLATE: &str = "response.translate";
pub const METHOD_RESPONSE_NORMALIZE_BEFORE: &str = "response.normalize_before";
pub const METHOD_RESPONSE_NORMALIZE_AFTER: &str = "response.normalize_after";
pub const METHOD_RESPONSE_INTERCEPT_AFTER: &str = "response.intercept_after";
pub const METHOD_RESPONSE_INTERCEPT_STREAM_CHUNK: &str = "response.intercept_stream_chunk";

pub const METHOD_WEBSOCKET_RESPONSE_EVENT: &str = "websocket.response_event";

pub const METHOD_THINKING_IDENTIFIER: &str = "thinking.identifier";
pub const METHOD_THINKING_APPLY: &str = "thinking.apply";

pub const METHOD_USAGE_HANDLE: &str = "usage.handle";

pub const METHOD_COMMAND_LINE_REGISTER: &str = "command_line.register";
pub const METHOD_COMMAND_LINE_EXECUTE: &str = "command_line.execute";

pub const METHOD_MANAGEMENT_REGISTER: &str = "management.register";
pub const METHOD_MANAGEMENT_HANDLE: &str = "management.handle";

pub const METHOD_QUOTA_IDENTIFIER: &str = "quota.identifier";
pub const METHOD_QUOTA_DESCRIBE: &str = "quota.describe";
pub const METHOD_QUOTA_FETCH: &str = "quota.fetch";
pub const METHOD_QUOTA_RESET: &str = "quota.reset";

pub const METHOD_HOST_HTTP_DO: &str = "host.http.do";
pub const METHOD_HOST_HTTP_DO_STREAM: &str = "host.http.do_stream";
pub const METHOD_HOST_HTTP_OPERATION_OPEN: &str = "host.http.operation_open";
pub const METHOD_HOST_HTTP_CANCEL: &str = "host.http.cancel";
pub const METHOD_HOST_HTTP_STREAM_READ: &str = "host.http.stream_read";
pub const METHOD_HOST_HTTP_STREAM_CLOSE: &str = "host.http.stream_close";
pub const METHOD_HOST_MODEL_EXECUTE: &str = "host.model.execute";
pub const METHOD_HOST_MODEL_EXECUTE_STREAM: &str = "host.model.execute_stream";
pub const METHOD_HOST_MODEL_STREAM_READ: &str = "host.model.stream_read";
pub const METHOD_HOST_MODEL_STREAM_CLOSE: &str = "host.model.stream_close";
pub const METHOD_HOST_STREAM_EMIT: &str = "host.stream.emit";
pub const METHOD_HOST_STREAM_CLOSE: &str = "host.stream.close";
pub const METHOD_HOST_LOG: &str = "host.log";
pub const METHOD_HOST_AUTH_LIST: &str = "host.auth.list";
pub const METHOD_HOST_AUTH_GET: &str = "host.auth.get";
pub const METHOD_HOST_AUTH_GET_RUNTIME: &str = "host.auth.get_runtime";
pub const METHOD_HOST_AUTH_SAVE: &str = "host.auth.save";
pub const METHOD_HOST_AFFINITY_LOOKUP: &str = "host.affinity.lookup";
pub const METHOD_HOST_ROUTING_RESET_COOLDOWN: &str = "host.routing.reset_cooldown";

/// Response envelope of every RPC call in both directions.
#[derive(Debug, Default, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub result: Option<Box<RawValue>>,
    #[serde(default)]
    pub error: Option<RpcErrorBody>,
}

/// The `error` object of a failed envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RpcErrorBody {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub retryable: bool,
    /// HTTP status to surface to the client; 0 means 500.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub http_status: i32,
}

fn is_false(v: &bool) -> bool {
    !*v
}
fn is_zero(v: &i32) -> bool {
    *v == 0
}

/// Serialized failed envelope (Go: `NewErrorEnvelope`).
pub fn error_envelope(code: &str, message: &str, http_status: i32) -> Vec<u8> {
    let body = RpcErrorBody { code: code.into(), message: message.into(), retryable: false, http_status };
    serde_json::to_vec(&serde_json::json!({"ok": false, "error": body})).unwrap_or_default()
}

/// Serialized successful envelope around an already serialized result.
pub fn ok_envelope(result: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(result.len() + 24);
    out.extend_from_slice(br#"{"ok":true,"result":"#);
    out.extend_from_slice(if result.is_empty() { b"{}" } else { result });
    out.push(b'}');
    out
}

/// True when `raw` is a failed envelope with an error object.
pub fn is_error_envelope(raw: &[u8]) -> bool {
    serde_json::from_slice::<Envelope>(raw).map(|e| !e.ok && e.error.is_some()).unwrap_or(false)
}

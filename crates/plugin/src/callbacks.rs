//! Plugin-to-host callbacks (`host.*` methods; Go: `host_callbacks.go`,
//! `host_model_stream_callbacks.go`, `affinity_callbacks.go`).
//!
//! A plugin reaches [`Host::call_from_plugin`] on whatever thread its library runs the call from;
//! the handlers are async, so the entry point blocks on the runtime captured by the host.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::http::{ProxySetting, parse_proxy};
use cpa_pluginapi::abi;
use cpa_pluginapi::api::{
    AFFINITY_STATUS_BOUND, AFFINITY_STATUS_UNSUPPORTED, Header, HostAffinityLookupRequest, HostAffinityLookupResponse,
    HostModelExecutionRequest, HostModelExecutionResponse, HostModelStreamCloseRequest, HostModelStreamReadRequest,
    HostModelStreamReadResponse, HostModelStreamResponse, HttpRequest, HttpWireProfile,
};
use cpa_pluginapi::wire::{b64, nul};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::bridge::{ModelChunk, OperationHandle, same_instance};
use crate::client::CallbackInstance;
use crate::ctx::CallCtx;
use crate::error::HostError;
use crate::host::Host;
use crate::httpclient::HostHttpClient;
use crate::loader::HostCallbacks;

// ---- nested model execution (implemented by the server) ----

/// Go `handlers.ModelExecutionRequest`.
#[derive(Debug, Clone, Default)]
pub struct ModelExecutionRequest {
    pub entry_protocol: String,
    pub exit_protocol: String,
    pub model: String,
    pub stream: bool,
    pub body: Vec<u8>,
    pub headers: Header,
    pub query: BTreeMap<String, Vec<String>>,
    pub alt: String,
    pub skip_interceptor_plugin_id: String,
    pub skip_router_plugin_id: String,
    pub forced_provider: String,
    pub auth_id: String,
    pub proxy_url: String,
    pub path: String,
}

#[derive(Debug, Clone, Default)]
pub struct ModelExecutionResponse {
    pub status_code: i64,
    pub headers: Header,
    pub body: Vec<u8>,
}

/// Terminal stream error (Go: `ModelExecutionStreamError`).
#[derive(Debug, Clone, Default)]
pub struct ModelStreamError {
    pub status_code: i64,
    pub message: String,
    pub headers: Header,
}

impl ModelStreamError {
    /// Message, or the HTTP status text when empty.
    pub fn text(&self) -> String {
        if !self.message.is_empty() {
            return self.message.clone();
        }
        http::StatusCode::from_u16(self.status_code as u16)
            .ok()
            .and_then(|s| s.canonical_reason().map(str::to_string))
            .unwrap_or_default()
    }
}

pub struct ModelExecutionStream {
    pub status_code: i64,
    pub headers: Header,
    pub chunks: mpsc::Receiver<ModelChunk>,
}

/// Failure of a nested model execution (Go: `*interfaces.ErrorMessage`).
#[derive(Debug, Clone, Default)]
pub struct ModelExecError {
    pub status: i32,
    pub message: String,
}

/// Runs model requests on behalf of plugins (Go: `modelExecutor`, implemented by
/// `BaseAPIHandler`).
#[async_trait]
pub trait ModelExecutor: Send + Sync {
    async fn execute_model(&self, ctx: &CallCtx, req: ModelExecutionRequest) -> Result<ModelExecutionResponse, ModelExecError>;
    async fn execute_model_stream(&self, ctx: &CallCtx, req: ModelExecutionRequest) -> Result<ModelExecutionStream, ModelExecError>;
}

fn model_exec_error(e: ModelExecError) -> HostError {
    if e.status > 0 {
        HostError::with_status(if e.message.is_empty() { "model execution failed".into() } else { e.message }, e.status)
    } else if !e.message.is_empty() {
        HostError::msg(e.message)
    } else {
        HostError::msg("model execution failed")
    }
}

// ---- wire payloads ----

#[derive(Default, Deserialize)]
#[serde(default)]
struct RpcHostHttpRequest {
    host_callback_id: String,
    operation_id: String,
    method: String,
    url: String,
    #[serde(with = "nul")]
    headers: Header,
    #[serde(with = "b64")]
    body: Vec<u8>,
    wire_profile: Option<HttpWireProfile>,
    request: Option<InnerHttpRequest>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct InnerHttpRequest {
    method: String,
    url: String,
    #[serde(with = "nul")]
    headers: Header,
    #[serde(with = "b64")]
    body: Vec<u8>,
    wire_profile: Option<HttpWireProfile>,
}

#[derive(Serialize)]
struct RpcHostHttpStreamResponse {
    status_code: i64,
    #[serde(skip_serializing_if = "Header::is_empty")]
    headers: Header,
    #[serde(skip_serializing_if = "String::is_empty")]
    stream_id: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StreamIdRequest {
    stream_id: String,
}

#[derive(Serialize)]
struct RpcHostHttpStreamReadResponse {
    #[serde(skip_serializing_if = "Vec::is_empty", with = "b64")]
    payload: Vec<u8>,
    #[serde(skip_serializing_if = "String::is_empty")]
    error: String,
    #[serde(skip_serializing_if = "cpa_pluginapi::wire::is_false")]
    done: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OperationOpenRequest {
    host_callback_id: String,
}

#[derive(Serialize)]
struct OperationOpenResponse {
    operation_id: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HttpCancelRequest {
    host_callback_id: String,
    operation_id: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HostLogRequest {
    host_callback_id: String,
    level: String,
    message: String,
    fields: Map<String, Value>,
}

#[derive(Default)]
struct RpcHostModelExecutionRequest {
    inner: HostModelExecutionRequest,
    host_callback_id: String,
}

impl<'de> Deserialize<'de> for RpcHostModelExecutionRequest {
    /// The callback id sits next to the flattened request fields; both match keys like Go does.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let host_callback_id = match &value {
            Value::Object(m) => m
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("host_callback_id"))
                .and_then(|(_, v)| v.as_str())
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        };
        let inner = cpa_pluginapi::fold::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(RpcHostModelExecutionRequest { inner, host_callback_id })
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StreamEmitRequest {
    stream_id: String,
    #[serde(with = "b64")]
    payload: Vec<u8>,
    error: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StreamCloseRequest {
    stream_id: String,
    error: String,
}

/// Serialized `{"ok":true,"result":...}` (Go: `marshalRPCResult`).
pub fn marshal_result<T: Serialize>(value: &T) -> Result<Vec<u8>, HostError> {
    let raw = serde_json::to_vec(value).map_err(|e| HostError::msg(e.to_string()))?;
    Ok(abi::ok_envelope(&raw))
}

pub fn empty_result() -> Vec<u8> {
    abi::ok_envelope(b"{}")
}

pub(crate) fn decode<T: DeserializeOwned>(raw: &[u8], what: &str) -> Result<T, HostError> {
    cpa_pluginapi::fold::from_slice(raw).map_err(|e| HostError::msg(format!("decode {what}: {e}")))
}

/// Who is calling: the plugin and its library instance (Go: the callback identity in the ctx).
#[derive(Clone)]
pub struct CbIdentity {
    pub plugin_id: String,
    pub instance: Option<Arc<CallbackInstance>>,
}

impl HostCallbacks for Host {
    fn call_from_plugin(&self, plugin_id: &str, instance: &Arc<CallbackInstance>, method: &str, request: &[u8]) -> Vec<u8> {
        let host = self.arc();
        let Some(handle) = self.runtime_handle() else {
            return abi::error_envelope("host_call_failed", "host runtime is unavailable", 0);
        };
        let ident = CbIdentity { plugin_id: plugin_id.trim().to_string(), instance: Some(instance.clone()) };
        let method = method.to_string();
        let request = request.to_vec();
        let result = handle.block_on(async move { host.call_from_plugin_async(&ident, &method, &request).await });
        match result {
            Ok(bytes) => bytes,
            Err(e) => abi::error_envelope("host_call_failed", &e.message, e.status),
        }
    }
}

impl Host {
    /// Dispatches one `host.*` call (Go: `callFromPlugin`).
    pub async fn call_from_plugin_async(self: &Arc<Self>, id: &CbIdentity, method: &str, req: &[u8]) -> Result<Vec<u8>, HostError> {
        if id.instance.as_ref().is_some_and(|i| i.is_closed()) {
            return Err(HostError::msg("host plugin callback instance is closed"));
        }
        match method {
            abi::METHOD_HOST_MODEL_EXECUTE => self.cb_model_execute(id, req).await,
            abi::METHOD_HOST_MODEL_EXECUTE_STREAM => self.cb_model_execute_stream(id, req).await,
            abi::METHOD_HOST_MODEL_STREAM_READ => self.cb_model_stream_read(req).await,
            abi::METHOD_HOST_MODEL_STREAM_CLOSE => self.cb_model_stream_close(req),
            abi::METHOD_HOST_HTTP_DO => self.cb_http_do(id, req).await,
            abi::METHOD_HOST_HTTP_DO_STREAM => self.cb_http_do_stream(id, req).await,
            abi::METHOD_HOST_HTTP_OPERATION_OPEN => self.cb_http_operation_open(id, req),
            abi::METHOD_HOST_HTTP_CANCEL => self.cb_http_cancel(id, req),
            abi::METHOD_HOST_HTTP_STREAM_READ => self.cb_http_stream_read(id, req).await,
            abi::METHOD_HOST_HTTP_STREAM_CLOSE => self.cb_http_stream_close(id, req),
            abi::METHOD_HOST_STREAM_EMIT => self.cb_stream_emit(req).await,
            abi::METHOD_HOST_STREAM_CLOSE => self.cb_stream_close(req).await,
            abi::METHOD_HOST_LOG => self.cb_log(req),
            abi::METHOD_HOST_AUTH_LIST => self.cb_auth_list(req),
            abi::METHOD_HOST_AUTH_GET => self.cb_auth_get(req),
            abi::METHOD_HOST_AUTH_GET_RUNTIME => self.cb_auth_get_runtime(req),
            abi::METHOD_HOST_AUTH_SAVE => self.cb_auth_save(req).await,
            abi::METHOD_HOST_AFFINITY_LOOKUP => self.cb_affinity_lookup(req),
            other => Err(HostError::msg(format!("unsupported host callback {other}"))),
        }
    }

    /// Plugin that owns a callback (Go: `callbackCallerPluginID`).
    fn callback_caller_plugin_id(&self, id: &CbIdentity, callback_id: &str) -> String {
        if !id.plugin_id.is_empty() {
            return id.plugin_id.clone();
        }
        self.bridges.contexts.plugin_id(callback_id)
    }

    // ---- HTTP ----

    pub(crate) fn new_http_client(&self, auth: Option<cpa_auth::Auth>, request_proxy: &str) -> HostHttpClient {
        HostHttpClient { cfg: self.runtime_config(), auth, request_proxy_url: request_proxy.trim().to_string() }
    }

    async fn cb_http_do(self: &Arc<Self>, id: &CbIdentity, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let (req, callback_id, operation_id) = decode_http_request(raw)?;
        let op = self.acquire_http_operation(id, &callback_id, &operation_id).await?;
        let result = self.new_http_client(None, "").do_request(&op.ctx, req).await;
        op.finish();
        marshal_result(&result?)
    }

    async fn cb_http_do_stream(self: &Arc<Self>, id: &CbIdentity, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let (req, callback_id, operation_id) = decode_http_request(raw)?;
        let op = self.acquire_http_operation(id, &callback_id, &operation_id).await?;
        let op_ctx = op.ctx.clone();
        let plugin_id = op.plugin_id.clone();
        let op_id = op.operation_id.clone();
        let instance = op.instance.clone();
        let stream = match self.new_http_client(None, "").do_stream(&op_ctx, req).await {
            Ok(s) => s,
            Err(e) => {
                op.finish();
                return Err(e);
            }
        };
        // The stream owns the operation from here on; closing the stream finishes it.
        let ops = self.bridges.http_ops.clone();
        let (finish_ctx, finish_plugin, finish_id) = op.detach();
        let on_close: Box<dyn FnOnce() + Send> = {
            let (ops, plugin, op_id) = (ops.clone(), finish_plugin.clone(), finish_id.clone());
            Box::new(move || ops.finish_by_id(&plugin, &op_id))
        };
        let stream_id = self.bridges.http_streams.open(&plugin_id, &instance, stream.chunks, op_ctx.clone(), on_close);
        let resp = RpcHostHttpStreamResponse { status_code: i64::from(stream.status), headers: stream.headers, stream_id: stream_id.clone() };
        let raw = marshal_result(&resp);
        let cleanup: Box<dyn FnOnce() + Send> = {
            let (streams, plugin, instance, sid) = (self.bridges.http_streams.clone(), plugin_id.clone(), instance.clone(), stream_id.clone());
            Box::new(move || streams.close(&plugin, &instance, &sid))
        };
        let _ = finish_ctx;
        if !ops.set_cleanup(&plugin_id, &op_id, cleanup) {
            return Err(HostError::canceled());
        }
        raw
    }

    fn cb_http_operation_open(self: &Arc<Self>, id: &CbIdentity, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: OperationOpenRequest = decode(raw, "host http operation open request")?;
        let operation_id = self.open_http_operation(id, &req.host_callback_id)?;
        marshal_result(&OperationOpenResponse { operation_id })
    }

    fn cb_http_cancel(self: &Arc<Self>, id: &CbIdentity, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: HttpCancelRequest = decode(raw, "host http cancel request")?;
        let operation_id = req.operation_id.trim();
        if operation_id.is_empty() {
            return Err(HostError::msg("host http operation id is required"));
        }
        let plugin_id = self.callback_caller_plugin_id(id, &req.host_callback_id);
        let instance = match &id.instance {
            Some(i) => Some(i.clone()),
            None => self.bridges.contexts.lookup(&req.host_callback_id).and_then(|(_, _, i)| i),
        };
        self.bridges.http_ops.cancel(&plugin_id, &instance, operation_id);
        Ok(empty_result())
    }

    async fn cb_http_stream_read(self: &Arc<Self>, id: &CbIdentity, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: StreamIdRequest = decode(raw, "host http stream read request")?;
        let ctx = CallCtx::background();
        let (chunk, done) = self.bridges.http_streams.read(&ctx, &id.plugin_id, &id.instance, &req.stream_id).await.map_err(HostError::msg)?;
        let mut resp = RpcHostHttpStreamReadResponse { payload: Vec::new(), error: String::new(), done };
        if let Some(chunk) = chunk {
            resp.payload = chunk.payload.to_vec();
            if let Some(err) = chunk.err {
                resp.error = err;
                resp.done = true;
            }
        }
        marshal_result(&resp)
    }

    fn cb_http_stream_close(self: &Arc<Self>, id: &CbIdentity, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: StreamIdRequest = decode(raw, "host http stream close request")?;
        self.bridges.http_streams.close(&id.plugin_id, &id.instance, &req.stream_id);
        Ok(empty_result())
    }

    /// Looks up a callback scope and verifies it belongs to the calling plugin instance.
    #[allow(clippy::type_complexity)]
    fn scope_for_caller(
        &self,
        id: &CbIdentity,
        callback_id: &str,
    ) -> Result<(CallCtx, String, Option<Arc<CallbackInstance>>), HostError> {
        let mut plugin_id = id.plugin_id.clone();
        let mut instance = id.instance.clone();
        let mut parent = CallCtx::background();
        if !callback_id.is_empty() {
            let Some((ctx, cb_plugin, cb_instance)) = self.bridges.contexts.lookup(callback_id) else {
                return Err(HostError::msg("host callback ID is not open"));
            };
            parent = ctx;
            if !plugin_id.is_empty() && cb_plugin != plugin_id {
                return Err(HostError::msg("host callback ID does not belong to the calling plugin"));
            }
            if plugin_id.is_empty() {
                plugin_id = cb_plugin;
            }
            if !same_instance(&instance, &cb_instance) && (instance.is_some() || cb_instance.is_some()) {
                return Err(HostError::msg("host callback ID does not belong to the calling plugin instance"));
            }
            if instance.is_none() {
                instance = cb_instance;
            }
        }
        Ok((parent, plugin_id, instance))
    }

    /// Go `openHostHTTPOperation`.
    fn open_http_operation(self: &Arc<Self>, id: &CbIdentity, callback_id: &str) -> Result<String, HostError> {
        let callback_id = callback_id.trim().to_string();
        let (parent, plugin_id, instance) = self.scope_for_caller(id, &callback_id)?;
        let (operation_id, _) = self.create_http_operation(&plugin_id, instance, &callback_id, &parent, false)?;
        Ok(operation_id)
    }

    /// Go `createHostHTTPOperation`.
    fn create_http_operation(
        self: &Arc<Self>,
        plugin_id: &str,
        instance: Option<Arc<CallbackInstance>>,
        callback_id: &str,
        parent: &CallCtx,
        claimed: bool,
    ) -> Result<(String, CallCtx), HostError> {
        let Some((operation_id, ctx)) = self.bridges.http_ops.open(plugin_id, instance.clone(), callback_id, parent, claimed) else {
            return Err(HostError::msg("host http operation bridge is unavailable"));
        };
        if !callback_id.is_empty() {
            let ops = self.bridges.http_ops.clone();
            let (p, i, o) = (plugin_id.to_string(), instance, operation_id.clone());
            let cleanup: Box<dyn FnOnce() + Send> = Box::new(move || ops.cancel(&p, &i, &o));
            let handle = self.bridges.contexts.add_cleanup(callback_id, cleanup);
            let attached = handle.and_then(|stop| self.bridges.http_ops.set_scope_cleanup(plugin_id, &operation_id, stop).then_some(()));
            if attached.is_none() {
                self.bridges.http_ops.cancel(plugin_id, &None, &operation_id);
                return Err(HostError::msg("host callback context closed while opening HTTP operation"));
            }
        }
        Ok((operation_id, ctx))
    }

    /// Go `acquireHostHTTPOperation`.
    async fn acquire_http_operation(
        self: &Arc<Self>,
        id: &CbIdentity,
        callback_id: &str,
        operation_id: &str,
    ) -> Result<OperationHandle, HostError> {
        let operation_id = operation_id.trim();
        let callback_id = callback_id.trim();
        if operation_id.is_empty() {
            let (parent, plugin_id, instance) = self.scope_for_caller(id, callback_id)?;
            let (op_id, ctx) = self.create_http_operation(&plugin_id, instance.clone(), callback_id, &parent, true)?;
            return Ok(self.bridges.http_ops.handle(&plugin_id, &op_id, ctx, instance));
        }
        let mut plugin_id = id.plugin_id.clone();
        let mut instance = id.instance.clone();
        if (plugin_id.is_empty() || instance.is_none())
            && let Some((_, cb_plugin, cb_instance)) = self.bridges.contexts.lookup(callback_id)
        {
            if plugin_id.is_empty() {
                plugin_id = cb_plugin;
            }
            if instance.is_none() {
                instance = cb_instance;
            }
        }
        self.bridges
            .http_ops
            .claim(&plugin_id, &instance, operation_id, callback_id)
            .ok_or_else(|| HostError::msg(format!("host http operation {operation_id:?} is not open")))
    }

    // ---- executor stream bridge ----

    async fn cb_stream_emit(self: &Arc<Self>, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: StreamEmitRequest = decode(raw, "stream emit request")?;
        let item = if req.error.is_empty() { Ok(Bytes::from(req.payload)) } else { Err(req.error) };
        let Some(tx) = self.bridges.streams.sender(&req.stream_id) else {
            return Err(HostError::msg(format!("stream {} is not open", req.stream_id)));
        };
        if req.stream_id.is_empty() {
            return Err(HostError::msg("stream id is required"));
        }
        tx.send(item).await.map_err(|_| HostError::msg(format!("stream {} is not open", req.stream_id)))?;
        Ok(empty_result())
    }

    async fn cb_stream_close(self: &Arc<Self>, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: StreamCloseRequest = decode(raw, "stream close request")?;
        self.bridges.streams.close(&req.stream_id, &req.error).await;
        Ok(empty_result())
    }

    // ---- model execution ----

    async fn cb_model_execute(self: &Arc<Self>, id: &CbIdentity, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: RpcHostModelExecutionRequest = decode(raw, "host model execution request")?;
        if req.inner.stream {
            return Err(HostError::msg("host.model.execute requires stream=false"));
        }
        validate_model_proxy(&req.inner.proxy_url)?;
        let Some(executor) = self.model_executor() else {
            return Err(HostError::msg("host model executor is unavailable"));
        };
        let skip = self.callback_caller_plugin_id(id, &req.host_callback_id);
        let ctx = self.bridges.contexts.resolve(&req.host_callback_id, &CallCtx::background());
        let resp = executor.execute_model(&ctx, model_request_from_plugin(req.inner, &skip)).await.map_err(model_exec_error)?;
        marshal_result(&HostModelExecutionResponse { status_code: resp.status_code, headers: resp.headers, body: resp.body })
    }

    async fn cb_model_execute_stream(self: &Arc<Self>, id: &CbIdentity, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: RpcHostModelExecutionRequest = decode(raw, "host model execution stream request")?;
        if !req.inner.stream {
            return Err(HostError::msg("host.model.execute_stream requires stream=true"));
        }
        validate_model_proxy(&req.inner.proxy_url)?;
        let Some(executor) = self.model_executor() else {
            return Err(HostError::msg("host model executor is unavailable"));
        };
        let skip = self.callback_caller_plugin_id(id, &req.host_callback_id);
        let callback_ctx = self.bridges.contexts.resolve(&req.host_callback_id, &CallCtx::background());
        // Request cancellation is detached; callback cleanup owns the stream lifetime.
        let stream_ctx = callback_ctx.detached();
        let callback_id = req.host_callback_id.clone();
        let stream = match executor.execute_model_stream(&stream_ctx, model_request_from_plugin(req.inner, &skip)).await {
            Ok(s) => s,
            Err(e) => {
                stream_ctx.cancel();
                return Err(model_exec_error(e));
            }
        };
        let stream_id = self.bridges.model_streams.open(stream.chunks, stream_ctx);
        if !callback_id.is_empty() {
            let streams = self.bridges.model_streams.clone();
            let sid = stream_id.clone();
            let _ = self.bridges.contexts.add_cleanup(&callback_id, Box::new(move || streams.close(&sid)));
        }
        marshal_result(&HostModelStreamResponse { status_code: stream.status_code, headers: stream.headers, stream_id })
    }

    async fn cb_model_stream_read(self: &Arc<Self>, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: HostModelStreamReadRequest = decode(raw, "host model stream read request")?;
        let (chunk, done) = self.bridges.model_streams.read(&CallCtx::background(), &req.stream_id).await.map_err(HostError::msg)?;
        let mut resp = HostModelStreamReadResponse { payload: Vec::new(), error: String::new(), done };
        if let Some(chunk) = chunk {
            resp.payload = chunk.payload.to_vec();
            if let Some(err) = chunk.err {
                resp.error = err.text();
                resp.done = true;
            }
        }
        marshal_result(&resp)
    }

    fn cb_model_stream_close(self: &Arc<Self>, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: HostModelStreamCloseRequest = decode(raw, "host model stream close request")?;
        self.bridges.model_streams.close(&req.stream_id);
        Ok(empty_result())
    }

    // ---- logging ----

    fn cb_log(self: &Arc<Self>, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: HostLogRequest = decode(raw, "host log request")?;
        let ctx = self.bridges.contexts.resolve(&req.host_callback_id, &CallCtx::background());
        let message = if req.message.trim().is_empty() { "plugin log".to_string() } else { req.message.trim().to_string() };
        let request_id = ctx.request_id.clone();
        match req.level.trim().to_lowercase().as_str() {
            "trace" => tracing::trace!(target: "cpa::plugin", request_id = %request_id, "{message}"),
            "info" => tracing::info!(target: "cpa::plugin", request_id = %request_id, "{message}"),
            "warn" | "warning" => tracing::warn!(target: "cpa::plugin", request_id = %request_id, "{message}"),
            "error" => tracing::error!(target: "cpa::plugin", request_id = %request_id, "{message}"),
            _ => tracing::debug!(target: "cpa::plugin", request_id = %request_id, "{message}"),
        }
        Ok(empty_result())
    }

    // ---- affinity ----

    fn cb_affinity_lookup(self: &Arc<Self>, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let mut req: HostAffinityLookupRequest = decode(raw, "host affinity lookup request")?;
        req.provider = req.provider.trim().to_string();
        req.model = req.model.trim().to_string();
        req.session_id = req.session_id.trim().to_string();
        if req.provider.is_empty() {
            return Err(HostError::msg("provider is required"));
        }
        if req.model.is_empty() {
            return Err(HostError::msg("model is required"));
        }
        if req.session_id.is_empty() {
            return Err(HostError::msg("session_id is required"));
        }
        let now = Some(chrono::Utc::now());
        let Some(manager) = self.auth_manager() else {
            return marshal_result(&HostAffinityLookupResponse {
                status: AFFINITY_STATUS_UNSUPPORTED.into(),
                observed_at: now,
                ..Default::default()
            });
        };
        let (auth, status) = manager.lookup_session_affinity(&req.provider, &req.model, &req.session_id);
        let Some(mut auth) = auth.filter(|_| status == AFFINITY_STATUS_BOUND) else {
            return marshal_result(&HostAffinityLookupResponse { status: status.into(), observed_at: now, ..Default::default() });
        };
        let index = auth.ensure_index();
        let disabled = auth.disabled || auth.status == cpa_auth::Status::Disabled;
        let unavailable = auth.unavailable || auth.status == cpa_auth::Status::Error;
        marshal_result(&HostAffinityLookupResponse {
            status: AFFINITY_STATUS_BOUND.into(),
            auth_index: index,
            observed_at: now,
            disabled,
            unavailable,
        })
    }
}

fn decode_http_request(raw: &[u8]) -> Result<(HttpRequest, String, String), HostError> {
    let req: RpcHostHttpRequest = decode(raw, "host http request")?;
    let operation_id = req.operation_id.trim().to_string();
    if let Some(inner) = req.request {
        let wire_profile = inner.wire_profile.or(req.wire_profile);
        return Ok((
            HttpRequest { method: inner.method, url: inner.url, headers: inner.headers, body: inner.body, wire_profile },
            req.host_callback_id,
            operation_id,
        ));
    }
    Ok((
        HttpRequest { method: req.method, url: req.url, headers: req.headers, body: req.body, wire_profile: req.wire_profile },
        req.host_callback_id,
        operation_id,
    ))
}

fn model_request_from_plugin(req: HostModelExecutionRequest, skip_plugin_id: &str) -> ModelExecutionRequest {
    ModelExecutionRequest {
        entry_protocol: req.entry_protocol,
        exit_protocol: req.exit_protocol,
        model: req.model,
        stream: req.stream,
        body: req.body,
        headers: req.headers,
        query: req.query,
        alt: req.alt,
        skip_interceptor_plugin_id: skip_plugin_id.to_string(),
        skip_router_plugin_id: skip_plugin_id.to_string(),
        forced_provider: req.forced_provider,
        auth_id: req.auth_id,
        proxy_url: req.proxy_url.trim().to_string(),
        path: req.path.trim().to_string(),
    }
}

/// `proxyutil.ValidRequestProxy` applied to `proxy_url` (empty is fine): a concrete proxy with a
/// host and a valid port.
fn validate_model_proxy(raw: &str) -> Result<(), HostError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(());
    }
    let valid = match parse_proxy(raw) {
        Ok(ProxySetting::Proxy(p)) => url::Url::parse(&p).is_ok_and(|u| {
            u.host_str().is_some_and(|h| !h.trim().is_empty())
                && u.port().map(|p| p >= 1).unwrap_or(true)
        }),
        _ => false,
    };
    if valid { Ok(()) } else { Err(HostError::with_status("invalid proxy_url", 400)) }
}


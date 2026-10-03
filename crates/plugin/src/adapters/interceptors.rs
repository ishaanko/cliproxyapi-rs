//! Request/response interceptors, lifecycle notifications and websocket observers (Go:
//! `adapters_interceptors.go`).

use std::sync::Arc;

use cpa_pluginapi::abi;
use cpa_pluginapi::api::{
    RequestCompletion, RequestInterceptRequest, RequestInterceptResponse, ResponseInterceptRequest, ResponseInterceptResponse,
    STREAM_CHUNK_HEADER_INIT_INDEX, StreamChunkInterceptRequest, StreamChunkInterceptResponse, WebSocketResponseEvent,
};
use serde_json::{Map, Value};

use crate::caps::Record;
use crate::convert::merge_headers;
use crate::ctx::CallCtx;
use crate::host::Host;

/// Metadata key the executor reads the inbound request path from.
const REQUEST_PATH_METADATA_KEY: &str = "request_path";

fn omits_request_bodies(schema_version: u32) -> bool {
    schema_version >= abi::SCHEMA_VERSION_STREAM_CHUNK_OMIT_REQUEST_BODY
}

fn omits_history(schema_version: u32) -> bool {
    schema_version >= abi::SCHEMA_VERSION_STREAM_CHUNK_OMIT_HISTORY
}

#[derive(Clone, Copy)]
enum Stage {
    Before,
    After,
}

impl Host {
    fn any_active(&self, pred: impl Fn(&Record) -> bool) -> bool {
        self.active_records().iter().any(|r| !self.is_plugin_fused(&r.id) && pred(r))
    }

    pub fn has_request_interceptors(&self) -> bool {
        self.any_active(|r| r.caps().request_interceptor)
    }

    pub fn has_stream_interceptors(&self) -> bool {
        self.any_active(|r| r.caps().stream_chunk_interceptor)
    }

    pub fn has_websocket_response_observers(&self) -> bool {
        self.any_active(|r| r.caps().websocket_response_observer)
    }

    /// Whether an active stream interceptor still needs request bodies on payload chunks.
    pub fn stream_chunk_payload_includes_request_body(&self) -> bool {
        self.any_active(|r| r.caps().stream_chunk_interceptor && !omits_request_bodies(r.schema_version()))
    }

    /// Whether an active stream interceptor still needs history chunks on payload chunks.
    pub fn stream_chunk_payload_includes_history(&self) -> bool {
        self.any_active(|r| r.caps().stream_chunk_interceptor && !omits_history(r.schema_version()))
    }

    pub async fn intercept_request_before_auth(
        self: &Arc<Self>,
        ctx: &CallCtx,
        req: RequestInterceptRequest,
        skip_plugin_id: &str,
    ) -> RequestInterceptResponse {
        self.intercept_request(ctx, req, Stage::Before, skip_plugin_id).await
    }

    pub async fn intercept_request_after_auth(
        self: &Arc<Self>,
        ctx: &CallCtx,
        req: RequestInterceptRequest,
        skip_plugin_id: &str,
    ) -> RequestInterceptResponse {
        self.intercept_request(ctx, req, Stage::After, skip_plugin_id).await
    }

    async fn call_request_interceptor(
        self: &Arc<Self>,
        ctx: &CallCtx,
        rec: &Record,
        stage: Stage,
        req: &RequestInterceptRequest,
    ) -> Option<RequestInterceptResponse> {
        if !self.usable(rec) {
            return None;
        }
        let method = match stage {
            Stage::Before => abi::METHOD_REQUEST_INTERCEPT_BEFORE,
            Stage::After => abi::METHOD_REQUEST_INTERCEPT_AFTER,
        };
        match self.rpc_cb::<RequestInterceptResponse>(rec, ctx, method, req).await {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!("pluginhost: request interceptor {} failed: {e}", rec.id);
                None
            }
        }
    }

    async fn intercept_request(
        self: &Arc<Self>,
        ctx: &CallCtx,
        req: RequestInterceptRequest,
        stage: Stage,
        skip_plugin_id: &str,
    ) -> RequestInterceptResponse {
        let skip = skip_plugin_id.trim();
        let mut current = RequestInterceptResponse { headers: req.headers.clone(), ..Default::default() };
        let mut current_base = req.body.clone();
        let mut body_modified = false;
        for rec in self.active_records() {
            if self.is_plugin_fused(&rec.id) || !rec.caps().request_interceptor || rec.id == skip {
                continue;
            }
            let mut next = req.clone();
            next.headers = current.headers.clone();
            next.body = current_base.clone();
            if !current.path.is_empty() {
                next.metadata.insert(REQUEST_PATH_METADATA_KEY.into(), Value::String(current.path.clone()));
            }
            let Some(resp) = self.call_request_interceptor(ctx, &rec, stage, &next).await else { continue };
            current.headers = merge_headers(&current.headers, &resp.headers, &resp.clear_headers);
            if !resp.body.is_empty() {
                current_base = resp.body.clone();
                body_modified = true;
            }
            if !resp.path.trim().is_empty() {
                current.path = resp.path.trim().to_string();
            }
            if resp.terminate {
                current.terminate = true;
                current.status_code = resp.status_code;
                current.response_headers = resp.response_headers;
                current.response_body = resp.response_body;
                break;
            }
        }
        if body_modified {
            current.body = current_base;
        }
        current
    }

    /// Schedules terminal notifications without blocking response delivery (Go:
    /// `CompleteRequestExcept`).
    pub fn complete_request(self: &Arc<Self>, ctx: &CallCtx, completion: RequestCompletion, skip_plugin_id: &str) {
        let skip = skip_plugin_id.trim().to_string();
        let ctx = ctx.detached();
        for rec in self.active_records() {
            if self.is_plugin_fused(&rec.id) || !rec.caps().request_lifecycle_plugin || rec.id == skip || !self.record_current(&rec) {
                continue;
            }
            let host = self.clone();
            let ctx = ctx.clone();
            let completion = completion.clone();
            tokio::spawn(async move {
                if let Err(e) = host.rpc_cb::<crate::client::Empty>(&rec, &ctx, abi::METHOD_REQUEST_COMPLETE, &completion).await {
                    tracing::warn!("pluginhost: request lifecycle plugin {} failed: {e}", rec.id);
                }
            });
        }
    }

    /// Chained response interceptors for a non-streaming response (Go: `InterceptResponseExcept`).
    pub async fn intercept_response(
        self: &Arc<Self>,
        ctx: &CallCtx,
        req: ResponseInterceptRequest,
        skip_plugin_id: &str,
    ) -> ResponseInterceptResponse {
        let skip = skip_plugin_id.trim();
        let mut current = ResponseInterceptResponse { headers: req.response_headers.clone(), body: req.body.clone(), ..Default::default() };
        for rec in self.active_records() {
            if self.is_plugin_fused(&rec.id) || !rec.caps().response_interceptor || rec.id == skip {
                continue;
            }
            let mut next = req.clone();
            next.response_headers = current.headers.clone();
            next.body = current.body.clone();
            if !self.usable(&rec) {
                continue;
            }
            match self.rpc_cb::<ResponseInterceptResponse>(&rec, ctx, abi::METHOD_RESPONSE_INTERCEPT_AFTER, &next).await {
                Ok(resp) => {
                    current.headers = merge_headers(&current.headers, &resp.headers, &resp.clear_headers);
                    if !resp.body.is_empty() {
                        current.body = resp.body;
                    }
                }
                Err(e) => tracing::warn!("pluginhost: response interceptor {} failed: {e}", rec.id),
            }
        }
        current
    }

    /// Chained stream chunk interceptors (Go: `InterceptStreamChunkExcept`).
    pub async fn intercept_stream_chunk(
        self: &Arc<Self>,
        ctx: &CallCtx,
        req: StreamChunkInterceptRequest,
        skip_plugin_id: &str,
    ) -> StreamChunkInterceptResponse {
        let skip = skip_plugin_id.trim();
        let mut current = StreamChunkInterceptResponse { headers: req.response_headers.clone(), body: req.body.clone(), ..Default::default() };
        for rec in self.active_records() {
            if self.is_plugin_fused(&rec.id) || !rec.caps().stream_chunk_interceptor || current.drop_chunk || rec.id == skip {
                continue;
            }
            let mut next = req.clone();
            next.response_headers = current.headers.clone();
            let payload_chunk = req.chunk_index != STREAM_CHUNK_HEADER_INIT_INDEX;
            // Schema v3+ omits request bodies on payload chunks; v5+ also omits history.
            if payload_chunk && omits_request_bodies(rec.schema_version()) {
                next.original_request = Vec::new();
                next.request_body = Vec::new();
            }
            next.body = current.body.clone();
            if payload_chunk && omits_history(rec.schema_version()) {
                next.history_chunks = Vec::new();
            }
            if !self.usable(&rec) {
                continue;
            }
            match self.rpc_cb::<StreamChunkInterceptResponse>(&rec, ctx, abi::METHOD_RESPONSE_INTERCEPT_STREAM_CHUNK, &next).await {
                Ok(resp) => {
                    current.headers = merge_headers(&current.headers, &resp.headers, &resp.clear_headers);
                    if !resp.body.is_empty() {
                        current.body = resp.body;
                    }
                    if resp.drop_chunk {
                        current.drop_chunk = true;
                    }
                }
                Err(e) => tracing::warn!("pluginhost: stream chunk interceptor {} failed: {e}", rec.id),
            }
        }
        current
    }

    /// Delivers an upstream websocket response event to observers (Go:
    /// `ObserveWebSocketResponseEventExcept`).
    pub async fn observe_websocket_response_event(self: &Arc<Self>, ctx: &CallCtx, event: WebSocketResponseEvent, skip_plugin_id: &str) {
        let skip = skip_plugin_id.trim();
        let ctx = ctx.detached();
        for rec in self.active_records() {
            if self.is_plugin_fused(&rec.id) || !rec.caps().websocket_response_observer || rec.id == skip || !self.record_current(&rec) {
                continue;
            }
            if let Err(e) = self.rpc_cb::<crate::client::Empty>(&rec, &ctx, abi::METHOD_WEBSOCKET_RESPONSE_EVENT, &event).await {
                tracing::warn!("pluginhost: websocket response observer {} failed: {e}", rec.id);
            }
        }
    }
}

/// A fresh metadata map for plugin payloads.
pub fn clone_metadata(src: &Map<String, Value>) -> Map<String, Value> {
    src.clone()
}

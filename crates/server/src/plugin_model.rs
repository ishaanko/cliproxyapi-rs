//! Nested model execution for plugins (Go: sdk/api/handlers `model_execution.go`): the plugin host
//! calls back into the handler pipeline for `host.model.*` requests.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use cpa_core::format::Format;
use cpa_plugin::callbacks::{
    ModelExecError, ModelExecutionRequest, ModelExecutionResponse, ModelExecutionStream, ModelExecutor, ModelStreamError,
};
use cpa_plugin::convert::{headers_from_go, headers_to_go, query_from_go};
use cpa_plugin::ctx::RequestMeta;
use cpa_plugin::{CallCtx, bridge::ModelChunk};
use cpa_runtime::executor::meta;
use tokio::sync::mpsc;

use crate::error::ErrorMessage;
use crate::exec::{ExecArgs, ExecStream, Pipeline};
use crate::req::ReqInfo;
use crate::state::AppState;

/// Runs plugin-initiated model executions through the same pipeline as client requests.
pub struct ServerModelExecutor {
    state: AppState,
}

impl ServerModelExecutor {
    pub fn new(state: AppState) -> Arc<Self> {
        Arc::new(ServerModelExecutor { state })
    }

    /// The request facts the nested execution inherits: the inbound request when the call came
    /// from one, else what the conductor put in the context.
    fn req_info(&self, ctx: &CallCtx, req: &ModelExecutionRequest) -> ReqInfo {
        let mut info = if let Some(info) = ctx.ext.as_ref().and_then(|e| e.downcast_ref::<ReqInfo>()) {
            info.clone()
        } else {
            let md = ctx.ext.as_ref().and_then(|e| e.downcast_ref::<RequestMeta>());
            let text = |key: &str| md.and_then(|m| m.0.get(key)).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let api_key = text(cpa_runtime::conductor::usage::META_CLIENT_API_KEY);
            ReqInfo {
                method: Method::POST,
                route: String::new(),
                path: String::new(),
                raw_query: String::new(),
                query: Vec::new(),
                headers: HeaderMap::new(),
                remote: None,
                client_ip: text(meta::RESOLVED_CLIENT_IP),
                api_key: (!api_key.is_empty()).then_some(api_key),
                request_id: {
                    let id = text(cpa_runtime::conductor::usage::META_REQUEST_ID);
                    if id.is_empty() { ctx.request_id.clone() } else { id }
                },
                api_log: Default::default(),
                trace: Default::default(),
            }
        };
        if !req.path.trim().is_empty() {
            info.path = req.path.trim().to_string();
        }
        info
    }

    /// `executeModelFormats` arguments of a nested request.
    fn args<'a>(&self, req: &'a ModelExecutionRequest, entry: Format, exit: Option<Format>) -> ExecArgs<'a> {
        let mut a = ExecArgs::new(entry, &req.model, Bytes::from(req.body.clone()), &req.alt);
        a.exit = exit;
        a.allow_image_model = is_image_protocol(&req.entry_protocol) || is_image_protocol(&req.exit_protocol);
        a.forced_provider = Some(req.forced_provider.as_str()).filter(|s| !s.trim().is_empty());
        a.pinned_auth_id = Some(req.auth_id.as_str()).filter(|s| !s.trim().is_empty());
        a.skip_plugin_id = Some(req.skip_interceptor_plugin_id.as_str()).filter(|s| !s.trim().is_empty());
        a.proxy_url = Some(req.proxy_url.as_str()).filter(|s| !s.trim().is_empty());
        a.request_path = Some(req.path.as_str()).filter(|s| !s.trim().is_empty());
        a.internal_source = true;
        if !req.headers.is_empty() {
            a.headers = Some(headers_from_go(&req.headers));
        }
        if !req.query.is_empty() {
            a.query = Some(query_from_go(&req.query));
        }
        a
    }
}

fn is_image_protocol(protocol: &str) -> bool {
    protocol.trim().eq_ignore_ascii_case("openai-image")
}

fn mode_error(message: &str) -> ModelExecError {
    ModelExecError { status: 400, message: message.to_string() }
}

fn exec_error(e: &ErrorMessage) -> ModelExecError {
    ModelExecError { status: e.status_or_500() as i32, message: e.text.clone() }
}

fn protocol(raw: &str) -> Result<Format, ModelExecError> {
    Format::parse(raw.trim()).ok_or_else(|| ModelExecError { status: 400, message: format!("unsupported protocol {raw:?}") })
}

#[async_trait]
impl ModelExecutor for ServerModelExecutor {
    async fn execute_model(&self, ctx: &CallCtx, req: ModelExecutionRequest) -> Result<ModelExecutionResponse, ModelExecError> {
        ctx.mark_nested();
        if req.stream {
            return Err(mode_error("ExecuteModel requires Stream=false"));
        }
        let entry = protocol(&req.entry_protocol)?;
        let exit = if req.exit_protocol.trim().is_empty() { None } else { Some(protocol(&req.exit_protocol)?) };
        let pipeline = Pipeline::new(&self.state, &self.req_info(ctx, &req));
        let ok = pipeline.execute(self.args(&req, entry, exit)).await.map_err(|e| exec_error(&e))?;
        Ok(ModelExecutionResponse { status_code: 200, headers: headers_to_go(&ok.headers), body: ok.body.to_vec() })
    }

    async fn execute_model_stream(&self, ctx: &CallCtx, req: ModelExecutionRequest) -> Result<ModelExecutionStream, ModelExecError> {
        ctx.mark_nested();
        if !req.stream {
            return Err(mode_error("ExecuteModelStream requires Stream=true"));
        }
        let entry = protocol(&req.entry_protocol)?;
        let exit = if req.exit_protocol.trim().is_empty() { None } else { Some(protocol(&req.exit_protocol)?) };
        let pipeline = Pipeline::new(&self.state, &self.req_info(ctx, &req));
        let ExecStream { headers, mut rx } = pipeline.execute_stream(self.args(&req, entry, exit)).await;
        // `prepareModelExecutionStream`: a failure before the first payload is a call error.
        let first = tokio::select! {
            item = rx.recv() => item,
            () = ctx.token().cancelled() => None,
        };
        let pending = match first {
            Some(Err(e)) => return Err(exec_error(&e)),
            Some(Ok(payload)) => Some(payload),
            None => None,
        };
        let (tx, chunks) = mpsc::channel::<ModelChunk>(1);
        let cancel = ctx.token().clone();
        tokio::spawn(async move {
            if let Some(payload) = pending
                && !send(&tx, &cancel, ModelChunk { payload, err: None }).await
            {
                return;
            }
            loop {
                let item = tokio::select! {
                    item = rx.recv() => item,
                    () = cancel.cancelled() => return,
                };
                match item {
                    None => return,
                    Some(Ok(payload)) => {
                        if !send(&tx, &cancel, ModelChunk { payload, err: None }).await {
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        let err = ModelStreamError {
                            status_code: e.status_or_500() as i64,
                            message: e.text.clone(),
                            headers: headers_to_go(&e.addon),
                        };
                        let _ = send(&tx, &cancel, ModelChunk { payload: Bytes::new(), err: Some(err) }).await;
                        return;
                    }
                }
            }
        });
        Ok(ModelExecutionStream { status_code: 200, headers: headers_to_go(&headers), chunks })
    }
}

async fn send(tx: &mpsc::Sender<ModelChunk>, cancel: &tokio_util::sync::CancellationToken, chunk: ModelChunk) -> bool {
    tokio::select! {
        r = tx.send(chunk) => r.is_ok(),
        () = cancel.cancelled() => false,
    }
}

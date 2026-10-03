//! Scheduler and model router plugins (Go: `scheduler.go`, `model_router.go`).

use std::sync::Arc;

use async_trait::async_trait;
use cpa_pluginapi::abi;
use cpa_pluginapi::api::{
    ModelRouteRequest, ModelRouteResponse, ROUTE_TARGET_EXECUTOR, ROUTE_TARGET_PROVIDER, ROUTE_TARGET_SELF,
    SCHEDULER_BUILTIN_FILL_FIRST, SCHEDULER_BUILTIN_ROUND_ROBIN, SchedulerPickRequest, SchedulerPickResponse,
};
use cpa_runtime::conductor::PluginScheduler;
use cpa_runtime::executor::ExecError;
use cpa_translator::Format;

use crate::caps::Record;
use crate::ctx::CallCtx;
use crate::host::Host;

fn valid_scheduler_builtin(delegate: &str) -> bool {
    delegate == SCHEDULER_BUILTIN_ROUND_ROBIN || delegate == SCHEDULER_BUILTIN_FILL_FIRST
}

/// Go `normalizeSchedulerResponse`: `Err` carries the reason a handled response is unusable.
fn normalize_scheduler_response(mut resp: SchedulerPickResponse, req: &SchedulerPickRequest) -> Result<SchedulerPickResponse, &'static str> {
    resp.auth_id = resp.auth_id.trim().to_string();
    resp.delegate_builtin = resp.delegate_builtin.trim().to_string();
    resp.reject_code = resp.reject_code.trim().to_string();
    resp.reject_reason = resp.reject_reason.trim().to_string();
    if resp.reject {
        if resp.reject_code.is_empty() {
            resp.reject_code = "auth_unavailable".into();
        }
        if resp.reject_reason.is_empty() {
            resp.reject_reason = "scheduler rejected candidate selection".into();
        }
        return Ok(resp);
    }
    let has_auth = !resp.auth_id.is_empty();
    let has_delegate = !resp.delegate_builtin.is_empty();
    if !has_auth && !has_delegate {
        return Err("missing auth id or delegate");
    }
    if has_auth {
        if !req.candidates.iter().any(|c| c.id.trim() == resp.auth_id) {
            return Err("unknown auth id");
        }
        return Ok(resp);
    }
    if !valid_scheduler_builtin(&resp.delegate_builtin) {
        return Err("unknown delegate");
    }
    Ok(resp)
}

/// Go `normalizeModelRouteResponse`.
fn normalize_model_route_response(router_plugin_id: &str, mut resp: ModelRouteResponse) -> Option<ModelRouteResponse> {
    resp.target_model = resp.target_model.trim().to_string();
    match resp.target_kind.as_str() {
        ROUTE_TARGET_SELF => {
            resp.target = router_plugin_id.trim().to_string();
            (!resp.target.is_empty()).then_some(resp)
        }
        ROUTE_TARGET_EXECUTOR => {
            resp.target = resp.target.trim().to_string();
            (!resp.target.is_empty()).then_some(resp)
        }
        ROUTE_TARGET_PROVIDER => {
            resp.target = resp.target.trim().to_lowercase();
            (!resp.target.is_empty()).then_some(resp)
        }
        _ => None,
    }
}

impl Host {
    fn scheduler_record(&self) -> Option<Arc<Record>> {
        self.active_records().into_iter().find(|r| !self.is_plugin_fused(&r.id) && r.caps().scheduler)
    }

    pub fn has_scheduler(&self) -> bool {
        self.scheduler_record().is_some()
    }

    pub fn scheduler_wants_across_priorities(&self) -> bool {
        self.scheduler_record().is_some_and(|r| r.caps().scheduler && r.caps().scheduler_across_priorities)
    }

    /// Go `PickAuth`: `Ok(None)` when no plugin handled the pick.
    pub async fn pick_auth(&self, ctx: &CallCtx, mut req: SchedulerPickRequest) -> Result<Option<SchedulerPickResponse>, crate::client::PluginError> {
        let Some(rec) = self.scheduler_record() else { return Ok(None) };
        if !self.usable(&rec) {
            return Ok(None);
        }
        req.plugin = rec.meta.clone();
        let resp: SchedulerPickResponse = match self.rpc(&rec, ctx, abi::METHOD_SCHEDULER_PICK, &req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(plugin_id = %rec.id, "pluginhost: scheduler rejected auth pick: {e}");
                return Err(e);
            }
        };
        if !resp.handled {
            return Ok(None);
        }
        match normalize_scheduler_response(resp, &req) {
            Ok(r) => Ok(Some(r)),
            Err(reason) => {
                tracing::warn!(plugin_id = %rec.id, "pluginhost: scheduler returned invalid response: {reason}");
                Ok(None)
            }
        }
    }

    // ---- model routers ----

    pub fn has_model_routers(&self) -> bool {
        self.has_model_routers_except("")
    }

    pub fn has_model_routers_except(&self, skip_plugin_id: &str) -> bool {
        let skip = skip_plugin_id.trim();
        self.active_records().iter().any(|r| r.caps().model_router && !self.is_plugin_fused(&r.id) && r.id != skip)
    }

    /// Whether a built-in provider currently has an auth registered (Go: `HasBuiltinProvider`).
    pub fn has_builtin_provider(&self, provider: &str) -> bool {
        self.auth_manager().is_some_and(|m| m.has_provider_auth(provider))
    }

    /// Built-in providers with an auth (Go: `BuiltinProviders`).
    pub fn builtin_providers(&self) -> Vec<String> {
        self.auth_manager().map(|m| m.available_providers()).unwrap_or_default()
    }

    /// First valid routing decision among the routers (Go: `RouteModelExcept`).
    pub async fn route_model(self: &Arc<Self>, ctx: &CallCtx, mut req: ModelRouteRequest, skip_plugin_id: &str) -> Option<ModelRouteResponse> {
        let skip = skip_plugin_id.trim();
        let providers = self.builtin_providers();
        req.available_providers = providers;
        for rec in self.active_records() {
            if !rec.caps().model_router || self.is_plugin_fused(&rec.id) || rec.id == skip {
                continue;
            }
            let mut next = req.clone();
            next.plugin = rec.meta.clone();
            next.plugin_id = rec.id.clone();
            if self.is_plugin_fused(&rec.id) {
                continue;
            }
            let resp: ModelRouteResponse = match self.rpc_cb(&rec, ctx, abi::METHOD_MODEL_ROUTE, &next).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(plugin_id = %rec.id, "pluginhost: model router failed: {e}");
                    continue;
                }
            };
            if !resp.handled {
                continue;
            }
            let Some(resp) = normalize_model_route_response(&rec.id, resp) else {
                tracing::warn!(plugin_id = %rec.id, "pluginhost: model router returned invalid target");
                continue;
            };
            match resp.target_kind.as_str() {
                ROUTE_TARGET_PROVIDER => {
                    if !self.has_builtin_provider(&resp.target) {
                        tracing::warn!(plugin_id = %rec.id, target_provider = %resp.target, "pluginhost: model router returned unavailable provider");
                        continue;
                    }
                    return Some(resp);
                }
                ROUTE_TARGET_SELF | ROUTE_TARGET_EXECUTOR => {
                    let source = Format::parse(&next.source_format);
                    if !source.is_some_and(|f| self.executor_plugin_ready(&resp.target, f, next.stream)) {
                        tracing::warn!(plugin_id = %rec.id, target_plugin_id = %resp.target, "pluginhost: model router returned unavailable executor plugin");
                        continue;
                    }
                    return Some(resp);
                }
                other => {
                    tracing::warn!(plugin_id = %rec.id, target_kind = %other, "pluginhost: model router returned unsupported target kind");
                    continue;
                }
            }
        }
        None
    }
}

#[async_trait]
impl PluginScheduler for Host {
    async fn pick_auth(&self, req: SchedulerPickRequest) -> Result<Option<SchedulerPickResponse>, ExecError> {
        match Host::pick_auth(self, &CallCtx::background(), req).await {
            Ok(r) => Ok(r),
            Err(e) => Err(ExecError::new(u16::try_from(e.status).unwrap_or(0), e.message)),
        }
    }

    fn has_scheduler(&self) -> bool {
        Host::has_scheduler(self)
    }

    fn wants_across_priorities(&self) -> bool {
        self.scheduler_wants_across_priorities()
    }
}

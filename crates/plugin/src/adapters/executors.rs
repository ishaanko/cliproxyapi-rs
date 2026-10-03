//! Plugin executors (Go: `adapters_executors.go`): registration with the auth manager, format
//! selection, request/response translation around the plugin call, usage reporting and stream
//! bridging.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_core::registry::{ModelInfo as RegistryModel, ModelRegistry};
use cpa_core::thinking::parse_suffix;
use cpa_executors::helps::responses_usage::ensure_responses_usage_details;
use cpa_executors::helps::text::extract_stream_json_payload;
use cpa_executors::helps::usage::{StreamUsageBuffer, UsageReporter};
use cpa_pluginapi::abi;
use cpa_pluginapi::api::{
    AuthRefreshRequest, AuthRefreshResponse, ExecutorHttpRequest, ExecutorHttpResponse, ExecutorRequest, ExecutorResponse,
    ExecutorStreamResponse,
};
use cpa_runtime::conductor::SharedManager;
use cpa_runtime::executor::{DynExecutor, ExecError, Executor, Options, Request, Response, StreamResult};
use cpa_translator::registry::Param;
use cpa_translator::{Ctx as TranslatorCtx, Format};
use tokio::sync::mpsc;

use super::models::{ModelClientRegistration, ModelRegistration, normalize_executor_formats};
use crate::bridge::StreamItem;
use crate::caps::Record;
use crate::convert::{
    headers_from_go, headers_to_go, host_config_summary, plugin_visible_metadata, preserve_file_auth_priority, query_to_go,
    storage_json_from_auth,
};
use crate::ctx::CallCtx;
use crate::host::Host;
use crate::usage_helpers::{
    observe_plugin_executor_stream_ttft, observe_plugin_executor_stream_usage, parse_plugin_executor_response_usage,
};

/// Go type name of the adapter, reported as the usage record's executor type.
const EXECUTOR_TYPE_NAME: &str = "executorAdapter";
const MAX_LINE_BUFFER_SIZE: usize = 64 * 1024;

/// A plugin executor registered with the auth manager as the executor of one provider.
pub struct ExecutorAdapter {
    host: Arc<Host>,
    record: Arc<Record>,
    provider: String,
    input_formats: Vec<Format>,
    output_formats: Vec<Format>,
}

/// Request/options after format selection (Go: `preparedExecutorCall`).
struct Prepared {
    req: Request,
    opts: Options,
    requested: Format,
    input: Format,
    output: Format,
}

impl Host {
    /// Provider key an executor plugin serves: the registered model provider, else its executor
    /// identifier (Go: `executorProvider`).
    pub(crate) fn executor_provider(&self, rec: &Record) -> Option<String> {
        if !self.record_current(rec) {
            return None;
        }
        let mut provider = self.model_provider(&rec.id);
        if provider.is_empty() {
            if self.is_plugin_fused(&rec.id) {
                return None;
            }
            provider = rec.executor_identifier();
            if provider.is_empty() {
                return None;
            }
        }
        let provider = provider.trim().to_lowercase();
        (!provider.is_empty()).then_some(provider)
    }

    pub(crate) fn new_executor_adapter(self: &Arc<Self>, rec: &Arc<Record>, provider: &str) -> Arc<ExecutorAdapter> {
        Arc::new(ExecutorAdapter {
            host: self.clone(),
            record: rec.clone(),
            provider: provider.to_string(),
            input_formats: normalize_executor_formats(&rec.caps().executor_input_formats),
            output_formats: normalize_executor_formats(&rec.caps().executor_output_formats),
        })
    }

    fn provider_has_native_executor(&self, manager: &SharedManager, provider: &str) -> bool {
        manager.executor(provider).is_some_and(|e| !self.owns_executor(&e))
    }

    fn model_has_native_executor(&self, manager: &SharedManager, registry: &ModelRegistry, model_id: &str) -> bool {
        registry.get_model_providers(model_id).iter().any(|p| self.provider_has_native_executor(manager, p))
    }

    /// True when `executor` is an adapter created by this host (Go: `OwnsExecutor`).
    pub fn owns_executor(&self, executor: &DynExecutor) -> bool {
        let ptr = Arc::as_ptr(executor) as *const ();
        self.state.lock().executor_adapters.values().any(|a| Arc::as_ptr(a) as *const () == ptr)
    }

    /// Whether any active executor plugin would serve `provider` (Go: `HasExecutorCandidateProvider`).
    pub fn has_executor_candidate_provider(&self, provider: &str) -> bool {
        let provider = provider.trim().to_lowercase();
        if provider.is_empty() {
            return false;
        }
        self.active_records().iter().any(|rec| {
            rec.caps().executor && !self.is_plugin_fused(&rec.id) && self.executor_provider(rec).as_deref() == Some(provider.as_str())
        })
    }

    /// Registers plugin executors and the models they own (Go: `RegisterExecutors`).
    pub fn register_executors(self: &Arc<Self>, manager: &SharedManager, registry: &ModelRegistry) {
        let snap = self.snapshot();
        let records = self.active_records_from(&snap);
        let mut registrations: Vec<ModelRegistration> = self.state.lock().model_registrations.values().cloned().collect();
        registrations.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.plugin_id.cmp(&b.plugin_id)));
        let mut selected_models: HashMap<String, Vec<RegistryModel>> = HashMap::new();
        let mut provider_models: HashMap<String, Vec<RegistryModel>> = HashMap::new();
        let mut claimed_models: HashSet<String> = HashSet::new();
        let mut claimed_providers: HashMap<String, String> = HashMap::new();
        for reg in &registrations {
            if !reg.has_executor {
                append_models_for_provider(&mut provider_models, &reg.provider, &reg.models);
            }
        }
        for rec in &records {
            if !rec.caps().executor || self.is_plugin_fused(&rec.id) {
                continue;
            }
            let Some(provider) = self.executor_provider(rec) else { continue };
            let reg = self.model_registration(&rec.id);
            if self.provider_has_native_executor(manager, &provider) {
                append_models_for_provider(&mut provider_models, &provider, &reg.models);
                continue;
            }
            if reg.models.is_empty() {
                continue;
            }
            if let Some(owner) = claimed_providers.get(&provider)
                && !owner.is_empty()
                && owner != &rec.id
            {
                continue;
            }
            for model in &reg.models {
                let model_id = model.id.trim().to_string();
                if model_id.is_empty() || claimed_models.contains(&model_id) {
                    continue;
                }
                if self.model_has_native_executor(manager, registry, &model_id) {
                    continue;
                }
                claimed_models.insert(model_id);
                claimed_providers.insert(provider.clone(), rec.id.clone());
                selected_models.entry(rec.id.clone()).or_default().push(model.clone());
            }
        }

        let mut seen_providers: HashSet<String> = HashSet::new();
        let mut next_providers: HashSet<String> = HashSet::new();
        let mut next_model_clients: HashSet<String> = HashSet::new();
        let mut executor_registrations: Vec<(String, Arc<ExecutorAdapter>)> = Vec::new();
        let mut model_clients: Vec<ModelClientRegistration> = Vec::new();
        for rec in &records {
            if !rec.caps().executor || self.is_plugin_fused(&rec.id) {
                continue;
            }
            let Some(provider) = self.executor_provider(rec) else { continue };
            let reg = self.model_registration(&rec.id);
            if !reg.models.is_empty() && selected_models.get(&rec.id).is_none_or(Vec::is_empty) {
                continue;
            }
            if !seen_providers.insert(provider.clone()) {
                continue;
            }
            if self.provider_has_native_executor(manager, &provider) {
                continue;
            }
            next_providers.insert(provider.clone());
            executor_registrations.push((provider.clone(), self.new_executor_adapter(rec, &provider)));
            let selected = selected_models.get(&rec.id).cloned().unwrap_or_default();
            append_models_for_provider(&mut provider_models, &provider, &selected);
            if !selected.is_empty() {
                let client_id = format!("plugin:{}:{}:executor", rec.id, provider);
                next_model_clients.insert(client_id.clone());
                model_clients.push(ModelClientRegistration { client_id, provider: provider.clone(), models: selected });
            }
        }
        self.commit_executor_state(&snap, manager, registry, provider_models, executor_registrations, next_providers, model_clients, next_model_clients);
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_executor_state(
        &self,
        snap: &Arc<crate::host::Snapshot>,
        manager: &SharedManager,
        registry: &ModelRegistry,
        provider_models: HashMap<String, Vec<RegistryModel>>,
        registrations: Vec<(String, Arc<ExecutorAdapter>)>,
        next_providers: HashSet<String>,
        model_clients: Vec<ModelClientRegistration>,
        next_model_clients: HashSet<String>,
    ) {
        let stale_clients: Vec<String> = {
            let mut st = self.state.lock();
            if !Arc::ptr_eq(&self.snapshot(), snap) {
                return;
            }
            st.provider_models = provider_models;
            let stale_providers: Vec<String> = st.executor_providers.iter().filter(|p| !next_providers.contains(*p)).cloned().collect();
            st.executor_providers = next_providers;
            let stale_clients: Vec<String> =
                st.executor_model_client_ids.iter().filter(|c| !next_model_clients.contains(*c)).cloned().collect();
            st.executor_model_client_ids = next_model_clients;
            for (provider, adapter) in registrations {
                if provider.is_empty() {
                    continue;
                }
                st.executor_adapters.insert(provider, adapter.clone());
                manager.register_executor(adapter);
            }
            for provider in stale_providers {
                let Some(existing) = manager.executor(&provider) else { continue };
                let ptr = Arc::as_ptr(&existing) as *const ();
                if st.executor_adapters.values().any(|a| Arc::as_ptr(a) as *const () == ptr) {
                    manager.unregister_executor(&provider);
                    st.executor_adapters.remove(&provider);
                }
            }
            stale_clients
        };
        for c in &model_clients {
            registry.register_client(&c.client_id, &c.provider, &c.models);
        }
        for id in &stale_clients {
            registry.unregister_client(id);
        }
    }

    // ---- direct execution through a named plugin (model router targets) ----

    fn executor_adapter_for_plugin(self: &Arc<Self>, plugin_id: &str) -> Result<Arc<ExecutorAdapter>, ExecError> {
        let plugin_id = plugin_id.trim();
        if plugin_id.is_empty() {
            return Err(ExecError::new(0, "target executor plugin id is required"));
        }
        for rec in self.active_records() {
            if rec.id != plugin_id {
                continue;
            }
            if self.is_plugin_fused(&rec.id) {
                return Err(ExecError::new(0, format!("plugin executor {plugin_id} is unavailable")));
            }
            if !rec.caps().executor {
                return Err(ExecError::new(0, format!("plugin {plugin_id} does not declare an executor")));
            }
            let Some(provider) = self.executor_provider(&rec) else {
                return Err(ExecError::new(0, format!("plugin executor {plugin_id} has no provider identifier")));
            };
            return Ok(self.new_executor_adapter(&rec, &provider));
        }
        Err(ExecError::new(0, format!("plugin executor {plugin_id} not found")))
    }

    /// Executor input format a direct plugin route would select (Go: `PluginExecutorRequestToFormat`).
    pub fn plugin_executor_request_to_format(self: &Arc<Self>, plugin_id: &str, _req: &Request, opts: &Options) -> Option<Format> {
        let adapter = self.executor_adapter_for_plugin(plugin_id).ok()?;
        adapter.select_input_format(opts.source_format).ok()
    }

    /// Go `ExecutePluginExecutor`.
    pub async fn execute_plugin_executor(self: &Arc<Self>, ctx: &CallCtx, plugin_id: &str, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.executor_adapter_for_plugin(plugin_id)?.execute_inner(ctx, None, req, opts).await
    }

    /// Go `ExecutePluginExecutorStream`.
    pub async fn execute_plugin_executor_stream(
        self: &Arc<Self>,
        ctx: &CallCtx,
        plugin_id: &str,
        req: Request,
        opts: Options,
    ) -> Result<StreamResult, ExecError> {
        self.executor_adapter_for_plugin(plugin_id)?.execute_stream_inner(ctx, None, req, opts).await
    }

    /// Go `CountPluginExecutor`.
    pub async fn count_plugin_executor(self: &Arc<Self>, ctx: &CallCtx, plugin_id: &str, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.executor_adapter_for_plugin(plugin_id)?.count_tokens_inner(ctx, None, req, opts).await
    }

    /// Whether the named plugin could execute the routed request (Go: `executorPluginReady`).
    pub(crate) fn executor_plugin_ready(self: &Arc<Self>, plugin_id: &str, source_format: Format, stream: bool) -> bool {
        let _ = stream;
        let plugin_id = plugin_id.trim();
        if plugin_id.is_empty() {
            return false;
        }
        for rec in self.active_records() {
            if rec.id != plugin_id || self.is_plugin_fused(&rec.id) {
                continue;
            }
            if !rec.caps().executor || !rec.caps().scope_allows_static_models() {
                return false;
            }
            let Some(provider) = self.executor_provider(&rec) else { return false };
            let adapter = self.new_executor_adapter(&rec, &provider);
            return adapter.supports_formats(source_format, source_format);
        }
        false
    }

    /// Whether any active plugin translates responses (Go: `hasResponseTranslator`).
    pub(crate) fn has_response_translator(&self) -> bool {
        self.active_records().iter().any(|r| !self.is_plugin_fused(&r.id) && r.caps().response_translator)
    }
}

fn append_models_for_provider(out: &mut HashMap<String, Vec<RegistryModel>>, provider: &str, models: &[RegistryModel]) {
    let provider = provider.trim().to_lowercase();
    if provider.is_empty() || models.is_empty() {
        return;
    }
    let list = out.entry(provider).or_default();
    let mut seen: HashSet<String> = list.iter().map(|m| m.id.trim().to_string()).filter(|id| !id.is_empty()).collect();
    for m in models {
        let id = m.id.trim().to_string();
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        list.push(m.clone());
    }
}

impl ExecutorAdapter {
    fn identifier_str(&self) -> &str {
        &self.provider
    }

    fn available(&self) -> bool {
        self.host.usable(&self.record)
    }

    fn unavailable_error(&self) -> ExecError {
        ExecError::new(0, format!("plugin executor {} is unavailable", self.provider))
    }

    fn response_translation_available(&self, from: Format, to: Format) -> bool {
        if from == to {
            return true;
        }
        cpa_translator::registry::has_response_transformer(to, from) || self.host.has_response_translator()
    }

    fn select_input_format(&self, requested: Format) -> Result<Format, ExecError> {
        if self.input_formats.is_empty() {
            return Err(ExecError::new(0, format!("plugin executor {} declares no input formats", self.identifier_str())));
        }
        if self.input_formats.contains(&requested) {
            return Ok(requested);
        }
        for f in &self.input_formats {
            if cpa_translator::registry::has_request_transformer(requested, *f) {
                return Ok(*f);
            }
        }
        Err(ExecError::new(0, format!("plugin executor {} does not support input format \"{}\"", self.identifier_str(), requested)))
    }

    fn select_output_format(&self, requested: Format, input: Format) -> Result<Format, ExecError> {
        if self.output_formats.is_empty() {
            return Err(ExecError::new(0, format!("plugin executor {} declares no output formats", self.identifier_str())));
        }
        if self.output_formats.contains(&requested) {
            return Ok(requested);
        }
        if self.output_formats.contains(&input) && self.response_translation_available(input, requested) {
            return Ok(input);
        }
        for f in &self.output_formats {
            if self.response_translation_available(*f, requested) {
                return Ok(*f);
            }
        }
        Err(ExecError::new(0, format!("plugin executor {} does not support output format \"{}\"", self.identifier_str(), requested)))
    }

    /// Go `supportsExecutorFormats`.
    pub(crate) fn supports_formats(&self, source: Format, requested: Format) -> bool {
        match self.select_input_format(source) {
            Ok(input) => self.select_output_format(requested, input).is_ok(),
            Err(_) => false,
        }
    }

    fn prepare(&self, req: Request, opts: Options) -> Result<Prepared, ExecError> {
        let input_requested = opts.source_format;
        let requested = opts.response_format_or_source();
        let input = self.select_input_format(input_requested)?;
        let output = self.select_output_format(requested, input)?;
        let mut req = req;
        let mut opts = opts;
        if input_requested != input {
            let translated = cpa_translator::registry::translate_request(input_requested, input, &req.model, &req.payload, opts.stream);
            req.payload = Bytes::from(translated);
        }
        req.format = output;
        opts.source_format = input;
        opts.response_format = Some(output);
        Ok(Prepared { req, opts, requested, input, output })
    }

    /// Plugin-visible request (Go: `buildExecutorRequest`).
    fn build_request(&self, auth: Option<&Auth>, req: &Request, opts: &Options) -> ExecutorRequest {
        let mut merged: HashMap<String, serde_json::Value> = req.metadata.clone();
        for (k, v) in &opts.metadata {
            merged.insert(k.clone(), v.clone());
        }
        ExecutorRequest {
            auth_id: auth.map(|a| a.id.clone()).unwrap_or_default(),
            auth_provider: auth.map(|a| a.provider.clone()).unwrap_or_default(),
            model: req.model.clone(),
            format: req.format.as_str().to_string(),
            stream: opts.stream,
            alt: opts.alt.clone(),
            headers: headers_to_go(&opts.headers),
            query: query_to_go(&opts.query),
            original_request: opts.original_request.to_vec(),
            source_format: opts.source_format.as_str().to_string(),
            payload: req.payload.to_vec(),
            metadata: plugin_visible_metadata(&merged),
            storage_json: storage_json_from_auth(auth),
            auth_metadata: auth.map(|a| a.metadata.clone()).unwrap_or_default(),
            auth_attributes: auth.map(|a| a.attributes.clone()).unwrap_or_default(),
        }
    }

    /// Context for one plugin call: a child of `parent` that is canceled when the call's future
    /// is dropped (Go: the request context ending). The attempt and nested markers are shared
    /// with `parent`.
    fn call_ctx(&self, parent: &CallCtx, opts: &Options) -> (CallCtx, tokio_util::sync::DropGuard) {
        let mut ctx = parent.child();
        if ctx.request_id.is_empty() {
            let trace = opts.metadata.get(cpa_runtime::executor::meta::TRACE_ID).and_then(|v| v.as_str()).unwrap_or("");
            ctx.request_id = trace.to_string();
        }
        let guard = ctx.token().clone().drop_guard();
        (ctx, guard)
    }

    /// Context for calls made by the conductor, which has no request context of its own: the
    /// execution metadata is attached for nested host model executions.
    pub(crate) fn conductor_ctx(opts: &Options) -> CallCtx {
        CallCtx::background().with_ext(Arc::new(crate::ctx::RequestMeta(opts.metadata.clone())))
    }

    fn to_exec_error_ctx(ctx: &CallCtx, e: crate::client::PluginError) -> ExecError {
        let mut err = Self::to_exec_error(e);
        err.upstream_attempted = ctx.upstream_attempted();
        err
    }

    fn reporter(&self, auth: Option<&Auth>, model: &str, opts: &Options) -> Option<UsageReporter> {
        let auth = auth?;
        let parsed = parse_suffix(model);
        let model_name = if parsed.model_name.trim().is_empty() { model.to_string() } else { parsed.model_name.trim().to_string() };
        Some(UsageReporter::new(&self.provider, EXECUTOR_TYPE_NAME, &model_name, Some(auth), Some(opts)))
    }

    fn to_exec_error(e: crate::client::PluginError) -> ExecError {
        let status = u16::try_from(e.status).unwrap_or(0);
        ExecError::new(status, e.message)
    }

    // ---- response translation ----

    fn original_request(prepared: &Prepared) -> &[u8] {
        if prepared.opts.original_request.is_empty() { &prepared.req.payload } else { &prepared.opts.original_request }
    }

    fn translate_response(&self, prepared: &Prepared, payload: &[u8], stream: bool, param: &mut Param) -> Vec<u8> {
        if prepared.output == prepared.requested {
            return if prepared.requested == Format::OpenAIResponse { ensure_responses_usage_details(payload) } else { payload.to_vec() };
        }
        if stream {
            let frames = self.translate_stream_payload(prepared, payload, param);
            return frames.concat();
        }
        let out = cpa_translator::registry::translate_non_stream(
            &TranslatorCtx::default(),
            prepared.output,
            prepared.requested,
            &prepared.req.model,
            Self::original_request(prepared),
            &prepared.req.payload,
            payload,
            param,
        )
        .unwrap_or_default();
        if prepared.requested == Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out }
    }

    fn translate_stream_payload(&self, prepared: &Prepared, payload: &[u8], param: &mut Param) -> Vec<Vec<u8>> {
        if prepared.output == prepared.requested {
            let out = if prepared.requested == Format::OpenAIResponse { ensure_responses_usage_details(payload) } else { payload.to_vec() };
            return vec![out];
        }
        let mut frames = cpa_translator::registry::translate_stream(
            &TranslatorCtx::default(),
            prepared.output,
            prepared.requested,
            &prepared.req.model,
            Self::original_request(prepared),
            &prepared.req.payload,
            payload,
            param,
        );
        if stream_translation_fell_back(prepared, payload, &frames) {
            return Vec::new();
        }
        if prepared.requested == Format::OpenAIResponse {
            for f in &mut frames {
                *f = ensure_responses_usage_details(f);
            }
        }
        frames
    }

    // ---- execution ----

    pub(crate) async fn execute_inner(&self, parent: &CallCtx, auth: Option<&Auth>, req: Request, opts: Options) -> Result<Response, ExecError> {
        if !self.available() {
            return Err(self.unavailable_error());
        }
        let reporter = self.reporter(auth, &req.model, &opts);
        let result = self.execute_checked(parent, auth, req, opts, reporter.as_ref()).await;
        if let (Err(e), Some(r)) = (&result, &reporter) {
            r.publish_failure(e);
        }
        result
    }

    async fn execute_checked(&self, parent: &CallCtx, auth: Option<&Auth>, req: Request, opts: Options, reporter: Option<&UsageReporter>) -> Result<Response, ExecError> {
        let prepared = self.prepare(req, opts)?;
        if let Some(r) = reporter {
            r.set_translated_reasoning_effort(&prepared.req.payload, prepared.input.as_str());
            r.start_response_ttft();
        }
        let (ctx, _cancel) = self.call_ctx(parent, &prepared.opts);
        let plugin_req = self.build_request(auth, &prepared.req, &prepared.opts);
        let resp: ExecutorResponse = self
            .host
            .rpc_cb(&self.record, &ctx, abi::METHOD_EXECUTOR_EXECUTE, &plugin_req)
            .await
            .map_err(|e| Self::to_exec_error_ctx(&ctx, e))?;
        if let Some(r) = reporter {
            r.record_first_packet();
            r.publish(parse_plugin_executor_response_usage(prepared.output.as_str(), &resp.payload));
            r.ensure_published();
        }
        let mut param = Param::default();
        let payload = self.translate_response(&prepared, &resp.payload, false, &mut param);
        Ok(Response {
            payload: Bytes::from(payload),
            metadata: crate::convert::map_to_hash(&resp.metadata),
            headers: headers_from_go(&resp.headers),
        })
    }

    pub(crate) async fn count_tokens_inner(&self, parent: &CallCtx, auth: Option<&Auth>, req: Request, opts: Options) -> Result<Response, ExecError> {
        if !self.available() {
            return Err(self.unavailable_error());
        }
        let prepared = self.prepare(req, opts)?;
        let (ctx, _cancel) = self.call_ctx(parent, &prepared.opts);
        let plugin_req = self.build_request(auth, &prepared.req, &prepared.opts);
        let resp: ExecutorResponse = self
            .host
            .rpc_cb(&self.record, &ctx, abi::METHOD_EXECUTOR_COUNT_TOKENS, &plugin_req)
            .await
            .map_err(|e| Self::to_exec_error_ctx(&ctx, e))?;
        let mut param = Param::default();
        let payload = self.translate_response(&prepared, &resp.payload, false, &mut param);
        Ok(Response {
            payload: Bytes::from(payload),
            metadata: crate::convert::map_to_hash(&resp.metadata),
            headers: headers_from_go(&resp.headers),
        })
    }

    pub(crate) async fn execute_stream_inner(&self, parent: &CallCtx, auth: Option<&Auth>, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        if !self.available() {
            return Err(self.unavailable_error());
        }
        let reporter = self.reporter(auth, &req.model, &opts);
        let result = self.execute_stream_checked(parent, auth, req, opts, reporter.clone()).await;
        if let (Err(e), Some(r)) = (&result, &reporter) {
            r.publish_failure(e);
        }
        result
    }

    async fn execute_stream_checked(
        &self,
        parent: &CallCtx,
        auth: Option<&Auth>,
        req: Request,
        opts: Options,
        reporter: Option<UsageReporter>,
    ) -> Result<StreamResult, ExecError> {
        let prepared = self.prepare(req, opts)?;
        if let Some(r) = &reporter {
            r.set_translated_reasoning_effort(&prepared.req.payload, prepared.input.as_str());
            r.start_response_ttft();
        }
        // The stream outlives this call, so its context is detached from the caller's drop.
        let ctx = {
            let mut c = parent.detached();
            if c.request_id.is_empty() {
                c.request_id = prepared.opts.metadata.get(cpa_runtime::executor::meta::TRACE_ID).and_then(|v| v.as_str()).unwrap_or("").to_string();
            }
            c
        };
        let plugin_req = self.build_request(auth, &prepared.req, &prepared.opts);
        let (stream_id, stream_rx, cleanup_stream) = self.host.bridges.streams.open(&ctx);
        let guard = self.host.bridges.contexts.open(&ctx, &self.record.id, Some(self.record.client.instance()));
        let resp: Result<ExecutorStreamResponse, _> = self
            .host
            .rpc_cb_stream(&self.record, &ctx, abi::METHOD_EXECUTOR_EXECUTE_STREAM, &plugin_req, &stream_id, &guard)
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                cleanup_stream();
                drop(guard);
                return Err(Self::to_exec_error_ctx(&ctx, e));
            }
        };
        let headers = headers_from_go(&resp.headers);
        let (out_tx, out_rx) = mpsc::channel::<Result<Bytes, ExecError>>(1);
        let prepared = Arc::new(prepared);
        let adapter_provider = self.provider.clone();
        let this = ExecutorAdapter {
            host: self.host.clone(),
            record: self.record.clone(),
            provider: adapter_provider,
            input_formats: self.input_formats.clone(),
            output_formats: self.output_formats.clone(),
        };
        if !resp.chunks.is_empty() {
            // The plugin returned the whole stream inline: nothing more to bridge.
            cleanup_stream();
            drop(guard);
            let items: Vec<StreamItem> = resp.chunks.into_iter().map(|c| Ok(Bytes::from(c.payload))).collect();
            let (tx, rx) = mpsc::channel::<StreamItem>(items.len().max(1));
            for it in items {
                let _ = tx.try_send(it);
            }
            drop(tx);
            tokio::spawn(async move { this.pump_stream(prepared, rx, out_tx, reporter, ctx, Box::new(|| {})).await });
        } else {
            // Async streaming plugins keep emitting after returning; keep callbacks alive until
            // the stream ends.
            let cleanup: Box<dyn FnOnce() + Send> = Box::new(move || {
                cleanup_stream();
                drop(guard);
            });
            tokio::spawn(async move { this.pump_stream(prepared, stream_rx, out_tx, reporter, ctx, cleanup).await });
        }
        Ok(StreamResult::new(headers, out_rx))
    }

    /// Observes usage, translates and forwards stream items (Go: `observeAndTranslateExecutorStream`
    /// plus `translateExecutorStreamChunks` and `mapExecutorStreamChunks`).
    async fn pump_stream(
        &self,
        prepared: Arc<Prepared>,
        mut rx: mpsc::Receiver<StreamItem>,
        out: mpsc::Sender<Result<Bytes, ExecError>>,
        reporter: Option<UsageReporter>,
        ctx: CallCtx,
        cleanup: Box<dyn FnOnce() + Send>,
    ) {
        struct Observe {
            usage: StreamUsageBuffer,
            err: Option<String>,
            line_buffer: Vec<u8>,
            published: bool,
        }
        let translate = prepared.requested != prepared.output || prepared.requested == Format::OpenAIResponse;
        let mut observe = Observe { usage: StreamUsageBuffer::default(), err: None, line_buffer: Vec::new(), published: false };
        let mut param = Param::default();
        let protocol = prepared.output.as_str();

        let publish = |observe: &mut Observe, reporter: &Option<UsageReporter>, ctx_err: bool| {
            if observe.published {
                return;
            }
            observe.published = true;
            let Some(r) = reporter else { return };
            if !observe.line_buffer.is_empty() {
                observe_plugin_executor_stream_usage(protocol, &observe.line_buffer, &mut observe.usage);
                observe.line_buffer.clear();
            }
            if let Some(err) = &observe.err {
                let e = ExecError::new(0, err.clone());
                r.publish_buffer_failure(&observe.usage, &e);
                r.ensure_published();
                return;
            }
            if ctx_err {
                let e = ExecError::new(0, "context canceled");
                r.publish_buffer_failure(&observe.usage, &e);
                r.ensure_published();
                return;
            }
            if !r.publish_buffer(&observe.usage) {
                r.ensure_published();
            }
        };

        let mut canceled = false;
        'outer: loop {
            let item = tokio::select! {
                () = out.closed() => { canceled = true; break 'outer; }
                i = rx.recv() => i,
            };
            let Some(item) = item else { break };
            match item {
                Err(message) => {
                    if observe.err.is_none() {
                        observe.err = Some(message.clone());
                    }
                    publish(&mut observe, &reporter, false);
                    if out.send(Err(ExecError::new(0, message))).await.is_err() {
                        canceled = true;
                        break;
                    }
                }
                Ok(payload) => {
                    if reporter.is_some() && !payload.is_empty() {
                        if let Some(r) = &reporter {
                            observe_plugin_executor_stream_ttft(protocol, r, &payload);
                        }
                        observe.line_buffer.extend_from_slice(&payload);
                        while let Some(idx) = observe.line_buffer.iter().position(|b| *b == b'\n') {
                            let line: Vec<u8> = observe.line_buffer.drain(..=idx).collect();
                            observe_plugin_executor_stream_usage(protocol, &line, &mut observe.usage);
                        }
                        if !observe.line_buffer.is_empty() {
                            let complete = extract_stream_json_payload(&observe.line_buffer).is_some_and(|j| !j.is_empty() && cpa_json::valid(j));
                            if complete {
                                observe_plugin_executor_stream_usage(protocol, &observe.line_buffer, &mut observe.usage);
                                observe.line_buffer.clear();
                            }
                        }
                        if observe.line_buffer.len() > MAX_LINE_BUFFER_SIZE {
                            observe_plugin_executor_stream_usage(protocol, &observe.line_buffer, &mut observe.usage);
                            observe.line_buffer.clear();
                        }
                    }
                    let frames: Vec<Vec<u8>> = if translate { self.translate_stream_payload(&prepared, &payload, &mut param) } else { vec![payload.to_vec()] };
                    for f in frames {
                        if out.send(Ok(Bytes::from(f))).await.is_err() {
                            canceled = true;
                            break 'outer;
                        }
                    }
                }
            }
        }
        if !canceled && translate {
            // Stream ended: translate the synthetic [DONE] tail for chat-completions output.
            if prepared.output == Format::OpenAI {
                for f in self.translate_stream_payload(&prepared, b"data: [DONE]", &mut param) {
                    if out.send(Ok(Bytes::from(f))).await.is_err() {
                        break;
                    }
                }
            }
        }
        publish(&mut observe, &reporter, canceled || ctx.is_canceled());
        cleanup();
    }

    /// Credential refresh through the plugin's auth provider (Go: `Refresh`).
    pub(crate) async fn refresh_inner(&self, auth: &Auth) -> Result<Auth, ExecError> {
        if !self.available() {
            return Err(self.unavailable_error());
        }
        let Some(rec) = self.host.auth_provider_record(&auth.provider) else {
            return Ok(auth.clone());
        };
        let host_cfg = host_config_summary(self.host.runtime_config().as_deref());
        let ctx = CallCtx::background();
        let req = AuthRefreshRequest {
            auth_id: auth.id.clone(),
            auth_provider: auth.provider.clone(),
            storage_json: storage_json_from_auth(Some(auth)),
            metadata: auth.metadata.clone(),
            attributes: auth.attributes.clone(),
            host: host_cfg,
        };
        let resp: AuthRefreshResponse =
            self.host.rpc_cb(&rec, &ctx, abi::METHOD_AUTH_REFRESH, &req).await.map_err(Self::to_exec_error)?;
        let mut data = resp.auth;
        if data.provider.trim().is_empty() {
            data.provider = auth.provider.clone();
        }
        if data.id.trim().is_empty() {
            data.id = auth.id.clone();
        }
        if data.file_name.trim().is_empty() {
            data.file_name = auth.file_name.clone();
        }
        if data.label.trim().is_empty() {
            data.label = auth.label.clone();
        }
        if data.prefix.trim().is_empty() {
            data.prefix = auth.prefix.clone();
        }
        if data.proxy_url.trim().is_empty() {
            data.proxy_url = auth.proxy_url.clone();
        }
        if data.metadata.is_empty() {
            data.metadata = auth.metadata.clone();
        }
        if data.attributes.is_empty() {
            data.attributes = auth.attributes.clone();
        } else {
            for (k, v) in &auth.attributes {
                data.attributes.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        preserve_file_auth_priority(&mut data, auth);
        if data.storage_json.is_empty() {
            data.storage_json = storage_json_from_auth(Some(auth));
        }
        data.next_refresh_after = match resp.next_refresh_after {
            Some(t) => Some(t),
            None => auth.next_refresh_after,
        };
        let path = auth.attributes.get(cpa_auth::types::ATTRIBUTE_PATH).cloned().unwrap_or_default();
        let Some(mut next) = self.host.auth_data_to_core_auth(&data, &path, &data.file_name) else {
            return Err(ExecError::new(0, format!("plugin executor {} refresh returned invalid auth data", self.provider)));
        };
        next.created_at = auth.created_at;
        next.updated_at = auth.updated_at;
        Ok(next)
    }

    /// Executor-owned HTTP request (Go: `HttpRequest`), used by the management API call tool.
    pub async fn http_request(&self, auth: Option<&Auth>, req: ExecutorHttpRequest) -> Result<ExecutorHttpResponse, ExecError> {
        if !self.available() {
            return Err(self.unavailable_error());
        }
        let mut req = req;
        if let Some(a) = auth {
            req.auth_id = a.id.clone();
            req.auth_provider = a.provider.clone();
            req.storage_json = storage_json_from_auth(Some(a));
            req.metadata = a.metadata.clone();
            req.attributes = a.attributes.clone();
        }
        let ctx = CallCtx::background();
        let guard = ctx.token().clone().drop_guard();
        let out = self.host.rpc_cb(&self.record, &ctx, abi::METHOD_EXECUTOR_HTTP_REQUEST, &req).await.map_err(Self::to_exec_error);
        drop(guard);
        out
    }
}

/// An unchanged single frame after translation is the registry's passthrough fallback, not a
/// translated frame (Go: `executorStreamTranslationFellBack`).
fn stream_translation_fell_back(prepared: &Prepared, payload: &[u8], frames: &[Vec<u8>]) -> bool {
    if prepared.output == prepared.requested {
        return false;
    }
    if frames.len() != 1 || frames[0] != payload {
        return false;
    }
    cpa_translator::registry::has_stream_response_transformer(prepared.requested, prepared.output)
}

#[async_trait]
impl Executor for ExecutorAdapter {
    fn identifier(&self) -> &str {
        &self.provider
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let ctx = Self::conductor_ctx(&opts);
        self.execute_inner(&ctx, Some(auth), req, opts).await
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let ctx = Self::conductor_ctx(&opts);
        self.execute_stream_inner(&ctx, Some(auth), req, opts).await
    }

    fn request_to_format(&self, _req: &Request, opts: &Options) -> Option<Format> {
        self.select_input_format(opts.source_format).ok()
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        self.refresh_inner(auth).await
    }

    async fn count_tokens(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let ctx = Self::conductor_ctx(&opts);
        self.count_tokens_inner(&ctx, Some(auth), req, opts).await
    }
}

//! Translator hooks and thinking appliers backed by plugins (Go: `adapters_usage_translation.go`).

use std::sync::Arc;

use cpa_core::registry::ModelInfo;
use cpa_core::thinking::{ProviderApplier, ThinkingConfig, ThinkingError};
use cpa_pluginapi::abi;
use cpa_pluginapi::api::{PayloadResponse, RequestTransformRequest, ResponseTransformRequest, ThinkingApplyRequest};
use cpa_pluginapi::api::ThinkingConfig as PluginThinkingConfig;
use cpa_translator::registry::{Ctx, PluginHooks};
use cpa_translator::Format;

use crate::caps::Record;
use crate::convert::registry_model_to_plugin;
use crate::host::Host;

/// The plugin host as the translator registry's hooks.
pub struct TranslatorHooks(pub Arc<Host>);

impl Host {
    fn hook_records(&self, pred: impl Fn(&Record) -> bool) -> Vec<Arc<Record>> {
        self.active_records().into_iter().filter(|r| !self.is_plugin_fused(&r.id) && pred(r)).collect()
    }

    #[allow(clippy::too_many_arguments)] // mirrors Go's `transformRequest`
    fn transform_request(&self, rec: &Record, method: &str, from: Format, to: Format, model: &str, body: &[u8], stream: bool) -> Option<Vec<u8>> {
        if !self.usable(rec) {
            return None;
        }
        let req = RequestTransformRequest {
            from_format: from.as_str().to_string(),
            to_format: to.as_str().to_string(),
            model: model.to_string(),
            stream,
            body: body.to_vec(),
        };
        match self.rpc_blocking::<PayloadResponse>(rec, method, &req) {
            Ok(r) if !r.body.is_empty() => Some(r.body),
            _ => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_response(
        &self,
        rec: &Record,
        method: &str,
        from: Format,
        to: Format,
        model: &str,
        original: &[u8],
        request: &[u8],
        body: &[u8],
        stream: bool,
    ) -> Option<Vec<u8>> {
        if !self.usable(rec) {
            return None;
        }
        let req = ResponseTransformRequest {
            from_format: from.as_str().to_string(),
            to_format: to.as_str().to_string(),
            model: model.to_string(),
            stream,
            original_request: original.to_vec(),
            translated_request: request.to_vec(),
            body: body.to_vec(),
        };
        match self.rpc_blocking::<PayloadResponse>(rec, method, &req) {
            Ok(r) if !r.body.is_empty() => Some(r.body),
            _ => None,
        }
    }

    /// Re-registers the thinking appliers of the given plugin records (Go:
    /// `refreshThinkingProviders`).
    pub(crate) fn refresh_thinking_providers(self: &Arc<Self>, records: &[Arc<Record>]) {
        cpa_core::thinking::clear_plugin_providers();
        for rec in records {
            if !rec.caps().thinking_applier || self.is_plugin_fused(&rec.id) || !self.record_current(rec) {
                continue;
            }
            let provider = rec.thinking_identifier().trim().to_lowercase();
            if provider.is_empty() {
                continue;
            }
            let applier = ThinkingAdapter { host: self.clone(), record: rec.clone(), provider: provider.clone() };
            cpa_core::thinking::register_plugin_provider(&rec.id, &provider, rec.priority, Arc::new(applier));
        }
    }
}

impl PluginHooks for TranslatorHooks {
    fn normalize_request(&self, _ctx: &Ctx, from: Format, to: Format, model: &str, body: &[u8], stream: bool) -> Vec<u8> {
        let host = &self.0;
        let mut current = body.to_vec();
        for rec in host.hook_records(|r| r.caps().request_normalizer) {
            if let Some(out) = host.transform_request(&rec, abi::METHOD_REQUEST_NORMALIZE, from, to, model, &current, stream) {
                current = out;
            }
        }
        current
    }

    fn translate_request(&self, _ctx: &Ctx, from: Format, to: Format, model: &str, body: &[u8], stream: bool) -> Option<Vec<u8>> {
        let host = &self.0;
        for rec in host.hook_records(|r| r.caps().request_translator) {
            if let Some(out) = host.transform_request(&rec, abi::METHOD_REQUEST_TRANSLATE, from, to, model, body, stream) {
                return Some(out);
            }
        }
        None
    }

    fn normalize_response_before(&self, _ctx: &Ctx, from: Format, to: Format, model: &str, original: &[u8], request: &[u8], body: &[u8], stream: bool) -> Vec<u8> {
        let host = &self.0;
        let mut current = body.to_vec();
        for rec in host.hook_records(|r| r.caps().response_before_translator) {
            if let Some(out) = host.transform_response(&rec, abi::METHOD_RESPONSE_NORMALIZE_BEFORE, from, to, model, original, request, &current, stream) {
                current = out;
            }
        }
        current
    }

    fn translate_response(&self, _ctx: &Ctx, from: Format, to: Format, model: &str, original: &[u8], request: &[u8], body: &[u8], stream: bool) -> Option<Vec<u8>> {
        let host = &self.0;
        for rec in host.hook_records(|r| r.caps().response_translator) {
            if let Some(out) = host.transform_response(&rec, abi::METHOD_RESPONSE_TRANSLATE, from, to, model, original, request, body, stream) {
                return Some(out);
            }
        }
        None
    }

    fn normalize_response_after(&self, _ctx: &Ctx, from: Format, to: Format, model: &str, original: &[u8], request: &[u8], body: &[u8], stream: bool) -> Vec<u8> {
        let host = &self.0;
        let mut current = body.to_vec();
        for rec in host.hook_records(|r| r.caps().response_after_translator) {
            if let Some(out) = host.transform_response(&rec, abi::METHOD_RESPONSE_NORMALIZE_AFTER, from, to, model, original, request, &current, stream) {
                current = out;
            }
        }
        current
    }
}

/// A plugin thinking applier exposed as a provider applier (Go: `thinkingAdapter`).
pub struct ThinkingAdapter {
    host: Arc<Host>,
    record: Arc<Record>,
    provider: String,
}

impl ProviderApplier for ThinkingAdapter {
    fn apply(&self, body: &[u8], config: &ThinkingConfig, model_info: Option<&ModelInfo>) -> Result<Vec<u8>, ThinkingError> {
        if !self.host.usable(&self.record) {
            return Ok(body.to_vec());
        }
        let req = ThinkingApplyRequest {
            provider: self.provider.clone(),
            model: model_info.map(registry_model_to_plugin).unwrap_or_default(),
            config: PluginThinkingConfig { mode: config.mode.as_str().to_string(), budget: config.budget, level: config.level.clone() },
            body: body.to_vec(),
        };
        let ctx = crate::ctx::CallCtx::background();
        let resp = self.host.rpc_cb_blocking::<PayloadResponse>(&self.record, &ctx, abi::METHOD_THINKING_APPLY, &req);
        match resp {
            Ok(r) if !r.body.is_empty() => Ok(r.body),
            _ => Ok(body.to_vec()),
        }
    }
}

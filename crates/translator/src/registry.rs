//! Translator registry (Go: sdk/translator/registry.go, plugin_hooks.go).

use std::any::Any;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use cpa_core::format::Format;
use cpa_core::registry::ModelInfo;
use cpa_core::thinking;
use cpa_json::{Value, J};

/// Values Go reads from `context.Context` inside translators.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    /// Gemini `alt` query parameter (e.g. "sse"), read by the antigravity->gemini responses.
    pub alt: Option<String>,
}

/// Per-stream translator state (Go: `param *any`).
///
/// Translators lazily initialise their own state type with [`Param::state`]. A translator
/// that hits an unrecoverable tool-input error records it in `tool_input_error`; the
/// registry then suppresses passthrough fallbacks (Go: `ToolInputError()` contract).
#[derive(Default)]
pub struct Param {
    state: Option<Box<dyn Any + Send + Sync>>,
    pub tool_input_error: Option<String>,
}

impl Param {
    /// True until a translator stores state (Go: `*param == nil`).
    pub fn is_empty(&self) -> bool {
        self.state.is_none()
    }

    /// Get the state of type `T`, initialising it with `init` on first use. If a state of a
    /// different type is present it is replaced.
    pub fn state<T: Any + Send + Sync>(&mut self, init: impl FnOnce() -> T) -> &mut T {
        if !self.state.as_ref().is_some_and(|s| s.is::<T>()) {
            self.state = Some(Box::new(init()));
        }
        self.state.as_mut().and_then(|s| s.downcast_mut::<T>()).expect("state type checked above")
    }

    /// Existing state of type `T`, if any.
    pub fn get<T: Any + Send + Sync>(&mut self) -> Option<&mut T> {
        self.state.as_mut().and_then(|s| s.downcast_mut::<T>())
    }
}

/// Go: sdktranslator.RequestEnvelope.
#[derive(Debug, Clone, Default)]
pub struct RequestEnvelope {
    pub model: String,
    pub stream: bool,
    pub body: Vec<u8>,
    pub model_info: Option<ModelInfo>,
    pub configuration_updates_changed: bool,
}

pub type RequestFn = fn(model: &str, raw: &[u8], stream: bool) -> Vec<u8>;
pub type RequestEnvelopeFn = fn(ctx: &Ctx, req: RequestEnvelope) -> RequestEnvelope;
pub type StreamFn = fn(ctx: &Ctx, model: &str, original: &[u8], translated: &[u8], raw: &[u8], param: &mut Param) -> Vec<Vec<u8>>;
/// `None` mirrors a Go translator returning a nil body.
pub type NonStreamFn = fn(ctx: &Ctx, model: &str, original: &[u8], translated: &[u8], raw: &[u8], param: &mut Param) -> Option<Vec<u8>>;
pub type TokenCountFn = fn(ctx: &Ctx, count: i64) -> Vec<u8>;
/// Stream end hook: events to emit when the upstream stream ended (Go: the optional
/// `ToolInputError`/`FinalizeToolInput` contract behind `helps.FinalizeApplyPatchStream`).
pub type FinalizeFn = fn(param: &mut Param) -> Vec<Vec<u8>>;

#[derive(Clone, Copy, Default)]
pub struct ResponseFns {
    pub stream: Option<StreamFn>,
    pub non_stream: Option<NonStreamFn>,
    pub token_count: Option<TokenCountFn>,
    /// Called by executors at upstream EOF, before any synthetic success event, so a stream
    /// that stopped mid apply_patch becomes a failure. Not a response transformer: it does not
    /// count for [`Registry::has_response_transformer`].
    pub finalize: Option<FinalizeFn>,
}

#[derive(Clone, Copy)]
enum RequestTransform {
    Plain(RequestFn),
    Envelope(RequestEnvelopeFn),
}

/// Optional translator extension hooks provided by plugins (Go: `PluginHooks`). Response hooks
/// take `from` = upstream format and `to` = client format, like the registry's response API.
pub trait PluginHooks: Send + Sync {
    fn normalize_request(&self, ctx: &Ctx, from: Format, to: Format, model: &str, body: &[u8], stream: bool) -> Vec<u8>;
    fn translate_request(&self, ctx: &Ctx, from: Format, to: Format, model: &str, body: &[u8], stream: bool) -> Option<Vec<u8>>;
    #[allow(clippy::too_many_arguments)]
    fn normalize_response_before(&self, ctx: &Ctx, from: Format, to: Format, model: &str, original: &[u8], request: &[u8], body: &[u8], stream: bool) -> Vec<u8>;
    #[allow(clippy::too_many_arguments)]
    fn translate_response(&self, ctx: &Ctx, from: Format, to: Format, model: &str, original: &[u8], request: &[u8], body: &[u8], stream: bool) -> Option<Vec<u8>>;
    #[allow(clippy::too_many_arguments)]
    fn normalize_response_after(&self, ctx: &Ctx, from: Format, to: Format, model: &str, original: &[u8], request: &[u8], body: &[u8], stream: bool) -> Vec<u8>;
}

#[derive(Default)]
pub struct Registry {
    requests: HashMap<(Format, Format), RequestTransform>,
    /// Keyed (client, upstream), like Go's `responses[from][to]` at registration.
    responses: HashMap<(Format, Format), ResponseFns>,
    hooks: RwLock<Option<Arc<dyn PluginHooks>>>,
}

/// True when `body` is a well-formed JSON object whose single top-level `model` is exactly
/// `model`: the passthrough model rewrite is then a no-op and the body needs no parse and
/// re-serialization. Anything else (missing, other type, duplicates, malformed) answers false.
fn body_has_model(body: &[u8], model: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Probe<'a> {
        #[serde(default, borrow)]
        model: crate::common::fast::Field<crate::common::fast::Str<'a>>,
    }
    serde_json::from_slice::<Probe>(body).is_ok_and(|p| p.model.as_ref().is_some_and(|m| &**m == model))
}

/// Raw JSON of the Responses `configuration_update` input items (Go: `configurationUpdates`).
fn configuration_updates(body: &[u8]) -> Vec<String> {
    cpa_json::raw_children(body, "input")
        .into_iter()
        .filter(|raw| cpa_json::parse_str(raw).g("type").str() == "configuration_update")
        .map(str::to_string)
        .collect()
}

impl Registry {
    /// Go: Register(from=client, to=upstream, request, response).
    pub fn register(&mut self, client: Format, upstream: Format, request: Option<RequestFn>, response: ResponseFns) {
        if let Some(f) = request {
            self.requests.insert((client, upstream), RequestTransform::Plain(f));
        }
        self.responses.insert((client, upstream), response);
    }

    /// Go: RegisterRequestEnvelope (overrides the plain request transform).
    pub fn register_request_envelope(&mut self, client: Format, upstream: Format, f: RequestEnvelopeFn) {
        self.requests.insert((client, upstream), RequestTransform::Envelope(f));
    }

    /// Go: SetPluginHooks.
    pub fn set_plugin_hooks(&self, hooks: Option<Arc<dyn PluginHooks>>) {
        if let Ok(mut slot) = self.hooks.write() {
            *slot = hooks;
        }
    }

    fn hooks(&self) -> Option<Arc<dyn PluginHooks>> {
        self.hooks.read().ok().and_then(|h| h.clone())
    }

    /// Go: HasPluginHooks.
    pub fn has_plugin_hooks(&self) -> bool {
        self.hooks().is_some()
    }

    pub fn has_stream_response_transformer(&self, client: Format, upstream: Format) -> bool {
        self.responses.get(&(client, upstream)).is_some_and(|r| r.stream.is_some())
    }

    pub fn has_non_stream_response_transformer(&self, client: Format, upstream: Format) -> bool {
        self.responses.get(&(client, upstream)).is_some_and(|r| r.non_stream.is_some())
    }

    /// Plugin request normalizers only (Go: Registry.NormalizeRequest).
    pub fn normalize_request(&self, ctx: &Ctx, from: Format, to: Format, model: &str, body: Vec<u8>, stream: bool) -> Vec<u8> {
        match self.hooks() {
            Some(h) => h.normalize_request(ctx, from, to, model, &body, stream),
            None => body,
        }
    }

    pub fn has_request_transformer(&self, client: Format, upstream: Format) -> bool {
        self.requests.contains_key(&(client, upstream))
    }

    pub fn has_response_transformer(&self, client: Format, upstream: Format) -> bool {
        self.responses
            .get(&(client, upstream))
            .is_some_and(|r| r.stream.is_some() || r.non_stream.is_some() || r.token_count.is_some())
    }

    /// Finalizes a stream at upstream EOF (see [`ResponseFns::finalize`]). Records the failure in
    /// `param.tool_input_error` and returns the events to send; empty when the pair has no hook.
    pub fn finalize_stream(&self, upstream: Format, client: Format, param: &mut Param) -> Vec<Vec<u8>> {
        match self.responses.get(&(client, upstream)).and_then(|r| r.finalize) {
            Some(f) => f(param),
            None => vec![],
        }
    }

    pub fn translate_request_envelope(&self, ctx: &Ctx, client: Format, upstream: Format, mut req: RequestEnvelope) -> RequestEnvelope {
        let hooks = self.hooks();
        match self.requests.get(&(client, upstream)).copied() {
            Some(t) => {
                let summary = crate::common::extract_translated_summary_config(&req.body, client.as_str(), upstream.as_str());
                req = match t {
                    RequestTransform::Plain(f) => {
                        req.body = f(&req.model, &req.body, req.stream);
                        req
                    }
                    RequestTransform::Envelope(f) => f(ctx, req),
                };
                req.body = thinking::apply_summary_config_for_model(req.body, upstream.as_str(), &req.model, &summary);
                if let Some(h) = &hooks {
                    // Request normalizers run after native translation and own the final provider
                    // payload, including any summary field they remove.
                    let before = configuration_updates(&req.body);
                    req.body = h.normalize_request(ctx, client, upstream, &req.model, &req.body, req.stream);
                    req.configuration_updates_changed = req.configuration_updates_changed || before != configuration_updates(&req.body);
                }
                req
            }
            None => {
                // Fallback: pass through, normalising the model field (Go does the same).
                if !req.model.is_empty() && !body_has_model(&req.body, &req.model) {
                    let mut v = cpa_json::parse(&req.body);
                    // sjson turns an empty, null or scalar body into `{"model":...}` and refuses arrays.
                    if v.g("model").str() != req.model && !v.is_array() {
                        if !v.is_object() {
                            v = Value::Object(Default::default());
                        }
                        cpa_json::set(&mut v, "model", req.model.clone());
                        req.body = cpa_json::to_vec(&v);
                    }
                }
                let Some(h) = hooks else { return req };
                // Plugin normalizers canonicalize the source before a plugin request translator
                // handles the missing native route.
                let before = configuration_updates(&req.body);
                req.body = h.normalize_request(ctx, client, upstream, &req.model, &req.body, req.stream);
                req.configuration_updates_changed = req.configuration_updates_changed || before != configuration_updates(&req.body);
                let summary = crate::common::extract_translated_summary_config(&req.body, client.as_str(), upstream.as_str());
                if let Some(translated) = h.translate_request(ctx, client, upstream, &req.model, &req.body, req.stream) {
                    req.body = thinking::apply_summary_config_for_model(translated, upstream.as_str(), &req.model, &summary);
                }
                req
            }
        }
    }

    /// Stream response translation. `upstream` produced `raw`; output is in `client` dialect.
    #[allow(clippy::too_many_arguments)]
    pub fn translate_stream(
        &self,
        ctx: &Ctx,
        upstream: Format,
        client: Format,
        model: &str,
        original: &[u8],
        translated: &[u8],
        raw: &[u8],
        param: &mut Param,
    ) -> Vec<Vec<u8>> {
        let hooks = self.hooks();
        let stream_fn = self.responses.get(&(client, upstream)).and_then(|r| r.stream);
        // Borrowed unless a plugin normalizer rewrites the line (this runs once per upstream line).
        let body: Cow<[u8]> = match &hooks {
            Some(h) => Cow::Owned(h.normalize_response_before(ctx, upstream, client, model, original, translated, raw, true)),
            None => Cow::Borrowed(raw),
        };
        let mut outputs: Option<Vec<Vec<u8>>> = None;
        let mut used_native = false;
        if let Some(f) = stream_fn {
            used_native = true;
            outputs = Some(f(ctx, model, original, translated, &body, param));
        } else if let Some(h) = &hooks
            && let Some(t) = h.translate_response(ctx, upstream, client, model, original, translated, &body, true)
        {
            outputs = Some(vec![t]);
        }
        // Retained tool failures are never recovered by raw fallback or plugin normalization.
        if param.tool_input_error.is_some() {
            return outputs.unwrap_or_default();
        }
        let mut outputs = match outputs {
            Some(o) => o,
            None if !used_native => vec![body.into_owned()],
            None => Vec::new(),
        };
        if let Some(h) = &hooks {
            for out in &mut outputs {
                *out = h.normalize_response_after(ctx, upstream, client, model, original, translated, out, true);
            }
        }
        outputs
    }

    /// Non-stream response translation; `None` mirrors a nil Go result.
    #[allow(clippy::too_many_arguments)]
    pub fn translate_non_stream(
        &self,
        ctx: &Ctx,
        upstream: Format,
        client: Format,
        model: &str,
        original: &[u8],
        translated: &[u8],
        raw: &[u8],
        param: &mut Param,
    ) -> Option<Vec<u8>> {
        let hooks = self.hooks();
        let non_stream = self.responses.get(&(client, upstream)).and_then(|r| r.non_stream);
        let mut body: Vec<u8> = match &hooks {
            Some(h) => h.normalize_response_before(ctx, upstream, client, model, original, translated, raw, false),
            None => raw.to_vec(),
        };
        let mut nil_result = false;
        if let Some(f) = non_stream {
            match f(ctx, model, original, translated, &body, param) {
                Some(out) => body = out,
                None => nil_result = true,
            }
        } else if let Some(h) = &hooks
            && let Some(t) = h.translate_response(ctx, upstream, client, model, original, translated, &body, false)
        {
            body = t;
        }
        if param.tool_input_error.is_some() || nil_result {
            return None;
        }
        if let Some(h) = &hooks {
            body = h.normalize_response_after(ctx, upstream, client, model, original, translated, &body, false);
        }
        Some(body)
    }

    pub fn translate_token_count(&self, ctx: &Ctx, upstream: Format, client: Format, count: i64, raw: &[u8]) -> Vec<u8> {
        match self.responses.get(&(client, upstream)).and_then(|r| r.token_count) {
            Some(f) => f(ctx, count),
            None => raw.to_vec(),
        }
    }
}

static GLOBAL: LazyLock<Registry> = LazyLock::new(|| {
    let mut r = Registry::default();
    crate::antigravity::register(&mut r);
    crate::claude::register(&mut r);
    crate::codex::register(&mut r);
    crate::gemini::register(&mut r);
    crate::interactions::register(&mut r);
    crate::openai::register(&mut r);
    r
});

/// The process-wide registry with every built-in translator.
pub fn global() -> &'static Registry {
    &GLOBAL
}

/// Installs (or clears) the plugin hooks of the global registry (Go: `SetPluginHooks`).
pub fn set_plugin_hooks(hooks: Option<Arc<dyn PluginHooks>>) {
    global().set_plugin_hooks(hooks);
}

pub fn has_plugin_hooks() -> bool {
    global().has_plugin_hooks()
}

pub fn has_request_transformer(client: Format, upstream: Format) -> bool {
    global().has_request_transformer(client, upstream)
}

pub fn has_response_transformer(client: Format, upstream: Format) -> bool {
    global().has_response_transformer(client, upstream)
}

pub fn has_stream_response_transformer(client: Format, upstream: Format) -> bool {
    global().has_stream_response_transformer(client, upstream)
}

pub fn has_non_stream_response_transformer(client: Format, upstream: Format) -> bool {
    global().has_non_stream_response_transformer(client, upstream)
}

/// Plugin request normalizers only (Go: package-level `NormalizeRequest`).
pub fn normalize_request(ctx: &Ctx, from: Format, to: Format, model: &str, body: Vec<u8>, stream: bool) -> Vec<u8> {
    global().normalize_request(ctx, from, to, model, body, stream)
}

pub fn translate_request(client: Format, upstream: Format, model: &str, body: &[u8], stream: bool) -> Vec<u8> {
    let req = RequestEnvelope { model: model.to_string(), stream, body: body.to_vec(), ..Default::default() };
    global().translate_request_envelope(&Ctx::default(), client, upstream, req).body
}

pub fn translate_request_envelope(ctx: &Ctx, client: Format, upstream: Format, req: RequestEnvelope) -> RequestEnvelope {
    global().translate_request_envelope(ctx, client, upstream, req)
}

#[allow(clippy::too_many_arguments)]
pub fn translate_stream(ctx: &Ctx, upstream: Format, client: Format, model: &str, original: &[u8], translated: &[u8], raw: &[u8], param: &mut Param) -> Vec<Vec<u8>> {
    global().translate_stream(ctx, upstream, client, model, original, translated, raw, param)
}

#[allow(clippy::too_many_arguments)]
pub fn translate_non_stream(ctx: &Ctx, upstream: Format, client: Format, model: &str, original: &[u8], translated: &[u8], raw: &[u8], param: &mut Param) -> Option<Vec<u8>> {
    global().translate_non_stream(ctx, upstream, client, model, original, translated, raw, param)
}

/// Stream end hook of the global registry; executors call it at upstream EOF before emitting any
/// synthetic success (Go: `helps.FinalizeApplyPatchStream`).
pub fn translate_finalize(upstream: Format, client: Format, param: &mut Param) -> Vec<Vec<u8>> {
    global().finalize_stream(upstream, client, param)
}

pub fn translate_token_count(ctx: &Ctx, upstream: Format, client: Format, count: i64, raw: &[u8]) -> Vec<u8> {
    global().translate_token_count(ctx, upstream, client, count, raw)
}

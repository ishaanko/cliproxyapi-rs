//! Per-attempt usage reporter (Go: helps `UsageReporter`).
//!
//! An executor creates one reporter per upstream attempt, feeds it TTFT marks, the served model
//! and token usage, and finally calls [`UsageReporter::publish`], [`publish_failure`] or
//! [`ensure_published`]; exactly one record is produced per reporter. Where the record goes is
//! decided by an optional [`UsageSink`], by default the [`UsageCollector`](cpa_runtime::usage_report::UsageCollector) the conductor attached
//! to the attempt's [`Options`] (so the conductor builds the usage event from this record); the
//! finished record is always retrievable with
//! [`UsageReporter::record`], and [`UsageReporter::usage_metadata`] renders the compact
//! `Response.metadata["usage"]` object the conductor reads for non-stream responses.
//!
//! Go reads request facts from `context.Context` (client API key, requested alias, session ids,
//! reasoning effort, service tier, stream flag); here they come from the execution
//! [`Options`] metadata.
//!
//! [`publish_failure`]: UsageReporter::publish_failure
//! [`ensure_published`]: UsageReporter::ensure_published

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use cpa_auth::Auth;
use cpa_core::thinking::extract_translated_reasoning_effort;
use cpa_json::lazy::Doc;
use cpa_runtime::executor::{ExecError, Options, meta};
use futures_util::{Stream, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::accounting::{Detail, ensure_token_breakdown_for_provider};
use super::parse::StreamUsageBuffer;
use crate::helps::response_model::{
    MAX_RESPONSE_MODEL_LENGTH, MODEL_SUBSTITUTION_WARNS, ModelSubstitutionKey, extract_response_model_event, extract_response_model_event_doc,
    is_model_substituted, normalize_model_name,
};

/// Service tier recorded when the request set none (Go: `usage.DefaultServiceTier`).
pub const DEFAULT_SERVICE_TIER: &str = "default";
/// Metadata key the client-facing layer may set with the downstream API key.
pub const META_CLIENT_API_KEY: &str = "client_api_key";
/// Metadata key carrying the inbound request id.
pub const META_TRACE_ID: &str = "trace_id";

pub use cpa_runtime::usage_report::{Failure, Record, UsageSink};

#[derive(Default)]
struct State {
    stream: bool,
    session_id: String,
    parent_session_id: String,
    access_token_hash: String,
    reasoning: String,
    response_model: String,
    upstream_model: String,
    ttft: Duration,
    first_packet: Duration,
    first_packet_set: bool,
    ttft_start: Option<Instant>,
    ttft_set: bool,
    /// The published record, kept only for reporters without a sink (the sink owns it otherwise).
    record: Option<Record>,
    /// Usage detail of the published record (what `published_detail` returns).
    published_detail: Option<Detail>,
}

struct Inner {
    request_id: String,
    trace_id: String,
    provider: String,
    base_url: String,
    executor_type: String,
    model: String,
    alias: String,
    auth_id: String,
    auth_index: String,
    auth_type: String,
    api_key: String,
    source: String,
    service_tier: String,
    generate: bool,
    requested_at: Instant,
    requested_at_utc: DateTime<Utc>,
    sink: Option<Arc<dyn UsageSink>>,
    /// A terminal event already reported the served model; later frames skip parsing.
    response_model_final: AtomicBool,
    /// The provider takes the generic response-model extraction (see `generic_model_fast`).
    generic_model: bool,
    published: AtomicBool,
    state: Mutex<State>,
}

/// Cheap-to-clone handle to one attempt's usage state.
#[derive(Clone)]
pub struct UsageReporter {
    inner: Arc<Inner>,
}

/// Parent session ids only count for agent sessions or ids sharing the primary's prefix.
fn is_hierarchy_parent(primary: &str, parent: &str) -> bool {
    if parent.is_empty() || primary.is_empty() || primary == parent {
        return false;
    }
    if primary.contains(":agent:") {
        return true;
    }
    let (idx1, idx2) = (primary.find(':'), parent.find(':'));
    match (idx1, idx2) {
        (Some(i1), Some(i2)) if i1 > 0 && i2 > 0 && primary[..i1] == parent[..i2] => true,
        (None, None) => true,
        _ => false,
    }
}

fn meta_string(opts: Option<&Options>, key: &str) -> String {
    opts.and_then(|o| o.metadata.get(key))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// SHA-256 hex of the auth's access token (`access_token`, `accessToken`, or the same inside a
/// `token` / `Token` object); "" when there is none.
pub fn access_token_sha256(auth: &Auth) -> String {
    let nonblank = |v: Option<&Value>| {
        v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
    };
    let token = ["access_token", "accessToken"]
        .iter()
        .find_map(|k| nonblank(auth.metadata.get(*k)))
        .or_else(|| {
            ["token", "Token"].iter().find_map(|k| match auth.metadata.get(*k) {
                Some(Value::Object(m)) => ["access_token", "accessToken"].iter().find_map(|tk| nonblank(m.get(*tk))),
                _ => None,
            })
        });
    match token {
        Some(t) => hex::encode(Sha256::digest(t.as_bytes())),
        None => String::new(),
    }
}

/// The account shown as the usage "source": Vertex project, else the OAuth email / API key,
/// else the client API key.
fn resolve_usage_source(auth: Option<&Auth>, ctx_api_key: &str) -> String {
    if let Some(auth) = auth {
        if auth.provider.trim().eq_ignore_ascii_case("vertex") {
            for key in ["project_id", "project"] {
                let v = auth.meta_str(key);
                if !v.is_empty() {
                    return v;
                }
            }
        }
        let (_, value) = auth.account_info();
        if !value.is_empty() {
            return value.trim().to_string();
        }
        let email = auth.meta_str("email");
        if !email.is_empty() {
            return email;
        }
        let api_key = auth.attr("api_key");
        if !api_key.is_empty() {
            return api_key;
        }
    }
    ctx_api_key.trim().to_string()
}

impl UsageReporter {
    /// Creates a reporter for `model` on `provider`. `auth` and `opts` supply credential and
    /// request facts; both may be absent.
    pub fn new(provider: &str, executor_type: &str, model: &str, auth: Option<&Auth>, opts: Option<&Options>) -> Self {
        Self::with_sink(provider, executor_type, model, auth, opts, None)
    }

    /// [`new`](Self::new) with a sink that receives the finished record. Without one, the
    /// options' collector (when the conductor attached it) is the sink.
    pub fn with_sink(
        provider: &str,
        executor_type: &str,
        model: &str,
        auth: Option<&Auth>,
        opts: Option<&Options>,
        sink: Option<Arc<dyn UsageSink>>,
    ) -> Self {
        let api_key = meta_string(opts, META_CLIENT_API_KEY);
        let mut alias = meta_string(opts, meta::REQUESTED_MODEL);
        if alias.is_empty() {
            alias = model.trim().to_string();
        }
        let session_id = meta_string(opts, meta::CANONICAL_SESSION_ID);
        let mut parent_session_id = meta_string(opts, meta::PARENT_SESSION_ID);
        if session_id.is_empty() || session_id == parent_session_id || !is_hierarchy_parent(&session_id, &parent_session_id) {
            parent_session_id.clear();
        }
        let mut base_url = String::new();
        let (mut auth_id, mut auth_index, mut token_hash) = (String::new(), String::new(), String::new());
        if let Some(auth) = auth {
            base_url = auth.attr("base_url");
            if base_url.is_empty() {
                base_url = auth.meta_str("base_url");
            }
            auth_id = auth.id.clone();
            auth_index = if auth.index.trim().is_empty() { auth.clone().ensure_index() } else { auth.index.trim().to_string() };
            token_hash = access_token_sha256(auth);
        }
        let sink = sink.or_else(|| {
            opts.and_then(|o| o.usage_collector.clone()).map(|c| {
                c.mark_reporter_attached();
                Arc::new(c) as Arc<dyn UsageSink>
            })
        });
        let generate = !matches!(opts.and_then(|o| o.metadata.get(meta::GENERATE)), Some(Value::Bool(false)));
        let inner = Inner {
            request_id: uuid::Uuid::new_v4().to_string(),
            trace_id: meta_string(opts, META_TRACE_ID),
            provider: provider.to_string(),
            base_url,
            executor_type: executor_type.trim().to_string(),
            model: model.to_string(),
            alias,
            auth_id,
            auth_index,
            auth_type: auth.map(|a| a.auth_kind().to_string()).unwrap_or_default(),
            source: resolve_usage_source(auth, &api_key),
            api_key,
            service_tier: Some(meta_string(opts, meta::SERVICE_TIER)).filter(|t| !t.is_empty()).unwrap_or_else(|| DEFAULT_SERVICE_TIER.to_string()),
            generate,
            requested_at: Instant::now(),
            requested_at_utc: Utc::now(),
            sink,
            response_model_final: AtomicBool::new(false),
            generic_model: crate::helps::response_model::is_generic_provider(provider),
            published: AtomicBool::new(false),
            state: Mutex::new(State {
                stream: opts.is_some_and(|o| o.stream),
                session_id,
                parent_session_id,
                access_token_hash: token_hash,
                reasoning: meta_string(opts, meta::REASONING_EFFORT),
                ..Default::default()
            }),
        };
        Self { inner: Arc::new(inner) }
    }

    // ---- request facts

    pub fn set_stream(&self, stream: bool) {
        self.inner.state.lock().stream = stream;
    }

    /// Sets explicit session and parent session ids (call before publishing).
    pub fn set_session_hierarchy(&self, session_id: &str, parent_session_id: &str) {
        let (session, parent) = (session_id.trim(), parent_session_id.trim());
        let mut s = self.inner.state.lock();
        s.session_id = session.to_string();
        s.parent_session_id = if session.is_empty() || session == parent || !is_hierarchy_parent(session, parent) {
            String::new()
        } else {
            parent.to_string()
        };
    }

    /// Records the token version actually used upstream.
    pub fn update_access_token_fingerprint(&self, auth: &Auth) {
        self.inner.state.lock().access_token_hash = access_token_sha256(auth);
    }

    /// Records the translated upstream reasoning effort from the final payload.
    pub fn set_translated_reasoning_effort(&self, payload: &[u8], format: &str) {
        // Every path `cpa_core::thinking` reads starts at one of these top-level keys; a
        // well-formed object without them has no effort, so skip its full parse.
        const EFFORT_ROOTS: [&str; 8] =
            ["thinking", "output_config", "reasoning_effort", "reasoning", "generationConfig", "generation_config", "request", "input"];
        let effort = match Doc::lazy(payload) {
            Some(doc) if !EFFORT_ROOTS.iter().any(|k| doc.has(k)) => String::new(),
            _ => extract_translated_reasoning_effort(payload, format),
        };
        self.inner.state.lock().reasoning = effort;
    }

    pub fn request_id(&self) -> &str {
        &self.inner.request_id
    }

    pub fn trace_id(&self) -> &str {
        &self.inner.trace_id
    }

    // ---- response model

    /// Stores the model reported by an upstream frame; the substitution warning is emitted when
    /// the record is published.
    pub fn observe_response_model(&self, payload: &[u8]) {
        if self.is_response_model_final() {
            return;
        }
        // Common chat chunk: decided without a parse or an allocation.
        if self.inner.generic_model
            && let Some(data) = crate::helps::text::json_payload(payload)
            && let Some((served, terminal)) = crate::helps::response_model::generic_model_fast(data)
        {
            self.apply_response_model_ref(served, terminal);
            return;
        }
        let (served, terminal) = extract_response_model_event(payload, &self.inner.provider);
        self.apply_response_model(served, terminal);
    }

    fn apply_response_model_ref(&self, served: &str, terminal: bool) {
        if !served.is_empty() {
            let mut state = self.inner.state.lock();
            if state.response_model != served {
                state.response_model = served.to_string();
            }
        }
        if terminal {
            self.inner.response_model_final.store(true, Ordering::Release);
        }
    }

    /// [`Self::observe_response_model`] for a frame indexed by the caller (`payload` is the raw
    /// frame, `doc` its JSON object), so other observers can share the scan.
    pub fn observe_response_model_doc(&self, payload: &[u8], doc: &Doc<'_>) {
        if self.is_response_model_final() || crate::helps::text::json_payload(payload).is_none() {
            return;
        }
        let (served, terminal) = extract_response_model_event_doc(doc, &self.inner.provider);
        self.apply_response_model(served, terminal);
    }

    fn apply_response_model(&self, served: String, terminal: bool) {
        if !served.is_empty() {
            self.inner.state.lock().response_model = served;
        }
        if terminal {
            self.inner.response_model_final.store(true, Ordering::Release);
        }
    }

    /// Sets the served model directly when valid and not already final.
    pub fn set_response_model(&self, model: &str) {
        if self.is_response_model_final() {
            return;
        }
        let model = model.trim();
        if model.is_empty() || model.len() > MAX_RESPONSE_MODEL_LENGTH {
            return;
        }
        self.inner.state.lock().response_model = model.to_string();
    }

    pub fn response_model(&self) -> String {
        self.inner.state.lock().response_model.clone()
    }

    /// Whether a terminal event already finalized the response model.
    pub fn is_response_model_final(&self) -> bool {
        self.inner.response_model_final.load(Ordering::Acquire)
    }

    /// Records the upstream model expected to be served when it differs from the requested one
    /// (substitution detection compares against it; the record keeps the requested model).
    pub fn set_upstream_model(&self, model: &str) {
        self.inner.state.lock().upstream_model = model.trim().to_string();
    }

    pub fn upstream_model(&self) -> String {
        self.inner.state.lock().upstream_model.clone()
    }

    /// Warns about a silent upstream model swap, throttled per credential and model pair; the
    /// credential is labelled by index only, never by account.
    fn warn_model_substitution(&self) {
        let i = &self.inner;
        let served = self.response_model();
        let mut expected = self.upstream_model();
        if expected.is_empty() {
            expected = i.model.clone();
        }
        if served.is_empty() || !is_model_substituted(&expected, &served) {
            return;
        }
        if !i.model.is_empty() && !is_model_substituted(&i.model, &served) {
            return;
        }
        let provider = if i.provider.is_empty() { "codex" } else { i.provider.as_str() };
        let key = ModelSubstitutionKey {
            provider: provider.to_string(),
            auth_id: i.auth_id.clone(),
            requested: normalize_model_name(&expected),
            served: normalize_model_name(&served),
        };
        if !MODEL_SUBSTITUTION_WARNS.allow(key) {
            return;
        }
        let index = if i.auth_index.trim().is_empty() { "nil" } else { i.auth_index.trim() };
        tracing::warn!(
            request_id = %i.request_id,
            "{provider} executor: upstream served model {served:?} for requested model {:?} (auth_index={index})",
            i.model
        );
    }

    // ---- TTFT

    /// Marks the moment the request is sent (first call wins).
    pub fn start_response_ttft(&self) {
        let mut s = self.inner.state.lock();
        if !s.ttft_set && s.ttft_start.is_none() {
            s.ttft_start = Some(Instant::now());
        }
    }

    /// Whether effective TTFT (first token) is already recorded.
    pub fn is_ttft_set(&self) -> bool {
        self.inner.state.lock().ttft_set
    }

    pub fn is_first_packet_set(&self) -> bool {
        self.inner.state.lock().first_packet_set
    }

    /// Records the arrival of the first upstream packet as a TTFT fallback.
    pub fn record_first_packet(&self) {
        self.observe_token_event(false);
    }

    /// Records the first-packet fallback on the first frame and, when `is_token`, the effective
    /// TTFT; a no-op once effective TTFT is set.
    pub fn observe_token_event(&self, is_token: bool) {
        let mut s = self.inner.state.lock();
        if s.ttft_set {
            return;
        }
        let Some(start) = s.ttft_start else {
            return;
        };
        if !is_token && s.first_packet_set {
            return;
        }
        if !s.first_packet_set {
            s.first_packet = start.elapsed();
            s.first_packet_set = true;
        }
        if is_token {
            s.ttft = start.elapsed();
            s.ttft_set = true;
            s.ttft_start = None;
        }
    }

    /// Records TTFT as the arrival of the first response byte (non-stream and generic paths).
    pub fn mark_first_response_byte(&self) {
        let mut s = self.inner.state.lock();
        if s.ttft_set {
            return;
        }
        if let Some(start) = s.ttft_start.take() {
            s.ttft = start.elapsed();
            s.ttft_set = true;
        }
    }

    /// Wraps an upstream body stream so the first non-empty chunk marks TTFT: the first response
    /// byte, or with `packet_only` only the first-packet fallback (protocol-aware streaming
    /// executors mark effective TTFT on substantive token events instead).
    /// The result owns a clone of the reporter (`use<S, E>`), so it can move into spawned tasks.
    pub fn observe_body_stream<S, E>(&self, stream: S, packet_only: bool) -> impl Stream<Item = Result<Bytes, E>> + use<S, E>
    where
        S: Stream<Item = Result<Bytes, E>>,
    {
        self.start_response_ttft();
        let reporter = self.clone();
        let mut marked = false;
        stream.inspect(move |item| {
            if !marked && item.as_ref().is_ok_and(|b| !b.is_empty()) {
                marked = true;
                if packet_only {
                    reporter.record_first_packet();
                } else {
                    reporter.mark_first_response_byte();
                }
            }
        })
    }

    fn ttft_duration(s: &State) -> Duration {
        if s.ttft_set {
            s.ttft
        } else if s.first_packet_set {
            s.first_packet
        } else {
            Duration::ZERO
        }
    }

    // ---- publishing

    /// Publishes the attempt's usage (once).
    pub fn publish(&self, detail: Detail) {
        self.publish_with_outcome(detail, false, Failure::default());
    }

    /// Publishes the latest usage observed in a stream (and its served model when none was
    /// reported yet). Returns false when the buffer observed no usage.
    pub fn publish_buffer(&self, buffer: &StreamUsageBuffer) -> bool {
        let Some(detail) = buffer.detail() else {
            return false;
        };
        self.adopt_buffer_model(buffer);
        self.publish(detail);
        true
    }

    /// Publishes a stream failure with whatever usage the buffer observed before it.
    pub fn publish_buffer_failure(&self, buffer: &StreamUsageBuffer, err: &ExecError) {
        self.adopt_buffer_model(buffer);
        self.publish_failure_with_detail(buffer.detail_ref().cloned().unwrap_or_default(), err);
    }

    fn adopt_buffer_model(&self, buffer: &StreamUsageBuffer) {
        if !buffer.response_model().is_empty() && self.response_model().is_empty() {
            self.set_response_model(buffer.response_model());
        }
    }

    /// Publishes a failed attempt without token usage.
    pub fn publish_failure(&self, err: &ExecError) {
        self.publish_with_outcome(Detail::default(), true, fail_from_error(err));
    }

    /// Publishes a failed attempt with the usage observed before it failed.
    pub fn publish_failure_with_detail(&self, detail: Detail, err: &ExecError) {
        self.publish_with_outcome(detail, true, fail_from_error(err));
    }

    /// Publishes a failure when `result` is an error (Go: `defer reporter.TrackFailure(&err)`).
    pub fn track_failure<T>(&self, result: &Result<T, ExecError>) {
        if let Err(err) = result {
            self.publish_failure(err);
        }
    }

    /// Guarantees a record is emitted even when the upstream response had no usage fields.
    pub fn ensure_published(&self) {
        self.publish_once(|| self.build_record(Detail::default(), false, Failure::default()));
    }

    /// Publishes a separate record for a side model (for example image-tool usage); skipped
    /// when the model is blank or the detail has no tokens.
    pub fn publish_additional_model(&self, model: &str, detail: Detail) {
        let model = model.trim();
        if model.is_empty() {
            return;
        }
        let detail = ensure_token_breakdown_for_provider(detail, &self.inner.provider, &self.inner.executor_type);
        if !detail.has_token_usage() {
            return;
        }
        let mut record = self.build_record_for_model(model, detail, false, Failure::default());
        record.request_id = uuid::Uuid::new_v4().to_string();
        if let Some(sink) = &self.inner.sink {
            sink.publish(record);
        }
    }

    fn publish_with_outcome(&self, detail: Detail, failed: bool, fail: Failure) {
        let detail = ensure_token_breakdown_for_provider(detail, &self.inner.provider, &self.inner.executor_type);
        self.publish_once(|| self.build_record(detail, failed, fail));
    }

    fn publish_once(&self, build: impl FnOnce() -> Record) {
        if self.inner.published.swap(true, Ordering::AcqRel) {
            return;
        }
        let record = build();
        match &self.inner.sink {
            Some(sink) => {
                self.inner.state.lock().published_detail = Some(record.detail.clone());
                sink.publish(record);
            }
            None => {
                let mut s = self.inner.state.lock();
                s.published_detail = Some(record.detail.clone());
                s.record = Some(record);
            }
        }
        self.warn_model_substitution();
    }

    /// The published record, once published. Only reporters without a sink keep it (with a sink
    /// the record moves to the sink); use [`published_detail`](Self::published_detail) then.
    pub fn record(&self) -> Option<Record> {
        self.inner.state.lock().record.clone()
    }

    /// The usage detail of the published record, once published (any reporter).
    pub fn published_detail(&self) -> Option<Detail> {
        self.inner.state.lock().published_detail.clone()
    }

    fn build_record(&self, detail: Detail, failed: bool, fail: Failure) -> Record {
        self.build_record_for_model(&self.inner.model, detail, failed, fail)
    }

    fn build_record_for_model(&self, model: &str, detail: Detail, failed: bool, fail: Failure) -> Record {
        let i = &self.inner;
        let s = i.state.lock();
        // Additional-model records describe a side model the response model never refers to.
        let response_model = if model == i.model { s.response_model.clone() } else { String::new() };
        Record {
            request_id: i.request_id.clone(),
            trace_id: i.trace_id.clone(),
            provider: i.provider.clone(),
            base_url: i.base_url.clone(),
            executor_type: i.executor_type.clone(),
            model: model.to_string(),
            alias: i.alias.trim().to_string(),
            api_key: i.api_key.clone(),
            session_id: s.session_id.clone(),
            parent_session_id: s.parent_session_id.clone(),
            auth_id: i.auth_id.clone(),
            auth_index: i.auth_index.clone(),
            access_token_sha256: s.access_token_hash.clone(),
            auth_type: i.auth_type.clone(),
            source: i.source.clone(),
            reasoning_effort: s.reasoning.clone(),
            service_tier: i.service_tier.clone(),
            response_service_tier: detail.response_service_tier.trim().to_string(),
            response_model,
            generate: i.generate,
            stream: s.stream,
            requested_at: i.requested_at_utc,
            latency: i.requested_at.elapsed(),
            ttft: Self::ttft_duration(&s),
            failed,
            fail,
            detail,
        }
    }

    /// `{"input_tokens", "output_tokens", "reasoning_tokens", "cached_tokens", "total_tokens"}`
    /// for `Response.metadata["usage"]`, which the conductor turns into the usage record.
    pub fn usage_metadata(detail: &Detail) -> Value {
        json!({
            "input_tokens": detail.input_tokens,
            "output_tokens": detail.output_tokens,
            "reasoning_tokens": detail.reasoning_tokens,
            "cached_tokens": detail.cached_tokens,
            "total_tokens": detail.total_tokens,
        })
    }
}

fn fail_from_error(err: &ExecError) -> Failure {
    let body = match err.body.as_deref() {
        Some(b) if !b.is_empty() => String::from_utf8_lossy(b).into_owned(),
        _ => err.message.trim().to_string(),
    };
    Failure { status_code: err.status, body }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_translator::Format;

    #[derive(Default)]
    struct Collect(Mutex<Vec<Record>>);
    impl UsageSink for Collect {
        fn publish(&self, record: Record) {
            self.0.lock().push(record);
        }
    }

    #[test]
    fn publishes_exactly_once_with_request_facts() {
        let sink = Arc::new(Collect::default());
        let mut opts = Options::new(Format::OpenAI);
        opts.stream = true;
        opts.metadata.insert(meta::REQUESTED_MODEL.into(), json!("my-alias"));
        opts.metadata.insert(meta::SERVICE_TIER.into(), json!("flex"));
        let mut auth = Auth::new("a1", "claude");
        auth.metadata.insert("email".into(), json!("me@example.com"));
        auth.metadata.insert("access_token".into(), json!("tok"));
        let reporter = UsageReporter::with_sink("claude", "ClaudeExecutor", "claude-x", Some(&auth), Some(&opts), Some(sink.clone()));
        reporter.start_response_ttft();
        reporter.observe_response_model(br#"data: {"type":"message_start","message":{"model":"claude-y"}}"#);
        reporter.observe_token_event(false);
        reporter.observe_token_event(true);
        reporter.publish(Detail { input_tokens: 3, output_tokens: 2, ..Default::default() });
        reporter.publish(Detail { input_tokens: 99, ..Default::default() });
        reporter.ensure_published();
        let records = sink.0.lock();
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!((r.model.as_str(), r.alias.as_str(), r.source.as_str()), ("claude-x", "my-alias", "me@example.com"));
        assert_eq!((r.detail.total_tokens, r.response_model.as_str(), r.stream, r.service_tier.as_str()), (5, "claude-y", true, "flex"));
        assert_eq!(r.access_token_sha256.len(), 64);
        assert!(r.detail.token_breakdown.valid());
    }

    #[test]
    fn failure_records_status_and_body() {
        let reporter = UsageReporter::new("openai-compatibility", "OpenAICompatExecutor", "m", None, None);
        let err = ExecError::new(429, "slow down").with_body(&b"{\"error\":\"x\"}"[..]);
        reporter.track_failure::<()>(&Err(err));
        let r = reporter.record().unwrap();
        assert!(r.failed);
        assert_eq!((r.fail.status_code, r.fail.body.as_str()), (429, "{\"error\":\"x\"}"));
    }

    #[test]
    fn ttft_falls_back_to_first_packet() {
        let reporter = UsageReporter::new("codex", "CodexExecutor", "m", None, None);
        reporter.observe_token_event(false); // no start yet: ignored
        assert!(!reporter.is_first_packet_set());
        reporter.start_response_ttft();
        reporter.record_first_packet();
        assert!(reporter.is_first_packet_set() && !reporter.is_ttft_set());
        reporter.mark_first_response_byte();
        assert!(reporter.is_ttft_set());
    }

    #[test]
    fn hierarchy_rules() {
        assert!(is_hierarchy_parent("a:agent:1", "a"));
        assert!(is_hierarchy_parent("x:1", "x:2"));
        assert!(is_hierarchy_parent("child", "parent"));
        assert!(!is_hierarchy_parent("x:1", "y:2"));
        assert!(!is_hierarchy_parent("same", "same"));
    }
}

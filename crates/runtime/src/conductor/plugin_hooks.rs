//! Plugin hook points of the conductor (Go: `PluginScheduler` in conductor.go /
//! conductor_selection.go and `applyRequestAfterAuthInterceptor` in conductor_execution.go).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_core::format::Format;
use cpa_pluginapi::api::{SchedulerAuthCandidate, SchedulerOptions, SchedulerPickRequest, SchedulerPickResponse};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use super::pick::auth_priority;
use super::session::info::{bound_session_identity, extract_session_info};
use crate::executor::{
    DynExecutor, ExecError, Options, Request, RequestAfterAuthInterceptRequest, RequestTerminated, meta,
};

/// A scheduler supplied by a plugin (Go: `PluginScheduler`): consulted before the built-in
/// selector.
#[async_trait]
pub trait PluginScheduler: Send + Sync {
    /// `Ok(None)` when the plugin did not make a decision; an error is returned to the caller
    /// as the selection failure.
    async fn pick_auth(&self, req: SchedulerPickRequest) -> Result<Option<SchedulerPickResponse>, ExecError>;

    /// False when no active plugin currently provides a scheduler (Go: `HasScheduler`).
    fn has_scheduler(&self) -> bool {
        true
    }

    /// Whether candidates across all priority tiers should be offered (Go:
    /// `SchedulerWantsAcrossPriorities`).
    fn wants_across_priorities(&self) -> bool {
        false
    }
}

impl super::Manager {
    pub fn set_plugin_scheduler(&self, scheduler: Option<Arc<dyn PluginScheduler>>) {
        *self.plugin_scheduler.write() = scheduler;
    }

    pub(crate) fn active_plugin_scheduler(&self) -> Option<Arc<dyn PluginScheduler>> {
        self.plugin_scheduler.read().clone().filter(|s| s.has_scheduler())
    }

    /// Go: `PluginSchedulerWantsAcrossPriorities`.
    pub fn plugin_scheduler_wants_across_priorities(&self) -> bool {
        self.plugin_scheduler.read().as_ref().is_some_and(|s| s.wants_across_priorities())
    }
}

/// Go `schedulerAttributeSensitive`: attribute names that never reach plugins.
fn scheduler_attribute_sensitive(key: &str) -> bool {
    let key = key.trim().to_lowercase();
    let normalized: String = key.chars().map(|c| if matches!(c, '-' | '.' | ' ') { '_' } else { c }).collect();
    let compact: String = key.chars().filter(|c| !matches!(c, '_' | '-' | '.' | ' ')).collect();
    [
        "api_key",
        "apikey",
        "token",
        "secret",
        "cookie",
        "credential",
        "password",
        "storage",
        "authorization",
        "auth_header",
        "proxy_url",
    ]
    .iter()
    .any(|f| key.contains(f) || normalized.contains(f) || compact.contains(f))
}

fn scheduler_safe_attributes(src: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    src.iter().filter(|(k, _)| !scheduler_attribute_sensitive(k)).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Go `schedulerAuthCandidates`.
pub(crate) fn scheduler_auth_candidates(auths: &[Auth]) -> Vec<SchedulerAuthCandidate> {
    auths
        .iter()
        .map(|a| SchedulerAuthCandidate {
            id: a.id.clone(),
            provider: a.provider.trim().to_lowercase(),
            priority: auth_priority(a),
            status: status_name(a),
            attributes: scheduler_safe_attributes(&a.attributes),
            metadata: serde_json::Map::new(),
        })
        .collect()
}

fn status_name(a: &Auth) -> String {
    use cpa_auth::Status;
    match a.status {
        Status::Unknown => "unknown",
        Status::Active => "active",
        Status::Pending => "pending",
        Status::Refreshing => "refreshing",
        Status::Error => "error",
        Status::Disabled => "disabled",
    }
    .to_string()
}

/// Go `schedulerProviders`: distinct lowercase provider keys with `mixed` removed.
pub(crate) fn scheduler_providers(provider: &str, providers: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in std::iter::once(provider).chain(providers.iter().map(String::as_str)) {
        let v = v.trim().to_lowercase();
        if v.is_empty() || v == "mixed" || out.contains(&v) {
            continue;
        }
        out.push(v);
    }
    out
}

/// Go `schedulerOptions`: request headers and metadata offered to the plugin.
pub(crate) fn scheduler_options(opts: &Options) -> SchedulerOptions {
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in &opts.headers {
        headers
            .entry(canonical_header_key(name.as_str()))
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    // Host-only request facts (client ip, api key, ...) never reach plugins.
    const HOST_ONLY: [&str; 8] = [
        "client_ip",
        "resolved_client_ip",
        "x_forwarded_for",
        "user_agent",
        "request_id",
        "trace_id",
        "client_api_key",
        "cpa.session_affinity_ids",
    ];
    let mut keys: Vec<&String> = opts.metadata.keys().filter(|k| !HOST_ONLY.contains(&k.as_str())).collect();
    keys.sort();
    let mut metadata = serde_json::Map::new();
    for k in keys {
        metadata.insert(k.clone(), opts.metadata[k].clone());
    }
    SchedulerOptions { headers, metadata }
}

/// Go `textproto.CanonicalHeaderKey`.
fn canonical_header_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = true;
    for c in key.chars() {
        out.push(if upper { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() });
        upper = c == '-';
    }
    out
}

/// Which request protocol the executor will receive (Go: `requestToFormat`).
fn request_to_format(provider: &str, executor: &DynExecutor, req: &Request, opts: &Options) -> Format {
    if let Some(f) = executor.request_to_format(req, opts) {
        return f;
    }
    if opts.alt == "responses/compact" && !opts.stream {
        return Format::OpenAIResponse;
    }
    match provider.trim().to_lowercase().as_str() {
        "codex" | "xai" | "meta" => Format::Codex,
        "claude" => Format::Claude,
        "gemini" | "vertex" | "aistudio" => Format::Gemini,
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => Format::OpenAI,
        "antigravity" => Format::Antigravity,
        "devin" => Format::Interactions,
        _ => Format::OpenAI,
    }
}

/// Go `mergeRequestHeaders`: `clear` first (case-insensitively), then `updates` replace.
fn merge_request_headers(current: &HeaderMap, updates: Option<&HeaderMap>, clear: &[String]) -> HeaderMap {
    let mut out = current.clone();
    for key in clear {
        if let Ok(name) = HeaderName::from_bytes(key.trim().as_bytes()) {
            out.remove(&name);
        }
    }
    if let Some(updates) = updates {
        let names: Vec<HeaderName> = updates.keys().cloned().collect();
        for name in names {
            out.remove(&name);
            for v in updates.get_all(&name) {
                if let Ok(v) = HeaderValue::from_bytes(v.as_bytes()) {
                    out.append(name.clone(), v);
                }
            }
        }
    }
    out
}

/// Runs the plugin interceptor for one attempt (Go: `applyRequestAfterAuthInterceptor`); the
/// returned error is a [`RequestTerminated`] when a plugin ended the request.
pub(crate) async fn apply_request_after_auth_interceptor(
    executor: &DynExecutor,
    provider: &str,
    mut req: Request,
    mut opts: Options,
    requested_model: &str,
) -> Result<(Request, Options), ExecError> {
    let Some(interceptor) = opts.request_after_auth.clone() else { return Ok((req, opts)) };
    let to_format = request_to_format(provider, executor, &req, &opts);
    let resp = (interceptor.0)(RequestAfterAuthInterceptRequest {
        source_format: opts.source_format,
        to_format,
        model: req.model.clone(),
        requested_model: requested_model.to_string(),
        stream: opts.stream,
        headers: opts.headers.clone(),
        body: req.payload.clone(),
        metadata: opts.metadata.clone(),
    })
    .await;
    if resp.headers.is_some() || !resp.clear_headers.is_empty() {
        opts.headers = merge_request_headers(&opts.headers, resp.headers.as_ref(), &resp.clear_headers);
    }
    if !resp.body.is_empty() {
        req.payload = resp.body.clone();
        opts.original_request = resp.body.clone();
    }
    let path = resp.path.trim();
    if !path.is_empty() {
        opts.metadata.insert(meta::REQUEST_PATH.into(), Value::String(path.to_string()));
    }
    if resp.terminate {
        return Err(ExecError::request_terminated(RequestTerminated {
            status: resp.status_code,
            headers: resp.response_headers,
            body: resp.response_body,
        }));
    }
    let eval_payload: Bytes = if opts.original_request.is_empty() { req.payload.clone() } else { opts.original_request.clone() };
    if !resp.clear_headers.is_empty() || !resp.body.is_empty() {
        match extract_session_info(&opts.headers, &eval_payload, &opts.metadata) {
            Some(info) if !info.session_id.is_empty() => {
                opts.metadata.insert(meta::CANONICAL_SESSION_ID.into(), Value::String(bound_session_identity(&info.session_id)));
                if !info.parent_session_id.is_empty() && info.parent_session_id != info.session_id {
                    opts.metadata.insert(meta::PARENT_SESSION_ID.into(), Value::String(bound_session_identity(&info.parent_session_id)));
                } else {
                    opts.metadata.remove(meta::PARENT_SESSION_ID);
                }
            }
            _ => {
                opts.metadata.remove(meta::CANONICAL_SESSION_ID);
                opts.metadata.remove(meta::PARENT_SESSION_ID);
                opts.metadata.remove("lcp_affinity_session_id");
            }
        }
    } else if resp.headers.as_ref().is_some_and(|h| !h.is_empty())
        && let Some(info) = extract_session_info(&opts.headers, &[], &opts.metadata)
        && !info.session_id.is_empty()
    {
        opts.metadata.insert(meta::CANONICAL_SESSION_ID.into(), Value::String(bound_session_identity(&info.session_id)));
        if !info.parent_session_id.is_empty() && info.parent_session_id != info.session_id {
            opts.metadata.insert(meta::PARENT_SESSION_ID.into(), Value::String(bound_session_identity(&info.parent_session_id)));
        }
    }
    Ok((req, opts))
}

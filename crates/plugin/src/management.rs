//! Plugin-owned Management API and resource routes (Go: `management.go`).

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use cpa_pluginapi::abi;
use cpa_pluginapi::api::{
    Header, ManagementRegistrationRequest, ManagementRegistrationResponse, ManagementRequest, ManagementResponse, ManagementRoute,
    ResourceRoute,
};
use http::HeaderMap;
use serde_json::Value;

use crate::convert::{header_get, headers_to_go, query_to_go};
use crate::ctx::CallCtx;
use crate::host::Host;

pub const MANAGEMENT_BASE_PATH: &str = "/v0/management";
pub const RESOURCE_PLUGIN_BASE_PATH: &str = "/v0/resource/plugins";
const LEGACY_PLUGIN_ROUTE_PREFIX: &str = "/plugins";

#[derive(Debug, Clone)]
pub struct ManagementRouteRecord {
    pub plugin_id: String,
    pub path: PathBuf,
    pub version: String,
    pub schema_version: u32,
    pub route: ManagementRoute,
}

#[derive(Debug, Clone)]
pub struct ResourceRouteRecord {
    pub plugin_id: String,
    pub path: PathBuf,
    pub version: String,
    pub route: ResourceRoute,
}

/// What a plugin route produced for the HTTP layer to write.
#[derive(Debug, Clone, Default)]
pub struct PluginHttpResponse {
    pub status: u16,
    pub headers: Header,
    pub body: Vec<u8>,
}

impl PluginHttpResponse {
    /// `http.Error(w, msg, status)`.
    fn http_error(msg: &str, status: u16) -> Self {
        let mut headers = BTreeMap::new();
        headers.insert("Content-Type".to_string(), vec!["text/plain; charset=utf-8".to_string()]);
        headers.insert("X-Content-Type-Options".to_string(), vec!["nosniff".to_string()]);
        PluginHttpResponse { status, headers, body: format!("{msg}\n").into_bytes() }
    }
}

fn route_key(method: &str, path: &str) -> String {
    format!("{} {}", method.trim().to_uppercase(), path.trim())
}

fn has_ws(s: &str) -> bool {
    s.chars().any(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
}

/// Go `normalizeManagementRoute`: method plus the full `/v0/management/...` path.
fn normalize_management_route(item: &ManagementRoute) -> Option<(String, String)> {
    let mut method = item.method.trim().to_uppercase();
    if method.is_empty() {
        method = "GET".into();
    }
    if has_ws(&method) {
        return None;
    }
    let mut path = item.path.trim().to_string();
    if path.is_empty() {
        return None;
    }
    if !path.starts_with('/') {
        path = format!("/{path}");
    }
    if let Some(rest) = path.strip_prefix(&format!("{MANAGEMENT_BASE_PATH}/")) {
        path = format!("/{rest}");
    }
    let path = path.trim_end_matches('/').to_string();
    if path.is_empty() {
        return None;
    }
    let full = format!("{MANAGEMENT_BASE_PATH}{path}");
    if !full.starts_with(&format!("{MANAGEMENT_BASE_PATH}/")) {
        return None;
    }
    if has_ws(&full) || full.contains(':') || full.contains('*') {
        return None;
    }
    Some((method, full))
}

fn route_declares_legacy_menu_resource(method: &str, item: &ManagementRoute) -> bool {
    method.trim().eq_ignore_ascii_case("GET") && !item.menu.trim().is_empty()
}

/// Go `normalizeResourceRoute`: the full `/v0/resource/plugins/<id>/...` path.
fn normalize_resource_route(plugin_id: &str, item: &ResourceRoute) -> Option<String> {
    let plugin_id = plugin_id.trim();
    if plugin_id.is_empty() {
        return None;
    }
    let mut path = item.path.trim().to_string();
    if path.is_empty() {
        return None;
    }
    if !path.starts_with('/') {
        path = format!("/{path}");
    }
    let base = format!("{RESOURCE_PLUGIN_BASE_PATH}/{plugin_id}");
    if let Some(rest) = path.strip_prefix(&format!("{base}/")) {
        path = format!("/{rest}");
    } else if let Some(rest) = path.strip_prefix(&format!("{LEGACY_PLUGIN_ROUTE_PREFIX}/{plugin_id}/")) {
        path = format!("/{rest}");
    }
    let path = path.trim_end_matches('/').to_string();
    if path.is_empty() {
        return None;
    }
    let full = format!("{base}{path}");
    if !full.starts_with(&format!("{base}/")) {
        return None;
    }
    if has_ws(&full) || full.contains(':') || full.contains('*') || full.contains("..") {
        return None;
    }
    Some(full)
}

/// Go `htmlsanitize.String` (`html.EscapeString`).
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

fn escape_json_value(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(html_escape(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(escape_json_value).collect()),
        Value::Object(m) => {
            let sorted: BTreeMap<String, Value> = m.into_iter().map(|(k, v)| (k, escape_json_value(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        other => other,
    }
}

fn is_json_content_type(ct: &str) -> bool {
    let media = ct.split(';').next().unwrap_or("").trim().to_lowercase();
    media == "application/json" || media.ends_with("+json")
}

/// Go `htmlsanitize.JSONBodyIfLikely`: HTML-escapes every string of a JSON document; other
/// bodies pass through.
pub fn escape_json_body_if_likely(body: &[u8], content_type: &str) -> Vec<u8> {
    let trimmed = String::from_utf8_lossy(body).trim().to_string();
    let looks = trimmed.starts_with('{') || trimmed.starts_with('[');
    if !(is_json_content_type(content_type) || looks) {
        return body.to_vec();
    }
    match serde_json::from_str::<Value>(&trimmed) {
        Ok(v) => serde_json::to_string(&escape_json_value(v)).map(|s| s.replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029").into_bytes()).unwrap_or_else(|_| body.to_vec()),
        Err(_) => body.to_vec(),
    }
}

impl Host {
    /// Rebuilds the route tables from the active plugins (Go: `RegisterManagementRoutes`).
    /// `reserved` holds `METHOD path` keys of routes the host owns.
    pub async fn register_management_routes(self: &Arc<Self>, ctx: &CallCtx, reserved: &HashSet<String>) {
        let mut next_routes: std::collections::HashMap<String, ManagementRouteRecord> = Default::default();
        let mut next_resources: std::collections::HashMap<String, ResourceRouteRecord> = Default::default();
        for rec in self.active_records() {
            if !rec.caps().management_api || self.is_plugin_fused(&rec.id) {
                continue;
            }
            if !self.usable(&rec) {
                continue;
            }
            let req = ManagementRegistrationRequest {
                plugin: rec.meta.clone(),
                base_path: MANAGEMENT_BASE_PATH.into(),
                resource_base_path: format!("{RESOURCE_PLUGIN_BASE_PATH}/{}", rec.id),
            };
            let resp: ManagementRegistrationResponse = match self.rpc(&rec, ctx, abi::METHOD_MANAGEMENT_REGISTER, &req).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("pluginhost: management registrar {} failed: {e}", rec.id);
                    continue;
                }
            };
            for item in resp.routes {
                let Some((method, path)) = normalize_management_route(&item) else {
                    tracing::warn!("pluginhost: plugin {} declared invalid management route {} {}", rec.id, item.method, item.path);
                    continue;
                };
                if route_declares_legacy_menu_resource(&method, &item) {
                    let as_resource = ResourceRoute { path: item.path.clone(), menu: item.menu.clone(), description: item.description.clone() };
                    if !register_resource_route(&mut next_resources, &rec, as_resource) {
                        tracing::warn!("pluginhost: plugin {} declared invalid resource route {}", rec.id, item.path);
                    }
                    continue;
                }
                let key = route_key(&method, &path);
                if reserved.contains(&key) {
                    tracing::warn!("pluginhost: plugin {} management route {key} conflicts with an existing route and was skipped", rec.id);
                    continue;
                }
                if next_routes.contains_key(&key) {
                    tracing::warn!("pluginhost: plugin {} management route {key} conflicts with a higher-priority plugin and was skipped", rec.id);
                    continue;
                }
                let mut route = item;
                route.method = method;
                route.path = path;
                next_routes.insert(
                    key,
                    ManagementRouteRecord {
                        plugin_id: rec.id.clone(),
                        path: rec.path.clone(),
                        version: rec.version.clone(),
                        schema_version: rec.schema_version(),
                        route,
                    },
                );
            }
            for item in resp.resources {
                let path = item.path.clone();
                if !register_resource_route(&mut next_resources, &rec, item) {
                    tracing::warn!("pluginhost: plugin {} declared invalid resource route {path}", rec.id);
                }
            }
        }
        let mut st = self.state.lock();
        st.management_routes = next_routes;
        st.resource_routes = next_resources;
    }

    /// Whether a plugin declares the given authenticated management route.
    pub fn has_management_route(&self, method: &str, path: &str) -> bool {
        let key = route_key(method, path);
        let st = self.state.lock();
        st.management_routes.get(&key).is_some_and(|r| !self.is_plugin_fused(&r.plugin_id))
    }

    /// Dispatches an authenticated Management API request to a plugin route (Go:
    /// `ServeManagementHTTP`); `None` when no plugin route matches.
    pub async fn serve_management_http(
        self: &Arc<Self>,
        ctx: &CallCtx,
        method: &str,
        path: &str,
        headers: &HeaderMap,
        query: &[(String, String)],
        body: &[u8],
    ) -> Option<PluginHttpResponse> {
        let key = route_key(method, path);
        let record = self.state.lock().management_routes.get(&key).cloned()?;
        if self.is_plugin_fused(&record.plugin_id) {
            return None;
        }
        let req = ManagementRequest {
            method: method.to_string(),
            path: path.to_string(),
            headers: headers_to_go(headers),
            query: query_to_go(query),
            body: body.to_vec(),
        };
        let resp = match self.call_management_handler(ctx, &record.plugin_id, &record.path, &record.version, req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("pluginhost: management handler {} failed: {e}", record.plugin_id);
                return Some(PluginHttpResponse::http_error("plugin management handler failed", 502));
            }
        };
        let mut body = resp.body;
        if record.schema_version < abi::SCHEMA_VERSION_RAW_MANAGEMENT_RESPONSE {
            let ct = header_get(&resp.headers, "Content-Type").and_then(|v| v.first().cloned()).unwrap_or_default();
            body = escape_json_body_if_likely(&body, &ct);
        }
        let status = if resp.status_code == 0 { 200 } else { u16::try_from(resp.status_code).unwrap_or(200) };
        Some(PluginHttpResponse { status, headers: resp.headers, body })
    }

    /// Dispatches an unauthenticated resource request to a plugin route (Go: `ServeResourceHTTP`).
    pub async fn serve_resource_http(
        self: &Arc<Self>,
        ctx: &CallCtx,
        method: &str,
        path: &str,
        headers: &HeaderMap,
        query: &[(String, String)],
    ) -> Option<PluginHttpResponse> {
        if !method.eq_ignore_ascii_case("GET") {
            return None;
        }
        let key = route_key("GET", path);
        let record = self.state.lock().resource_routes.get(&key).cloned()?;
        if self.is_plugin_fused(&record.plugin_id) {
            return None;
        }
        let req = ManagementRequest {
            method: "GET".into(),
            path: path.to_string(),
            headers: headers_to_go(headers),
            query: query_to_go(query),
            body: Vec::new(),
        };
        let resp = match self.call_management_handler(ctx, &record.plugin_id, &record.path, &record.version, req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("pluginhost: resource handler {} failed: {e}", record.plugin_id);
                return Some(PluginHttpResponse::http_error("plugin resource handler failed", 502));
            }
        };
        let status = if resp.status_code == 0 { 200 } else { u16::try_from(resp.status_code).unwrap_or(200) };
        Some(PluginHttpResponse { status, headers: resp.headers, body: resp.body })
    }

    /// Go `callManagementHandler`/`callResourceHandler`: a stale or fused plugin answers with an
    /// empty response.
    async fn call_management_handler(
        self: &Arc<Self>,
        ctx: &CallCtx,
        plugin_id: &str,
        path: &std::path::Path,
        version: &str,
        req: ManagementRequest,
    ) -> Result<ManagementResponse, crate::client::PluginError> {
        if self.is_plugin_fused(plugin_id) || !self.plugin_identity_current(plugin_id, path, version) {
            return Ok(ManagementResponse::default());
        }
        let Some(rec) = self.active_records().into_iter().find(|r| r.id == plugin_id) else {
            return Ok(ManagementResponse::default());
        };
        self.rpc_cb(&rec, ctx, abi::METHOD_MANAGEMENT_HANDLE, &req).await
    }
}

fn register_resource_route(
    routes: &mut std::collections::HashMap<String, ResourceRouteRecord>,
    rec: &crate::caps::Record,
    item: ResourceRoute,
) -> bool {
    let Some(path) = normalize_resource_route(&rec.id, &item) else { return false };
    let key = route_key("GET", &path);
    if routes.contains_key(&key) {
        tracing::warn!("pluginhost: plugin {} resource route {key} conflicts with a higher-priority plugin and was skipped", rec.id);
        return true;
    }
    let mut route = item;
    route.path = path;
    routes.insert(
        key,
        ResourceRouteRecord { plugin_id: rec.id.clone(), path: rec.path.clone(), version: rec.version.clone(), route },
    );
    true
}

//! Management API client (Go: internal/tui/client.go). Every call is a blocking-style async
//! request with a 10 second timeout; errors are plain display strings shown in the UI.

use std::time::Duration;

use parking_lot::RwLock;
use serde_json::{Value, json};

use crate::jsonutil::get_string;

/// Client errors are shown verbatim in the UI, so a message string is all that is kept.
pub type Result<T> = std::result::Result<T, String>;

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8317";

/// HTTP client bound to one management base URL and bearer secret.
pub struct Client {
    base_url: String,
    secret_key: RwLock<String>,
    http: reqwest::Client,
}

/// `url.QueryEscape`: spaces become `+`, unreserved characters pass through.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `NewClientWithBaseURL` normalisation: blank falls back to the default local address, a missing
/// scheme becomes `http://`, trailing slashes are dropped.
pub fn normalize_base_url(base_url: &str) -> String {
    let base_url = base_url.trim();
    if base_url.is_empty() {
        return DEFAULT_BASE_URL.to_string();
    }
    let lower = base_url.to_lowercase();
    let with_scheme = if lower.starts_with("http://") || lower.starts_with("https://") {
        base_url.to_string()
    } else {
        format!("http://{base_url}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

fn is_loopback_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    match url.host_str() {
        Some(host) => {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host.eq_ignore_ascii_case("localhost")
                || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        }
        None => false,
    }
}

impl Client {
    /// `NewClient`: management API on localhost at `port`.
    pub fn new(port: i64, secret_key: &str) -> Self {
        Self::with_base_url(&format!("http://127.0.0.1:{port}"), secret_key)
    }

    pub fn with_base_url(base_url: &str, secret_key: &str) -> Self {
        let base_url = normalize_base_url(base_url);
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
        // Go's default transport never proxies loopback addresses.
        if is_loopback_url(&base_url) {
            builder = builder.no_proxy();
        }
        let http = builder.build().unwrap_or_default();
        Client {
            base_url,
            secret_key: RwLock::new(secret_key.trim().to_string()),
            http,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `SetSecretKey`: replaces the bearer token used by later requests.
    pub fn set_secret_key(&self, secret_key: &str) {
        *self.secret_key.write() = secret_key.trim().to_string();
    }

    /// `doRequest`: returns the body bytes and status code. A body is always sent as JSON.
    async fn do_request(&self, method: reqwest::Method, path: &str, body: Option<String>) -> Result<(Vec<u8>, u16)> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self.http.request(method, url);
        let key = self.secret_key.read().clone();
        if !key.is_empty() {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
        if let Some(body) = body {
            req = req.header("Content-Type", "application/json").body(body);
        }
        let resp = req.send().await.map_err(|e| describe_error(&e))?;
        let status = resp.status().as_u16();
        let data = resp.bytes().await.map_err(|e| describe_error(&e))?;
        Ok((data.to_vec(), status))
    }

    /// Request that treats status >= 400 as `HTTP <code>: <body>`.
    async fn checked(&self, method: reqwest::Method, path: &str, body: Option<String>) -> Result<Vec<u8>> {
        let (data, code) = self.do_request(method, path, body).await?;
        if code >= 400 {
            return Err(format!("HTTP {code}: {}", String::from_utf8_lossy(&data).trim()));
        }
        Ok(data)
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        self.checked(reqwest::Method::GET, path, None).await
    }

    async fn put(&self, path: &str, body: String) -> Result<Vec<u8>> {
        self.checked(reqwest::Method::PUT, path, Some(body)).await
    }

    async fn patch(&self, path: &str, body: String) -> Result<Vec<u8>> {
        self.checked(reqwest::Method::PATCH, path, Some(body)).await
    }

    /// `getJSON`: the body must be a JSON object.
    pub async fn get_json(&self, path: &str) -> Result<Value> {
        let data = self.get(path).await?;
        match serde_json::from_slice::<Value>(&data) {
            Ok(v @ Value::Object(_)) => Ok(v),
            Ok(Value::Null) => Ok(Value::Null),
            Ok(other) => Err(format!(
                "json: cannot unmarshal {} into Go value of type map[string]interface {{}}",
                json_kind(&other)
            )),
            Err(e) => Err(e.to_string()),
        }
    }

    /// `postJSON`: POST a JSON body; only the status code is checked.
    pub async fn post_json(&self, path: &str, body: &Value) -> Result<()> {
        let (_, code) = self.do_request(reqwest::Method::POST, path, Some(body.to_string())).await?;
        if code >= 400 {
            return Err(format!("HTTP {code}"));
        }
        Ok(())
    }

    /// `GetConfig`: the parsed config.
    pub async fn get_config(&self) -> Result<Value> {
        self.get_json("/v0/management/config").await
    }

    /// `GetConfigYAML`: raw config.yaml.
    pub async fn get_config_yaml(&self) -> Result<String> {
        let data = self.get("/v0/management/config.yaml").await?;
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    /// `PutConfigYAML`.
    pub async fn put_config_yaml(&self, yaml: &str) -> Result<()> {
        self.put("/v0/management/config.yaml", yaml.to_string()).await.map(drop)
    }

    /// `GetAuthFiles`: API returns `{"files": [...]}`.
    pub async fn get_auth_files(&self) -> Result<Vec<Value>> {
        let wrapper = self.get_json("/v0/management/auth-files").await?;
        extract_list(&wrapper, "files")
    }

    /// `DeleteAuthFile`.
    pub async fn delete_auth_file(&self, name: &str) -> Result<()> {
        let path = format!("/v0/management/auth-files?name={}", query_escape(name));
        let (_, code) = self.do_request(reqwest::Method::DELETE, &path, None).await?;
        if code >= 400 {
            return Err(format!("delete failed (HTTP {code})"));
        }
        Ok(())
    }

    /// `ToggleAuthFile`: enable or disable. Object keys are written sorted like Go's map marshaling.
    pub async fn toggle_auth_file(&self, name: &str, disabled: bool) -> Result<()> {
        let body = json!({"disabled": disabled, "name": name}).to_string();
        self.patch("/v0/management/auth-files/status", body).await.map(drop)
    }

    /// `PatchAuthFileFields`: `fields` is a JSON object; `name` is added to it.
    pub async fn patch_auth_file_fields(&self, name: &str, mut fields: Value) -> Result<()> {
        if let Value::Object(map) = &mut fields {
            map.insert("name".into(), Value::String(name.to_string()));
        }
        // Go marshals the map with sorted keys.
        let sorted: std::collections::BTreeMap<String, Value> = match fields {
            Value::Object(map) => map.into_iter().collect(),
            _ => Default::default(),
        };
        let body = serde_json::to_string(&sorted).map_err(|e| e.to_string())?;
        self.patch("/v0/management/auth-files/fields", body).await.map(drop)
    }

    /// `RefreshAuthFile`: force-refresh one credential.
    pub async fn refresh_auth_file(&self, name: &str) -> Result<()> {
        self.post_json("/v0/management/auth-files/refresh", &json!({"name": name})).await
    }

    /// `RefreshAllAuthFiles`.
    pub async fn refresh_all_auth_files(&self) -> Result<()> {
        self.post_json("/v0/management/auth-files/refresh", &json!({"all": true})).await
    }

    /// `GetLogs`: returns the lines and the latest timestamp (never below `after`).
    pub async fn get_logs(&self, after: i64, limit: usize) -> Result<(Vec<String>, i64)> {
        // url.Values.Encode sorts keys, so `after` precedes `limit`.
        let mut query = Vec::new();
        if after > 0 {
            query.push(format!("after={after}"));
        }
        if limit > 0 {
            query.push(format!("limit={limit}"));
        }
        let mut path = "/v0/management/logs".to_string();
        if !query.is_empty() {
            path.push('?');
            path.push_str(&query.join("&"));
        }
        let wrapper = self.get_json(&path).await?;
        let mut lines = Vec::new();
        match wrapper.get("lines") {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => {
                for item in items {
                    match item {
                        Value::String(s) => lines.push(s.clone()),
                        Value::Null => lines.push(String::new()),
                        other => {
                            return Err(format!(
                                "json: cannot unmarshal {} into Go value of type string",
                                json_kind(other)
                            ));
                        }
                    }
                }
            }
            Some(other) => {
                return Err(format!(
                    "json: cannot unmarshal {} into Go value of type []string",
                    json_kind(other)
                ));
            }
        }
        let mut latest = after;
        if let Some(Value::Number(n)) = wrapper.get("latest-timestamp")
            && let Some(f) = n.as_f64()
        {
            latest = f as i64;
        }
        Ok((lines, latest.max(after)))
    }

    /// `GetAPIKeys`: API returns `{"api-keys": [...]}`.
    pub async fn get_api_keys(&self) -> Result<Vec<String>> {
        let wrapper = self.get_json("/v0/management/api-keys").await?;
        match wrapper.get("api-keys") {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => Ok(items
                .iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s.clone()),
                    Value::Null => Ok(String::new()),
                    other => Err(format!(
                        "json: cannot unmarshal {} into Go value of type string",
                        json_kind(other)
                    )),
                })
                .collect::<Result<Vec<_>>>()?),
            Some(other) => Err(format!(
                "json: cannot unmarshal {} into Go value of type []string",
                json_kind(other)
            )),
        }
    }

    /// `AddAPIKey`. The Go client sends `old=null`, which the server rejects as "missing fields"
    /// (it needs both `old` and `new`); with `old == new` the server appends an unknown key.
    pub async fn add_api_key(&self, key: &str) -> Result<()> {
        let body = json!({"new": key, "old": key}).to_string();
        self.patch("/v0/management/api-keys", body).await.map(drop)
    }

    /// `EditAPIKey`: replace the key at `index`.
    pub async fn edit_api_key(&self, index: usize, value: &str) -> Result<()> {
        let body = json!({"index": index, "value": value}).to_string();
        self.patch("/v0/management/api-keys", body).await.map(drop)
    }

    /// `DeleteAPIKey`.
    pub async fn delete_api_key(&self, index: usize) -> Result<()> {
        let path = format!("/v0/management/api-keys?index={index}");
        let (_, code) = self.do_request(reqwest::Method::DELETE, &path, None).await?;
        if code >= 400 {
            return Err(format!("delete failed (HTTP {code})"));
        }
        Ok(())
    }

    pub async fn get_gemini_keys(&self) -> Result<Vec<Value>> {
        self.get_wrapped_key_list("/v0/management/gemini-api-key", "gemini-api-key").await
    }

    pub async fn get_interactions_keys(&self) -> Result<Vec<Value>> {
        self.get_wrapped_key_list("/v0/management/interactions-api-key", "interactions-api-key").await
    }

    pub async fn get_claude_keys(&self) -> Result<Vec<Value>> {
        self.get_wrapped_key_list("/v0/management/claude-api-key", "claude-api-key").await
    }

    pub async fn get_codex_keys(&self) -> Result<Vec<Value>> {
        self.get_wrapped_key_list("/v0/management/codex-api-key", "codex-api-key").await
    }

    pub async fn get_xai_keys(&self) -> Result<Vec<Value>> {
        self.get_wrapped_key_list("/v0/management/xai-api-key", "xai-api-key").await
    }

    pub async fn get_vertex_keys(&self) -> Result<Vec<Value>> {
        self.get_wrapped_key_list("/v0/management/vertex-api-key", "vertex-api-key").await
    }

    pub async fn get_openai_compat(&self) -> Result<Vec<Value>> {
        self.get_wrapped_key_list("/v0/management/openai-compatibility", "openai-compatibility").await
    }

    async fn get_wrapped_key_list(&self, path: &str, key: &str) -> Result<Vec<Value>> {
        let wrapper = self.get_json(path).await?;
        extract_list(&wrapper, key)
    }

    /// `GetDebug`.
    pub async fn get_debug(&self) -> Result<bool> {
        let wrapper = self.get_json("/v0/management/debug").await?;
        Ok(wrapper.get("debug").and_then(Value::as_bool).unwrap_or(false))
    }

    /// `GetAuthStatus`: polls an OAuth session, returning `(status, error message)` where status
    /// is "wait", "ok" or "error".
    pub async fn get_auth_status(&self, state: &str) -> Result<(String, String)> {
        let path = format!("/v0/management/get-auth-status?state={}", query_escape(state));
        let wrapper = self.get_json(&path).await?;
        Ok((get_string(&wrapper, "status"), get_string(&wrapper, "error")))
    }

    /// `CancelAuthSession`: drops a pending OAuth session on the server.
    pub async fn cancel_auth_session(&self, state: &str) -> Result<()> {
        let state = state.trim();
        if state.is_empty() {
            return Ok(());
        }
        let path = format!("/v0/management/oauth-session?state={}", query_escape(state));
        let (_, code) = self.do_request(reqwest::Method::DELETE, &path, None).await?;
        if code >= 400 {
            return Err(format!("HTTP {code}"));
        }
        Ok(())
    }

    /// `PutBoolField`.
    pub async fn put_bool_field(&self, path: &str, value: bool) -> Result<()> {
        self.put_value(path, json!(value)).await
    }

    /// `PutIntField`.
    pub async fn put_int_field(&self, path: &str, value: i64) -> Result<()> {
        self.put_value(path, json!(value)).await
    }

    /// `PutStringField`.
    pub async fn put_string_field(&self, path: &str, value: &str) -> Result<()> {
        self.put_value(path, json!(value)).await
    }

    async fn put_value(&self, path: &str, value: Value) -> Result<()> {
        let body = json!({ "value": value }).to_string();
        self.put(&format!("/v0/management/{path}"), body).await.map(drop)
    }

    /// `DeleteField`: DELETE of a config field; the status code is ignored.
    pub async fn delete_field(&self, path: &str) -> Result<()> {
        self.do_request(reqwest::Method::DELETE, &format!("/v0/management/{path}"), None).await.map(drop)
    }

    /// Dashboard OAuth start helper: `GET /v0/management/<path>?is_webui=true`.
    pub async fn start_oauth(&self, api_path: &str) -> Result<Value> {
        self.get_json(&format!("/v0/management/{api_path}?is_webui=true")).await
    }
}

/// `extractList`: an array of objects under `key`; absent or null is an empty list.
pub fn extract_list(wrapper: &Value, key: &str) -> Result<Vec<Value>> {
    match wrapper.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Object(_) | Value::Null => Ok(item.clone()),
                other => Err(format!(
                    "json: cannot unmarshal {} into Go value of type map[string]interface {{}}",
                    json_kind(other)
                )),
            })
            .collect(),
        Some(other) => Err(format!(
            "json: cannot unmarshal {} into Go value of type []map[string]interface {{}}",
            json_kind(other)
        )),
    }
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Flattens reqwest's nested error chain into one line like Go's wrapped errors.
fn describe_error(e: &reqwest::Error) -> String {
    use std::error::Error;
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    msg
}

//! Generates the temp config file handed to the server under test.
//!
//! `ConfigSpec` is layout-neutral; it renders either the legacy flat layout or the v8 layout
//! (both are accepted by the Go reference), then a tiny YAML emitter writes it out.

use std::path::Path;

use serde_json::{Map, Value, json};

pub const CLIENT_KEY: &str = "e2e-client-key";
pub const CLIENT_KEY_2: &str = "e2e-client-key-2";
pub const MGMT_SECRET: &str = "e2e-mgmt-secret";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Layout {
    #[default]
    Legacy,
    V8,
}

#[derive(Clone, Debug, Default)]
pub struct ModelCfg {
    pub name: String,
    pub alias: String,
    /// Callable through the image endpoints (`image: true`).
    pub image: bool,
}

/// One upstream API key (one credential).
#[derive(Clone, Debug, Default)]
pub struct KeyEntry {
    pub api_key: String,
    pub base_url: String,
    pub prefix: Option<String>,
    pub priority: Option<i64>,
    pub headers: Vec<(String, String)>,
    pub models: Vec<ModelCfg>,
    pub excluded_models: Vec<String>,
    pub websockets: Option<bool>,
    /// Weighted-round-robin share (key-level setting).
    pub weight: Option<i64>,
    /// `request-scoped-errors` rules.
    pub scoped_errors: Vec<ScopedRule>,
    /// Codex key may serve the Alpha Search endpoint (`alpha-search`).
    pub alpha_search: Option<bool>,
}

/// Custom upstream error classification rule (`status` + body substring -> `action`).
#[derive(Clone, Debug)]
pub struct ScopedRule {
    pub status: u16,
    pub matches: &'static str,
    pub action: &'static str,
}

#[derive(Clone, Debug, Default)]
pub struct CompatProvider {
    pub name: String,
    pub base_url: String,
    pub keys: Vec<String>,
    pub models: Vec<ModelCfg>,
    pub prefix: Option<String>,
    pub headers: Vec<(String, String)>,
}

/// One dynamic plugin enabled for the scenario: `id` is the library file stem under the plugin
/// build directory (`<id>.so`), `settings` land in `plugins.configs.<id>` next to `enabled`.
#[derive(Clone, Debug)]
pub struct PluginSpec {
    pub id: &'static str,
    pub priority: i64,
    pub settings: Vec<(&'static str, Value)>,
}

impl PluginSpec {
    pub fn new(id: &'static str) -> Self {
        PluginSpec { id, priority: 0, settings: vec![] }
    }

    pub fn priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }

    pub fn setting(mut self, key: &'static str, value: Value) -> Self {
        self.settings.push((key, value));
        self
    }
}

#[derive(Clone, Debug)]
pub struct ConfigSpec {
    pub client_keys: Vec<String>,
    pub management_secret: Option<String>,
    pub request_retry: u32,
    pub max_retry_credentials: u32,
    pub max_retry_interval: u32,
    pub strategy: Option<String>,
    pub transient_cooldown_seconds: Option<i32>,
    pub disable_cooling: bool,
    pub passthrough_headers: bool,
    pub nonstream_keepalive: u32,
    pub keepalive_seconds: u32,
    pub bootstrap_retries: u32,
    pub force_model_prefix: bool,
    /// `usage-statistics-enabled`: records usage events for the management usage queue.
    pub usage_statistics: bool,
    /// `request-log`: write a log file for every request (otherwise only error requests).
    pub request_log: bool,
    pub claude: Vec<KeyEntry>,
    pub codex: Vec<KeyEntry>,
    pub gemini: Vec<KeyEntry>,
    pub compat: Vec<CompatProvider>,
    /// Plugins to load; the libraries are copied into the scenario's `plugins/` directory.
    pub plugins: Vec<PluginSpec>,
    /// Extra headers on the readiness probe (an exclusive frontend auth plugin replaces the
    /// client key check).
    pub ready_headers: Vec<(&'static str, &'static str)>,
    /// Credential files written into the auth dir before the server starts.
    pub auth_files: Vec<(&'static str, &'static str)>,
    /// xAI keys (none by default: they would add models to every listing).
    pub xai: Vec<KeyEntry>,
    /// `multimedia` settings (`disable-image-generation`, `gpt-image-2-base-model`,
    /// `video-result-auth-cache-ttl`).
    pub multimedia: Vec<(&'static str, Value)>,
    /// Provider-wide `codex:` settings (`response-steering`, ...), legacy layout only.
    pub codex_settings: Vec<(&'static str, Value)>,
}

impl ConfigSpec {
    /// Baseline: two keys per provider family, all pointing at the mock.
    pub fn baseline(mock_port: u16) -> Self {
        let base = |family: &str| format!("http://127.0.0.1:{mock_port}/{family}");
        let keys = |family: &str, label: &str| {
            (1..=2)
                .map(|i| KeyEntry { api_key: format!("sk-{label}-{i}"), base_url: base(family), ..Default::default() })
                .collect::<Vec<_>>()
        };
        ConfigSpec {
            client_keys: vec![CLIENT_KEY.into(), CLIENT_KEY_2.into()],
            management_secret: Some(MGMT_SECRET.into()),
            request_retry: 2,
            max_retry_credentials: 0,
            max_retry_interval: 0,
            strategy: None,
            transient_cooldown_seconds: None,
            disable_cooling: false,
            passthrough_headers: false,
            nonstream_keepalive: 0,
            keepalive_seconds: 0,
            bootstrap_retries: 0,
            force_model_prefix: false,
            usage_statistics: false,
            request_log: false,
            claude: keys("anthropic", "claude"),
            codex: keys("codex", "codex"),
            gemini: keys("gemini", "gemini"),
            plugins: vec![],
            ready_headers: vec![],
            auth_files: vec![],
            compat: vec![CompatProvider {
                name: "mockcompat".into(),
                base_url: base("compat"),
                keys: vec!["sk-compat-1".into(), "sk-compat-2".into()],
                models: vec![
                    ModelCfg { name: "mock-gpt-4o".into(), alias: "compat-gpt-4o".into(), image: false },
                    ModelCfg { name: "mock-reason".into(), alias: "compat-reason".into(), image: false },
                ],
                prefix: None,
                headers: vec![],
            }],
            xai: vec![],
            multimedia: vec![],
            codex_settings: vec![],
        }
    }
}

fn model_list(models: &[ModelCfg]) -> Value {
    let one = |m: &ModelCfg| {
        let mut v = json!({"name": m.name, "alias": m.alias});
        if m.image {
            v["image"] = json!(true);
        }
        v
    };
    Value::Array(models.iter().map(one).collect())
}

/// Settings that stay key-level in both layouts.
fn key_fields(k: &KeyEntry, m: &mut Map<String, Value>) {
    if let Some(w) = k.weight {
        m.insert("weight".into(), json!(w));
    }
    if let Some(w) = k.websockets {
        m.insert("websockets".into(), json!(w));
    }
    if let Some(a) = k.alpha_search {
        m.insert("alpha-search".into(), json!(a));
    }
}

/// Per-key fields shared by every provider family (without api-key/base-url/key-level settings).
fn shared_fields(k: &KeyEntry, m: &mut Map<String, Value>) {
    if let Some(p) = &k.prefix {
        m.insert("prefix".into(), json!(p));
    }
    if let Some(p) = k.priority {
        m.insert("priority".into(), json!(p));
    }
    if !k.headers.is_empty() {
        m.insert("headers".into(), Value::Object(k.headers.iter().map(|(a, b)| (a.clone(), json!(b))).collect()));
    }
    if !k.models.is_empty() {
        m.insert("models".into(), model_list(&k.models));
    }
    if !k.excluded_models.is_empty() {
        m.insert("excluded-models".into(), json!(k.excluded_models));
    }
    if !k.scoped_errors.is_empty() {
        let rules = k.scoped_errors.iter().map(|r| json!({"status": r.status, "match": [r.matches], "action": r.action}));
        m.insert("request-scoped-errors".into(), Value::Array(rules.collect()));
    }
}

fn legacy_keys(entries: &[KeyEntry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|k| {
                let mut m = Map::new();
                m.insert("api-key".into(), json!(k.api_key));
                m.insert("base-url".into(), json!(k.base_url));
                key_fields(k, &mut m);
                shared_fields(k, &mut m);
                Value::Object(m)
            })
            .collect(),
    )
}

/// v8 layout: entries with identical shared settings (base-url, prefix, headers, ...) form one
/// group; weights stay key-level.
fn v8_groups(entries: &[KeyEntry], name: &str) -> Value {
    let mut groups: Vec<(Map<String, Value>, Vec<&KeyEntry>)> = vec![];
    for e in entries {
        let mut shared = Map::new();
        shared.insert("base-url".into(), json!(e.base_url));
        shared_fields(e, &mut shared);
        match groups.iter_mut().find(|(s, _)| *s == shared) {
            Some((_, g)) => g.push(e),
            None => groups.push((shared, vec![e])),
        }
    }
    Value::Array(
        groups
            .into_iter()
            .enumerate()
            .map(|(i, (shared, es))| {
                let mut m = Map::new();
                m.insert("name".into(), json!(format!("{name}-{}", i + 1)));
                m.extend(shared);
                let keys = es.iter().map(|e| {
                    let mut key = Map::new();
                    key.insert("api-key".into(), json!(e.api_key));
                    key_fields(e, &mut key);
                    Value::Object(key)
                });
                m.insert("keys".into(), Value::Array(keys.collect()));
                Value::Object(m)
            })
            .collect(),
    )
}

fn compat_value(c: &CompatProvider, keys_field: &str) -> Value {
    let mut m = Map::new();
    m.insert("name".into(), json!(c.name));
    m.insert("base-url".into(), json!(c.base_url));
    if let Some(p) = &c.prefix {
        m.insert("prefix".into(), json!(p));
    }
    let keys: Vec<Value> = c.keys.iter().map(|k| json!({"api-key": k})).collect();
    m.insert(keys_field.into(), Value::Array(keys));
    m.insert("models".into(), model_list(&c.models));
    if !c.headers.is_empty() {
        m.insert("headers".into(), Value::Object(c.headers.iter().map(|(a, b)| (a.clone(), json!(b))).collect()));
    }
    Value::Object(m)
}

impl ConfigSpec {
    pub fn render(&self, layout: Layout, server_port: u16, auth_dir: &Path) -> String {
        let value = match layout {
            Layout::Legacy => self.legacy(server_port, auth_dir),
            Layout::V8 => self.v8(server_port, auth_dir),
        };
        let mut value = value;
        if !self.plugins.is_empty()
            && let Value::Object(root) = &mut value
        {
            root.insert("plugins".into(), self.plugins_value(auth_dir));
        }
        let mut out = String::new();
        emit(&value, 0, &mut out);
        out
    }

    /// The `plugins:` section; the library directory sits next to the auth dir.
    fn plugins_value(&self, auth_dir: &Path) -> Value {
        let dir = auth_dir.parent().unwrap_or(auth_dir).join("plugins");
        let mut configs = Map::new();
        for p in &self.plugins {
            let mut item = Map::new();
            item.insert("enabled".into(), json!(true));
            item.insert("priority".into(), json!(p.priority));
            for (k, v) in &p.settings {
                item.insert((*k).into(), v.clone());
            }
            configs.insert(p.id.into(), Value::Object(item));
        }
        json!({"enabled": true, "dir": dir.to_string_lossy(), "configs": configs})
    }

    fn management(&self) -> Value {
        let mut m = Map::new();
        m.insert("allow-remote".into(), json!(false));
        if let Some(s) = &self.management_secret {
            m.insert("secret-key".into(), json!(s));
        }
        // Never download the control panel from GitHub during tests.
        m.insert("disable-control-panel".into(), json!(true));
        Value::Object(m)
    }

    fn streaming(&self) -> Value {
        json!({"keepalive-seconds": self.keepalive_seconds, "bootstrap-retries": self.bootstrap_retries})
    }

    fn legacy(&self, port: u16, auth_dir: &Path) -> Value {
        let mut m = Map::new();
        m.insert("host".into(), json!("127.0.0.1"));
        m.insert("port".into(), json!(port));
        m.insert("auth-dir".into(), json!(auth_dir.to_string_lossy()));
        m.insert("api-keys".into(), json!(self.client_keys));
        m.insert("remote-management".into(), self.management());
        m.insert("request-retry".into(), json!(self.request_retry));
        m.insert("max-retry-credentials".into(), json!(self.max_retry_credentials));
        m.insert("max-retry-interval".into(), json!(self.max_retry_interval));
        let mut routing = Map::new();
        if let Some(s) = &self.strategy {
            routing.insert("strategy".into(), json!(s));
        }
        if !routing.is_empty() {
            m.insert("routing".into(), Value::Object(routing));
        }
        m.insert("force-model-prefix".into(), json!(self.force_model_prefix));
        m.insert("usage-statistics-enabled".into(), json!(self.usage_statistics));
        // Emitted only when on: the management config goldens predate the key.
        if self.request_log {
            m.insert("request-log".into(), json!(true));
        }
        m.insert("disable-cooling".into(), json!(self.disable_cooling));
        if let Some(t) = self.transient_cooldown_seconds {
            m.insert("transient-error-cooldown-seconds".into(), json!(t));
        }
        m.insert("passthrough-headers".into(), json!(self.passthrough_headers));
        m.insert("nonstream-keepalive-interval".into(), json!(self.nonstream_keepalive));
        m.insert("streaming".into(), self.streaming());
        m.insert("claude-api-key".into(), legacy_keys(&self.claude));
        m.insert("codex-api-key".into(), legacy_keys(&self.codex));
        m.insert("gemini-api-key".into(), legacy_keys(&self.gemini));
        if !self.xai.is_empty() {
            m.insert("xai-api-key".into(), legacy_keys(&self.xai));
        }
        for (key, value) in &self.multimedia {
            m.insert((*key).into(), value.clone());
        }
        if !self.codex_settings.is_empty() {
            m.insert("codex".into(), Value::Object(self.codex_settings.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()));
        }
        m.insert("openai-compatibility".into(), Value::Array(self.compat.iter().map(|c| compat_value(c, "api-key-entries")).collect()));
        Value::Object(m)
    }

    fn v8(&self, port: u16, auth_dir: &Path) -> Value {
        let mut retry = Map::new();
        retry.insert("request-retry".into(), json!(self.request_retry));
        retry.insert("max-retry-credentials".into(), json!(self.max_retry_credentials));
        retry.insert("max-retry-interval".into(), json!(self.max_retry_interval));
        let mut cooldown = Map::new();
        cooldown.insert("disable-cooling".into(), json!(self.disable_cooling));
        if let Some(t) = self.transient_cooldown_seconds {
            cooldown.insert("transient-error-cooldown-seconds".into(), json!(t));
        }
        let mut routing = Map::new();
        if let Some(s) = &self.strategy {
            routing.insert("strategy".into(), json!(s));
        }
        routing.insert("force-model-prefix".into(), json!(self.force_model_prefix));
        routing.insert("retry".into(), Value::Object(retry));
        routing.insert("cooldown".into(), Value::Object(cooldown));
        let mut api_keys = json!({
            "claude": v8_groups(&self.claude, "claude"),
            "codex": v8_groups(&self.codex, "codex"),
            "gemini": v8_groups(&self.gemini, "gemini"),
            "openai-compatibility": self.compat.iter().map(|c| compat_value(c, "keys")).collect::<Vec<_>>(),
        });
        if !self.xai.is_empty() {
            api_keys["xai"] = v8_groups(&self.xai, "xai");
        }
        let multimedia: Map<String, Value> = self.multimedia.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect();
        let mut out = json!({
            "config-version": 8,
            "server": {"host": "127.0.0.1", "port": port},
            "management": self.management(),
            "access": {"api-keys": self.client_keys},
            "routing": routing,
            "requests": {
                "passthrough-headers": self.passthrough_headers,
                "nonstream-keepalive-interval": self.nonstream_keepalive,
                "streaming": self.streaming(),
            },
            "oauth": {"auth-dir": auth_dir.to_string_lossy()},
            "observability": {
                "usage": {"usage-statistics-enabled": self.usage_statistics},
            },
            "api-keys": api_keys,
        });
        if self.request_log {
            out["observability"]["logs"] = json!({"request-log": true});
        }
        if !multimedia.is_empty() {
            out["multimedia"] = Value::Object(multimedia);
        }
        out
    }
}

/// Minimal block-style YAML emitter (strings are always double-quoted JSON strings).
fn emit(v: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                match val {
                    Value::Object(o) if !o.is_empty() => {
                        out.push_str(&format!("{pad}{k}:\n"));
                        emit(val, indent + 2, out);
                    }
                    Value::Array(a) if !a.is_empty() => {
                        out.push_str(&format!("{pad}{k}:\n"));
                        emit(val, indent + 2, out);
                    }
                    _ => out.push_str(&format!("{pad}{k}: {}\n", scalar(val))),
                }
            }
        }
        Value::Array(a) => {
            for item in a {
                match item {
                    Value::Object(o) if !o.is_empty() => {
                        let mut inner = String::new();
                        emit(item, indent + 2, &mut inner);
                        // Replace the first line's indentation with the list dash.
                        let first = inner.replacen(&" ".repeat(indent + 2), &format!("{pad}- "), 1);
                        out.push_str(&first);
                    }
                    _ => out.push_str(&format!("{pad}- {}\n", scalar(item))),
                }
            }
        }
        _ => out.push_str(&format!("{pad}{}\n", scalar(v))),
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::Array(a) if a.is_empty() => "[]".into(),
        Value::Object(o) if o.is_empty() => "{}".into(),
        _ => v.to_string(),
    }
}

//! Host behavior against in-process fake plugins (ported from the Go pluginhost tests that use
//! `newHostWithRecords`): registration, management routes, command-line flags and error
//! envelopes. The fakes speak the JSON RPC of the C ABI without a dynamic library.

use std::collections::HashMap;
use std::sync::Arc;

use cpa_config::{Config, PluginInstanceConfig};
use cpa_plugin::client::{CallbackInstance, RawClient};
use cpa_plugin::host::PluginLoader;
use cpa_plugin::loader::HostCallbacks;
use cpa_plugin::platform::PluginFile;
use cpa_plugin::{CallCtx, Host, PluginError};
use cpa_pluginapi::abi;
use parking_lot::Mutex;
use serde_json::{Value, json};

type Handler = Arc<dyn Fn(&str, &Value) -> Result<Value, PluginError> + Send + Sync>;

struct FakeClient {
    handler: Handler,
    instance: Arc<CallbackInstance>,
}

impl RawClient for FakeClient {
    fn call(&self, method: &str, request: &[u8]) -> Result<Vec<u8>, PluginError> {
        let req: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
        let out = match (self.handler)(method, &req) {
            Ok(result) => json!({"ok": true, "result": result}),
            Err(e) => json!({"ok": false, "error": {"code": e.code, "message": e.message, "http_status": e.status}}),
        };
        Ok(serde_json::to_vec(&out).expect("serializable envelope"))
    }

    fn shutdown(&self) {}

    fn callback_instance(&self) -> Arc<CallbackInstance> {
        self.instance.clone()
    }
}

struct FakeLoader(HashMap<String, Handler>);

impl PluginLoader for FakeLoader {
    fn open(&self, file: &PluginFile, _host: Arc<dyn HostCallbacks>, instance: Arc<CallbackInstance>) -> Result<Arc<dyn RawClient>, PluginError> {
        let handler = self.0.get(&file.id).cloned().ok_or_else(|| PluginError::msg(format!("missing test plugin for {}", file.path.display())))?;
        Ok(Arc::new(FakeClient { handler, instance }))
    }
}

/// One fake plugin: its registration and the handler for everything after it.
struct Fake {
    id: &'static str,
    priority: i64,
    schema_version: u32,
    capabilities: Value,
    handler: Handler,
}

impl Fake {
    fn new(id: &'static str, capabilities: Value, handler: impl Fn(&str, &Value) -> Result<Value, PluginError> + Send + Sync + 'static) -> Self {
        Fake { id, priority: 0, schema_version: abi::SCHEMA_VERSION, capabilities, handler: Arc::new(handler) }
    }

    fn priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }

    fn schema(mut self, version: u32) -> Self {
        self.schema_version = version;
        self
    }

    fn registration(&self) -> Value {
        json!({
            "schema_version": self.schema_version,
            "metadata": {"Name": self.id, "Version": "1.0.0", "Author": "test", "GitHubRepository": "https://github.com/router-for-me/CLIProxyAPI", "Logo": "", "ConfigFields": []},
            "capabilities": self.capabilities,
        })
    }
}

/// A host with `fakes` loaded from a temporary plugin directory.
async fn host_with(fakes: Vec<Fake>) -> (Arc<Host>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut handlers: HashMap<String, Handler> = HashMap::new();
    let mut cfg = Config::default();
    cfg.plugins.enabled = true;
    cfg.plugins.dir = dir.path().to_string_lossy().into_owned();
    for fake in fakes {
        std::fs::write(dir.path().join(format!("{}.so", fake.id)), b"x").expect("plugin file");
        let registration = fake.registration();
        let inner = fake.handler.clone();
        handlers.insert(
            fake.id.to_string(),
            Arc::new(move |method: &str, req: &Value| match method {
                abi::METHOD_PLUGIN_REGISTER | abi::METHOD_PLUGIN_RECONFIGURE => Ok(registration.clone()),
                _ => inner(method, req),
            }),
        );
        cfg.plugins.configs.insert(
            fake.id.to_string(),
            PluginInstanceConfig { enabled: Some(true), priority: fake.priority, raw: serde_yaml_ng::from_str(&format!("priority: {}\n", fake.priority)).expect("yaml") },
        );
    }
    let host = Host::with_loader(Arc::new(FakeLoader(handlers)));
    host.apply_config(&CallCtx::background(), Some(Arc::new(cfg))).await;
    (host, dir)
}

fn management_caps() -> Value {
    json!({"management_api": true})
}

fn route(method: &str, path: &str, extra: Value) -> Value {
    let mut r = json!({"Method": method, "Path": path});
    if let (Value::Object(m), Value::Object(e)) = (&mut r, extra) {
        m.extend(e);
    }
    r
}

fn body_response(body: &str) -> Value {
    json!({"Body": base64(body.as_bytes())})
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn routes_plugin(id: &'static str, routes: Value, resources: Value, answer: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Fake {
    Fake::new(id, management_caps(), move |method, req| match method {
        abi::METHOD_MANAGEMENT_REGISTER => Ok(json!({"Routes": routes, "Resources": resources})),
        abi::METHOD_MANAGEMENT_HANDLE => Ok(answer(req)),
        other => Err(PluginError::msg(format!("unexpected method {other}"))),
    })
}

async fn serve(host: &Arc<Host>, method: &str, path: &str, body: &[u8]) -> Option<cpa_plugin::PluginHttpResponse> {
    host.serve_management_http(&CallCtx::background(), method, path, &Default::default(), &[], body).await
}

fn text(resp: &cpa_plugin::PluginHttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\'', "&#39;").replace('"', "&#34;")
}

// ---- registration ----

#[tokio::test]
async fn registration_sends_the_host_schema_version_and_plugin_config() {
    let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("schema.so"), b"x").unwrap();
    let recorded = seen.clone();
    let handler: Handler = Arc::new(move |method: &str, req: &Value| {
        if method != abi::METHOD_PLUGIN_REGISTER {
            return Err(PluginError::msg(format!("unexpected method {method}")));
        }
        *recorded.lock() = Some(req.clone());
        Ok(json!({"schema_version": abi::SCHEMA_VERSION, "metadata": {"Name": "schema", "Version": "1.0.0", "Author": "test", "GitHubRepository": "https://github.com/router-for-me/CLIProxyAPI"}, "capabilities": {"management_api": true}}))
    });
    let mut cfg = Config::default();
    cfg.plugins.enabled = true;
    cfg.plugins.dir = dir.path().to_string_lossy().into_owned();
    cfg.plugins.configs.insert(
        "schema".into(),
        PluginInstanceConfig { enabled: Some(true), priority: 0, raw: serde_yaml_ng::from_str("mode: test\n").unwrap() },
    );
    let host = Host::with_loader(Arc::new(FakeLoader(HashMap::from([("schema".to_string(), handler)]))));
    host.apply_config(&CallCtx::background(), Some(Arc::new(cfg))).await;

    assert!(host.plugin_registered("schema"));
    let req = seen.lock().clone().expect("register request");
    assert_eq!(req["schema_version"], abi::SCHEMA_VERSION);
    let config_yaml = String::from_utf8(
        // `[]byte` fields are base64 on the wire.
        {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(req["config_yaml"].as_str().expect("config yaml")).unwrap()
        },
    )
    .unwrap();
    assert!(config_yaml.contains("mode: test"), "{config_yaml}");
}

#[tokio::test]
async fn a_future_schema_version_is_rejected() {
    let (host, _dir) = host_with(vec![routes_plugin("future-schema", json!([]), json!([]), |_| json!({})).schema(abi::SCHEMA_VERSION + 1)]).await;
    assert!(!host.plugin_registered("future-schema"));
}

#[tokio::test]
async fn model_router_registers_on_schema_1() {
    let (host, _dir) = host_with(vec![Fake::new("router", json!({"model_router": true}), |_, _| Ok(json!({}))).schema(1)]).await;
    assert!(host.plugin_registered("router"));
    assert!(host.has_model_routers());
}

// ---- management routes ----

#[tokio::test]
async fn reserved_routes_are_skipped_and_priority_decides_conflicts() {
    let high = routes_plugin(
        "high",
        json!([route("GET", "/config", json!({})), route("GET", "/plugins/shared/status", json!({}))]),
        json!([]),
        |req| body_response(&format!("high:{}", req["Path"].as_str().unwrap_or(""))),
    )
    .priority(10);
    let low = routes_plugin(
        "low",
        json!([route("GET", "/plugins/shared/status", json!({})), route("POST", "plugins/low/run", json!({}))]),
        json!([]),
        |_| json!({"StatusCode": 202, "Body": base64(b"low-only")}),
    )
    .priority(1);
    let (host, _dir) = host_with(vec![low, high]).await;
    host.register_management_routes(&CallCtx::background(), &["GET /v0/management/config".to_string()].into()).await;

    let shared = serve(&host, "GET", "/v0/management/plugins/shared/status", b"").await.expect("shared route");
    assert_eq!(text(&shared), "high:/v0/management/plugins/shared/status");
    let run = serve(&host, "POST", "/v0/management/plugins/low/run", b"").await.expect("low route");
    assert_eq!((run.status, text(&run).as_str()), (202, "low-only"));
    assert!(serve(&host, "GET", "/v0/management/config", b"").await.is_none(), "reserved route was served by a plugin");
}

fn json_plugin(id: &'static str, path: &'static str, body: &'static str, schema: u32) -> Fake {
    routes_plugin(
        id,
        json!([route("GET", path, json!({}))]),
        json!([]),
        move |_| json!({"Headers": {"Content-Type": ["application/json; charset=utf-8"]}, "Body": base64(body.as_bytes())}),
    )
    .schema(schema)
}

#[tokio::test]
async fn json_responses_of_legacy_schemas_are_html_escaped() {
    let body = r#"{"title": "<script>alert(1)</script>", "items": ["<b>first</b>", {"description": "safe & sound"}], "count": 1}"#;
    let (host, _dir) = host_with(vec![json_plugin("json", "/plugins/json/status", body, 1)]).await;
    host.register_management_routes(&CallCtx::background(), &Default::default()).await;
    let resp = serve(&host, "GET", "/v0/management/plugins/json/status", b"").await.expect("route");
    assert_eq!(resp.status, 200);
    let got: Value = serde_json::from_slice(&resp.body).expect("json body");
    assert_eq!(got["title"], html_escape("<script>alert(1)</script>"));
    assert_eq!(got["items"][0], html_escape("<b>first</b>"));
    assert_eq!(got["items"][1]["description"], html_escape("safe & sound"));
    assert_eq!(got["count"], 1);
}

#[tokio::test]
async fn schema_6_preserves_raw_json_while_schema_5_escapes_it() {
    let raw = r#"{"prompt":"You are a security auditor. Analyze <input> for vulnerabilities & \"threats\"."}"#;
    let (host, _dir) = host_with(vec![
        json_plugin("raw-json", "/plugins/raw-json/config", raw, abi::SCHEMA_VERSION_RAW_MANAGEMENT_RESPONSE),
        json_plugin("legacy-json", "/plugins/legacy-json/config", raw, abi::SCHEMA_VERSION_STREAM_CHUNK_OMIT_HISTORY),
    ])
    .await;
    host.register_management_routes(&CallCtx::background(), &Default::default()).await;
    let current = serve(&host, "GET", "/v0/management/plugins/raw-json/config", b"").await.expect("raw route");
    assert_eq!(text(&current), raw);
    let legacy = serve(&host, "GET", "/v0/management/plugins/legacy-json/config", b"").await.expect("legacy route");
    assert_eq!(
        text(&legacy),
        r#"{"prompt":"You are a security auditor. Analyze &lt;input&gt; for vulnerabilities &amp; &#34;threats&#34;."}"#
    );
}

#[tokio::test]
async fn a_panicking_handler_fuses_the_plugin_and_answers_502() {
    let panicking = Fake::new("panic", management_caps(), |method, _| match method {
        abi::METHOD_MANAGEMENT_REGISTER => Ok(json!({"Routes": [route("GET", "/plugins/panic", json!({}))]})),
        _ => panic!("boom"),
    });
    let (host, _dir) = host_with(vec![panicking]).await;
    host.register_management_routes(&CallCtx::background(), &Default::default()).await;
    let resp = serve(&host, "GET", "/v0/management/plugins/panic", b"").await.expect("route");
    assert_eq!(resp.status, 502);
    assert!(host.is_plugin_fused("panic"));
}

#[tokio::test]
async fn resources_are_served_under_the_plugin_resource_path() {
    let plugin = routes_plugin(
        "resource",
        json!([]),
        json!([{"Path": "/status", "Menu": "Status", "Description": "Shows plugin status."}]),
        |req| {
            assert_eq!(req["Path"], "/v0/resource/plugins/resource/status");
            json!({"Headers": {"Content-Type": ["text/html; charset=utf-8"]}, "Body": base64(b"<!doctype html><title>resource</title>")})
        },
    );
    let (host, _dir) = host_with(vec![plugin]).await;
    host.register_management_routes(&CallCtx::background(), &Default::default()).await;
    let resp = host
        .serve_resource_http(&CallCtx::background(), "GET", "/v0/resource/plugins/resource/status", &Default::default(), &[])
        .await
        .expect("resource");
    assert_eq!((resp.status, text(&resp).as_str()), (200, "<!doctype html><title>resource</title>"));
    assert_eq!(resp.headers["Content-Type"], vec!["text/html; charset=utf-8".to_string()]);
    assert!(host.serve_resource_http(&CallCtx::background(), "POST", "/v0/resource/plugins/resource/status", &Default::default(), &[]).await.is_none());
}

#[tokio::test]
async fn a_legacy_get_route_with_a_menu_registers_as_a_resource() {
    let plugin = routes_plugin(
        "legacy",
        json!([route("GET", "/plugins/legacy/status", json!({"Menu": "Legacy Status", "Description": "Shows legacy plugin status."}))]),
        json!([]),
        |_| body_response("legacy"),
    );
    let (host, _dir) = host_with(vec![plugin]).await;
    host.register_management_routes(&CallCtx::background(), &Default::default()).await;
    assert!(serve(&host, "GET", "/v0/management/plugins/legacy/status", b"").await.is_none());
    let resource = host
        .serve_resource_http(&CallCtx::background(), "GET", "/v0/resource/plugins/legacy/status", &Default::default(), &[])
        .await
        .expect("resource route");
    assert_eq!(text(&resource), "legacy");
}

#[tokio::test]
async fn registered_plugins_list_only_resource_menus() {
    let plugin = routes_plugin(
        "menu",
        json!([route("GET", "/plugins/menu/hidden", json!({}))]),
        json!([{"Path": "/status", "Menu": "Status", "Description": "Shows plugin status."}]),
        |_| json!({}),
    );
    let (host, _dir) = host_with(vec![plugin]).await;
    host.register_management_routes(&CallCtx::background(), &Default::default()).await;
    let plugins = host.registered_plugins();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].menus.len(), 1);
    let menu = &plugins[0].menus[0];
    assert_eq!((menu.path.as_str(), menu.menu.as_str(), menu.description.as_str()), ("/v0/resource/plugins/menu/status", "Status", "Shows plugin status."));
}

// ---- command line ----

fn cli_plugin(id: &'static str, flags: Value, executions: Arc<Mutex<Vec<Value>>>, response: Value) -> Fake {
    Fake::new(id, json!({"command_line_plugin": true}), move |method, req| match method {
        abi::METHOD_COMMAND_LINE_REGISTER => Ok(json!({"Flags": flags})),
        abi::METHOD_COMMAND_LINE_EXECUTE => {
            executions.lock().push(req.clone());
            Ok(response.clone())
        }
        other => Err(PluginError::msg(format!("unexpected method {other}"))),
    })
}

#[tokio::test]
async fn native_and_reserved_flags_are_skipped_and_priority_owns_shared_flags() {
    let high = cli_plugin(
        "high",
        json!([
            {"Name": "native", "Type": "bool", "Usage": "conflicting native flag"},
            {"Name": "help", "Type": "bool", "Usage": "reserved help flag"},
            {"Name": "h", "Type": "bool", "Usage": "reserved short help flag"},
            {"Name": "shared", "Type": "string", "Usage": "shared flag"},
        ]),
        Default::default(),
        json!({}),
    )
    .priority(10);
    let low = cli_plugin(
        "low",
        json!([{"Name": "shared", "Type": "string", "Usage": "lower priority shared flag"}, {"Name": "low-only", "Type": "int", "Usage": "low priority flag"}]),
        Default::default(),
        json!({}),
    )
    .priority(1);
    let (host, _dir) = host_with(vec![low, high]).await;
    let declared = host.register_command_line_flags(&CallCtx::background(), &["native".to_string()].into()).await;
    let mut names: Vec<&str> = declared.iter().map(|d| d.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["low-only", "shared"]);
    let shared = declared.iter().find(|d| d.name == "shared").expect("shared flag");
    assert_eq!(shared.usage, "shared flag", "the higher priority plugin owns the flag");
}

#[tokio::test]
async fn execution_receives_all_args_and_the_triggered_flags() {
    let executions: Arc<Mutex<Vec<Value>>> = Default::default();
    let plugin = cli_plugin("alpha", json!([{"Name": "plugin-command", "Type": "bool"}]), executions.clone(), json!({}));
    let (host, _dir) = host_with(vec![plugin]).await;
    host.register_command_line_flags(&CallCtx::background(), &Default::default()).await;
    host.set_command_line_flag("plugin-command", "true").unwrap();
    assert!(host.has_triggered_command_line_flags());

    let args = vec!["-plugin-command".to_string(), "tail".to_string()];
    let (code, handled) = host.execute_command_line(&CallCtx::background(), "cliproxy", &args, "/tmp/config.yaml", &[]).await;
    assert_eq!((code, handled), (0, true));
    let seen = executions.lock();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["Program"], "cliproxy");
    assert_eq!(seen[0]["ConfigPath"], "/tmp/config.yaml");
    assert_eq!(seen[0]["Args"], json!(["-plugin-command", "tail"]));
    assert_eq!(seen[0]["TriggeredFlags"]["plugin-command"]["Set"], true);
    assert_eq!(seen[0]["TriggeredFlags"]["plugin-command"]["Value"], "true");
}

#[test]
fn flag_values_are_normalized_like_go() {
    use cpa_plugin::cli::normalize_flag_value as norm;
    assert_eq!(norm("bool", ""), Some("false".into()));
    assert_eq!(norm("bool", "1"), Some("true".into()));
    assert_eq!(norm("bool", "maybe"), None);
    assert_eq!(norm("int", ""), Some("0".into()));
    assert_eq!(norm("int", "12"), Some("12".into()));
    assert_eq!(norm("int", "1.5"), None);
    assert_eq!(norm("duration", "90s"), Some("1m30s".into()));
    assert_eq!(norm("duration", ""), Some("0s".into()));
    assert_eq!(norm("string", " keep "), Some(" keep ".into()));
}

// ---- error envelopes ----

#[test]
fn plugin_http_status_survives_the_error_envelope() {
    let raw = abi::error_envelope("plugin_error", "license required", 403);
    let err = cpa_plugin::client::decode_envelope::<cpa_plugin::client::Empty>(&raw, abi::METHOD_EXECUTOR_EXECUTE_STREAM).unwrap_err();
    assert_eq!((err.message.as_str(), err.status), ("license required", 403));
    for status in [429, 503] {
        let raw = abi::error_envelope("host_call_failed", "synthetic", status);
        let err = cpa_plugin::client::decode_envelope::<cpa_plugin::client::Empty>(&raw, "x").unwrap_err();
        assert_eq!(err.status, status);
    }
}

#[test]
fn envelope_and_payload_keys_match_ignoring_case() {
    let raw = br#"{"OK":true,"Result":{"resources":[{"path":"/status","MENU":"m"}]}}"#;
    let got: cpa_pluginapi::api::ManagementRegistrationResponse = cpa_plugin::client::decode_envelope(raw, "management.register").unwrap();
    assert_eq!((got.resources[0].path.as_str(), got.resources[0].menu.as_str()), ("/status", "m"));
}

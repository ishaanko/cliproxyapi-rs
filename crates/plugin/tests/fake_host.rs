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

// ---- custom formats, executor http_request, routing callbacks ----

use cpa_runtime::conductor::Manager;
use cpa_runtime::executor::{Options, Request};
use cpa_translator::Format;

/// An executor plugin declaring `input`/`output` formats that records every executor call.
fn executor_plugin(id: &'static str, input: &str, output: &str, calls: Arc<Mutex<Vec<(String, Value)>>>) -> Fake {
    let caps = json!({"executor": true, "executor_model_scope": "both", "executor_input_formats": [input], "executor_output_formats": [output]});
    Fake::new(id, caps, move |method, req| {
        calls.lock().push((method.to_string(), req.clone()));
        match method {
            abi::METHOD_EXECUTOR_IDENTIFIER => Ok(json!({"identifier": id})),
            abi::METHOD_EXECUTOR_EXECUTE => Ok(json!({"Payload": base64(br#"{"ok":true}"#), "Headers": {"X-Plugin": ["1"]}})),
            abi::METHOD_EXECUTOR_HTTP_REQUEST => Ok(json!({"StatusCode": 0, "Headers": {"X-Echo": [req["Method"].as_str().unwrap_or_default()]}, "Body": base64(b"hello")})),
            other => Err(PluginError::msg(format!("unexpected method {other}"))),
        }
    })
}

#[tokio::test]
async fn custom_executor_formats_reach_the_plugin_unchanged() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (host, _dir) = host_with(vec![executor_plugin("protoexec", "x-proto", "x-proto", calls.clone())]).await;
    let proto = Format::intern("x-proto").expect("custom format");
    assert!(proto.is_custom());
    let req = Request { model: "m".into(), payload: bytes::Bytes::from_static(b"{}"), format: proto, metadata: Default::default() };
    let resp = host.execute_plugin_executor(&CallCtx::background(), "protoexec", req, Options::new(proto)).await.expect("execute");
    assert_eq!(&resp.payload[..], br#"{"ok":true}"#);
    let calls = calls.lock();
    let exec = calls.iter().find(|(m, _)| m == abi::METHOD_EXECUTOR_EXECUTE).expect("executor.execute call");
    assert_eq!((exec.1["Format"].as_str(), exec.1["SourceFormat"].as_str()), (Some("x-proto"), Some("x-proto")));
}

#[tokio::test]
async fn an_executor_without_a_translator_rejects_a_foreign_input_format() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (host, _dir) = host_with(vec![executor_plugin("protoexec", "x-proto", "x-proto", calls)]).await;
    let req = Request { model: "m".into(), payload: bytes::Bytes::from_static(b"{}"), format: Format::OpenAI, metadata: Default::default() };
    let err = host.execute_plugin_executor(&CallCtx::background(), "protoexec", req, Options::new(Format::OpenAI)).await.unwrap_err();
    assert_eq!(err.message, r#"plugin executor protoexec does not support input format "openai""#);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_custom_output_format_is_translated_by_a_plugin_response_translator() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let caps = json!({"executor": true, "executor_model_scope": "both", "executor_input_formats": ["openai"], "executor_output_formats": ["x-proto"], "response_translator": true});
    let plugin = Fake::new("protoxlate", caps, move |method, req| {
        recorded.lock().push((method.to_string(), req.clone()));
        match method {
            abi::METHOD_EXECUTOR_IDENTIFIER => Ok(json!({"identifier": "protoxlate"})),
            abi::METHOD_EXECUTOR_EXECUTE => Ok(json!({"Payload": base64(b"native-proto")})),
            abi::METHOD_RESPONSE_TRANSLATE => Ok(json!({"Body": base64(br#"{"object":"chat.completion"}"#)})),
            other => Err(PluginError::msg(format!("unexpected method {other}"))),
        }
    });
    let (host, _dir) = host_with(vec![plugin]).await;
    cpa_translator::registry::set_plugin_hooks(Some(Arc::new(cpa_plugin::TranslatorHooks(host.clone()))));
    let req = Request { model: "m".into(), payload: bytes::Bytes::from_static(b"{}"), format: Format::OpenAI, metadata: Default::default() };
    let resp = host.execute_plugin_executor(&CallCtx::background(), "protoxlate", req, Options::new(Format::OpenAI)).await;
    cpa_translator::registry::set_plugin_hooks(None);
    assert_eq!(&resp.expect("execute").payload[..], br#"{"object":"chat.completion"}"#);
    let calls = calls.lock();
    let exec = calls.iter().find(|(m, _)| m == abi::METHOD_EXECUTOR_EXECUTE).expect("executor.execute");
    assert_eq!(exec.1["Format"], "x-proto");
    let xlate = calls.iter().find(|(m, _)| m == abi::METHOD_RESPONSE_TRANSLATE).expect("response.translate");
    assert_eq!((xlate.1["FromFormat"].as_str(), xlate.1["ToFormat"].as_str()), (Some("x-proto"), Some("openai")));
}

#[test]
fn custom_format_names_are_interned_and_builtins_stay_builtin() {
    assert_eq!(Format::intern("claude"), Some(Format::Claude));
    assert_eq!(Format::intern(""), None);
    let (a, b) = (Format::intern("acme").unwrap(), Format::intern("acme").unwrap());
    assert_eq!((a, a.as_str()), (b, "acme"));
    assert_ne!(a, Format::intern("acme2").unwrap());
}

#[tokio::test]
async fn manager_http_request_goes_through_the_plugin_executor() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (host, _dir) = host_with(vec![executor_plugin("httpexec", "chat-completions", "chat-completions", calls.clone())]).await;
    let manager = Arc::new(Manager::new());
    let registry = cpa_core::registry::ModelRegistry::new();
    host.register_executors(&manager, &registry);
    let auth = cpa_auth::Auth::new("a1", "httpexec");
    let mut req = reqwest::Request::new(reqwest::Method::PATCH, "http://example.test/path?q=1".parse().unwrap());
    req.headers_mut().insert("x-in", "v".parse().unwrap());
    *req.body_mut() = Some(reqwest::Body::from("payload"));
    let resp = manager.http_request(&auth, req).await.expect("plugin http request");
    // Status 0 from the plugin means 200.
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["x-echo"], "PATCH");
    assert_eq!(&resp.bytes().await.unwrap()[..], b"hello");
    let calls = calls.lock();
    let sent = calls.iter().find(|(m, _)| m == abi::METHOD_EXECUTOR_HTTP_REQUEST).expect("http_request call");
    assert_eq!(sent.1["URL"], "http://example.test/path?q=1");
    assert_eq!(sent.1["AuthID"], "a1");
    assert_eq!(sent.1["Headers"]["X-In"][0], "v");
    assert_eq!(sent.1["Body"], base64(b"payload"));
}

fn reset_cooldown_call(host: &Arc<Host>, auth_index: &str) -> Result<Value, cpa_plugin::error::HostError> {
    let id = cpa_plugin::callbacks::CbIdentity { plugin_id: "tester".into(), instance: None };
    let body = serde_json::to_vec(&json!({"auth_index": auth_index})).unwrap();
    let raw = futures_executor_block(host.call_from_plugin_async(&id, abi::METHOD_HOST_ROUTING_RESET_COOLDOWN, &body))?;
    let envelope: Value = serde_json::from_slice(&raw).unwrap();
    Ok(envelope["result"].clone())
}

fn futures_executor_block<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

#[tokio::test(flavor = "multi_thread")]
async fn host_routing_reset_cooldown_clears_a_credential() {
    let (host, _dir) = host_with(vec![]).await;
    let manager = Arc::new(Manager::new());
    host.set_auth_manager(Some(manager.clone()));
    let mut auth = cpa_auth::Auth::new("claude-a.json", "claude");
    let next = chrono::Utc::now() + chrono::Duration::hours(65);
    auth.status = cpa_auth::Status::Error;
    auth.unavailable = true;
    auth.next_retry_after = Some(next);
    auth.quota = cpa_auth::types::QuotaState { exceeded: true, reason: "credential_quota".into(), next_recover_at: Some(next), backoff_level: 1, ..Default::default() };
    let mut registered = manager.register(auth).await.expect("register");
    let index = registered.ensure_index();

    let result = reset_cooldown_call(&host, &index).expect("reset");
    assert_eq!(result["auth_index"], index.as_str());
    let updated = manager.get("claude-a.json").expect("auth still registered");
    assert!(!updated.unavailable && !updated.quota.exceeded);
    assert_eq!(updated.status, cpa_auth::Status::Active);

    // Unknown and empty indexes fail like Go's `authByIndex`.
    assert!(reset_cooldown_call(&host, "missing").is_err());
    assert!(reset_cooldown_call(&host, "").is_err());
    host.set_auth_manager(None);
    assert!(reset_cooldown_call(&host, "any").is_err());
}

// ---- host HTTP bridge: header profile and request-log capture ----

/// A one-shot HTTP server: answers `ok` and hands back the raw request head it received.
async fn one_shot_server() -> (u16, tokio::sync::oneshot::Receiver<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut conn, _)) = listener.accept().await else { return };
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match conn.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
    });
    (port, rx)
}

// Go: TestHostHTTPClientAppliesWireProfile, through the `host.http.do` callback.
#[tokio::test]
async fn host_http_header_profile_reaches_the_wire_in_order() {
    let (host, _dir) = host_with(vec![]).await;
    let (port, head) = one_shot_server().await;
    let request = json!({
        "Method": "GET",
        "URL": format!("http://127.0.0.1:{port}/test"),
        "Headers": {"X-Custom-A": ["value-a"], "x-custom-b": ["value-b"], "User-Agent": ["test-agent"]},
        "wire_profile": {"http1_only": true, "disable_auto_compression": true, "header_profile": ["x-custom-b", "X-Custom-A", "User-Agent", "Host"]},
    });
    let id = cpa_plugin::callbacks::CbIdentity { plugin_id: "tester".into(), instance: None };
    let raw = host.call_from_plugin_async(&id, abi::METHOD_HOST_HTTP_DO, &serde_json::to_vec(&request).unwrap()).await.expect("host.http.do");
    let envelope: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(envelope["result"]["StatusCode"], 200);
    assert_eq!(envelope["result"]["Body"], base64(b"ok"));
    let head = head.await.expect("request head");
    let lines: Vec<&str> = head.split("\r\n\r\n").next().unwrap().split("\r\n").collect();
    assert_eq!(
        lines,
        ["GET /test HTTP/1.1", "x-custom-b: value-b", "X-Custom-A: value-a", "User-Agent: test-agent", &format!("Host: 127.0.0.1:{port}"), "Connection: close"]
    );
}

// Go: the bridge's `recordHTTPRequest` / `RecordAPIResponseMetadata` / `AppendAPIResponseChunk`.
#[tokio::test]
async fn host_http_calls_land_in_the_inbound_requests_upstream_log() {
    use cpa_plugin::httpclient::HostHttpClient;
    use cpa_pluginapi::api::HttpRequest;
    use cpa_runtime::apilog::{ApiLog, ApiLogHandle};

    let (port, _head) = one_shot_server().await;
    let log = Arc::new(ApiLog::default());
    let ctx = CallCtx::background().with_api_log(ApiLogHandle::new(log.clone()));
    let cfg = Config { request_log: true, ..Config::default() };
    let client = HostHttpClient { cfg: Some(Arc::new(cfg)), auth: None, request_proxy_url: String::new() };
    let req = HttpRequest {
        method: "POST".into(),
        url: format!("http://127.0.0.1:{port}/log"),
        headers: [("X-Test".to_string(), vec!["v".to_string()])].into(),
        body: b"{\"q\":1}".to_vec(),
        wire_profile: None,
    };
    let resp = client.do_request(&ctx, req).await.expect("request");
    assert_eq!((resp.status_code, resp.body.as_slice()), (200, &b"ok"[..]));
    let request = String::from_utf8(log.api_request()).unwrap();
    assert!(request.contains("=== API REQUEST 1 ===") && request.contains(&format!("Upstream URL: http://127.0.0.1:{port}/log")), "{request}");
    assert!(request.contains("HTTP Method: POST") && request.contains("X-Test: v") && request.contains("{\"q\":1}"), "{request}");
    let response = String::from_utf8(log.api_response()).unwrap();
    assert!(response.contains("Status: 200") && response.trim_end().ends_with("ok"), "{response}");

    // A failed exchange is logged as an error entry.
    let refused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = refused.local_addr().unwrap().port();
    drop(refused);
    let failed = client.do_request(&ctx, HttpRequest { url: format!("http://127.0.0.1:{dead}/"), ..Default::default() }).await.unwrap_err();
    assert!(failed.message.starts_with("execute host http request:"), "{}", failed.message);
    assert!(String::from_utf8(log.api_response()).unwrap().contains("Error: execute host http request:"));
}

#[test]
fn envelope_and_payload_keys_match_ignoring_case() {
    let raw = br#"{"OK":true,"Result":{"resources":[{"path":"/status","MENU":"m"}]}}"#;
    let got: cpa_pluginapi::api::ManagementRegistrationResponse = cpa_plugin::client::decode_envelope(raw, "management.register").unwrap();
    assert_eq!((got.resources[0].path.as_str(), got.resources[0].menu.as_str()), ("/status", "m"));
}

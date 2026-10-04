//! Ports of the Go handler and Responses-route tests for plugin executor usage
//! (`handlers_plugin_executor_usage_test.go`, `openai_responses_plugin_route_test.go`): a model
//! router sends `/v1/responses` to an executor plugin and the request's usage record carries the
//! Responses top-level `usage` tokens and `service_tier` (upstream 8fbf152).

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use base64::Engine;
use cpa_auth::{FileTokenStore, OAuthSessions};
use cpa_config::{Config, PluginInstanceConfig};
use cpa_plugin::client::{CallbackInstance, RawClient};
use cpa_plugin::host::PluginLoader;
use cpa_plugin::loader::HostCallbacks;
use cpa_plugin::platform::PluginFile;
use cpa_plugin::{CallCtx, Host, PluginError};
use cpa_pluginapi::abi;
use cpa_runtime::conductor::Manager;
use cpa_runtime::usage::{UsageListener, UsageRecord, UsageTracker};
use cpa_server::{AppState, build_router};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::watch;
use tower::ServiceExt;

const PLUGIN_ID: &str = "commandcode";
const MODEL: &str = "commandcode/deepseek/deepseek-v4.1-flash";
const RESPONSE: &str = r#"{"id":"resp_1","object":"response","service_tier":"default","usage":{"input_tokens":34,"output_tokens":499,"total_tokens":533}}"#;

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
        Ok(serde_json::to_vec(&out).expect("envelope"))
    }

    fn shutdown(&self) {}

    fn callback_instance(&self) -> Arc<CallbackInstance> {
        self.instance.clone()
    }
}

struct FakeLoader(Handler);

impl PluginLoader for FakeLoader {
    fn open(&self, _file: &PluginFile, _host: Arc<dyn HostCallbacks>, instance: Arc<CallbackInstance>) -> Result<Arc<dyn RawClient>, PluginError> {
        Ok(Arc::new(FakeClient { handler: self.0.clone(), instance }))
    }
}

/// Collects every usage record the tracker sees.
#[derive(Default)]
struct Records(Mutex<Vec<UsageRecord>>);

impl UsageListener for Records {
    fn handle_usage(&self, record: &UsageRecord) {
        self.0.lock().push(record.clone());
    }
}

struct Fixture {
    router: axum::Router,
    records: Arc<Records>,
    /// `Stream` flags of the `model.route` requests the plugin saw.
    routed_stream: Arc<Mutex<Vec<bool>>>,
}

/// A router plugin that sends every request to its own executor, which answers `payload`.
async fn fixture(payload: &'static str) -> Fixture {
    let routed_stream = Arc::new(Mutex::new(Vec::new()));
    let seen = routed_stream.clone();
    let registration = json!({
        "schema_version": abi::SCHEMA_VERSION,
        "metadata": {"Name": PLUGIN_ID, "Version": "1.0.0", "Author": "test", "GitHubRepository": "https://github.com/router-for-me/CLIProxyAPI", "Logo": "", "ConfigFields": []},
        "capabilities": {
            "model_router": true, "executor": true, "executor_model_scope": "both",
            "executor_input_formats": ["openai-response"], "executor_output_formats": ["openai-response"],
        },
    });
    let handler: Handler = Arc::new(move |method: &str, req: &Value| match method {
        abi::METHOD_PLUGIN_REGISTER | abi::METHOD_PLUGIN_RECONFIGURE => Ok(registration.clone()),
        abi::METHOD_EXECUTOR_IDENTIFIER => Ok(json!({"identifier": PLUGIN_ID})),
        abi::METHOD_MODEL_ROUTE => {
            seen.lock().push(req["Stream"].as_bool().unwrap_or(false));
            Ok(json!({"Handled": true, "TargetKind": "executor", "Target": PLUGIN_ID}))
        }
        abi::METHOD_EXECUTOR_EXECUTE => Ok(json!({"Payload": base64::engine::general_purpose::STANDARD.encode(payload)})),
        other => Err(PluginError::msg(format!("unexpected method {other}"))),
    });

    let plugin_dir = tempfile::tempdir().expect("plugin dir").keep();
    std::fs::write(plugin_dir.join(format!("{PLUGIN_ID}.so")), b"x").expect("plugin file");
    let mut cfg = Config { api_keys: vec!["k1".into()], ..Config::default() };
    cfg.plugins.enabled = true;
    cfg.plugins.dir = plugin_dir.to_string_lossy().into_owned();
    cfg.plugins.configs.insert(
        PLUGIN_ID.into(),
        PluginInstanceConfig { enabled: Some(true), priority: 0, raw: serde_yaml_ng::from_str("priority: 0\n").expect("yaml") },
    );
    let cfg = Arc::new(cfg);
    let host = Host::with_loader(Arc::new(FakeLoader(handler)));
    host.apply_config(&CallCtx::background(), Some(cfg.clone())).await;
    assert!(host.plugin_registered(PLUGIN_ID));

    let usage = Arc::new(UsageTracker::default());
    let records = Arc::new(Records::default());
    usage.register_listener("test", records.clone());
    let (_tx, cfg_rx) = watch::channel(cfg);
    std::mem::forget(_tx);
    let store_dir = tempfile::tempdir().expect("store dir").keep();
    let mut state = AppState::new(
        cfg_rx,
        Arc::new(Manager::default()),
        Arc::new(FileTokenStore::with_dir(&store_dir)),
        Arc::new(OAuthSessions::default()),
        usage,
    );
    state.plugins = Some(host);
    Fixture { router: build_router(state), records, routed_stream }
}

async fn post_responses(f: &Fixture, body: &str) -> (u16, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("x-api-key", "k1")
        .body(Body::from(body.to_string()))
        .expect("request");
    let resp = f.router.clone().oneshot(req).await.expect("infallible");
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// `TestHandlerPluginExecutorPublishesUsageNonStreamOpenAIResponseWithServiceTier` and
/// `TestResponsesWithoutStreamParsesPluginUsage`: the top-level Responses usage is not lost when
/// `service_tier` sits next to it, and the router is told the request is not streaming.
#[tokio::test]
async fn responses_route_publishes_top_level_usage_and_service_tier() {
    let f = fixture(RESPONSE).await;
    let body = format!(r#"{{"model":"{MODEL}","input":[{{"role":"user","type":"message","content":"Write me a poem"}}]}}"#);
    let (status, out) = post_responses(&f, &body).await;
    assert_eq!(status, 200, "{out}");
    assert!(out.contains(r#""total_tokens":533"#), "{out}");
    assert_eq!(*f.routed_stream.lock(), [false], "router saw stream=true for a body without stream");

    let records = f.records.0.lock();
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record.provider, PLUGIN_ID);
    assert!(!record.stream, "usage record stream = true, want false");
    let detail = record.detail();
    assert_eq!((detail.input_tokens, detail.output_tokens, detail.total_tokens), (34, 499, 533), "{detail:?}");
    assert_eq!(record.extra.response_service_tier, "default");
}

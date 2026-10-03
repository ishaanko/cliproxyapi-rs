//! Home-mode service against a mock Home: config over the subscription, dispatch through the
//! published bundle, usage and release reporting, and subscriber replacement after heartbeat loss.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::registry::ModelRegistry;
use cpa_home::testing::{self as t, MockHome, Reply};
use cpa_runtime::executor::{ExecError, Executor, Options, Request, Response, StreamResult};
use cpa_runtime::service::ServiceBuilder;
use cpa_translator::Format;
use serde_json::json;

const HOME_CONFIG: &str = "request-retry: 7\ncredential-concurrency:\n  lifecycle-config-revision: 1\n  cpa-heartbeat-timeout: 1s\n  release-flush-interval: 20ms\n  release-max-backoff: 100ms\n";

/// The queue and the current Home client are process-wide: the tests take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Echo {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Executor for Echo {
    fn identifier(&self) -> &str {
        "mock"
    }

    async fn execute(&self, auth: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Response { payload: Bytes::from(format!("served-by-{}", auth.id)), ..Default::default() })
    }

    async fn execute_stream(&self, _: &Auth, _: Request, _: Options) -> Result<StreamResult, ExecError> {
        unreachable!()
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        Ok(auth.clone())
    }

    async fn count_tokens(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
        unreachable!()
    }
}

fn dispatch() -> Reply {
    let body = json!({
        "model": "m",
        "auth_index": "auth-1",
        "auth": {"id": "auth-1", "provider": "mock", "status": "active"},
        "concurrency": {"accounted": true, "credential_id": "auth-1", "model": "m"},
    });
    t::bulk(body.to_string())
}

fn mock_home(heartbeat_timeout_config: &'static str) -> impl Fn(&[String]) -> Reply + Send + Sync + 'static {
    move |args| match args[0].to_lowercase().as_str() {
        "ping" => t::raw("+PONG\r\n"),
        "get" if args[1] == "config" => t::bulk(heartbeat_timeout_config),
        "subscribe" => t::subscribe_ack("config", 1),
        "rpop" => dispatch(),
        "lpush" => t::int(1),
        _ => t::err("ERR unknown command"),
    }
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn home_config(port: u16) -> Config {
    let mut cfg = Config::default();
    cfg.home.enabled = true;
    cfg.home.host = "127.0.0.1".into();
    cfg.home.port = i64::from(port);
    cfg.home.node_id = "node-1".into();
    cfg.host = "127.0.0.1".into();
    cfg.port = 18317;
    cpa_runtime::service::force_home_runtime_config(&mut cfg);
    cfg
}

async fn start(mock: &MockHome, calls: Arc<AtomicUsize>) -> cpa_runtime::service::Service {
    let dir = tempfile::tempdir().unwrap();
    let registry: &'static ModelRegistry = Box::leak(Box::new(ModelRegistry::new()));
    let service = ServiceBuilder::new(dir.path().join("config.yaml"))
        .dotenv_dir(None)
        .registry(registry)
        .antigravity_probe(false)
        .executor(Arc::new(Echo { calls }))
        .initial_config(home_config(mock.port()))
        .build()
        .unwrap();
    std::mem::forget(dir);
    service.start().await.unwrap();
    service
}

#[tokio::test]
async fn home_service_applies_config_dispatches_and_reports() {
    let _serial = SERIAL.lock().await;
    let mock = MockHome::start(mock_home(HOME_CONFIG)).await;
    let calls = Arc::new(AtomicUsize::new(0));
    cpa_home::queue::set_enabled(true);
    let service = start(&mock, calls.clone()).await;
    cpa_runtime::usage_queue::install(&service.usage());
    let manager = service.manager();
    wait_for("dispatch bundle", || manager.home_dispatch_bundle().is_some()).await;

    // The remote config replaced the local one but kept the listener and Home settings.
    let cfg = service.config();
    assert_eq!((cfg.request_retry, cfg.port, cfg.host.as_str()), (7, 18317, "127.0.0.1"));
    assert!(cfg.home.enabled && cfg.usage_statistics_enabled && cfg.disable_cooling && !cfg.save_cooldown_status);

    let resp = manager
        .execute(&["mock".to_string()], Request { model: "m".into(), payload: Bytes::from_static(b"{}"), format: Format::OpenAI, metadata: Default::default() }, Options::new(Format::OpenAI))
        .await
        .unwrap();
    assert_eq!(resp.payload, "served-by-auth-1");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Dispatch key, usage forwarding and the cumulative release all reached Home.
    wait_for("usage and release", || mock.count("lpush", Some("usage")) >= 1 && mock.count("lpush", Some("concurrency-release")) >= 1).await;
    let rpop = mock.commands().into_iter().find(|c| c[0].eq_ignore_ascii_case("rpop")).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&rpop[1]).unwrap()["model"], "m");
    let usage = mock.commands().into_iter().find(|c| c[0].eq_ignore_ascii_case("lpush") && c[1] == "usage").unwrap();
    let record: serde_json::Value = serde_json::from_str(&usage[2]).unwrap();
    assert_eq!((record["provider"].clone(), record["auth_index"].clone(), record["failed"].clone()), (json!("mock"), json!("auth-1"), json!(false)));
    let release = mock.commands().into_iter().find(|c| c[0] == "LPUSH" && c[1] == "concurrency-release").unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&release[2]).unwrap(), json!({"credential_id": "auth-1", "model": "m", "release_seq": 1}));
    // In-flight snapshots are published periodically.
    wait_for("in-flight snapshot", || mock.count("lpush", Some("in-flight-snapshot")) >= 1).await;
    service.shutdown();
    cpa_home::queue::set_enabled(false);
}

// Go: Service.Shutdown cancels the supervisor and waits: the registry drains and pending releases
// are flushed to Home before the client closes.
#[tokio::test]
async fn graceful_shutdown_flushes_releases_and_detaches_the_lifetime() {
    let _serial = SERIAL.lock().await;
    let mock = MockHome::start(mock_home(HOME_CONFIG)).await;
    let service = start(&mock, Arc::new(AtomicUsize::new(0))).await;
    let manager = service.manager();
    wait_for("dispatch bundle", || manager.home_dispatch_bundle().is_some()).await;
    let request = Request { model: "m".into(), payload: Bytes::from_static(b"{}"), format: Format::OpenAI, metadata: Default::default() };
    manager.execute(&["mock".to_string()], request, Options::new(Format::OpenAI)).await.unwrap();
    service.shutdown_home().await;
    assert!(mock.count("lpush", Some("concurrency-release")) >= 1, "release was not flushed before shutdown");
    assert!(manager.home_dispatch_bundle().is_none() && cpa_home::kv::current().is_none());
    service.shutdown();
}

#[tokio::test]
async fn heartbeat_loss_replaces_the_subscriber_lifetime() {
    let _serial = SERIAL.lock().await;
    // 150 ms heartbeat: the mock never pushes, so every lifetime ends by timeout.
    const SHORT: &str = "credential-concurrency:\n  lifecycle-config-revision: 1\n  cpa-heartbeat-timeout: 150ms\n  cpa-cancel-bound: 500ms\n  reclaim-grace: 10s\n  cleanup-interval: 1s\n  release-flush-interval: 20ms\n  release-max-backoff: 100ms\n";
    let mock = MockHome::start(mock_home(SHORT)).await;
    let service = start(&mock, Arc::new(AtomicUsize::new(0))).await;
    wait_for("second config fetch", || mock.count("get", Some("config")) >= 2).await;
    // The replacement lifetime subscribes again, with takeover once membership was acknowledged.
    wait_for("second subscribe", || mock.count("subscribe", None) >= 2).await;
    let subscribes: Vec<Vec<String>> = mock.commands().into_iter().filter(|c| c[0].eq_ignore_ascii_case("subscribe")).collect();
    assert_eq!(subscribes[0][..3], ["subscribe", "config", "1"]);
    assert!(subscribes[1].iter().any(|a| a == "takeover"), "{:?}", subscribes[1]);
    // Same membership identity across lifetimes.
    assert_eq!(subscribes[0].last(), subscribes[1].last());
    service.shutdown();
}

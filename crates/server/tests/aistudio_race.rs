//! A page that connects and drops at once must not leave a stale runtime-only credential: the
//! connect and disconnect events are applied in order.

use std::sync::Arc;
use std::time::Duration;

use cpa_auth::OAuthSessions;
use cpa_executors::gemini::wsrelay;
use cpa_runtime::service::ServiceBuilder;
use cpa_runtime::usage::UsageTracker;
use cpa_server::{AppState, aistudio, build_router};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[tokio::test]
async fn connect_then_immediate_drop_leaves_no_credential() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.yaml");
    let auth_dir = dir.path().join("auth");
    std::fs::create_dir_all(&auth_dir).expect("auth dir");
    std::fs::write(&config, format!("host: 127.0.0.1\nport: 0\nauth-dir: {}\napi-keys:\n  - k1\n", auth_dir.display())).expect("config");

    let service = Arc::new(ServiceBuilder::new(&config).watch(false).dotenv_dir(None).build().expect("service"));
    service.start().await.expect("start");
    aistudio::install_relay_hooks(&service);

    let state = AppState::new(
        service.subscribe_config(),
        service.manager(),
        service.store(),
        Arc::new(OAuthSessions::default()),
        Arc::new(UsageTracker::default()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = build_router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    for _ in 0..20 {
        let mut upgrade = format!("ws://{addr}/v1/ws").into_client_request().expect("upgrade request");
        upgrade.headers_mut().insert("x-api-key", "k1".parse().expect("header"));
        let (page, _) = tokio_tungstenite::connect_async(upgrade).await.expect("connect");
        drop(page);
    }

    let manager = service.manager();
    for _ in 0..200 {
        if wsrelay::global().connected().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Let the ordered queue drain after the last disconnect.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stale: Vec<String> = manager.list().into_iter().filter(|a| a.provider == "aistudio").map(|a| a.id).collect();
    assert!(stale.is_empty(), "stale runtime credentials: {stale:?}");
}

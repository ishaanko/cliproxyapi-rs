//! Port of Go `TestV8SharedUpstreamRequestSettingsAffectBothAuthKinds` (oauth_scope_test.go): the
//! shared `upstream:` / `client:` settings reach requests of API-key and OAuth credentials alike,
//! and the executors never mutate the shared config.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::any;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_auth::types::{AUTH_KIND_API_KEY, AUTH_KIND_OAUTH};
use cpa_runtime::executor::{DynExecutor, Options, Request};
use cpa_translator::Format;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::watch;

const SHARED_CONFIG: &str = "proxy-url: direct
disable-image-generation: true
client: {codex: {optimize-multi-agent-v2: true}}
upstream:
  codex: {orphan-delegation-compatibility: true}
  xai: {inject-x-search: true}
";

const PAYLOAD: &str = r#"{
  "input":[{"type":"function_call_output","name":"create_thread","namespace":"codex_app","output":"<codex_delegation>task</codex_delegation>"}],
  "tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}]}]
}"#;

/// Serves 400 for every request and records the request bodies.
async fn rejecting_upstream() -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let app = Router::new().fallback(any(move |body: Bytes| {
        let sink = sink.clone();
        async move {
            sink.lock().push(body.to_vec());
            (StatusCode::BAD_REQUEST, r#"{"error":{"message":"captured request"}}"#)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, seen)
}

fn auth_of(provider: &str, kind: &str, base_url: &str) -> Auth {
    let mut auth = Auth::new(format!("{provider}-{kind}"), provider);
    auth.proxy_url = "direct".into();
    auth.attributes.insert("auth_kind".into(), kind.into());
    auth.attributes.insert("base_url".into(), base_url.into());
    if kind == AUTH_KIND_API_KEY {
        auth.attributes.insert("api_key".into(), "test-key".into());
    } else {
        auth.metadata.insert("access_token".into(), json!("test-token"));
    }
    auth
}

#[tokio::test]
async fn shared_upstream_request_settings_affect_both_auth_kinds() {
    let cfg = Arc::new(cpa_config::parse_config_bytes(SHARED_CONFIG.as_bytes()).expect("config"));
    for provider in ["codex", "xai"] {
        for kind in [AUTH_KIND_API_KEY, AUTH_KIND_OAUTH] {
            for stream in [false, true] {
                let (url, seen) = rejecting_upstream().await;
                let (_tx, rx) = watch::channel(cfg.clone());
                let unscoped: DynExecutor = match provider {
                    "codex" => cpa_executors::codex::new(rx),
                    _ => cpa_executors::xai::new(rx),
                };
                // The conductor runs API-key credentials on the executor's scoped view.
                let executor = if kind == AUTH_KIND_API_KEY { unscoped.for_api_key().expect("scoped executor") } else { unscoped };
                let auth = auth_of(provider, kind, &url);
                let req = Request { model: "gpt-5.4".into(), payload: Bytes::from(PAYLOAD), format: Format::OpenAIResponse, metadata: Default::default() };
                let mut opts = Options::new(Format::OpenAIResponse);
                opts.stream = stream;
                opts.headers.insert("user-agent", "codex_cli_rs/0.144.1".parse().expect("header"));
                opts.headers.insert("x-openai-subagent", "collab_spawn".parse().expect("header"));
                let rejected = if stream {
                    executor.execute_stream(&auth, req, opts).await.is_err()
                } else {
                    executor.execute(&auth, req, opts).await.is_err()
                };
                let case = format!("{provider}/{kind}/stream={stream}");
                assert!(rejected, "{case}: expected the test upstream's rejection");
                let body: Value = serde_json::from_slice(seen.lock().first().unwrap_or_else(|| panic!("{case}: request did not reach upstream")))
                    .expect("upstream body json");
                assert_eq!(body["input"][0]["type"], "message", "{case}: shared orphan delegation; body={body}");
                if provider == "codex" {
                    assert_eq!(body["tools"][0]["name"], "collaboration-optimize", "{case}: client collaboration namespace");
                } else {
                    let has_x_search = body["tools"].as_array().is_some_and(|t| t.iter().any(|tool| tool["type"] == "x_search"));
                    assert!(has_x_search, "{case}: shared x_search setting was not applied; body={body}");
                }
                assert!(cfg.client.codex.optimize_multi_agent_v2 && cfg.codex.orphan_delegation_compatibility && cfg.xai.inject_x_search, "{case}: request changed shared configuration");
            }
        }
    }
}

//! Ports of the Home client tests (`internal/home/client_test.go`) against a mock Home.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use cpa_config::{CredentialConcurrencyConfig, GoDuration, HomeConfig};
use cpa_home::client::{
    ClusterNode, DispatchParams, KvSetOptions, RECOVERY_STABLE, RECOVERY_SWITCHING,
    RECOVERY_SWITCHING_TAKEOVER, RECOVERY_TAKEOVER_ELIGIBLE, build_kv_set_args,
    headers_to_lower_map, new_auth_dispatch_request, query_to_lower_map,
};
use cpa_home::conn::Kill;
use cpa_home::error::HomeError;
use cpa_home::requests::ConcurrencyReleaseFrame;
use cpa_home::testing::{self as t, MockHome, Reply};
use cpa_home::{Cancel, Client};
use http::HeaderMap;
use serde_json::{Value, json};

fn cfg(host: &str, port: i64) -> HomeConfig {
    HomeConfig { enabled: true, host: host.into(), port: port as _, ..Default::default() }
}

fn client_for(mock: &MockHome) -> Client {
    let c = Client::new(cfg("127.0.0.1", i64::from(mock.port())));
    c.set_test_operation_timeout(Duration::from_secs(2));
    c
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
    }
    h
}

fn request_json(p: &DispatchParams<'_>) -> Value {
    serde_json::to_value(new_auth_dispatch_request(p)).unwrap()
}

#[test]
fn dispatch_request_defaults_and_optional_fields() {
    let base = DispatchParams { model: "gpt-5", count: 0, ..Default::default() };
    assert_eq!(
        request_json(&base),
        json!({"type": "auth", "model": "gpt-5", "count": 1, "concurrency_protocol": 1})
    );

    let p = DispatchParams {
        model: "gpt-5",
        count: 3,
        session_id: " s1 ",
        parent_session_id: "p1",
        credential_policy: "codex_alpha_search_v1",
        pinned_auth_id: "auth-1",
        headers: headers(&[("X-Node-Kind", " fork "), ("X-Api-Key", " k ")]),
        ..Default::default()
    };
    let v = request_json(&p);
    assert_eq!(v["count"], 3);
    assert_eq!(v["session_id"], "s1");
    assert_eq!(v["parent_session_id"], "p1");
    assert_eq!(v["node_kind"], "fork");
    assert_eq!(v["credential_policy"], "codex_alpha_search_v1");
    assert_eq!(v["pinned_auth_id"], "auth-1");
    assert_eq!(v["headers"], json!({"x-api-key": "k", "x-node-kind": "fork"}));
    assert!(v.get("excluded_auth_ids").is_none() && v.get("retry_round").is_none());
}

#[test]
fn excluded_auth_ids_pin_count_to_one_and_empty_list_is_still_sent() {
    let p = DispatchParams {
        model: "m",
        count: 4,
        excluded_auth_ids: Some(vec!["a".into(), "b".into()]),
        ..Default::default()
    };
    let v = request_json(&p);
    assert_eq!((v["count"].clone(), v["excluded_auth_ids"].clone()), (json!(1), json!(["a", "b"])));
    let p = DispatchParams { model: "m", count: 4, excluded_auth_ids: Some(vec![]), ..Default::default() };
    let v = request_json(&p);
    assert_eq!((v["count"].clone(), v["excluded_auth_ids"].clone()), (json!(1), json!([])));
}

#[test]
fn retry_round_distinguishes_legacy_protocol_and_clamps_negative_rounds() {
    let legacy = request_json(&DispatchParams { model: "m", ..Default::default() });
    assert!(legacy.get("retry_round").is_none());
    let zero = request_json(&DispatchParams { model: "m", retry_round: Some(0), ..Default::default() });
    assert_eq!(zero["retry_round"], 0);
    let negative = request_json(&DispatchParams { model: "m", retry_round: Some(-4), ..Default::default() });
    assert_eq!(negative["retry_round"], 0);
}

#[test]
fn header_and_query_maps_are_lowercased_trimmed_and_joined() {
    let h = headers(&[("X-A", " 1 "), ("x-a", "2"), ("Content-Type", "json")]);
    assert_eq!(
        headers_to_lower_map(&h),
        BTreeMap::from([("content-type".into(), "json".into()), ("x-a".into(), "1, 2".into())])
    );
    let q = vec![("Key".into(), " v ".into()), ("key".into(), "w".into()), (" ".into(), "x".into())];
    assert_eq!(query_to_lower_map(&q), BTreeMap::from([("key".into(), "v, w".into())]));
}

#[test]
fn kv_set_args_follow_go() {
    let a = build_kv_set_args("key", b"value", KvSetOptions { ex: Duration::from_secs(2), nx: true, ..Default::default() }).unwrap();
    assert_eq!(a, vec![b"key".to_vec(), b"value".to_vec(), b"EX".to_vec(), b"2".to_vec(), b"NX".to_vec()]);
    let a = build_kv_set_args("key", b"value", KvSetOptions { px: Duration::from_millis(1500), xx: true, ..Default::default() }).unwrap();
    assert_eq!(a, vec![b"key".to_vec(), b"value".to_vec(), b"PX".to_vec(), b"1500".to_vec(), b"XX".to_vec()]);
    assert!(build_kv_set_args("key", b"v", KvSetOptions { ex: Duration::from_secs(1), px: Duration::from_millis(1), ..Default::default() }).is_err());
    assert!(build_kv_set_args("key", b"v", KvSetOptions { nx: true, xx: true, ..Default::default() }).is_err());
    assert!(build_kv_set_args("  ", b"v", KvSetOptions::default()).is_err());
    // Sub-unit TTLs round up.
    let a = build_kv_set_args("k", b"v", KvSetOptions { ex: Duration::from_millis(1), ..Default::default() }).unwrap();
    assert_eq!(a[3], b"1");
}

#[tokio::test]
async fn get_config_continues_after_cluster_discovery_response_errors() {
    for response in ["-ERR cluster command unsupported\r\n", ":1\r\n"] {
        let mock = MockHome::start(move |args| match args {
            [a, b, ..] if a.eq_ignore_ascii_case("CLUSTER") && b.eq_ignore_ascii_case("NODES") => t::raw(response),
            [a, k, ..] if a.eq_ignore_ascii_case("GET") && k == "config" => t::bulk("host: 127.0.0.1\n"),
            _ => t::err("ERR unexpected command"),
        })
        .await;
        let c = client_for(&mock);
        assert_eq!(c.get_config().await.unwrap(), b"host: 127.0.0.1\n");
        assert_eq!(mock.count("CLUSTER", Some("NODES")), 1);
        assert_eq!(mock.count("GET", Some("config")), 1);
    }
}

#[tokio::test]
async fn get_config_stops_after_cluster_transport_failure_and_disabled_discovery_skips_it() {
    let c = Client::new(cfg("127.0.0.1", 1));
    c.set_test_operation_timeout(Duration::from_millis(300));
    let err = c.get_config().await.unwrap_err();
    assert!(matches!(err, HomeError::ClusterDiscoveryTransport(_)), "{err:?}");

    let mut disabled = cfg("127.0.0.1", 1);
    disabled.disable_cluster_discovery = true;
    let c = Client::new(disabled);
    assert_eq!(c.refresh_cluster_nodes().await.unwrap(), false);
}

#[tokio::test]
async fn cluster_nodes_retarget_to_the_least_loaded_node() {
    let payload = r#"{"ok":true,"nodes":[{"ip":"10.0.0.2","port":8327,"client_count":5},{"ip":"10.0.0.1","port":8327,"client_count":1},{"ip":"","port":1},{"ip":"bad","port":0}]}"#;
    let mock = MockHome::start(move |args| {
        if args.first().is_some_and(|a| a == "CLUSTER") {
            t::bulk(payload)
        } else {
            t::err("ERR no")
        }
    })
    .await;
    let c = client_for(&mock);
    assert!(c.refresh_cluster_nodes().await.unwrap());
    assert_eq!(c.addr().as_deref(), Some("10.0.0.1:8327"));
    assert_eq!(c.cluster_nodes().len(), 2);
    assert_eq!(c.recovery_state(), RECOVERY_SWITCHING);
}

#[test]
fn failover_after_three_reconnect_failures_moves_to_the_next_node() {
    let c = Client::new(cfg("seed.example.com", 8327));
    c.set_cluster_state(
        vec![
            ClusterNode { ip: "seed.example.com".into(), port: 8327, client_count: 1, is_master: false, last_seen_at: None },
            ClusterNode { ip: "other.example.com".into(), port: 8327, client_count: 2, is_master: false, last_seen_at: None },
        ],
        2,
    );
    let (switched, addr) = c.failover_after_reconnect_failure();
    assert_eq!((switched, addr.as_str()), (true, "other.example.com:8327"));
    assert_eq!(c.reconnect_failures(), 0);
}

#[test]
fn failover_is_disabled_with_cluster_discovery_off() {
    let mut conf = cfg("seed.example.com", 8327);
    conf.disable_cluster_discovery = true;
    let c = Client::new(conf);
    c.set_cluster_state(
        vec![ClusterNode { ip: "other.example.com".into(), port: 8327, client_count: 0, is_master: false, last_seen_at: None }],
        2,
    );
    assert_eq!(c.failover_after_reconnect_failure(), (false, String::new()));
    assert_eq!(c.addr().as_deref(), Some("seed.example.com:8327"));
}

#[test]
fn new_lifetime_preserves_failover_and_membership_state() {
    let c = Client::new(cfg("seed.example.com", 8327));
    let instance = c.membership_instance_id();
    assert!(uuid::Uuid::parse_str(&instance).is_ok());
    c.enable_legacy_membership();
    c.set_cluster_state(
        vec![
            ClusterNode { ip: "failed.example.com".into(), port: 8327, client_count: 1, is_master: false, last_seen_at: None },
            ClusterNode { ip: "healthy.example.com".into(), port: 8327, client_count: 2, is_master: false, last_seen_at: None },
        ],
        2,
    );
    // The cluster nodes were learned while targeting `failed`.
    c.close();
    let next = c.new_lifetime();
    assert_eq!(next.membership_instance_id(), instance);
    assert!(next.legacy_membership());
    let fresh = Client::new(HomeConfig::default());
    assert!(fresh.membership_instance_id() != instance && !fresh.legacy_membership());
    assert_eq!(next.reconnect_failures(), 2);
    assert_eq!(next.cluster_nodes().len(), 2);
    // The closed predecessor does not fence its successor.
    assert!(!next.is_dispatch_fenced());
}

#[tokio::test]
async fn concurrency_release_does_not_open_before_membership_ready() {
    for state in [RECOVERY_TAKEOVER_ELIGIBLE, RECOVERY_SWITCHING, RECOVERY_SWITCHING_TAKEOVER] {
        let c = Client::new(cfg("next.example.com", 8327));
        c.set_recovery_state(state);
        let err = c
            .push_concurrency_release(&ConcurrencyReleaseFrame { credential_id: "a".into(), model: "m".into(), release_seq: 1 })
            .await
            .unwrap_err();
        assert!(matches!(err, HomeError::NotConnected));
        assert!(!c.has_release_client());
    }
}

#[test]
fn ambiguous_dispatch_suppresses_takeover_for_the_next_lifetime() {
    let c = Client::new(cfg("next.example.com", 8327));
    c.set_recovery_state(RECOVERY_SWITCHING_TAKEOVER);
    c.abort_ambiguous_dispatch();
    assert!(c.ambiguous_dispatch() && c.is_dispatch_fenced());
    c.suppress_takeover();
    assert_eq!(c.new_lifetime().recovery_state(), RECOVERY_SWITCHING);
}

#[test]
fn membership_protocol_error_classification() {
    let e = |s: &str| HomeError::Redis(s.into());
    assert!(e("ERR membership_takeover_unavailable").is_membership_takeover_unavailable());
    assert!(!e("ERR wrong number of arguments for 'subscribe' command").is_membership_takeover_unavailable());
    assert!(e("ERR wrong number of arguments for 'subscribe' command").is_legacy_membership_protocol());
    for unrelated in [e("ERR connection refused"), e("ERR duplicate certificate"), HomeError::Timeout] {
        assert!(!unrelated.is_membership_takeover_unavailable() && !unrelated.is_legacy_membership_protocol());
    }
}

#[tokio::test]
async fn in_flight_snapshot_and_reporting_use_dedicated_keys() {
    let mock = MockHome::start(|args| if args[0].eq_ignore_ascii_case("LPUSH") || args[0].eq_ignore_ascii_case("RPUSH") { t::int(1) } else { t::err("ERR no") }).await;
    let c = client_for(&mock);
    c.lpush_in_flight_snapshot(b"{}").await.unwrap();
    c.lpush_usage(b"{\"u\":1}").await.unwrap();
    c.lpush_usage(b"").await.unwrap();
    c.rpush_request_log(b"r").await.unwrap();
    c.rpush_app_log(b"a").await.unwrap();
    c.rpush_plugin_status(b"p").await.unwrap();
    let cmds = mock.commands();
    assert_eq!(cmds.iter().map(|c| (c[0].to_lowercase(), c[1].clone())).collect::<Vec<_>>(), vec![
        ("lpush".to_string(), "in-flight-snapshot".to_string()),
        ("lpush".to_string(), "usage".to_string()),
        ("rpush".to_string(), "request-log".to_string()),
        ("rpush".to_string(), "app-log".to_string()),
        ("rpush".to_string(), "plugin-status".to_string()),
    ]);
    // A failing in-flight push does not touch the heartbeat.
    let failing = MockHome::start(|_| t::err("ERR boom")).await;
    let c = client_for(&failing);
    assert!(c.lpush_in_flight_snapshot(b"{}").await.is_err());
    assert!(!c.heartbeat_ok());
}

#[tokio::test]
async fn concurrency_release_pushes_through_an_independent_pool() {
    let mock = MockHome::start(|_| t::int(1)).await;
    let c = client_for(&mock);
    c.push_concurrency_release(&ConcurrencyReleaseFrame { credential_id: "cred-a".into(), model: "model-a".into(), release_seq: 7 })
        .await
        .unwrap();
    assert!(c.has_release_client());
    let cmd = &mock.commands()[0];
    assert_eq!((cmd[0].as_str(), cmd[1].as_str()), ("LPUSH", "concurrency-release"));
    assert_eq!(serde_json::from_str::<Value>(&cmd[2]).unwrap(), json!({"credential_id": "cred-a", "model": "model-a", "release_seq": 7}));
    assert!(c.push_concurrency_release(&ConcurrencyReleaseFrame { credential_id: String::new(), model: "m".into(), release_seq: 1 }).await.is_err());
}

#[tokio::test]
async fn kv_operations_map_replies() {
    let mock = MockHome::start(|args| match args[0].to_uppercase().as_str() {
        "GET" => match args[1].as_str() {
            "miss" => t::nil(),
            _ => t::bulk("v"),
        },
        "SET" => if args.iter().any(|a| a == "NX") { t::nil() } else { t::ok() },
        "MGET" => t::raw("*3\r\n$1\r\na\r\n$-1\r\n$1\r\nc\r\n"),
        "MSET" => t::ok(),
        "DEL" => t::int(2),
        "EXPIRE" => t::int(1),
        "TTL" => t::int(-2),
        "INCRBY" => t::int(9),
        "CAS" => match args[2].as_str() {
            "1" => t::int(1),
            _ => t::int(0),
        },
        _ => t::err("ERR unknown command"),
    })
    .await;
    let c = client_for(&mock);
    assert_eq!(c.kv_get("miss").await.unwrap(), None);
    assert_eq!(c.kv_get("hit").await.unwrap().unwrap(), b"v");
    assert!(c.kv_set("k", b"v", KvSetOptions::default()).await.unwrap());
    assert!(!c.kv_set_nx("k", b"v", Duration::from_secs(5)).await.unwrap());
    assert_eq!(c.kv_mget(&["a".into(), "b".into(), "c".into()]).await.unwrap(), vec![Some(b"a".to_vec()), None, Some(b"c".to_vec())]);
    c.kv_mset(&BTreeMap::from([("b".into(), b"2".to_vec()), ("a".into(), b"1".to_vec())])).await.unwrap();
    assert_eq!(c.kv_del(&["x".into(), "y".into()]).await.unwrap(), 2);
    assert!(c.kv_expire("k", Duration::from_millis(200)).await.unwrap());
    assert_eq!(c.kv_ttl("k").await.unwrap(), (Duration::ZERO, false));
    assert_eq!(c.kv_incr_by("n", 3).await.unwrap(), 9);
    assert!(c.kv_compare_and_swap("k", b"old", true, b"new", Duration::from_millis(1500)).await.unwrap());
    assert!(!c.kv_compare_and_swap("k", b"", false, b"new", Duration::ZERO).await.unwrap());

    let cmds = mock.commands();
    let mset = cmds.iter().find(|c| c[0] == "MSET").unwrap();
    assert_eq!(mset[1..], ["a", "1", "b", "2"]);
    let expire = cmds.iter().find(|c| c[0].eq_ignore_ascii_case("expire")).unwrap();
    assert_eq!(expire[2], "1");
    let cas = cmds.iter().filter(|c| c[0] == "CAS").collect::<Vec<_>>();
    assert_eq!(cas[0][1..], ["k", "1", "old", "new", "PX", "1500"]);
    assert_eq!(cas[1][1..], ["k", "0", "", "new"]);
}

#[tokio::test]
async fn compare_and_swap_latches_an_unsupported_home() {
    let mock = MockHome::start(|_| t::err("ERR unknown command 'CAS'")).await;
    let c = client_for(&mock);
    let first = c.kv_compare_and_swap("k", b"", false, b"v", Duration::ZERO).await.unwrap_err();
    assert!(matches!(first, HomeError::CompareAndSwapUnsupported));
    let second = c.kv_compare_and_swap("k", b"", false, b"v", Duration::ZERO).await.unwrap_err();
    assert!(matches!(second, HomeError::CompareAndSwapUnsupported));
    assert_eq!(mock.count("CAS", None), 1);
    // A new lifetime re-probes.
    let next = c.new_lifetime();
    let _ = next.kv_compare_and_swap("k", b"", false, b"v", Duration::ZERO).await;
    assert_eq!(mock.count("CAS", None), 2);
}

#[tokio::test]
async fn plugin_tasks_and_sync_unsupported_detection() {
    let mock = MockHome::start(|args| match args[1].as_str() {
        "plugin-tasks" => t::bulk(r#"[{"id":3,"operation":"install","plugin_id":"p","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}]"#),
        "plugin-sync" => t::err("ERR unsupported key"),
        _ => t::nil(),
    })
    .await;
    let c = client_for(&mock);
    let tasks = c.get_plugin_tasks().await.unwrap();
    assert_eq!((tasks[0].id, tasks[0].operation.as_str()), (3, "install"));
    // The sync request goes on its own connection with an extra argument.
    let err = c.get_plugin_sync(br#"{"installed_versions":{}}"#).await.unwrap_err();
    assert!(matches!(err, HomeError::PluginSyncUnsupported(ref m) if m == "unsupported key"), "{err:?}");

    let body = MockHome::start(|_| t::bulk(r#"{"error":{"type":"plugin_sync_unsupported","message":""}}"#)).await;
    let c = client_for(&body);
    let err = c.get_plugin_sync(b"{}").await.unwrap_err();
    assert!(matches!(err, HomeError::PluginSyncUnsupported(ref m) if m == "plugin_sync_unsupported"), "{err:?}");

    let other = MockHome::start(|_| t::err("ERR boom")).await;
    let c = client_for(&other);
    assert!(matches!(c.get_plugin_sync(b"{}").await.unwrap_err(), HomeError::Redis(_)));
}

// ---- auth dispatch ----

fn dispatch_params() -> DispatchParams<'static> {
    DispatchParams { model: "gpt-5", count: 1, ..Default::default() }
}

#[tokio::test]
async fn rpop_auth_maps_replies_and_probes_first() {
    let mock = MockHome::start(|args| match args[0].to_lowercase().as_str() {
        "ping" => t::raw("+PONG\r\n"),
        "rpop" => t::bulk(r#"{"auth":{"id":"a"}}"#),
        _ => t::err("ERR no"),
    })
    .await;
    let c = client_for(&mock);
    assert_eq!(c.rpop_auth(&dispatch_params()).await.unwrap(), br#"{"auth":{"id":"a"}}"#);
    let cmds = mock.commands();
    assert_eq!(cmds[0][0].to_lowercase(), "ping");
    assert_eq!(cmds[1][0].to_lowercase(), "rpop");
    let key: Value = serde_json::from_str(&cmds[1][1]).unwrap();
    assert_eq!(key["type"], "auth");

    let nil = MockHome::start(|args| if args[0].eq_ignore_ascii_case("ping") { t::raw("+PONG\r\n") } else { t::nil() }).await;
    assert!(matches!(client_for(&nil).rpop_auth(&dispatch_params()).await, Err(HomeError::AuthNotFound)));

    let empty = MockHome::start(|args| if args[0].eq_ignore_ascii_case("ping") { t::raw("+PONG\r\n") } else { t::bulk("") }).await;
    assert!(matches!(client_for(&empty).rpop_auth(&dispatch_params()).await, Err(HomeError::EmptyResponse)));

    let model = client_for(&mock).rpop_auth(&DispatchParams { model: "  ", ..Default::default() }).await.unwrap_err();
    assert_eq!(model.to_string(), "home: requested model is empty");
}

#[tokio::test]
async fn complete_server_error_is_deterministic_not_ambiguous() {
    let mock = MockHome::start(|args| if args[0].eq_ignore_ascii_case("ping") { t::raw("+PONG\r\n") } else { t::err("ERR no auth") }).await;
    let c = client_for(&mock);
    let err = c.rpop_auth(&dispatch_params()).await.unwrap_err();
    assert!(matches!(err, HomeError::Redis(_)) && !err.is_ambiguous_dispatch());
    assert!(!c.is_dispatch_fenced());
}

#[tokio::test]
async fn request_read_then_close_is_ambiguous() {
    let mock = MockHome::start(|args| if args[0].eq_ignore_ascii_case("ping") { t::raw("+PONG\r\n") } else { Reply::Close }).await;
    let c = client_for(&mock);
    let err = c.rpop_auth(&dispatch_params()).await.unwrap_err();
    assert!(err.is_ambiguous_dispatch(), "{err:?}");
    // Timeout after the request was issued is ambiguous too.
    let slow = MockHome::start(|args| if args[0].eq_ignore_ascii_case("ping") { t::raw("+PONG\r\n") } else { Reply::Silent }).await;
    let c = client_for(&slow);
    c.set_test_operation_timeout(Duration::from_millis(200));
    assert!(c.rpop_auth(&dispatch_params()).await.unwrap_err().is_ambiguous_dispatch());
}

#[tokio::test]
async fn pre_send_failure_is_deterministic() {
    let mock = MockHome::start(|_| Reply::Close).await;
    let c = client_for(&mock);
    let err = c.rpop_auth(&dispatch_params()).await.unwrap_err();
    assert!(!err.is_ambiguous_dispatch(), "{err:?}");
    assert_eq!(mock.count("rpop", None), 0);
    let c = Client::new(cfg("127.0.0.1", 1));
    assert!(!c.rpop_auth(&dispatch_params()).await.unwrap_err().is_ambiguous_dispatch());
}

#[tokio::test]
async fn close_permanently_fences_dispatch() {
    let mock = MockHome::start(|_| t::raw("+PONG\r\n")).await;
    let c = client_for(&mock);
    c.close();
    assert!(matches!(c.rpop_auth(&dispatch_params()).await, Err(HomeError::DispatchFenced)));
    assert!(matches!(c.ping().await, Err(HomeError::DispatchFenced)));
    assert_eq!(mock.count("ping", None) + mock.count("rpop", None), 0);
}

#[tokio::test]
async fn abort_ambiguous_dispatch_closes_a_blocked_rpop_without_waiting() {
    let mock = MockHome::start(|args| if args[0].eq_ignore_ascii_case("ping") { t::raw("+PONG\r\n") } else { Reply::Silent }).await;
    let c = Arc::new(client_for(&mock));
    c.set_test_operation_timeout(Duration::from_secs(30));
    let task = {
        let c = c.clone();
        tokio::spawn(async move { c.rpop_auth(&dispatch_params()).await })
    };
    while mock.count("rpop", None) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let started = std::time::Instant::now();
    c.abort_ambiguous_dispatch();
    let err = tokio::time::timeout(Duration::from_secs(2), task).await.expect("rpop did not unblock").unwrap().unwrap_err();
    assert!(err.is_ambiguous_dispatch() && started.elapsed() < Duration::from_secs(2));
    assert!(c.ambiguous_dispatch() && c.is_dispatch_fenced() && !c.heartbeat_ok());
}

#[tokio::test]
async fn refresh_auth_requires_an_index_and_maps_replies() {
    let mock = MockHome::start(|args| if args[1].contains("refresh") { t::bulk("{\"id\":\"a\"}") } else { t::nil() }).await;
    let c = client_for(&mock);
    assert!(c.get_refresh_auth(" ", "").await.is_err());
    assert_eq!(c.get_refresh_auth("idx", " abc ").await.unwrap(), b"{\"id\":\"a\"}");
    let key: Value = serde_json::from_str(&mock.commands()[0][1]).unwrap();
    assert_eq!(key, json!({"type": "refresh", "auth_index": "idx", "access_token_sha256": "abc"}));
}

// ---- subscriber lifetime ----

fn lifecycle_payload(timeout: &str) -> String {
    format!("credential-concurrency:\n  lifecycle-config-revision: 1\n  cpa-heartbeat-timeout: {timeout}\n")
}

async fn run_lifetime(c: &Client, cancel: &Cancel, ready_states: &mut Vec<bool>) -> Result<(), HomeError> {
    let mut on_config = |raw: &[u8]| -> Result<(), HomeError> {
        let parsed = cpa_config::parse_config_bytes(raw).map_err(HomeError::other)?;
        c.set_lifecycle_config(parsed.credential_concurrency)
    };
    let recovery = &mut *ready_states;
    let mut on_ready = || recovery.push(c.recovery_state() == RECOVERY_STABLE);
    c.run_config_subscriber_lifetime(cancel, &mut on_config, &mut on_ready).await
}

#[tokio::test]
async fn subscriber_returns_after_heartbeat_loss_and_fails_over() {
    let payload = lifecycle_payload("100ms");
    let mock = MockHome::start(move |args| match args[0].to_lowercase().as_str() {
        "get" if args[1] == "config" => t::bulk(&payload),
        "subscribe" if args[1] == "config" => t::subscribe_ack("config", 1),
        _ => t::ok(),
    })
    .await;
    let c = client_for(&mock);
    c.set_cluster_state(vec![ClusterNode { ip: "failover.example.com".into(), port: 8327, client_count: 0, is_master: false, last_seen_at: None }], 0);
    c.set_recovery_state(RECOVERY_SWITCHING_TAKEOVER);
    let cancel: Cancel = Arc::new(Kill::default());
    let mut ready = vec![];
    let err = run_lifetime(&c, &cancel, &mut ready).await.unwrap_err();
    assert!(err.is_timeout(), "{err:?}");
    assert_eq!(ready, vec![true], "ACK plus a fresh command probe clears the takeover state");
    assert!(!c.heartbeat_ok());
    assert_eq!(c.addr().as_deref(), Some("failover.example.com:8327"));
    assert_eq!(c.recovery_state(), RECOVERY_SWITCHING_TAKEOVER);
    assert_eq!(mock.count("GET", Some("config")), 1);
    let sub = mock.commands().into_iter().find(|c| c[0].eq_ignore_ascii_case("subscribe")).unwrap();
    assert_eq!(sub, vec!["subscribe", "config", "1", "takeover", &c.membership_instance_id()]);
}

#[tokio::test]
async fn subscriber_rejects_an_invalid_ack() {
    let payload = lifecycle_payload("1s");
    let mock = MockHome::start(move |args| match args[0].to_lowercase().as_str() {
        "get" => t::bulk(&payload),
        "subscribe" => t::subscribe_ack("other", 1),
        _ => t::ok(),
    })
    .await;
    let c = client_for(&mock);
    let cancel: Cancel = Arc::new(Kill::default());
    let mut ready = vec![];
    let err = run_lifetime(&c, &cancel, &mut ready).await.unwrap_err();
    assert_eq!(err.to_string(), "invalid Home subscription ACK");
    assert!(ready.is_empty() && !c.heartbeat_ok());
    // Not managed: the failed lifetime closes the client.
    assert!(c.is_dispatch_fenced());
}

#[tokio::test]
async fn subscriber_uses_legacy_subscribe_without_lifecycle_revision() {
    let mock = MockHome::start(|args| match args[0].to_lowercase().as_str() {
        "get" => t::bulk("host: 127.0.0.1\n"),
        "subscribe" => t::subscribe_ack("config", 1),
        _ => t::ok(),
    })
    .await;
    let c = client_for(&mock);
    c.set_managed_lifetime(true);
    let cancel: Cancel = Arc::new(Kill::default());
    let mut ready = vec![];
    c.set_test_operation_timeout(Duration::from_millis(150));
    let err = run_lifetime(&c, &cancel, &mut ready).await.unwrap_err();
    assert!(err.is_timeout());
    let sub = mock.commands().into_iter().find(|c| c[0].eq_ignore_ascii_case("subscribe")).unwrap();
    assert_eq!(sub, vec!["subscribe", "config"]);
    // Managed lifetimes leave closing to the service.
    assert!(!c.is_dispatch_fenced());
}

#[tokio::test]
async fn subscriber_applies_config_pushes_and_cluster_updates_then_cancels() {
    let payload = lifecycle_payload("5s");
    let mock = MockHome::start(move |args| match args[0].to_lowercase().as_str() {
        "get" => t::bulk(&payload),
        "subscribe" => t::subscribe_ack("config", 1),
        _ => t::ok(),
    })
    .await;
    let c = Arc::new(client_for(&mock));
    c.set_managed_lifetime(true);
    let cancel: Cancel = Arc::new(Kill::default());
    let configs = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let ready = Arc::new(tokio::sync::Notify::new());
    let task = {
        let (c, cancel, configs, ready) = (c.clone(), cancel.clone(), configs.clone(), ready.clone());
        tokio::spawn(async move {
            let cb_configs = configs.clone();
            let c2 = c.clone();
            let mut on_config = move |raw: &[u8]| -> Result<(), HomeError> {
                cb_configs.lock().push(String::from_utf8_lossy(raw).into_owned());
                let parsed = cpa_config::parse_config_bytes(raw).map_err(HomeError::other)?;
                c2.set_lifecycle_config(parsed.credential_concurrency)
            };
            let mut on_ready = || ready.notify_one();
            c.run_config_subscriber_lifetime(&cancel, &mut on_config, &mut on_ready).await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), ready.notified()).await.expect("never ready");
    assert!(c.heartbeat_ok());
    mock.push_all(&t::message_frame("config", "  host: 1.2.3.4\n  "));
    mock.push_all(&t::message_frame("cluster", r#"{"nodes":[{"ip":"10.1.1.1","port":8327,"client_count":3}]}"#));
    mock.push_all(&t::message_frame("other", "ignored"));
    for _ in 0..100 {
        if configs.lock().len() == 2 && c.cluster_nodes().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(configs.lock()[1], "host: 1.2.3.4");
    assert_eq!(c.cluster_nodes()[0].ip, "10.1.1.1");
    cancel.kill();
    let err = tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap_err();
    assert_eq!(err.to_string(), "context canceled");
    assert!(!c.heartbeat_ok());
}

#[test]
fn subscription_parameters_follow_revision_membership_and_recovery() {
    let c = Client::new(cfg("127.0.0.1", 6379));
    let mut lifecycle = CredentialConcurrencyConfig::default();
    lifecycle.lifecycle_config_revision = 9;
    lifecycle.cpa_heartbeat_timeout = GoDuration::from_secs(4);
    lifecycle.cpa_cancel_bound = GoDuration::from_secs(5);
    c.set_lifecycle_config(lifecycle).unwrap();
    let id = c.membership_instance_id();
    let (args, timeout) = c.subscription_parameters();
    assert_eq!((args, timeout), (vec!["config".to_string(), "9".into(), id.clone()], Duration::from_secs(4)));
    c.set_recovery_state(RECOVERY_SWITCHING_TAKEOVER);
    assert_eq!(c.subscription_parameters().0, vec!["config".to_string(), "9".into(), "takeover".into(), id.clone()]);
    c.enable_legacy_membership();
    assert_eq!(c.subscription_parameters().0, vec!["config".to_string(), "9".into()]);
}

#[test]
fn lifecycle_config_defaults_and_validation() {
    let c = Client::new(cfg("127.0.0.1", 6379));
    assert_eq!(c.limiter_config().cpa_heartbeat_timeout, GoDuration::from_secs(3));
    assert_eq!(c.subscription_parameters().1, Duration::from_secs(3));
    let mut cfg = CredentialConcurrencyConfig::default().with_defaults();
    cfg.cpa_heartbeat_timeout = GoDuration::from_secs(20);
    c.set_lifecycle_config(cfg).unwrap();
    assert_eq!(c.limiter_config().cpa_heartbeat_timeout, GoDuration::from_secs(20));
    let mut bad = CredentialConcurrencyConfig::default().with_defaults();
    bad.release_max_backoff = GoDuration::from_millis(1);
    assert!(c.set_lifecycle_config(bad).is_err());
}

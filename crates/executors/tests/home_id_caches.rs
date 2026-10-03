//! Home-mode behaviour of the Claude/Codex id caches, the Claude credential device pool and the
//! Claude device profile (Go: helps/session_id_cache_test.go, user_id_cache_test.go,
//! cache_helpers_test.go, claude_credential_identity_test.go, claude_device_profile_test.go).
//!
//! The Home client is process-global, so every test holds `SERIAL` and runs against its own
//! in-memory Home (a mock RESP server over `FakeKv`) that can inject failures per command.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cpa_auth::Auth;
use cpa_auth::claude::DEVICE_IDS_METADATA_KEY;
use cpa_config::HomeConfig;
use cpa_executors::claude::helps::credential_identity::ensure_claude_credential_device_pool_required;
use cpa_executors::claude::helps::device_profile::{
    DEFAULT_CLAUDE_FINGERPRINT_ARCH, DEFAULT_CLAUDE_FINGERPRINT_OS, DEFAULT_CLAUDE_FINGERPRINT_PACKAGE_VERSION,
    DEFAULT_CLAUDE_FINGERPRINT_RUNTIME_VERSION, DEFAULT_CLAUDE_FINGERPRINT_USER_AGENT, CLAUDE_DEVICE_PROFILE_TTL,
    reset_claude_device_profile_cache, resolve_claude_device_profile_required,
};
use cpa_executors::helps::id_cache::{
    CodexCache, ID_TTL, cached_session_id, cached_session_id_required, cached_session_id_required_blocking,
    cached_user_id_required, cached_user_id_required_blocking, codex_prompt_cache_key, get_codex_cache,
    get_codex_cache_required, is_valid_user_id, set_codex_cache, set_codex_cache_best_effort,
    set_codex_cache_required,
};
use cpa_home::kv::{clear_current, hash_key_part, set_current};
use cpa_home::testing::{FakeKv, MockHome, err, ok};
use cpa_home::Client;
use http::HeaderMap;
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

static SERIAL: Mutex<()> = Mutex::const_new(());

/// A fake Home installed as the process-wide client for the length of one test.
struct Home {
    mock: MockHome,
    kv: FakeKv,
    _serial: MutexGuard<'static, ()>,
}

impl Drop for Home {
    fn drop(&mut self) {
        clear_current();
    }
}

/// Installs a fake Home; commands for which `fail` returns true get an error reply and
/// commands for which `drop_write` returns true are acknowledged without storing anything.
async fn home_with(
    fail: impl Fn(&[String]) -> bool + Send + Sync + 'static,
    drop_write: impl Fn(&[String]) -> bool + Send + Sync + 'static,
) -> Home {
    let serial = SERIAL.lock().await;
    let kv = FakeKv::new();
    let handler_kv = kv.clone();
    let mock = MockHome::start(move |args| {
        if fail(args) {
            err("ERR injected failure")
        } else if drop_write(args) {
            ok()
        } else {
            handler_kv.handle(args)
        }
    })
    .await;
    let cfg = HomeConfig { enabled: true, host: "127.0.0.1".into(), port: i64::from(mock.port()), ..Default::default() };
    let client = Arc::new(Client::new(cfg));
    client.set_test_operation_timeout(Duration::from_secs(2));
    client.set_heartbeat_ok_for_tests(true);
    set_current(client);
    Home { mock, kv, _serial: serial }
}

async fn home() -> Home {
    home_with(|_| false, |_| false).await
}

fn is_cmd(args: &[String], name: &str) -> bool {
    args.first().is_some_and(|a| a.eq_ignore_ascii_case(name))
}

impl Home {
    fn count(&self, name: &str) -> usize {
        self.mock.count(name, None)
    }

    /// Number of `SET ... NX` commands.
    fn set_nx_count(&self) -> usize {
        self.mock.commands().iter().filter(|c| is_cmd(c, "SET") && c.iter().any(|a| a == "NX")).count()
    }
}

// ------------------------------------------------------------------ session and user ids

#[tokio::test(flavor = "multi_thread")]
async fn session_id_is_shared_through_home_kv() {
    let home = home().await;
    let first = cached_session_id_required("api-key-1").await.expect("first");
    let second = cached_session_id_required("api-key-1").await.expect("second");
    assert_eq!(first, second);
    assert!(Uuid::parse_str(&first).is_ok());
    assert_eq!(home.set_nx_count(), 1);
    assert_eq!(home.count("expire"), 1);
    let key = format!("cpa:claude:session-id:{}", hash_key_part("api-key-1"));
    assert_eq!(home.kv.get(&key).as_deref(), Some(first.as_bytes()));
    assert_eq!(home.kv.ttl(&key), Some(ID_TTL));
    // The synchronous entry points read the same shared value.
    assert_eq!(cached_session_id_required_blocking("api-key-1").expect("blocking"), first);
    assert_eq!(cached_session_id("api-key-1"), first);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_api_key_never_touches_home_kv() {
    let home = home().await;
    let session = cached_session_id_required("").await.expect("session");
    assert!(Uuid::parse_str(&session).is_ok());
    assert!(is_valid_user_id(&cached_user_id_required("").await.expect("user")));
    assert!(is_valid_user_id(&cached_user_id_required_blocking("").expect("user blocking")));
    assert!(home.mock.commands().is_empty(), "{:?}", home.mock.commands());
}

#[tokio::test(flavor = "multi_thread")]
async fn session_id_home_failures_are_errors() {
    let get_fails = home_with(|a| is_cmd(a, "get"), |_| false).await;
    assert!(cached_session_id_required("api-key-1").await.is_err());
    // The non-required entry point falls back to a fresh UUID.
    assert!(Uuid::parse_str(&cached_session_id("api-key-1")).is_ok());
    drop(get_fails);

    let set_fails = home_with(|a| is_cmd(a, "SET"), |_| false).await;
    assert!(cached_session_id_required("api-key-1").await.is_err());
    drop(set_fails);

    let expire_fails = home_with(|a| is_cmd(a, "expire"), |_| false).await;
    let key = format!("cpa:claude:session-id:{}", hash_key_part("api-key-1"));
    expire_fails.kv.put(&key, Uuid::new_v4().to_string());
    assert!(cached_session_id_required("api-key-1").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn session_id_requires_a_read_after_set() {
    let _home = home_with(|_| false, |a| is_cmd(a, "SET")).await;
    let err = cached_session_id_required("api-key-1").await.expect_err("not persisted");
    assert!(err.to_string().contains("home kv session id missing after set"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn user_id_is_shared_through_home_kv() {
    let home = home().await;
    let first = cached_user_id_required("api-key-1").await.expect("first");
    let second = cached_user_id_required("api-key-1").await.expect("second");
    assert_eq!(first, second);
    assert!(is_valid_user_id(&first));
    // One SET NX for the session id and one for the user id; the second call only refreshes.
    assert_eq!(home.set_nx_count(), 2);
    assert_eq!(home.count("expire"), 1);
    let user_key = format!("cpa:claude:user-id:{}", hash_key_part("api-key-1"));
    assert_eq!(home.kv.ttl(&user_key), Some(ID_TTL));
    // The user id embeds the shared session id.
    let session = cached_session_id_required("api-key-1").await.expect("session");
    let parsed: serde_json::Value = serde_json::from_str(&first).expect("json");
    assert_eq!(parsed["session_id"], session.as_str());
    assert_eq!(cached_user_id_required_blocking("api-key-1").expect("blocking"), first);
}

#[tokio::test(flavor = "multi_thread")]
async fn user_id_home_failures_are_errors() {
    let get_fails = home_with(|a| is_cmd(a, "get"), |_| false).await;
    assert!(cached_user_id_required("api-key-1").await.is_err());
    drop(get_fails);

    let set_fails = home_with(|a| is_cmd(a, "SET"), |_| false).await;
    assert!(cached_user_id_required("api-key-1").await.is_err());
    drop(set_fails);

    let expire_fails = home_with(|a| is_cmd(a, "expire"), |_| false).await;
    let key = format!("cpa:claude:user-id:{}", hash_key_part("api-key-1"));
    expire_fails.kv.put(&key, cpa_executors::helps::id_cache::generate_fake_user_id());
    assert!(cached_user_id_required("api-key-1").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn user_id_requires_a_read_after_set() {
    let _home = home_with(|_| false, |a| is_cmd(a, "SET")).await;
    assert!(cached_user_id_required("api-key-1").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn non_home_mode_uses_the_local_map() {
    let _serial = SERIAL.lock().await;
    clear_current();
    let first = cached_session_id_required("api-key-local").await.expect("first");
    assert_eq!(cached_session_id_required_blocking("api-key-local").expect("second"), first);
    assert_eq!(cached_user_id_required("api-key-local").await.expect("user"), cached_user_id_required("api-key-local").await.expect("user"));
}

// ------------------------------------------------------------------ Codex prompt cache

#[tokio::test(flavor = "multi_thread")]
async fn codex_cache_required_fails_when_home_is_unavailable() {
    let _serial = SERIAL.lock().await;
    let disabled = Arc::new(Client::new(HomeConfig { enabled: false, ..Default::default() }));
    set_current(disabled);
    let cache = CodexCache { id: "cache-id".into(), expire: Instant::now() + Duration::from_secs(3600) };
    let err = set_codex_cache_required("cpa:codex:prompt-cache:test", cache.clone()).await.expect_err("unavailable");
    assert!(err.to_string().contains("home kv store unavailable"), "{err}");
    assert!(get_codex_cache_required("cpa:codex:prompt-cache:test").await.is_err());
    assert!(!set_codex_cache_best_effort("cpa:codex:prompt-cache:test", cache).await);
    assert!(get_codex_cache("cpa:codex:prompt-cache:test").is_none());
    clear_current();
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_cache_round_trips_through_home_kv() {
    let home = home().await;
    let key = codex_prompt_cache_key("gpt-5", "user");
    let cache = CodexCache { id: "id-1".into(), expire: Instant::now() + Duration::from_secs(600) };
    set_codex_cache_required(&key, cache).await.expect("set");

    // Stored with Go's JSON field names and a TTL of the remaining lifetime.
    let stored: serde_json::Value = serde_json::from_slice(&home.kv.get(&key).expect("stored")).expect("json");
    assert_eq!(stored["ID"], "id-1");
    assert!(stored["Expire"].as_str().is_some_and(|s| s.parse::<chrono::DateTime<chrono::Utc>>().is_ok()));
    let ttl = home.kv.ttl(&key).expect("ttl");
    assert!(ttl <= Duration::from_secs(600) && ttl >= Duration::from_secs(595), "{ttl:?}");

    let got = get_codex_cache_required(&key).await.expect("get").expect("hit");
    assert_eq!(got.id, "id-1");
    assert!(got.expire > Instant::now() + Duration::from_secs(590));
    assert_eq!(get_codex_cache(&key).map(|c| c.id).as_deref(), Some("id-1"));

    // The sync setter writes through Home too; an already expired entry is never stored.
    let key2 = codex_prompt_cache_key("gpt-5", "other");
    assert!(set_codex_cache(&key2, CodexCache { id: "id-2".into(), expire: Instant::now() + Duration::from_secs(60) }));
    assert!(home.kv.get(&key2).is_some());
    assert!(!set_codex_cache("k-expired", CodexCache { id: "x".into(), expire: Instant::now() }));
    assert!(set_codex_cache_required("k-expired", CodexCache { id: "x".into(), expire: Instant::now() }).await.is_ok());
    assert!(home.kv.get("k-expired").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_codex_cache_in_home_is_deleted() {
    let home = home().await;
    home.kv.put("cpa:codex:prompt-cache:old", r#"{"ID":"stale","Expire":"2001-02-03T04:05:06.789Z"}"#);
    assert!(get_codex_cache_required("cpa:codex:prompt-cache:old").await.expect("get").is_none());
    assert!(home.kv.get("cpa:codex:prompt-cache:old").is_none());
    // Malformed values are errors, absent keys are misses.
    home.kv.put("cpa:codex:prompt-cache:bad", "not json");
    assert!(get_codex_cache_required("cpa:codex:prompt-cache:bad").await.is_err());
    assert!(get_codex_cache_required("cpa:codex:prompt-cache:none").await.expect("miss").is_none());
}

// ------------------------------------------------------------------ credential device pool

const LEGACY: [&str; 5] = [
    "0000000000000000000000000000000000000000000000000000000000000000",
    "1111111111111111111111111111111111111111111111111111111111111111",
    "2222222222222222222222222222222222222222222222222222222222222222",
    "3333333333333333333333333333333333333333333333333333333333333333",
    "4444444444444444444444444444444444444444444444444444444444444444",
];

fn pool_key(auth: &mut Auth) -> String {
    format!("cpa:claude:credential-device-pool:{}", hash_key_part(&auth.ensure_index()))
}

#[tokio::test(flavor = "multi_thread")]
async fn device_pool_migrates_a_legacy_home_value_to_one_device() {
    let home = home().await;
    let mut auth = Auth::new("legacy-five-device-credential", "claude");
    let key = pool_key(&mut auth);
    home.kv.put(&key, serde_json::to_vec(&LEGACY).expect("json"));

    let ids = ensure_claude_credential_device_pool_required(&mut auth).await.expect("pool");
    assert_eq!(ids, vec![LEGACY[0].to_string()]);
    // One persistent XX rewrite of the canonical value.
    let sets: Vec<_> = home.mock.commands().into_iter().filter(|c| is_cmd(c, "SET")).collect();
    assert_eq!(sets.len(), 1, "{sets:?}");
    assert!(sets[0].iter().any(|a| a == "XX") && !sets[0].iter().any(|a| a == "NX" || a == "EX" || a == "PX"));
    let stored: Vec<String> = serde_json::from_slice(&home.kv.get(&key).expect("stored")).expect("json");
    assert_eq!(stored, vec![LEGACY[0].to_string()]);
    assert_eq!(auth.metadata.get(DEVICE_IDS_METADATA_KEY), Some(&serde_json::json!([LEGACY[0]])));
}

#[tokio::test(flavor = "multi_thread")]
async fn device_pool_is_created_once_and_shared_between_nodes() {
    let home = home().await;
    let mut first = Auth::new("shared-credential", "claude");
    let ids = ensure_claude_credential_device_pool_required(&mut first).await.expect("pool");
    assert_eq!(ids.len(), 1);
    let key = pool_key(&mut first);
    assert!(home.kv.get(&key).is_some());

    // Another node's copy of the credential adopts the stored pool instead of generating its own.
    let mut second = Auth::new("shared-credential", "claude");
    assert_eq!(ensure_claude_credential_device_pool_required(&mut second).await.expect("pool"), ids);
    assert_eq!(second.metadata.get(DEVICE_IDS_METADATA_KEY), first.metadata.get(DEVICE_IDS_METADATA_KEY));

    // A canonical local pool short-circuits Home entirely.
    let before = home.mock.commands().len();
    assert_eq!(ensure_claude_credential_device_pool_required(&mut second).await.expect("pool"), ids);
    assert_eq!(home.mock.commands().len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn device_pool_home_failures_are_errors() {
    let get_fails = home_with(|a| is_cmd(a, "get"), |_| false).await;
    let err = ensure_claude_credential_device_pool_required(&mut Auth::new("a", "claude")).await.expect_err("get");
    assert!(err.message.starts_with("ensure Claude credential device pool: Home KV get:"), "{}", err.message);
    drop(get_fails);

    let set_fails = home_with(|a| is_cmd(a, "SET"), |_| false).await;
    let err = ensure_claude_credential_device_pool_required(&mut Auth::new("a", "claude")).await.expect_err("set");
    assert!(err.message.starts_with("ensure Claude credential device pool: Home KV set:"), "{}", err.message);
    drop(set_fails);

    let lost = home_with(|_| false, |a| is_cmd(a, "SET")).await;
    let err = ensure_claude_credential_device_pool_required(&mut Auth::new("a", "claude")).await.expect_err("missing");
    assert_eq!(err.message, "ensure Claude credential device pool: Home KV value missing after set");
    drop(lost);

    let _serial = SERIAL.lock().await;
    set_current(Arc::new(Client::new(HomeConfig { enabled: false, ..Default::default() })));
    let err = ensure_claude_credential_device_pool_required(&mut Auth::new("a", "claude")).await.expect_err("client");
    assert!(err.message.starts_with("ensure Claude credential device pool: Home KV client:"), "{}", err.message);
    clear_current();
}

// ------------------------------------------------------------------ device profile

const BASELINE_UA: &str = DEFAULT_CLAUDE_FINGERPRINT_USER_AGENT;

fn device_headers(user_agent: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (name, value) in [
        ("user-agent", user_agent),
        ("x-stainless-package-version", DEFAULT_CLAUDE_FINGERPRINT_PACKAGE_VERSION),
        ("x-stainless-runtime-version", DEFAULT_CLAUDE_FINGERPRINT_RUNTIME_VERSION),
        ("x-stainless-os", "Windows"),
        ("x-stainless-arch", "x64"),
    ] {
        h.insert(name, value.parse().expect("header value"));
    }
    h
}

/// Home KV key of the profile of `auth` (scope `auth:<id>`, plus the subclient when set).
fn profile_key(auth: &Auth, subclient: Option<&str>) -> String {
    let mut scope = format!("auth:{}", auth.id);
    if let Some(sub) = subclient {
        scope.push_str(&format!("|subclient:{sub}"));
    }
    format!("cpa:claude:device-profile:{}", hash_key_part(&scope))
}

fn stored_profile(user_agent: &str, package: &str, runtime: &str) -> String {
    serde_json::json!({
        "user_agent": user_agent, "package_version": package, "runtime_version": runtime, "os": "Windows", "arch": "x64"
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn home_profile_read_without_candidate_normalizes_and_refreshes() {
    let home = home().await;
    let auth = Auth::new("auth-1", "claude");
    home.kv.put(&profile_key(&auth, None), stored_profile("claude-cli/2.2.0 (external, cli)", "0.80.0", "v24.4.0"));

    let profile = resolve_claude_device_profile_required(Some(&auth), "api-key", &HeaderMap::new(), None).await.expect("profile");
    // An unmeasured stored software tuple reads as the baseline, with the platform pinned.
    assert_eq!(profile.user_agent, BASELINE_UA);
    assert_eq!((profile.os.as_str(), profile.arch.as_str()), (DEFAULT_CLAUDE_FINGERPRINT_OS, DEFAULT_CLAUDE_FINGERPRINT_ARCH));
    assert_eq!(home.count("expire"), 1);
    assert_eq!(home.kv.ttl(&profile_key(&auth, None)), Some(CLAUDE_DEVICE_PROFILE_TTL));
}

#[tokio::test(flavor = "multi_thread")]
async fn home_profile_candidate_locks_rereads_and_writes() {
    let home = home().await;
    let auth = Auth::new("auth-1", "claude");
    let profile = resolve_claude_device_profile_required(Some(&auth), "api-key", &device_headers(BASELINE_UA), None)
        .await
        .expect("profile");
    assert_eq!(profile.user_agent, BASELINE_UA);

    let cmds = home.mock.commands();
    let lock = cmds.iter().find(|c| is_cmd(c, "SET") && c.iter().any(|a| a == "NX")).expect("lock");
    assert!(lock[1].starts_with("cpa:claude:device-profile-lock:"));
    assert_eq!(lock[2], "1");
    assert_eq!(&lock[3..], ["EX", "5", "NX"]);
    assert_eq!(home.count("get"), 1, "re-read after the lock");
    let write = cmds.iter().find(|c| is_cmd(c, "SET") && !c.iter().any(|a| a == "NX")).expect("write");
    assert_eq!(write[1], profile_key(&auth, None));
    assert_eq!(&write[3..], ["EX", &CLAUDE_DEVICE_PROFILE_TTL.as_secs().to_string()]);
    // Platform comes from the baseline, not from the client.
    let stored: serde_json::Value = serde_json::from_str(&write[2]).expect("json");
    assert_eq!(stored["os"], DEFAULT_CLAUDE_FINGERPRINT_OS);
    assert_eq!(stored["arch"], DEFAULT_CLAUDE_FINGERPRINT_ARCH);
}

#[tokio::test(flavor = "multi_thread")]
async fn home_profile_separates_vscode_agent_sdk_from_cli() {
    let home = home().await;
    let auth = Auth::new("auth-home-subclient-isolation", "claude");
    let cli = resolve_claude_device_profile_required(Some(&auth), "api-key", &device_headers(BASELINE_UA), None)
        .await
        .expect("cli");
    let vscode_ua = "claude-cli/2.1.280 (external, claude-vscode, agent-sdk/0.3.220)";
    let vscode = resolve_claude_device_profile_required(Some(&auth), "api-key", &device_headers(vscode_ua), None)
        .await
        .expect("vscode");
    assert_eq!(cli.user_agent, BASELINE_UA);
    assert_eq!(vscode.user_agent, vscode_ua);

    let writes = home.mock.commands().iter().filter(|c| is_cmd(c, "SET") && !c.iter().any(|a| a == "NX")).count();
    assert_eq!(writes, 2, "separate CLI and VS Code profiles");
    let cli_key = profile_key(&auth, None);
    let vscode_key = profile_key(&auth, Some("claude-vscode"));
    assert_ne!(cli_key, vscode_key);
    assert!(home.kv.get(&cli_key).is_some() && home.kv.get(&vscode_key).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn home_profile_does_not_downgrade_to_an_unmeasured_candidate() {
    let home = home().await;
    let auth = Auth::new("auth-1", "claude");
    home.kv.put(&profile_key(&auth, None), stored_profile("claude-cli/2.4.0 (external, cli)", "0.90.0", "v24.5.0"));

    let profile = resolve_claude_device_profile_required(
        Some(&auth),
        "api-key",
        &device_headers("claude-cli/2.3.0 (external, cli)"),
        None,
    )
    .await
    .expect("profile");
    // The 2.3.0 candidate is not a measured baseline tuple, so it is ignored and the stored 2.4.0
    // profile (also unmeasured) reads as the baseline.
    assert_eq!(profile.user_agent, BASELINE_UA);
    assert_eq!(home.mock.commands().iter().filter(|c| is_cmd(c, "SET")).count(), 0);
    assert_eq!(home.count("expire"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn home_profile_failures_are_errors() {
    let auth = Auth::new("auth-1", "claude");
    let candidate = device_headers(BASELINE_UA);
    let none = HeaderMap::new();

    let read = home_with(|a| is_cmd(a, "get"), |_| false).await;
    assert!(resolve_claude_device_profile_required(Some(&auth), "api-key", &none, None).await.is_err());
    drop(read);

    let lock = home_with(|a| is_cmd(a, "SET"), |_| false).await;
    assert!(resolve_claude_device_profile_required(Some(&auth), "api-key", &candidate, None).await.is_err());
    drop(lock);

    // The lock was not acquired (silently dropped here as "already held" is simulated by a pre-set
    // lock) and there is no stored profile to serve.
    let lock_miss = home().await;
    lock_miss.kv.put(
        &format!(
            "cpa:claude:device-profile-lock:{}",
            hash_key_part("auth:auth-1")
        ),
        "1",
    );
    let err = resolve_claude_device_profile_required(Some(&auth), "api-key", &candidate, None).await.expect_err("lock miss");
    assert!(err.to_string().contains("lock not acquired and profile missing"), "{err}");
    drop(lock_miss);

    let write = home_with(|a| is_cmd(a, "SET") && !a.iter().any(|x| x == "NX"), |_| false).await;
    assert!(resolve_claude_device_profile_required(Some(&auth), "api-key", &candidate, None).await.is_err());
    drop(write);
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_loser_serves_the_stored_profile() {
    let home = home().await;
    let auth = Auth::new("auth-1", "claude");
    home.kv.put(&format!("cpa:claude:device-profile-lock:{}", hash_key_part("auth:auth-1")), "1");
    let key = profile_key(&auth, None);
    home.kv.put(&key, stored_profile("claude-cli/2.1.280 (external, cli)", "0.112.1", "v26.3.0"));
    let profile = resolve_claude_device_profile_required(Some(&auth), "api-key", &device_headers(BASELINE_UA), None)
        .await
        .expect("profile");
    assert_eq!(profile.user_agent, BASELINE_UA);
}

#[tokio::test(flavor = "multi_thread")]
async fn non_home_profile_keeps_the_local_cache() {
    let _serial = SERIAL.lock().await;
    clear_current();
    reset_claude_device_profile_cache();
    let auth = Auth::new("auth-non-home", "claude");
    let first = resolve_claude_device_profile_required(Some(&auth), "api-key", &device_headers(BASELINE_UA), None).await.expect("first");
    let second = resolve_claude_device_profile_required(Some(&auth), "api-key", &HeaderMap::new(), None).await.expect("second");
    assert_eq!(second.user_agent, first.user_agent);
}

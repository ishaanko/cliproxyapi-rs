//! Home KV mode of the `cpa_core` caches (ports of the Home cases of internal/cache/*_test.go)
//! against the in-memory fake Home. The Home client is process-global, so every test holds `LOCK`
//! and installs its own fake.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use cpa_core::cache::*;
use cpa_executors::helps::home_kv;
use cpa_home::testing::{self as t, FakeKv, MockHome};
use cpa_home::Client;
use cpa_json::J;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::const_new(());

struct Home {
    _guard: MutexGuard<'static, ()>,
    _mock: MockHome,
    kv: FakeKv,
    client: Arc<Client>,
}

async fn home() -> Home {
    let guard = LOCK.lock().await;
    home_kv::install();
    let (mock, kv, client) = t::install_fake_home().await;
    Home { _guard: guard, _mock: mock, kv, client }
}

fn sha(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

const SIG: &str = "abc123validSignature1234567890123456789012345678901234567890";

#[tokio::test(flavor = "multi_thread")]
async fn kimi_home_generation_prevents_aba_delete() {
    let h = home().await;
    let (a, b) = (br#"[{"type":"thinking","signature":"A"}]"#, br#"[{"type":"thinking","signature":"B"}]"#);
    assert!(cache_kimi_thinking_replay_best_effort("k3", "execution:home-aba", a));
    let key = format!("cpa:kimi:thinking-replay:{}:{}", sha("k3"), sha("execution:home-aba"));
    assert_eq!(h.kv.ttl(&key), Some(Duration::from_secs(3600)));
    let value: serde_json::Value = serde_json::from_slice(&h.kv.get(&key).unwrap()).unwrap();
    assert_eq!(value["content"], serde_json::json!([{"type": "thinking", "signature": "A"}]));
    assert!(value["generation"].as_str().is_some_and(|g| !g.is_empty()));

    let (content, snapshot) = get_kimi_thinking_replay_with_snapshot_required("k3", "execution:home-aba").unwrap();
    assert_eq!(content.as_deref(), Some(a.as_slice()));
    assert!(cache_kimi_thinking_replay_best_effort("k3", "execution:home-aba", b));
    assert!(cache_kimi_thinking_replay_best_effort("k3", "execution:home-aba", a));
    assert!(
        !delete_kimi_thinking_replay_if_unchanged("k3", "execution:home-aba", &snapshot).unwrap(),
        "a stale snapshot must not delete a newer generation with repeated content"
    );
    assert_eq!(get_kimi_thinking_replay_required("k3", "execution:home-aba").unwrap().as_deref(), Some(a.as_slice()));
}

#[tokio::test(flavor = "multi_thread")]
async fn kimi_home_miss_reserves_a_tombstone_that_fences_stale_writers() {
    let h = home().await;
    let (first, f1) = get_kimi_thinking_replay_with_snapshot_required("k3", "execution:fence").unwrap();
    let (second, f2) = get_kimi_thinking_replay_with_snapshot_required("k3", "execution:fence").unwrap();
    assert!(first.is_none() && second.is_none());
    let key = format!("cpa:kimi:thinking-replay:{}:{}", sha("k3"), sha("execution:fence"));
    let tombstone: serde_json::Value = serde_json::from_slice(&h.kv.get(&key).unwrap()).unwrap();
    assert_eq!(tombstone["deleted"], true);
    assert!(delete_kimi_thinking_replay_if_unchanged("k3", "execution:fence", &f1).unwrap());
    let stale = br#"[{"type":"thinking","signature":"stale"}]"#;
    assert!(!replace_kimi_thinking_replay_if_unchanged("k3", "execution:fence", &f2, stale).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_home_appends_assistant_turns() {
    let h = home().await;
    let first = br#"[{"type":"thinking","thinking":"first","signature":"sig-1"},{"type":"tool_use","id":"toolu-1","name":"Read","input":{"path":"one"}}]"#;
    let second = br#"[{"type":"thinking","thinking":"second","signature":"sig-2"},{"type":"tool_use","id":"toolu-2","name":"Read","input":{"path":"two"}}]"#;
    assert!(cache_claude_thinking_replay_best_effort("claude:auth:model", "execution:multi-turn", first));
    let (found, snapshot) = get_claude_thinking_replay_with_snapshot_required("claude:auth:model", "execution:multi-turn").unwrap();
    assert!(found.is_some());
    assert!(replace_claude_thinking_replay_if_unchanged("claude:auth:model", "execution:multi-turn", &snapshot, second).unwrap());
    let contents = get_claude_thinking_replay_required("claude:auth:model", "execution:multi-turn").unwrap().unwrap();
    assert_eq!(contents, vec![first.to_vec(), second.to_vec()]);
    let key = format!("cpa:claude:thinking-replay:{}:{}", sha("claude:auth:model"), sha("execution:multi-turn"));
    assert!(h.kv.get(&key).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn signature_home_read_slides_ttl_and_never_falls_back_to_local() {
    let h = home().await;
    let key = format!("cpa:signature:claude:{}", sha("thinking text"));
    h.kv.put(&key, SIG);
    assert_eq!(get_cached_signature_required("claude-opus", "thinking text").unwrap(), SIG);
    assert_eq!(h.kv.ttl(&key), Some(Duration::from_secs(3 * 3600)), "read refreshes the 3h TTL");

    // A local entry is invisible in Home mode, and a Home miss is a plain miss.
    assert_eq!(get_cached_signature_required("claude-opus", "other text").unwrap(), "");
    assert_eq!(get_cached_signature_required("gemini-3", "other text").unwrap(), "skip_thought_signature_validator");

    assert!(cache_signature_best_effort("claude-opus", "written", SIG));
    let written = format!("cpa:signature:claude:{}", sha("written"));
    assert_eq!(h.kv.get(&written).as_deref(), Some(SIG.as_bytes()));
    assert!(!cache_signature_best_effort("claude-opus", "short", "too short"));

    delete_cached_signature_required("claude-opus", "written").unwrap();
    assert!(h.kv.get(&written).is_none());
    clear_signature_cache("");
    assert!(h.kv.get(&key).is_some(), "ClearSignatureCache does not touch Home");
}

#[tokio::test(flavor = "multi_thread")]
async fn home_unavailable_reports_errors_without_local_fallback() {
    let h = home().await;
    h.client.set_heartbeat_ok_for_tests(false);
    assert!(get_cached_signature_required("claude-opus", "x").is_err());
    assert_eq!(get_cached_signature("gemini-3", "x"), "", "the non-required read swallows errors");
    assert!(!cache_signature_best_effort("claude-opus", "x", SIG));
    assert!(get_kimi_thinking_replay_with_snapshot_required("k3", "s").is_err());
    assert!(delete_codex_reasoning_replay_item_required("gpt-5.4", "s").is_err());
    assert!(get_codex_reasoning_replay_items("gpt-5.4", "s").is_none());

    // Home mode off again: the in-process cache serves.
    cpa_home::kv::clear_current();
    assert!(cache_signature_best_effort("claude-opus", "local-text", SIG));
    assert_eq!(get_cached_signature("claude-opus", "local-text"), SIG);
    clear_signature_cache("");
}

fn gpt_sig(seed: u8) -> String {
    let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
    payload[0] = 0x80;
    for (i, b) in payload.iter_mut().enumerate().skip(9) {
        *b = seed.wrapping_add(i as u8);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
}

fn codex_item(seed: u8) -> Vec<u8> {
    format!(r#"{{"type":"reasoning","summary":[],"content":null,"encrypted_content":"{}"}}"#, gpt_sig(seed)).into_bytes()
}

fn turn(id: &str) -> Vec<u8> {
    format!(r#"{{"type":"{CODEX_REASONING_REPLAY_TURN_TYPE}","id":"{id}"}}"#).into_bytes()
}

/// A stored `[][]byte`: the JSON array of base64 strings.
fn decode_stored(raw: &[u8]) -> Vec<Vec<u8>> {
    let encoded: Vec<String> = serde_json::from_slice(raw).unwrap();
    encoded.iter().map(|s| base64::engine::general_purpose::STANDARD.decode(s).unwrap()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_home_stores_base64_json_and_slides_ttl() {
    let h = home().await;
    let item = codex_item(3);
    assert!(cache_codex_reasoning_replay_items("gpt-5.4", "session-home", &[item.clone()]));
    let key = format!("cpa:codex:reasoning-replay:{}:{}", sha("gpt-5.4"), sha("session-home"));
    assert_eq!(h.kv.ttl(&key), Some(Duration::from_secs(3600)));
    assert_eq!(decode_stored(&h.kv.get(&key).unwrap()), vec![item.clone()]);

    let items = get_codex_reasoning_replay_items_required("gpt-5.4", "session-home").unwrap().unwrap();
    assert_eq!(items, vec![item]);
    assert!(get_codex_reasoning_replay_items_required("gpt-5.4", "missing").unwrap().is_none());
    delete_codex_reasoning_replay_item_required("gpt-5.4", "session-home").unwrap();
    assert!(h.kv.get(&key).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_home_append_preserves_cumulative_turns_and_dedups() {
    let _h = home().await;
    let first = vec![turn("turn-1"), codex_item(11)];
    let second = vec![turn("turn-2"), codex_item(12)];
    assert!(append_codex_reasoning_replay_items_best_effort("gpt-5.4", "append", &first));
    assert!(append_codex_reasoning_replay_items_best_effort("gpt-5.4", "append", &second));
    assert!(append_codex_reasoning_replay_items_best_effort("gpt-5.4", "append", &second));
    let items = get_codex_reasoning_replay_items_required("gpt-5.4", "append").unwrap().unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(cpa_json::parse(&items[0]).g("id").str(), "turn-1");
    assert_eq!(cpa_json::parse(&items[2]).g("id").str(), "turn-2");
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_home_append_cas_preserves_concurrent_turns() {
    let _h = home().await;
    let mut tasks = Vec::new();
    for n in 0..16u8 {
        tasks.push(tokio::spawn(async move {
            let items = vec![turn(&format!("turn-{n}")), codex_item(30 + n)];
            append_codex_reasoning_replay_items_best_effort("gpt-5.4", "concurrent", &items)
        }));
    }
    for task in tasks {
        assert!(task.await.unwrap());
    }
    let items = get_codex_reasoning_replay_items_required("gpt-5.4", "concurrent").unwrap().unwrap();
    assert_eq!(items.len(), 32);
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_caches_reject_empty_scope_without_touching_home() {
    let h = home().await;
    assert!(get_codex_reasoning_replay_items_required("", "s").unwrap().is_none());
    assert!(!cache_codex_reasoning_replay_items("gpt-5.4", "", &[codex_item(6)]));
    delete_codex_reasoning_replay_item_required("gpt-5.4", "").unwrap();
    assert!(get_kimi_thinking_replay_with_snapshot_required("", "s").unwrap().0.is_none());
    assert!(h._mock.commands().is_empty(), "no KV command for an empty scope: {:?}", h._mock.commands());
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_home_store_reports_backend_errors_and_keeps_previous_entry() {
    let h = home().await;
    // A plain function_call is a replay anchor (reasoning would need Grok-shaped encrypted content).
    let call = br#"{"type":"function_call","call_id":"c1","name":"run","arguments":"{}"}"#.to_vec();
    assert_eq!(store_xai_reasoning_replay_items("grok-4", "s", &[call.clone()]), XaiReasoningReplayStoreStatus::Stored);
    let key = format!("cpa:xai:reasoning-replay:{}:{}", sha("grok-4"), sha("s"));
    assert_eq!(decode_stored(&h.kv.get(&key).unwrap()), vec![call.clone()]);
    assert_eq!(
        store_xai_reasoning_replay_items("grok-4", "s", &[br#"{"type":"message"}"#.to_vec()]),
        XaiReasoningReplayStoreStatus::NoReplayableState
    );
    h.client.set_heartbeat_ok_for_tests(false);
    assert_eq!(store_xai_reasoning_replay_items("grok-4", "s", &[call]), XaiReasoningReplayStoreStatus::BackendError);
    assert!(get_xai_reasoning_replay_items_required("grok-4", "s").is_err());
}

fn ag_item(signature: &str) -> Vec<u8> {
    format!(r#"{{"type":"thought_signature","contentIndex":1,"partIndex":0,"thoughtSignature":"{signature}"}}"#).into_bytes()
}

const AG_MODEL: &str = "gemini-3.6-flash-high";

#[tokio::test(flavor = "multi_thread")]
async fn antigravity_home_absent_snapshot_is_fenced_by_a_tombstone() {
    let h = home().await;
    let (items, snapshot) = get_antigravity_reasoning_replay_items_with_snapshot_required(AG_MODEL, "absent");
    assert!(items.unwrap().is_none());
    let key = format!("cpa:antigravity:reasoning-replay:{}:{}", sha(AG_MODEL), sha("absent"));
    let stored = decode_stored(&h.kv.get(&key).unwrap());
    let marker = cpa_json::parse(&stored[0]);
    assert_eq!(marker.g("type").str(), "cpa_antigravity_replay_generation");
    assert!(marker.g("deleted").bool());
    // The key expires or is rewritten: the stale snapshot must not publish.
    h.kv.put(&key, serde_json::to_vec(&Vec::<String>::new()).unwrap());
    let item = ag_item("home-absent-stale-signature-123456");
    let swapped = replace_antigravity_reasoning_replay_items_if_unchanged(AG_MODEL, "absent", &snapshot, &[item]).unwrap();
    assert!(!swapped);
}

#[tokio::test(flavor = "multi_thread")]
async fn antigravity_home_conditional_mutation_rejects_stale_snapshot() {
    let _h = home().await;
    let (old, new, stale) = (ag_item("old-home-signature-123456"), ag_item("new-home-signature-123456"), ag_item("stale-home-signature-123456"));
    assert!(cache_antigravity_reasoning_replay_items(AG_MODEL, "stale", &[old]));
    let (found, snapshot) = get_antigravity_reasoning_replay_items_with_snapshot_required(AG_MODEL, "stale");
    assert!(found.unwrap().is_some());
    assert!(cache_antigravity_reasoning_replay_items(AG_MODEL, "stale", &[new]));
    assert!(!replace_antigravity_reasoning_replay_items_if_unchanged(AG_MODEL, "stale", &snapshot, &[stale]).unwrap());
    assert!(!delete_antigravity_reasoning_replay_items_if_unchanged(AG_MODEL, "stale", &snapshot).unwrap());
    let items = get_antigravity_reasoning_replay_items(AG_MODEL, "stale").unwrap();
    assert_eq!(items.len(), 1);
    assert!(String::from_utf8_lossy(&items[0]).contains("new-home-signature"));
}

#[tokio::test(flavor = "multi_thread")]
async fn antigravity_home_descendant_chain_is_accepted_and_non_prefix_rotates_the_branch() {
    let _h = home().await;
    let (prefix, middle, latest) = (
        ag_item("home-descendant-prefix-123456"),
        ag_item("home-descendant-middle-123456"),
        ag_item("home-descendant-latest-123456"),
    );
    assert!(cache_antigravity_reasoning_replay_items(AG_MODEL, "descendant", &[prefix.clone()]));
    let (_, stale) = get_antigravity_reasoning_replay_items_with_snapshot_required(AG_MODEL, "descendant");
    let (_, first) = get_antigravity_reasoning_replay_items_with_snapshot_required(AG_MODEL, "descendant");
    assert!(replace_antigravity_reasoning_replay_items_if_unchanged(AG_MODEL, "descendant", &first, &[prefix.clone(), middle.clone()]).unwrap());
    // `stale` read the one-item chain; the current value only extends it on the same branch.
    assert!(replace_antigravity_reasoning_replay_items_if_unchanged(AG_MODEL, "descendant", &stale, &[prefix, middle, latest]).unwrap());
    assert_eq!(get_antigravity_reasoning_replay_items(AG_MODEL, "descendant").unwrap().len(), 3);

    let (old, new, newest) = (ag_item("non-prefix-home-old-123456"), ag_item("non-prefix-home-new-123456"), ag_item("non-prefix-home-latest-123456"));
    assert!(cache_antigravity_reasoning_replay_items(AG_MODEL, "rotate", &[old]));
    let (_, a) = get_antigravity_reasoning_replay_items_with_snapshot_required(AG_MODEL, "rotate");
    let (_, b) = get_antigravity_reasoning_replay_items_with_snapshot_required(AG_MODEL, "rotate");
    assert!(replace_antigravity_reasoning_replay_items_if_unchanged(AG_MODEL, "rotate", &a, &[new.clone()]).unwrap());
    assert!(
        !replace_antigravity_reasoning_replay_items_if_unchanged(AG_MODEL, "rotate", &b, &[new, newest]).unwrap(),
        "a descendant of the replaced chain must not cross the non-prefix reset"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn antigravity_home_values_stay_legacy_array_readable() {
    let h = home().await;
    assert!(cache_antigravity_reasoning_replay_items(AG_MODEL, "legacy", &[ag_item("legacy-readable-signature-123456")]));
    let key = format!("cpa:antigravity:reasoning-replay:{}:{}", sha(AG_MODEL), sha("legacy"));
    let stored = decode_stored(&h.kv.get(&key).unwrap());
    assert_eq!(stored.len(), 2);
    assert_eq!(cpa_json::parse(&stored[0]).g("type").str(), "cpa_antigravity_replay_generation");
    assert!(String::from_utf8_lossy(&stored[1]).contains("legacy-readable-signature"));
}

#[tokio::test(flavor = "multi_thread")]
async fn antigravity_home_without_cas_reports_the_unsupported_error() {
    let guard = LOCK.lock().await;
    home_kv::install();
    let kv = FakeKv::new();
    let handler_kv = kv.clone();
    let mock = MockHome::start(move |args| {
        if args[0].eq_ignore_ascii_case("CAS") { t::err("ERR unknown command 'cas'") } else { handler_kv.handle(args) }
    })
    .await;
    let cfg = cpa_config::HomeConfig { enabled: true, host: "127.0.0.1".into(), port: i64::from(mock.port()), ..Default::default() };
    let client = Arc::new(Client::new(cfg));
    client.set_test_operation_timeout(Duration::from_secs(2));
    client.set_heartbeat_ok_for_tests(true);
    cpa_home::kv::set_current(client);

    let (items, snapshot) = get_antigravity_reasoning_replay_items_with_snapshot_required(AG_MODEL, "no-cas");
    let err = items.unwrap_err();
    assert!(err.is_compare_and_swap_unsupported(), "{err}");
    // The failed read still returns a loaded snapshot, so later conditional writes stay guarded.
    let err = delete_antigravity_reasoning_replay_items_if_unchanged(AG_MODEL, "no-cas", &snapshot).unwrap_err();
    assert!(err.is_compare_and_swap_unsupported(), "{err}");
    drop(guard);
}

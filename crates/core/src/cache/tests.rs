//! Cache tests: a differential corpus of seeded operation sequences recorded from the Go caches,
//! plus the Go unit tests that pin TTL, eviction and snapshot semantics (driven by a mock clock).
//!
//! `testdata/golden.jsonl.gz` is produced by `testdata/regen.sh` from the Go generator
//! `testdata/zz_cache_golden_test.go.txt`.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use cpa_json::{Value, json};
use flate2::read::GzDecoder;

use super::*;

const HOUR: Duration = Duration::from_secs(3600);

fn golden() -> Vec<Value> {
    let gz = include_bytes!("testdata/golden.jsonl.gz");
    let mut text = String::new();
    GzDecoder::new(&gz[..]).read_to_string(&mut text).expect("gunzip golden");
    text.lines().map(|line| serde_json::from_str(line).expect("golden line")).collect()
}

fn strings(v: &Value) -> Vec<Vec<u8>> {
    v.as_array()
        .map(|a| a.iter().map(|s| s.as_str().unwrap().as_bytes().to_vec()).collect())
        .unwrap_or_default()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn texts(items: &Option<Vec<Vec<u8>>>) -> Value {
    match items {
        None => Value::Null,
        Some(items) => Value::Array(items.iter().map(|i| Value::String(text(i))).collect()),
    }
}

/// Replays one recorded sequence against fresh Rust caches and returns the per-op outputs.
fn replay(seq: &Value) -> Vec<Value> {
    let clock = Clock::mock();
    let kind = seq["cache"].as_str().unwrap();
    let signature = SignatureCache::new(clock.clone());
    let codex = CodexReasoningReplayCache::new(clock.clone());
    let xai = XaiReasoningReplayCache::new(clock.clone());
    let antigravity = AntigravityReasoningReplayCache::new(clock.clone());
    let kimi = KimiThinkingReplayCache::new(clock.clone());
    let claude = ClaudeThinkingReplayCache::new(clock.clone());
    let mut kimi_snaps: Vec<KimiThinkingReplaySnapshot> = vec![Default::default(); 3];
    let mut ag_snaps: Vec<AntigravityReasoningReplaySnapshot> = vec![Default::default(); 3];

    let mut outs = Vec::new();
    if kind == "modelGroups" {
        for g in seq["groups"].as_array().unwrap() {
            outs.push(json!(get_model_group(g["model"].as_str().unwrap())));
        }
        return outs;
    }
    for op in seq["ops"].as_array().unwrap() {
        let s = |k: &str| op[k].as_str().unwrap_or_default().to_string();
        let (model, session) = (s("model"), s("session"));
        let items = strings(&op["items"]);
        let slot = op["slot"].as_u64().unwrap_or(0) as usize;
        let out = match (kind, s("op").as_str()) {
            ("signature", "cache") => json!(signature.cache_signature_best_effort(&model, &s("text"), &s("sig"))),
            ("signature", "get") => json!([signature.get_cached_signature_required(&model, &s("text")), true]),
            ("signature", "delete") => {
                signature.delete_cached_signature_required(&model, &s("text"));
                json!(true)
            }
            ("signature", "clear") => {
                signature.clear(&model);
                Value::Null
            }
            ("signature", "hasValid") => json!(has_valid_signature(&model, &s("sig"))),

            ("codex", "cache") => json!(codex.cache_items(&model, &session, &items)),
            ("codex", "append") => json!(codex.append_items_best_effort(&model, &session, &items)),
            ("codex", "getItems") => {
                let got = codex.get_items(&model, &session);
                json!([texts(&got), got.is_some()])
            }
            ("codex", "getItem") => {
                let got = codex.get_item(&model, &session);
                json!([got.as_deref().map(text).unwrap_or_default(), got.is_some()])
            }
            ("codex", "delete") => {
                codex.delete_item(&model, &session);
                Value::Null
            }
            ("codex", "clear") => {
                codex.clear();
                Value::Null
            }

            ("xai", "store") => json!(match xai.store_items(&model, &session, &items) {
                XaiReasoningReplayStoreStatus::InvalidArgs => 0,
                XaiReasoningReplayStoreStatus::Stored => 1,
                XaiReasoningReplayStoreStatus::NoReplayableState => 2,
                XaiReasoningReplayStoreStatus::BackendError => 3,
            }),
            ("xai", "getItems") => {
                let got = xai.get_items(&model, &session);
                json!([texts(&got), got.is_some()])
            }
            ("xai", "getItem") => {
                let got = xai.get_item(&model, &session);
                json!([got.as_deref().map(text).unwrap_or_default(), got.is_some()])
            }
            ("xai", "delete") => {
                xai.delete_item(&model, &session);
                Value::Null
            }

            ("antigravity", "cache") => json!(antigravity.cache_items(&model, &session, &items)),
            ("antigravity", "snap") => {
                let (got, snap) = antigravity.get_items_with_snapshot_required(&model, &session);
                ag_snaps[slot] = snap;
                json!([texts(&got), got.is_some(), true])
            }
            ("antigravity", "replace") => {
                let r = antigravity.replace_items_if_unchanged(&model, &session, &ag_snaps[slot], &items);
                match r {
                    Ok(ok) => json!([ok, null]),
                    Err(e) => json!([false, e.to_string()]),
                }
            }
            ("antigravity", "deleteIf") => json!([antigravity.delete_items_if_unchanged(&model, &session, &ag_snaps[slot]), null]),
            ("antigravity", "getItems") => {
                let got = antigravity.get_items(&model, &session);
                json!([texts(&got), got.is_some()])
            }
            ("antigravity", "getItem") => {
                let got = antigravity.get_item(&model, &session);
                json!([got.as_deref().map(text).unwrap_or_default(), got.is_some()])
            }
            ("antigravity", "delete") => {
                antigravity.delete_item(&model, &session);
                Value::Null
            }

            ("kimi", "cache") => json!(kimi.cache_best_effort(&model, &session, s("content").as_bytes())),
            ("kimi", "snap") => {
                let (got, snap) = kimi.get_with_snapshot_required(&model, &session);
                kimi_snaps[slot] = snap;
                json!([got.as_deref().map(text).unwrap_or_default(), got.is_some(), true])
            }
            ("kimi", "replace") => json!([kimi.replace_if_unchanged(&model, &session, &kimi_snaps[slot], s("content").as_bytes()), null]),
            ("kimi", "deleteIf") => json!([kimi.delete_if_unchanged(&model, &session, &kimi_snaps[slot]), null]),
            ("kimi", "get") => {
                let got = kimi.get_required(&model, &session);
                json!([got.as_deref().map(text).unwrap_or_default(), got.is_some()])
            }
            ("kimi", "delete") => {
                kimi.delete_required(&model, &session);
                Value::Null
            }

            ("claude", "cache") => json!(claude.cache_best_effort(&model, &session, s("content").as_bytes())),
            ("claude", "snap") => {
                let (got, snap) = claude.get_with_snapshot_required(&model, &session);
                kimi_snaps[slot] = snap;
                json!([texts(&got), got.is_some(), true])
            }
            ("claude", "replace") => json!([claude.replace_if_unchanged(&model, &session, &kimi_snaps[slot], s("content").as_bytes()), null]),
            ("claude", "deleteIf") => json!([claude.delete_if_unchanged(&model, &session, &kimi_snaps[slot]), null]),
            ("claude", "get") => {
                let got = claude.get_required(&model, &session);
                json!([texts(&got), got.is_some()])
            }
            ("claude", "delete") => {
                claude.delete_required(&model, &session);
                Value::Null
            }
            other => panic!("unknown golden op {other:?}"),
        };
        outs.push(out);
    }
    outs
}

/// Items are compared as compact JSON (the Go bytes differ only in string escaping choices);
/// kimi/claude content is stored verbatim and compared exactly.
fn normalize(v: &Value, exact: bool) -> Value {
    match v {
        Value::String(s) if !exact && cpa_json::valid(s.as_bytes()) => {
            Value::String(cpa_json::to_string(&cpa_json::parse_str(s)))
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| normalize(v, exact)).collect()),
        other => other.clone(),
    }
}

#[test]
fn matches_go_golden_sequences() {
    let sequences = golden();
    let mut mismatches = 0;
    let mut shown = Vec::new();
    for seq in &sequences {
        let exact = matches!(seq["cache"].as_str().unwrap(), "kimi" | "claude" | "signature" | "modelGroups");
        let got = replay(seq);
        let want: Vec<Value> = if seq["cache"] == "modelGroups" {
            seq["groups"].as_array().unwrap().iter().map(|g| g["group"].clone()).collect()
        } else {
            seq["out"].as_array().unwrap().clone()
        };
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            if normalize(g, exact) != normalize(w, exact) {
                mismatches += 1;
                if shown.len() < 6 {
                    shown.push(format!(
                        "{} op#{i} {}\n  want {w}\n  got  {g}",
                        seq["cache"],
                        seq.get("ops").map(|o| o[i].to_string()).unwrap_or_default()
                    ));
                }
                break; // later ops depend on state; one mismatch per sequence is enough
            }
        }
    }
    assert!(mismatches == 0, "{mismatches} sequences differ:\n{}", shown.join("\n"));
}

// ---- signature cache

#[test]
fn signature_cache_ttl_slides_on_read_and_expires() {
    let clock = Clock::mock();
    let cache = SignatureCache::new(clock.clone());
    let sig = "s".repeat(60);
    assert!(cache.cache_signature_best_effort("claude-opus", "think", &sig));

    clock.advance(Duration::from_secs(2 * 3600 + 59 * 60));
    assert_eq!(cache.get_cached_signature_required("claude-sonnet", "think"), sig, "same group, within ttl");
    // The read refreshed the timestamp, so another 2h59m later it is still valid.
    clock.advance(Duration::from_secs(2 * 3600 + 59 * 60));
    assert_eq!(cache.get_cached_signature_required("claude-opus", "think"), sig);
    // Exactly the TTL is still valid; one second more is expired and removed.
    clock.advance(SIGNATURE_CACHE_TTL);
    assert_eq!(cache.get_cached_signature_required("claude-opus", "think"), sig);
    clock.advance(SIGNATURE_CACHE_TTL + Duration::from_secs(1));
    assert_eq!(cache.get_cached_signature_required("claude-opus", "think"), "");
    assert_eq!(cache.group_len("claude"), Some(0), "expired entry is removed, the bucket stays until purge");
    cache.purge_expired();
    assert_eq!(cache.group_len("claude"), None);
}

#[test]
fn signature_cache_gemini_misses_return_skip_sentinel() {
    let cache = SignatureCache::new(Clock::mock());
    let sentinel = "skip_thought_signature_validator";
    assert_eq!(cache.get_cached_signature_required("gemini-3", ""), sentinel);
    assert_eq!(cache.get_cached_signature_required("gemini-3", "never cached"), sentinel);
    assert_eq!(cache.get_cached_signature_required("claude-3", ""), "");
    assert!(!cache.cache_signature_best_effort("gemini-3", "t", "too short"));
    assert!(!cache.cache_signature_best_effort("gemini-3", "", &"s".repeat(60)));
    assert!(has_valid_signature("gemini-3", sentinel));
    assert!(!has_valid_signature("claude-3", sentinel));
}

#[test]
fn signature_cache_purge_drops_only_expired_entries() {
    let clock = Clock::mock();
    let cache = SignatureCache::new(clock.clone());
    let sig = "s".repeat(60);
    cache.cache_signature_best_effort("claude-a", "old", &sig);
    clock.advance(Duration::from_secs(2 * 3600));
    cache.cache_signature_best_effort("claude-a", "new", &sig);
    cache.cache_signature_best_effort("gpt-5", "old-gpt", &sig);
    clock.advance(Duration::from_secs(3600 + 1));
    cache.purge_expired();
    assert_eq!(cache.group_len("claude"), Some(1));
    assert_eq!(cache.group_len("gpt"), Some(1));
    clock.advance(Duration::from_secs(2 * 3600));
    cache.purge_expired();
    assert_eq!(cache.group_len("claude"), None);
    assert_eq!(cache.group_len("gpt"), None);
}

#[test]
fn signature_mode_toggles_round_trip() {
    // Process-wide atomics: only this test touches them.
    assert!(signature_cache_enabled());
    assert!(!signature_bypass_strict_mode());
    set_signature_cache_enabled(false);
    set_signature_bypass_strict_mode(true);
    assert!(!signature_cache_enabled());
    assert!(signature_bypass_strict_mode());
    set_signature_cache_enabled(true);
    set_signature_bypass_strict_mode(false);
    assert!(signature_cache_enabled());
}

// ---- bounded LRU

#[test]
fn bounded_lru_evicts_least_recently_used() {
    let evicted = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let sink = evicted.clone();
    let cache = BoundedLru::<String, String>::new(
        2,
        Some(Box::new(move |k, v| sink.lock().push(format!("{k}={v}")))),
    );
    assert_eq!(cache.get_or_add("a".into(), || "A".into()), "A");
    cache.get_or_add("b".into(), || "B".into());
    assert_eq!(cache.get(&"a".into()), Some("A".into()));
    cache.get_or_add("c".into(), || "C".into());

    assert_eq!(cache.get(&"b".into()), None, "least recently used entry b was not evicted");
    assert_eq!(cache.len(), 2);
    assert_eq!(*evicted.lock(), vec!["b=B".to_string()]);
    assert!(cache.delete(&"a".into()));
    assert!(!cache.delete(&"a".into()));
    assert_eq!(*evicted.lock(), vec!["b=B".to_string(), "a=A".to_string()]);
}

#[test]
fn bounded_lru_creates_one_value_per_key_concurrently() {
    let cache = Arc::new(BoundedLru::<String, usize>::new(2, None));
    let creates = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (cache, creates) = (cache.clone(), creates.clone());
            std::thread::spawn(move || {
                cache.get_or_add("key".into(), || {
                    creates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    42
                })
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 42);
    }
    assert_eq!(creates.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// ---- codex / xai replay

fn gpt_sig(seed: u8) -> String {
    // Fernet-shaped payload: version 0x80, 8 byte timestamp, 16 byte IV, 16 byte ciphertext, 32 byte HMAC.
    let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
    payload[0] = 0x80;
    for (i, b) in payload.iter_mut().enumerate().skip(9) {
        *b = seed.wrapping_add(i as u8);
    }
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)
}

fn codex_item(seed: u8) -> Vec<u8> {
    format!(
        r#"{{"type":"reasoning","summary":[],"content":null,"encrypted_content":"{}"}}"#,
        gpt_sig(seed)
    )
    .into_bytes()
}

#[test]
fn codex_scopes_by_model_and_session_and_normalizes() {
    let cache = CodexReasoningReplayCache::new(Clock::mock());
    let item = codex_item(7);
    let noisy = format!(
        r#"{{"type":"reasoning","summary":[{{"x":1}}],"content":"c","encrypted_content":"{}"}}"#,
        gpt_sig(7)
    );
    assert!(cache.cache_items("gpt-5.4", "session-a", &[noisy.into_bytes()]));
    assert!(cache.get_item("gpt-5.5", "session-a").is_none(), "no hit across models");
    assert!(cache.get_item("gpt-5.4", "session-b").is_none(), "no hit across sessions");
    assert_eq!(cache.get_item("gpt-5.4", "session-a").unwrap(), item);
    assert!(!cache.cache_items("gpt-5.4", "", &[item]), "empty session is rejected");
}

#[test]
fn codex_rejects_invalid_items() {
    let cache = CodexReasoningReplayCache::new(Clock::mock());
    for bad in [
        r#"{"type":"reasoning","encrypted_content":"gAAAAnot-a-fernet"}"#,
        r#"{"type":"reasoning","encrypted_content":5}"#,
        r#"{"type":"function_call","name":"f","arguments":"{}"}"#,
        r#"{"type":"unknown"}"#,
    ] {
        assert!(!cache.cache_items("m", "s", &[bad.as_bytes().to_vec()]), "{bad}");
    }
    assert!(cache.get_items("m", "s").is_none());
}

#[test]
fn codex_ttl_slides_and_append_discards_expired_state() {
    let clock = Clock::mock();
    let cache = CodexReasoningReplayCache::new(clock.clone());
    let turn = |id: &str| format!(r#"{{"type":"{CODEX_REASONING_REPLAY_TURN_TYPE}","id":"{id}"}}"#).into_bytes();
    assert!(cache.append_items_best_effort("m", "s", &[turn("t1"), codex_item(1)]));
    clock.advance(Duration::from_secs(50 * 60));
    assert_eq!(cache.get_items("m", "s").unwrap().len(), 2, "read refreshes the ttl");
    clock.advance(Duration::from_secs(50 * 60));
    assert!(cache.append_items_best_effort("m", "s", &[turn("t2"), codex_item(2)]));
    assert_eq!(cache.get_items("m", "s").unwrap().len(), 4);
    // A duplicate turn id is not appended twice.
    assert!(cache.append_items_best_effort("m", "s", &[turn("t2"), codex_item(3)]));
    assert_eq!(cache.get_items("m", "s").unwrap().len(), 4);

    clock.advance(HOUR + Duration::from_secs(1));
    assert!(cache.get_items("m", "s").is_none(), "expired");
    assert!(cache.append_items_best_effort("m", "s", &[turn("t3"), codex_item(4)]));
    assert_eq!(cache.get_items("m", "s").unwrap().len(), 2, "append after expiry starts over");
    assert_eq!(cache.get_item("m", "s").unwrap(), codex_item(4), "get_item skips turn markers");
}

#[test]
fn codex_append_bounds_turns_per_entry() {
    let cache = CodexReasoningReplayCache::new(Clock::mock());
    for turn in 0..=CODEX_REASONING_REPLAY_CACHE_MAX_TURNS_PER_ENTRY {
        let marker = format!(r#"{{"type":"{CODEX_REASONING_REPLAY_TURN_TYPE}","id":"turn-{turn}"}}"#);
        assert!(cache.append_items_best_effort("m", "s", &[marker.into_bytes(), codex_item(turn as u8)]));
    }
    let items = cache.get_items("m", "s").unwrap();
    assert_eq!(items.len(), CODEX_REASONING_REPLAY_CACHE_MAX_TURNS_PER_ENTRY * 2);
    assert_eq!(cpa_json::parse(&items[0])["id"], "turn-1");
}

#[test]
fn codex_batch_evicts_oldest_when_full() {
    let clock = Clock::mock();
    let cache = CodexReasoningReplayCache::new(clock.clone());
    let item = codex_item(9);
    for i in 0..=CODEX_REASONING_REPLAY_CACHE_MAX_ENTRIES {
        clock.advance(Duration::from_millis(1));
        assert!(cache.cache_items("gpt-5.4", &format!("session-{i}"), &[item.clone()]));
    }
    assert_eq!(
        cache.entry_count(),
        CODEX_REASONING_REPLAY_CACHE_MAX_ENTRIES + 1 - CODEX_REASONING_REPLAY_CACHE_EVICT_BATCH_SIZE
    );
    assert!(cache.get_item("gpt-5.4", "session-0").is_none(), "oldest entries go first");
    assert!(cache.get_item("gpt-5.4", &format!("session-{}", CODEX_REASONING_REPLAY_CACHE_MAX_ENTRIES)).is_some());
}

fn grok_sig() -> String {
    // High-entropy 64 byte payload that the Grok validator accepts (LCG bytes, no envelope).
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let bytes: Vec<u8> = (0..64)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect();
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD_NO_PAD, bytes)
}

#[test]
fn xai_stores_reasoning_and_messages_but_needs_an_anchor() {
    let cache = XaiReasoningReplayCache::new(Clock::mock());
    let grok = grok_sig();
    let reasoning = format!(r#"{{"type":"reasoning","encrypted_content":"{grok}"}}"#).into_bytes();
    let message = br#"{"type":"message","role":"Assistant","content":[{"type":"output_text","text":"hi"},{"type":"refusal","refusal":"no"},{"type":"image"}]}"#.to_vec();

    assert_eq!(cache.store_items("", "s", &[reasoning.clone()]), XaiReasoningReplayStoreStatus::InvalidArgs);
    assert_eq!(cache.store_items("m", "s", &[message.clone()]), XaiReasoningReplayStoreStatus::NoReplayableState);
    let codex = format!(r#"{{"type":"reasoning","encrypted_content":"{}"}}"#, gpt_sig(1)).into_bytes();
    assert_eq!(cache.store_items("m", "s", &[codex]), XaiReasoningReplayStoreStatus::NoReplayableState, "GPT blobs are not Grok");
    assert_eq!(cache.store_items("m", "s", &[message, reasoning]), XaiReasoningReplayStoreStatus::Stored);
    let items = cache.get_items("m", "s").unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        text(&items[0]),
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"},{"type":"refusal","refusal":"no"}]}"#
    );
}

#[test]
fn xai_ttl_expires() {
    let clock = Clock::mock();
    let cache = XaiReasoningReplayCache::new(clock.clone());
    let call = br#"{"type":"function_call","call_id":"c","name":"f","arguments":"{}"}"#.to_vec();
    assert!(cache.cache_items("m", "s", &[call]));
    clock.advance(HOUR);
    assert!(cache.get_items("m", "s").is_some(), "exactly the ttl is still valid");
    clock.advance(HOUR + Duration::from_secs(1));
    assert!(cache.get_items("m", "s").is_none());
}

// ---- antigravity replay snapshot protocol

const AG_MODEL: &str = "gemini-3.6-flash-high";

fn ag_item(signature: &str) -> Vec<u8> {
    format!(r#"{{"type":"thought_signature","contentIndex":1,"partIndex":0,"thoughtSignature":"{signature}"}}"#).into_bytes()
}

#[test]
fn antigravity_stale_snapshot_cannot_replace_or_delete_newer_state() {
    let cache = AntigravityReasoningReplayCache::new(Clock::mock());
    let (old, new, stale) = (ag_item("old-local-signature-123456"), ag_item("new-local-signature-123456"), ag_item("stale-local-signature-123456"));
    assert!(cache.cache_items(AG_MODEL, "stale-local", &[old]));
    let (_, snapshot) = cache.get_items_with_snapshot_required(AG_MODEL, "stale-local");
    assert!(cache.cache_items(AG_MODEL, "stale-local", &[new]));
    assert_eq!(cache.replace_items_if_unchanged(AG_MODEL, "stale-local", &snapshot, &[stale]), Ok(false));
    assert!(!cache.delete_items_if_unchanged(AG_MODEL, "stale-local", &snapshot));
    let items = cache.get_items(AG_MODEL, "stale-local").unwrap();
    assert_eq!(items.len(), 1);
    assert!(text(&items[0]).contains("new-local-signature"), "newer state was lost");
}

#[test]
fn antigravity_non_prefix_replace_rotates_branch() {
    let cache = AntigravityReasoningReplayCache::new(Clock::mock());
    let (old, new, latest) = (ag_item("non-prefix-old-signature-123456"), ag_item("non-prefix-new-signature-123456"), ag_item("non-prefix-latest-signature-123456"));
    assert!(cache.cache_items(AG_MODEL, "np", &[old]));
    let (_, first) = cache.get_items_with_snapshot_required(AG_MODEL, "np");
    let (_, stale) = cache.get_items_with_snapshot_required(AG_MODEL, "np");
    assert_eq!(cache.replace_items_if_unchanged(AG_MODEL, "np", &first, &[new.clone()]), Ok(true));
    assert_eq!(
        cache.replace_items_if_unchanged(AG_MODEL, "np", &stale, &[new, latest]),
        Ok(false),
        "stale descendant crossed a non-prefix reset"
    );
}

#[test]
fn antigravity_descendant_chain_is_accepted() {
    let cache = AntigravityReasoningReplayCache::new(Clock::mock());
    let (prefix, middle, latest) = (ag_item("descendant-prefix-signature-123456"), ag_item("descendant-middle-signature-123456"), ag_item("descendant-latest-signature-123456"));
    assert!(cache.cache_items(AG_MODEL, "d", &[prefix.clone()]));
    let (_, stale) = cache.get_items_with_snapshot_required(AG_MODEL, "d");
    let (_, first) = cache.get_items_with_snapshot_required(AG_MODEL, "d");
    assert_eq!(cache.replace_items_if_unchanged(AG_MODEL, "d", &first, &[prefix.clone(), middle.clone()]), Ok(true));
    assert_eq!(cache.replace_items_if_unchanged(AG_MODEL, "d", &stale, &[prefix, middle, latest]), Ok(true));
    assert_eq!(cache.get_items(AG_MODEL, "d").unwrap().len(), 3);
}

#[test]
fn antigravity_descendant_merge_rejects_reset_branch_aba() {
    let cache = AntigravityReasoningReplayCache::new(Clock::mock());
    let (prefix, middle, stale_latest) = (ag_item("reset-prefix-signature-123456"), ag_item("reset-middle-signature-123456"), ag_item("reset-stale-signature-123456"));
    assert!(cache.cache_items(AG_MODEL, "aba", &[prefix.clone()]));
    let (_, stale) = cache.get_items_with_snapshot_required(AG_MODEL, "aba");
    let (_, first) = cache.get_items_with_snapshot_required(AG_MODEL, "aba");
    assert_eq!(cache.replace_items_if_unchanged(AG_MODEL, "aba", &first, &[prefix.clone(), middle]), Ok(true));
    let (_, current) = cache.get_items_with_snapshot_required(AG_MODEL, "aba");
    assert!(cache.delete_items_if_unchanged(AG_MODEL, "aba", &current), "branch reset");
    let (_, reset) = cache.get_items_with_snapshot_required(AG_MODEL, "aba");
    assert_eq!(cache.replace_items_if_unchanged(AG_MODEL, "aba", &reset, &[prefix.clone()]), Ok(true));
    assert_eq!(
        cache.replace_items_if_unchanged(AG_MODEL, "aba", &stale, &[prefix, stale_latest]),
        Ok(false),
        "stale descendant crossed the reset branch"
    );
}

#[test]
fn antigravity_tombstones_fence_stale_first_writers() {
    let cache = AntigravityReasoningReplayCache::new(Clock::mock());
    let (_, stale) = cache.get_items_with_snapshot_required(AG_MODEL, "first");
    let (_, clear) = cache.get_items_with_snapshot_required(AG_MODEL, "first");
    assert!(cache.delete_items_if_unchanged(AG_MODEL, "first", &clear));
    let item = ag_item("stale-first-writer-signature-123456");
    assert_eq!(cache.replace_items_if_unchanged(AG_MODEL, "first", &stale, &[item.clone()]), Ok(false));

    // Even after the tombstone itself is evicted, the epoch bump keeps the stale writer out.
    cache.evict_oldest_for_test(1);
    assert_eq!(cache.replace_items_if_unchanged(AG_MODEL, "first", &stale, &[item]), Ok(false));
}

#[test]
fn antigravity_unrelated_eviction_does_not_block_absent_snapshot() {
    let clock = Clock::mock();
    let cache = AntigravityReasoningReplayCache::new(clock.clone());
    assert!(cache.cache_items(AG_MODEL, "older-live-entry", &[ag_item("evicted-live-signature-123456")]));
    clock.advance(Duration::from_secs(60));
    let (_, snapshot) = cache.get_items_with_snapshot_required(AG_MODEL, "untouched-absent-session");
    // The live entry is the oldest; evict it. The absent session's tombstone is newer and stays.
    cache.evict_oldest_for_test(1);
    let first = ag_item("first-write-after-unrelated-eviction-123456");
    // The eviction epoch moved, but the snapshot's own tombstone is still present, so its
    // revision still matches.
    assert_eq!(cache.replace_items_if_unchanged(AG_MODEL, "untouched-absent-session", &snapshot, &[first]), Ok(true));
}

#[test]
fn antigravity_tombstones_and_reservations_stay_within_entry_bound() {
    let cache = AntigravityReasoningReplayCache::new(Clock::mock());
    for index in 0..=ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ENTRIES {
        cache.delete_item(AG_MODEL, &format!("tombstone-{index}"));
    }
    assert!(cache.entry_count() <= ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ENTRIES);

    let cache = AntigravityReasoningReplayCache::new(Clock::mock());
    let mut latest = String::new();
    for index in 0..=ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ENTRIES {
        latest = format!("absent-reservation-{index}");
        let (got, _) = cache.get_items_with_snapshot_required(AG_MODEL, &latest);
        assert!(got.is_none());
    }
    assert!(cache.entry_count() <= ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ENTRIES);
    assert_eq!(cache.is_tombstone_for_test(AG_MODEL, &latest), Some(true), "latest reservation was evicted");
}

#[test]
fn antigravity_ttl_expiry_installs_a_fresh_tombstone() {
    let clock = Clock::mock();
    let cache = AntigravityReasoningReplayCache::new(clock.clone());
    assert!(cache.cache_items(AG_MODEL, "ttl", &[ag_item("ttl-signature-1234567890")]));
    clock.advance(HOUR);
    assert!(cache.get_items(AG_MODEL, "ttl").is_some(), "exactly the ttl is still valid");
    clock.advance(HOUR + Duration::from_secs(1));
    let (items, snapshot) = cache.get_items_with_snapshot_required(AG_MODEL, "ttl");
    assert!(items.is_none());
    assert_eq!(cache.is_tombstone_for_test(AG_MODEL, "ttl"), Some(true));
    // The snapshot is of the fresh tombstone, so a first write through it succeeds.
    let item = ag_item("ttl-second-signature-1234567890");
    assert_eq!(cache.replace_items_if_unchanged(AG_MODEL, "ttl", &snapshot, &[item]), Ok(true));
}

#[test]
fn antigravity_normalization_and_limits() {
    let cache = AntigravityReasoningReplayCache::new(Clock::mock());
    let skip = br#"{"type":"thought_signature","thoughtSignature":"skip_thought_signature_validator"}"#.to_vec();
    assert!(!cache.cache_items(AG_MODEL, "n", &[skip]));
    assert!(!cache.cache_items(AG_MODEL, "n", &[br#"{"type":"thought_signature","thoughtSignature":"short"}"#.to_vec()]));
    let part = br#"{"type":"function_call_part","functionCall":{"id":"fc1","name":"g","args":{"z":[1]}},"contentIndex":2.9}"#.to_vec();
    assert!(cache.cache_items(AG_MODEL, "n", &[part]));
    assert_eq!(
        text(&cache.get_item(AG_MODEL, "n").unwrap()),
        r#"{"type":"function_call_part","call_id":"fc1","name":"g","args":{"z":[1]},"contentIndex":2}"#
    );
    let too_many = vec![ag_item("many-signature-1234567890"); ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ITEMS_PER_ENTRY + 1];
    assert!(!cache.cache_items(AG_MODEL, "many", &too_many));
    let bad = cache.replace_items_if_unchanged(AG_MODEL, "many", &Default::default(), &too_many);
    assert_eq!(bad.unwrap_err().to_string(), "invalid antigravity reasoning replay items");
}

// ---- kimi / claude replay

const KIMI_OLD: &[u8] = br#"[{"type":"thinking","signature":"old"}]"#;
const KIMI_NEW: &[u8] = br#"[{"type":"thinking","signature":"new"}]"#;
const KIMI_STALE: &[u8] = br#"[{"type":"thinking","signature":"stale"}]"#;

#[test]
fn kimi_conditional_delete_and_replace_keep_newer_content() {
    let cache = KimiThinkingReplayCache::new(Clock::mock());
    assert!(cache.cache_best_effort("k3", "del", KIMI_OLD));
    let (_, snapshot) = cache.get_with_snapshot_required("k3", "del");
    assert!(cache.cache_best_effort("k3", "del", KIMI_NEW));
    assert!(cache.cache_best_effort("k3", "del", KIMI_OLD), "repeated bytes, newer generation");
    assert!(!cache.delete_if_unchanged("k3", "del", &snapshot), "stale snapshot deleted newer content");
    assert_eq!(cache.get_required("k3", "del").unwrap(), KIMI_OLD);

    let (got, miss) = cache.get_with_snapshot_required("k3", "rep");
    assert!(got.is_none());
    assert!(cache.cache_best_effort("k3", "rep", KIMI_NEW));
    assert!(!cache.replace_if_unchanged("k3", "rep", &miss, KIMI_STALE), "stale snapshot replaced concurrent content");
    assert_eq!(cache.get_required("k3", "rep").unwrap(), KIMI_NEW);
}

#[test]
fn kimi_tombstone_fences_concurrent_miss() {
    let cache = KimiThinkingReplayCache::new(Clock::mock());
    let (first_content, first) = cache.get_with_snapshot_required("k3", "fence");
    let (second_content, second) = cache.get_with_snapshot_required("k3", "fence");
    assert!(first_content.is_none() && second_content.is_none());
    assert!(cache.delete_if_unchanged("k3", "fence", &first));
    assert!(!cache.replace_if_unchanged("k3", "fence", &second, KIMI_STALE), "stale miss crossed a newer tombstone");
}

#[test]
fn kimi_tracks_aggregate_bytes_and_rejects_oversized_content() {
    let cache = KimiThinkingReplayCache::new(Clock::mock());
    let (first, second) = (br#"[{"type":"thinking","signature":"first"}]"#, br#"[{"type":"thinking","signature":"second"}]"#);
    assert!(cache.cache_best_effort("k3", "bytes-1", first));
    assert!(cache.cache_best_effort("k3", "bytes-2", second));
    assert_eq!(cache.total_bytes(), first.len() + second.len());
    cache.delete_required("k3", "bytes-1");
    assert_eq!(cache.total_bytes(), second.len());
    cache.clear();
    assert_eq!(cache.total_bytes(), 0);

    let mut oversized = vec![b' '; KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY + 1];
    oversized[0] = b'[';
    *oversized.last_mut().unwrap() = b']';
    assert!(!cache.cache_best_effort("k3", "oversized", &oversized));
}

#[test]
fn kimi_ttl_and_purge() {
    let clock = Clock::mock();
    let cache = KimiThinkingReplayCache::new(clock.clone());
    assert!(cache.cache_best_effort("k3", "ttl", KIMI_OLD));
    assert!(cache.cache_best_effort("k3", "ttl2", KIMI_OLD));
    clock.advance(HOUR);
    assert!(cache.get_required("k3", "ttl").is_some(), "exactly the ttl is still valid; the read refreshes it");
    clock.advance(Duration::from_secs(30 * 60));
    cache.purge_expired();
    assert_eq!(cache.entry_count(), 1, "the unread entry is purged, the refreshed one kept");
    clock.advance(Duration::from_secs(31 * 60));
    assert!(cache.get_required("k3", "ttl").is_none(), "expired");
}

#[test]
fn claude_appends_assistant_turns_and_dedupes_equal_json() {
    let cache = ClaudeThinkingReplayCache::new(Clock::mock());
    let first = br#"[{"type":"thinking","thinking":"first","signature":"sig-1"},{"type":"tool_use","id":"toolu-1","name":"Read","input":{"path":"one"}}]"#;
    let second = br#"[{"type":"thinking","thinking":"second","signature":"sig-2"},{"type":"tool_use","id":"toolu-2","name":"Read","input":{"path":"two"}}]"#;
    assert!(cache.cache_best_effort("claude:auth:model", "execution:multi-turn", first));
    let (_, snapshot) = cache.get_with_snapshot_required("claude:auth:model", "execution:multi-turn");
    assert!(cache.replace_if_unchanged("claude:auth:model", "execution:multi-turn", &snapshot, second));
    let contents = cache.get_required("claude:auth:model", "execution:multi-turn").unwrap();
    assert_eq!(contents, vec![first.to_vec(), second.to_vec()], "ordering preserved");

    // The same turn re-serialized (whitespace, key order) is a duplicate; a different number text is not.
    let (_, snapshot) = cache.get_with_snapshot_required("claude:auth:model", "execution:multi-turn");
    let reordered = br#"[ {"signature":"sig-2","thinking":"second","type":"thinking"}, {"input":{"path":"two"},"name":"Read","id":"toolu-2","type":"tool_use"} ]"#;
    assert!(cache.replace_if_unchanged("claude:auth:model", "execution:multi-turn", &snapshot, reordered));
    assert_eq!(cache.get_required("claude:auth:model", "execution:multi-turn").unwrap().len(), 2);
}

#[test]
fn claude_session_is_bounded_to_the_newest_turns() {
    let cache = ClaudeThinkingReplayCache::new(Clock::mock());
    assert!(cache.cache_best_effort("m", "s", br#"[{"n":0}]"#));
    for n in 1..=CLAUDE_THINKING_REPLAY_CACHE_MAX_TURNS_PER_SESSION + 5 {
        let (_, snapshot) = cache.get_with_snapshot_required("m", "s");
        assert!(cache.replace_if_unchanged("m", "s", &snapshot, format!(r#"[{{"n":{n}}}]"#).as_bytes()));
    }
    let contents = cache.get_required("m", "s").unwrap();
    assert_eq!(contents.len(), CLAUDE_THINKING_REPLAY_CACHE_MAX_TURNS_PER_SESSION);
    assert_eq!(text(&contents[0]), r#"[{"n":6}]"#);
}

#[test]
fn claude_clear_does_not_clear_kimi_state() {
    let (claude, kimi) = (ClaudeThinkingReplayCache::new(Clock::mock()), KimiThinkingReplayCache::new(Clock::mock()));
    let (kimi_content, claude_content) = (br#"[{"type":"thinking","signature":"kimi"}]"#, br#"[{"type":"thinking","signature":"claude"}]"#);
    assert!(kimi.cache_best_effort("shared-model", "execution:shared-session", kimi_content));
    assert!(claude.cache_best_effort("shared-model", "execution:shared-session", claude_content));
    claude.clear();
    assert_eq!(kimi.get_required("shared-model", "execution:shared-session").unwrap(), kimi_content);
    assert!(claude.get_required("shared-model", "execution:shared-session").is_none());
    assert_eq!(claude.entry_count(), 1, "the read installed a tombstone");
}

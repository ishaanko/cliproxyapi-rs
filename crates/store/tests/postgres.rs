//! Postgres store against a real database (set `CPA_STORE_DOCKER=1`; skipped otherwise).
//! Ports postgres_cooldown_store_test.go (against real SQL instead of a fake driver) and
//! disabled_login_save_test.go, plus config/auth mirroring round trips.

mod common;

use chrono::{Duration, Utc};
use cpa_auth::storage::{ClaudeTokenStorage, TokenStorage};
use cpa_auth::store::{SaveOptions, Store};
use cpa_auth::types::Status;
use cpa_auth::Auth;
use cpa_runtime::conductor::CooldownStateRecord;
use cpa_store::{PostgresStore, PostgresStoreConfig};

fn open(dsn: &str, spool: &std::path::Path, schema: &str) -> PostgresStore {
    PostgresStore::new(PostgresStoreConfig {
        dsn: dsn.to_string(),
        schema: schema.to_string(),
        spool_dir: spool.to_string_lossy().into_owned(),
        ..Default::default()
    })
    .expect("connect")
}

fn auth(id: &str, provider: &str, email: &str) -> Auth {
    let mut a = Auth::default();
    a.id = id.into();
    a.file_name = id.into();
    a.provider = provider.into();
    a.metadata.insert("type".into(), provider.into());
    a.metadata.insert("email".into(), email.into());
    a
}

#[test]
fn bootstrap_save_list_delete_roundtrip() {
    let Some((_c, dsn)) = common::postgres() else { return };
    let dir = tempfile::tempdir().unwrap();
    let example = dir.path().join("config.example.yaml");
    std::fs::write(&example, "port: 8317\r\napi-keys:\r\n  - k\r\n").unwrap();

    let store = open(&dsn, &dir.path().join("spool"), "cpa_test");
    store.bootstrap(example.to_str().unwrap()).unwrap();
    // The example seeds the database (CRLF normalized there); the spool keeps the raw copy.
    assert_eq!(std::fs::read_to_string(store.config_path()).unwrap(), "port: 8317\r\napi-keys:\r\n  - k\r\n");

    let mut a = auth("team/codex-a.json", "codex", "a@example.com");
    a.metadata.insert("weight".into(), 3.into());
    let path = store.save(&mut a, SaveOptions::default()).unwrap().unwrap();
    assert_eq!(path, store.auth_dir().join("team/codex-a.json"));
    assert_eq!(a.attr("source_backend"), "postgres");
    // Saving identical metadata again is a no-op that still reports the path.
    assert_eq!(store.save(&mut a, SaveOptions::default()).unwrap(), Some(path.clone()));

    let mut disabled = auth("off.json", "claude", "b@example.com");
    disabled.disabled = true;
    store.save(&mut disabled, SaveOptions { creation_intent: true }).unwrap().unwrap();

    let listed = store.list().unwrap();
    let ids: Vec<&str> = listed.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["off.json", "team/codex-a.json"]);
    assert_eq!(listed[0].status, Status::Disabled);
    assert!(listed[0].disabled);
    assert_eq!(listed[1].provider, "codex");
    assert_eq!(listed[1].label, "a@example.com");
    assert_eq!(listed[1].attr("email"), "a@example.com");

    // A fresh store on a new spool pulls everything back from the database.
    let other = open(&dsn, &dir.path().join("spool2"), "cpa_test");
    other.bootstrap("").unwrap();
    assert!(other.auth_dir().join("team/codex-a.json").exists());
    assert!(other.auth_dir().join("off.json").exists());
    assert_eq!(std::fs::read_to_string(other.config_path()).unwrap(), "port: 8317\napi-keys:\n  - k\n");

    // Go quirk: ids containing a separator are used as-is (relative to the cwd), so only the
    // record is removed here; bare names also remove the spool file.
    other.delete("team/codex-a.json").unwrap();
    other.delete("off.json").unwrap();
    assert!(!other.auth_dir().join("off.json").exists());
    store.delete("off.json").unwrap();
    assert!(store.list().unwrap().is_empty());
    let mut again = auth("keep.json", "claude", "k@example.com");
    store.save(&mut again, SaveOptions::default()).unwrap();
    let remaining = store.list().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, "keep.json");
}

#[test]
fn persist_files_and_config_follow_the_spool() {
    use cpa_runtime::service::StorePersister;
    let Some((_c, dsn)) = common::postgres() else { return };
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dsn, dir.path(), "");
    store.bootstrap("").unwrap();

    let file = store.auth_dir().join("x.json");
    std::fs::write(&file, r#"{"type":"codex","email":"x@y"}"#).unwrap();
    store.persist_auth_files("Sync auth x.json", &[file.to_string_lossy().into_owned()]).unwrap();
    assert_eq!(store.list().unwrap().len(), 1);

    // An empty or missing file removes the record.
    std::fs::write(&file, "").unwrap();
    store.persist_auth_files("Sync auth x.json", &["x.json".into()]).unwrap();
    assert!(store.list().unwrap().is_empty());

    std::fs::write(store.config_path(), "debug: true\n").unwrap();
    store.persist_config().unwrap();
    let again = open(&dsn, &dir.path().join("elsewhere"), "");
    again.bootstrap("").unwrap();
    assert_eq!(std::fs::read_to_string(again.config_path()).unwrap(), "debug: true\n");
    std::fs::remove_file(store.config_path()).unwrap();
    store.persist_config().unwrap();
}

#[test]
fn disabled_login_reaches_token_storage_only_with_creation_intent() {
    let Some((_c, dsn)) = common::postgres() else { return };
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dsn, dir.path(), "");
    store.bootstrap("").unwrap();

    let mut a = auth("canonical-disabled.json", "claude", "d@example.com");
    a.disabled = true;
    a.storage = Some(TokenStorage::Claude(ClaudeTokenStorage { email: "d@example.com".into(), ..Default::default() }));
    assert_eq!(store.save(&mut a, SaveOptions::default()).unwrap(), None);
    assert!(!store.auth_dir().join("canonical-disabled.json").exists());

    let path = store.save(&mut a, SaveOptions { creation_intent: true }).unwrap().unwrap();
    assert!(path.exists());
    assert_eq!(store.list().unwrap().len(), 1);
}

fn record(auth_id: &str, model: &str, updated_at: chrono::DateTime<Utc>) -> CooldownStateRecord {
    CooldownStateRecord { auth_id: auth_id.into(), model: model.into(), updated_at: Some(updated_at), ..Default::default() }
}

#[test]
fn cooldown_store_save_load() {
    let Some((_c, dsn)) = common::postgres() else { return };
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dsn, dir.path(), "");
    store.ensure_schema().unwrap();
    let cooldown = store.cooldown_store();

    let next_retry = "2026-03-15T12:00:00Z".parse::<chrono::DateTime<Utc>>().unwrap();
    let records = vec![CooldownStateRecord {
        provider: "codex".into(),
        auth_id: "account-1".into(),
        model: "gpt-test".into(),
        status: "error".into(),
        next_retry_after: Some(next_retry),
        reason: "rate limited".into(),
        updated_at: Some(next_retry - Duration::minutes(1)),
        ..Default::default()
    }];
    cooldown.save(&records).unwrap();
    assert_eq!(cooldown.load().unwrap(), records);

    // A zero UpdatedAt is normalized to "now".
    let mut zero = record("account-2", "gpt-test", Utc::now());
    zero.updated_at = None;
    cooldown.save(&[zero]).unwrap();
    let loaded = cooldown.load().unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].updated_at.is_some());

    cooldown.save(&[]).unwrap();
    assert!(cooldown.load().unwrap().is_empty());
}

#[test]
fn cooldown_store_merges_concurrent_instances() {
    let Some((_c, dsn)) = common::postgres() else { return };
    let dir = tempfile::tempdir().unwrap();
    let new_instance = || {
        let s = open(&dsn, dir.path(), "");
        s.ensure_schema().unwrap();
        s
    };
    let (a_store, b_store, stale_store) = (new_instance(), new_instance(), new_instance());
    let (a, b, stale) = (a_store.cooldown_store(), b_store.cooldown_store(), stale_store.cooldown_store());
    a.load().unwrap();
    b.load().unwrap();

    let updated_at = Utc::now() - Duration::minutes(1);
    let record_a = record("account-a", "model-a", updated_at);
    let record_b = record("account-b", "model-b", updated_at);
    a.save(&[record_a.clone()]).unwrap();
    b.save(&[record_b.clone()]).unwrap();
    assert_eq!(stale.load().unwrap().len(), 2);

    // A stale instance that no longer lists account-a must not delete a newer row.
    let newer_a = record("account-a", "model-a", updated_at + Duration::hours(1));
    a.save(&[newer_a]).unwrap();
    stale.save(&[record_b.clone()]).unwrap();
    let resurrect_store = new_instance();
    let resurrect = resurrect_store.cooldown_store();
    let active = resurrect.load().unwrap();
    assert_eq!(active.len(), 2);

    // After a real delete, an instance holding the old snapshot must not bring the row back.
    a.save(&[]).unwrap();
    resurrect.save(&active).unwrap();
    let reader_store = new_instance();
    let loaded = reader_store.cooldown_store().load().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].auth_id, "account-b");
}

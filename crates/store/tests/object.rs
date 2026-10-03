//! Object store against a signature-checking S3 gateway (set `CPA_STORE_DOCKER=1`; skipped
//! otherwise), plus the offline parts of disabled_login_save_test.go.

mod common;

use cpa_auth::storage::{ClaudeTokenStorage, TokenStorage};
use cpa_auth::store::{SaveOptions, Store};
use cpa_auth::Auth;
use cpa_runtime::service::StorePersister;
use cpa_store::{ObjectStoreConfig, ObjectTokenStore};

fn open(endpoint: &str, bucket: &str, root: &std::path::Path, prefix: &str, secret: &str) -> ObjectTokenStore {
    ObjectTokenStore::new(ObjectStoreConfig {
        endpoint: endpoint.into(),
        bucket: bucket.into(),
        access_key: common::S3_ACCESS_KEY.into(),
        secret_key: secret.into(),
        prefix: prefix.into(),
        local_root: root.to_string_lossy().into_owned(),
        path_style: true,
        ..Default::default()
    })
    .expect("object store")
}

fn auth(id: &str, email: &str) -> Auth {
    let mut a = Auth::default();
    a.id = id.into();
    a.file_name = id.into();
    a.provider = "codex".into();
    a.metadata.insert("type".into(), "codex".into());
    a.metadata.insert("email".into(), email.into());
    a
}

#[test]
fn constructor_validates_required_fields() {
    let cfg = |f: fn(&mut ObjectStoreConfig)| {
        let mut c = ObjectStoreConfig {
            endpoint: "e".into(),
            bucket: "b".into(),
            access_key: "a".into(),
            secret_key: "s".into(),
            ..Default::default()
        };
        f(&mut c);
        ObjectTokenStore::new(c).err().map(|e| e.to_string())
    };
    assert_eq!(cfg(|c| c.endpoint = " ".into()).as_deref(), Some("object store: endpoint is required"));
    assert_eq!(cfg(|c| c.bucket.clear()).as_deref(), Some("object store: bucket is required"));
    assert_eq!(cfg(|c| c.access_key.clear()).as_deref(), Some("object store: access key is required"));
    assert_eq!(cfg(|c| c.secret_key.clear()).as_deref(), Some("object store: secret key is required"));
}

#[test]
fn disabled_login_reaches_token_storage_only_with_creation_intent() {
    let dir = tempfile::tempdir().unwrap();
    // Saving a missing disabled credential never touches the network before the upload step, so
    // an unreachable endpoint is enough to observe the storage-reached boundary.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("127.0.0.1:{}", l.local_addr().unwrap().port())
    };
    let store = open(&closed, "b", dir.path(), "", "s");
    let mut a = auth("canonical-disabled.json", "d@example.com");
    a.disabled = true;
    a.storage = Some(TokenStorage::Claude(ClaudeTokenStorage::default()));
    assert_eq!(store.save(&mut a, SaveOptions::default()).unwrap(), None);
    assert!(!store.auth_dir().join("canonical-disabled.json").exists());

    // With intent the credential file is written, then the (unreachable) upload fails.
    let err = store.save(&mut a, SaveOptions { creation_intent: true }).unwrap_err();
    assert!(err.to_string().contains("put object auths/canonical-disabled.json"), "{err}");
    assert!(store.auth_dir().join("canonical-disabled.json").exists());
}

#[test]
fn bootstrap_save_list_delete_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let Some((_c, endpoint)) = common::s3(dir.path()) else { return };
    let example = dir.path().join("config.example.yaml");
    std::fs::write(&example, "port: 8317\n").unwrap();

    // Bad credentials are rejected by the gateway's signature check.
    let bad = open(&endpoint, "cpa-bucket", &dir.path().join("bad"), "", "wrong-secret");
    assert!(bad.bootstrap("").is_err());

    let store = open(&endpoint, "cpa-bucket", &dir.path().join("a"), "team/x", common::S3_SECRET_KEY);
    store.bootstrap(example.to_str().unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(store.config_path()).unwrap(), "port: 8317\n");

    let mut a = auth("sub/codex-a.json", "a@example.com");
    let path = store.save(&mut a, SaveOptions::default()).unwrap().unwrap();
    assert_eq!(path, store.auth_dir().join("sub/codex-a.json"));
    assert_eq!(a.attr("source_backend"), "objectstore");
    let mut plain = auth("b", "b@example.com");
    store.save(&mut plain, SaveOptions::default()).unwrap();
    assert!(store.auth_dir().join("b.json").exists());

    // A second instance with its own spool downloads config and auths from the bucket.
    let other = open(&endpoint, "cpa-bucket", &dir.path().join("b"), "team/x", common::S3_SECRET_KEY);
    other.bootstrap("").unwrap();
    assert_eq!(std::fs::read_to_string(other.config_path()).unwrap(), "port: 8317\n");
    let ids: Vec<String> = other.list().unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(ids, ["b.json", "sub/codex-a.json"]);

    // Edits in the spool propagate through the persister; deletes remove the object.
    std::fs::write(other.config_path(), "port: 9000\r\n").unwrap();
    other.persist_config().unwrap();
    let file = other.auth_dir().join("c.json");
    std::fs::write(&file, r#"{"type":"claude"}"#).unwrap();
    other.persist_auth_files("Sync auth c.json", &["c.json".into()]).unwrap();
    other.delete("b").unwrap();

    let third = open(&endpoint, "cpa-bucket", &dir.path().join("c"), "team/x", common::S3_SECRET_KEY);
    third.bootstrap("").unwrap();
    assert_eq!(std::fs::read_to_string(third.config_path()).unwrap(), "port: 9000\n");
    let ids: Vec<String> = third.list().unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(ids, ["c.json", "sub/codex-a.json"]);

    // Empty config removes the object, so the next bootstrap reseeds from the example.
    std::fs::write(third.config_path(), "").unwrap();
    third.persist_config().unwrap();
    let fourth = open(&endpoint, "cpa-bucket", &dir.path().join("d"), "team/x", common::S3_SECRET_KEY);
    fourth.bootstrap(example.to_str().unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(fourth.config_path()).unwrap(), "port: 8317\n");
}

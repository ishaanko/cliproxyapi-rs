//! Byte-for-byte compatibility with credential files written by the Go app. The fixtures in
//! `tests/fixtures/go` were produced by the real Go token stores (see `fixtures/gen/main.go`);
//! each test saves the same logical credential through the Rust store and compares the bytes.

use std::fs;
use std::path::Path;

use cpa_auth::storage::{
    ClaudeTokenStorage, CodexTokenStorage, KimiTokenStorage, MetaTokenStorage,
    VertexCredentialStorage, XaiTokenStorage,
};
use cpa_auth::{Auth, FileTokenStore, SaveOptions, Status, Store, TokenStorage};
use serde_json::{Value, json};

fn fixture(name: &str) -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/go")
        .join(name);
    fs::read(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

fn meta(v: Value) -> serde_json::Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => unreachable!("object literal"),
    }
}

/// Saves `auth` into a fresh dir and returns the written bytes.
fn saved_bytes(mut auth: Auth) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let store = FileTokenStore::with_dir(dir.path());
    let path = store
        .save(
            &mut auth,
            SaveOptions {
                creation_intent: true,
            },
        )
        .unwrap()
        .expect("saved");
    fs::read(path).unwrap()
}

fn assert_same(name: &str, auth: Auth) {
    let got = saved_bytes(auth);
    let want = fixture(name);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&want),
        "{name} differs from the Go output"
    );
    assert_eq!(got, want);
}

fn auth(id: &str, provider: &str, storage: Option<TokenStorage>, metadata: Value) -> Auth {
    let mut a = Auth::new(id, provider);
    a.storage = storage;
    a.metadata = meta(metadata);
    a
}

#[test]
fn claude_file_matches_go_including_html_escaping_and_legacy_key_rename() {
    let storage = ClaudeTokenStorage {
        access_token: "sk-ant-oat01-AT".into(),
        refresh_token: "sk-ant-ort01-RT".into(),
        last_refresh: "2026-10-01T12:00:00Z".into(),
        email: "user@example.com".into(),
        account_uuid: "acc-1".into(),
        organization_uuid: "org-1".into(),
        organization_name: "Org <One> & Co".into(),
        device_ids: vec!["ab".repeat(32)],
        expire: "2026-10-01T20:00:00Z".into(),
        ..Default::default()
    };
    assert_same(
        "claude.json",
        auth(
            "claude.json",
            "claude",
            Some(TokenStorage::Claude(storage)),
            json!({"email": "user@example.com", "proxy-url": "socks5://127.0.0.1:1080", "priority": 5,
                   "headers": {"X-Custom": "v"}, "note": "a<b>&c"}),
        ),
    );
}

#[test]
fn codex_files_match_go_with_and_without_plan_type() {
    let storage = CodexTokenStorage {
        id_token: "idt".into(),
        access_token: "at".into(),
        refresh_token: "rt".into(),
        account_id: "acct-1".into(),
        last_refresh: "2026-10-01T12:00:00Z".into(),
        email: "dev@example.com".into(),
        expire: "2026-10-01T20:00:00Z".into(),
        plan_type: "plus".into(),
        ..Default::default()
    };
    assert_same(
        "codex.json",
        auth(
            "codex.json",
            "codex",
            Some(TokenStorage::Codex(storage)),
            json!({"email": "dev@example.com", "plan_type": "plus", "websockets": true, "weight": 3}),
        ),
    );

    let mut disabled = auth(
        "codex-noplan.json",
        "codex",
        Some(TokenStorage::Codex(CodexTokenStorage {
            access_token: "at".into(),
            email: "e@x".into(),
            ..Default::default()
        })),
        json!({}),
    );
    disabled.disabled = true;
    assert_same("codex-noplan.json", disabled);
}

#[test]
fn xai_kimi_vertex_meta_pretty_files_match_go() {
    assert_same(
        "xai.json",
        auth(
            "xai.json",
            "xai",
            Some(TokenStorage::Xai(XaiTokenStorage {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                id_token: "idt".into(),
                token_type: "Bearer".into(),
                expires_in: 3600,
                expire: "2026-10-01T13:00:00Z".into(),
                last_refresh: "2026-10-01T12:00:00Z".into(),
                email: "grok@x.ai".into(),
                subject: "sub-1".into(),
                base_url: "https://api.x.ai/v1".into(),
                token_endpoint: "https://auth.x.ai/oauth2/token".into(),
                ..Default::default()
            })),
            json!({"email": "grok@x.ai", "label": "mine"}),
        ),
    );

    assert_same(
        "kimi-ai.json",
        auth(
            "kimi-ai.json",
            "kimi-ai",
            Some(TokenStorage::Kimi(KimiTokenStorage {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                token_type: "Bearer".into(),
                scope: "kimi-code".into(),
                device_id: "dev-1".into(),
                expired: "2026-10-01T13:00:00Z".into(),
                type_: "kimi-ai".into(),
                ..Default::default()
            })),
            json!({"timestamp": 1790000000000i64, "domain": "kimi.ai"}),
        ),
    );

    let sa = meta(json!({
        "type": "service_account", "project_id": "proj", "client_email": "sa@proj.iam",
        "private_key": "placeholder\nnot-a-real-key\n"
    }));
    assert_same(
        "vertex.json",
        auth(
            "vertex.json",
            "vertex",
            Some(TokenStorage::Vertex(VertexCredentialStorage {
                service_account: sa,
                project_id: "proj".into(),
                email: "sa@proj.iam".into(),
                location: "us-central1".into(),
                prefix: "team".into(),
                ..Default::default()
            })),
            json!({"label": "proj (sa@proj.iam)"}),
        ),
    );

    assert_same(
        "meta.json",
        auth(
            "meta.json",
            "meta",
            Some(TokenStorage::Meta(MetaTokenStorage {
                access_token: "key-1".into(),
                dca_token: "dca:tok".into(),
                api_key: "key-1".into(),
                token_type: "Bearer".into(),
                expires_in: 3600,
                dca_expired: "2026-10-01T13:00:00Z".into(),
                dca_expires_at: 1_790_000_000,
                last_refresh: "2026-10-01T12:00:00Z".into(),
                base_url: "https://api.meta.ai/v1".into(),
                email: "m@x.com".into(),
                name: "M".into(),
                ..Default::default()
            })),
            json!({"subs_tier_name": "pro", "is_subs_active": true, "access_token": "must-not-override", "note": "kept"}),
        ),
    );
}

#[test]
fn metadata_only_file_matches_go_compact_encoding() {
    assert_same(
        "antigravity-u@x.com.json",
        auth(
            "antigravity-u@x.com.json",
            "antigravity",
            None,
            json!({"type": "antigravity", "access_token": "ya29.at", "refresh_token": "1//rt", "expires_in": 3599,
                   "timestamp": 1790000000000i64, "expired": "2026-10-01T13:00:00Z", "email": "u@x.com",
                   "project_id": "proj-9", "request-retry": 2, "excluded-models": ["a", "b"]}),
        ),
    );
}

#[test]
fn rust_loads_every_go_fixture_and_derives_go_values() {
    let store =
        FileTokenStore::with_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go"));
    let auths = store.list().unwrap();
    let by_id = |id: &str| {
        auths
            .iter()
            .find(|a| a.id == id)
            .unwrap_or_else(|| panic!("missing {id}"))
    };
    assert_eq!(auths.len(), 8);

    let claude = by_id("claude.json");
    assert_eq!(
        (claude.provider.as_str(), claude.label.as_str()),
        ("claude", "user@example.com")
    );
    assert_eq!(claude.status, Status::Active);
    assert_eq!(claude.proxy_url, "socks5://127.0.0.1:1080");
    // The plain store path does not apply priority/weight (the synthesizer does); metadata keeps it.
    assert!(!claude.attributes.contains_key("priority"));
    assert_eq!(claude.metadata["priority"], 5);
    assert_eq!(claude.attributes["header:X-Custom"], "v");
    assert_eq!(claude.auth_kind(), "oauth");
    assert_eq!(
        claude.expiration_time().unwrap().to_rfc3339(),
        "2026-10-01T20:00:00+00:00"
    );

    let codex = by_id("codex.json");
    assert_eq!(codex.metadata["websockets"], true);
    let disabled = by_id("codex-noplan.json");
    assert!(disabled.disabled);
    assert_eq!(disabled.status, Status::Disabled);

    // Metadata-only credential: expiry comes from `expired`, label from email.
    let ag = by_id("antigravity-u@x.com.json");
    assert_eq!(ag.label, "u@x.com");
    assert_eq!(
        ag.expiration_time().unwrap().to_rfc3339(),
        "2026-10-01T13:00:00+00:00"
    );
    assert_eq!(ag.metadata["request_retry"], 2);

    // The vertex file has no email/label conflicts and keeps its nested service account intact.
    let vertex = by_id("vertex.json");
    assert_eq!(vertex.label, "proj (sa@proj.iam)");
    assert_eq!(
        vertex.metadata["service_account"]["client_email"],
        "sa@proj.iam"
    );
}

#[test]
fn go_written_file_survives_a_rust_rewrite_unchanged() {
    // Loading a Go file and saving the loaded metadata back must not drift (watcher write-back loop).
    let dir = tempfile::tempdir().unwrap();
    for name in ["claude.json", "codex.json", "antigravity-u@x.com.json"] {
        fs::write(dir.path().join(name), fixture(name)).unwrap();
    }
    let store = FileTokenStore::with_dir(dir.path());
    for mut a in store.list().unwrap() {
        let path = store.save(&mut a, SaveOptions::default()).unwrap().unwrap();
        let rewritten = fs::read(&path).unwrap();
        let original = fixture(path.file_name().unwrap().to_str().unwrap());
        let (a, b): (Value, Value) = (
            serde_json::from_slice(&rewritten).unwrap(),
            serde_json::from_slice(&original).unwrap(),
        );
        assert_eq!(a, b, "{} changed semantically on rewrite", path.display());
    }
}

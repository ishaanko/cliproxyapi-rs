//! Ports of the watcher diff tests plus an end-to-end hot reload test.

use std::sync::Arc;
use std::time::Duration;

use cpa_config::diff::*;
use cpa_config::watcher::ConfigWatcher;
use cpa_config::*;

fn contains(changes: &[String], want: &str) {
    assert!(
        changes.iter().any(|c| c == want),
        "missing {want:?} in {changes:#?}"
    );
}

fn cfg(f: impl FnOnce(&mut Config)) -> Config {
    let mut cfg = Config::default();
    f(&mut cfg);
    cfg
}

#[test]
fn change_details_cover_scalars_secrets_and_lists() {
    let old = cfg(|c| {
        c.port = 8080;
        c.auth_dir = "/tmp/auth-old".into();
        c.gemini_key = vec![GeminiKey {
            api_key: "old".into(),
            base_url: "http://old".into(),
            excluded_models: vec!["old-model".into()],
            ..Default::default()
        }];
        c.remote_management.secret_key = "old".into();
        c.remote_management.panel_github_repository = "repo-old".into();
        c.oauth_excluded_models
            .insert("providerA".into(), vec!["m1".into()]);
        c.openai_compatibility = vec![OpenAiCompatibility {
            name: "compat-a".into(),
            api_key_entries: vec![OpenAiCompatibilityApiKey {
                api_key: "k1".into(),
                ..Default::default()
            }],
            models: vec![OpenAiCompatibilityModel {
                name: "m1".into(),
                ..Default::default()
            }],
            ..Default::default()
        }];
    });
    let new = cfg(|c| {
        c.port = 9090;
        c.auth_dir = "/tmp/auth-new".into();
        c.codex.disable_codex_cloaking = true;
        c.gemini_key = vec![GeminiKey {
            api_key: "old".into(),
            base_url: "http://old".into(),
            excluded_models: vec!["old-model".into(), "extra".into()],
            ..Default::default()
        }];
        c.remote_management.allow_remote = true;
        c.remote_management.secret_key = "new".into();
        c.remote_management.panel_github_repository = "repo-new".into();
        c.oauth_excluded_models
            .insert("providerA".into(), vec!["m1".into(), "m2".into()]);
        c.oauth_excluded_models
            .insert("providerB".into(), vec!["x".into()]);
        c.openai_compatibility = vec![
            OpenAiCompatibility {
                name: "compat-a".into(),
                api_key_entries: vec![OpenAiCompatibilityApiKey {
                    api_key: "k1".into(),
                    ..Default::default()
                }],
                models: vec![
                    OpenAiCompatibilityModel {
                        name: "m1".into(),
                        ..Default::default()
                    },
                    OpenAiCompatibilityModel {
                        name: "m2".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            OpenAiCompatibility {
                name: "compat-b".into(),
                api_key_entries: vec![OpenAiCompatibilityApiKey {
                    api_key: "k2".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ];
    });
    let details = build_config_change_details(&old, &new);
    for want in [
        "port: 8080 -> 9090",
        "auth-dir: /tmp/auth-old -> /tmp/auth-new",
        "gemini[0].excluded-models: updated (1 -> 2 entries)",
        "remote-management.allow-remote: false -> true",
        "remote-management.secret-key: updated",
        "codex.disable-codex-cloaking: false -> true",
        "oauth-excluded-models[providera]: updated (1 -> 2 entries)",
        "oauth-excluded-models[providerb]: added (1 entries)",
        "openai-compatibility:",
        "  provider added: compat-b (api-keys=1, models=0)",
        "  provider updated: compat-a (models 1 -> 2)",
    ] {
        contains(&details, want);
    }
    assert!(build_config_change_details(&new, &new).is_empty());

    let created = build_config_change_details(
        &cfg(|c| c.api_keys = vec!["a".into()]),
        &cfg(|c| {
            c.api_keys = vec!["a".into(), "b".into(), "c".into()];
            c.remote_management.secret_key = "new-secret".into();
        }),
    );
    contains(&created, "api-keys count: 1 -> 3");
    contains(&created, "remote-management.secret-key: created");
}

#[test]
fn change_details_redact_endpoint_urls() {
    let with = |scheme_host: &str| {
        cfg(|c| {
            c.gemini_key = vec![GeminiKey {
                base_url: format!("https://{scheme_host}/v1?token=t"),
                ..Default::default()
            }];
            c.remote_management.panel_github_repository =
                format!("https://{scheme_host}/private?token=t");
        })
    };
    let details = build_config_change_details(
        &with("old-user:old-pass@old.example"),
        &with("new-user:new-pass@new.example"),
    );
    contains(
        &details,
        "gemini[0].base-url: https://old.example -> https://new.example",
    );
    contains(
        &details,
        "remote-management.panel-github-repository: https://old.example -> https://new.example",
    );
    let joined = details.join("\n");
    for leaked in ["old-user", "new-pass", "token", "/private", "/v1"] {
        assert!(!joined.contains(leaked), "leaked {leaked:?}: {joined}");
    }
    for (input, want) in [
        ("", "<none>"),
        ("http://[::1", "<redacted>"),
        (
            "http://user:pass@example.com:8080/path?x=1#frag",
            "http://example.com:8080",
        ),
        (
            "socks5://user:pass@192.168.1.1:1080/path?x=1",
            "socks5://192.168.1.1:1080",
        ),
        ("example.com:1234/path?x=1", "example.com:1234"),
        ("/just/path", "<redacted>"),
        ("https://example.com", "https://example.com"),
    ] {
        assert_eq!(format_url(input), want, "{input}");
    }
}

#[test]
fn oauth_channel_diffs_report_affected_channels() {
    use std::collections::BTreeMap;
    let map = |entries: &[(&str, &[&str])]| -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    v.iter().map(|s| (*s).to_string()).collect(),
                )
            })
            .collect()
    };
    let (changes, affected) = diff_oauth_excluded_model_changes(
        &map(&[
            ("ProviderA", &["model-1", "model-2"]),
            ("providerB", &["x"]),
        ]),
        &map(&[
            ("providerA", &["model-1", "model-3"]),
            ("providerC", &["y"]),
        ]),
    );
    contains(
        &changes,
        "oauth-excluded-models[providera]: updated (2 -> 2 entries)",
    );
    contains(&changes, "oauth-excluded-models[providerb]: removed");
    contains(
        &changes,
        "oauth-excluded-models[providerc]: added (1 entries)",
    );
    assert_eq!(affected, ["providera", "providerb", "providerc"]);

    let summary = summarize_excluded_models(&["A".into(), " a ".into(), "B".into(), "b".into()]);
    assert_eq!(summary.count, 2);
    assert!(!summary.hash.is_empty());
    assert_eq!(summarize_excluded_models(&[]), ModelsSummary::default());
    assert_eq!(
        compute_excluded_models_hash(&[" A ".into(), "b".into()]),
        compute_excluded_models_hash(&["B".into(), "a".into()])
    );
    assert_eq!(compute_excluded_models_hash(&[]), "");

    // Unchanged config leaves nothing to rebuild.
    let alias = BTreeMap::from([(
        "codex".to_string(),
        vec![OAuthModelAlias {
            name: "a".into(),
            alias: "b".into(),
            ..Default::default()
        }],
    )]);
    assert!(
        diff_oauth_model_alias_changes(&alias, &alias.clone())
            .0
            .is_empty()
    );
}

#[test]
fn reload_plan_flags_what_the_server_must_redo() {
    let base = Config::default();
    assert!(ReloadPlan::between(None, &base).auth_dir_changed);
    assert_eq!(
        ReloadPlan::between(Some(&base), &base),
        ReloadPlan::default()
    );
    let plan = ReloadPlan::between(Some(&base), &cfg(|c| c.request_retry = 3));
    assert!(plan.force_auth_refresh && !plan.auth_dir_changed);
    let plan = ReloadPlan::between(
        Some(&base),
        &cfg(|c| {
            c.auth_dir = "/elsewhere".into();
            c.oauth_excluded_models
                .insert("codex".into(), vec!["m".into()]);
        }),
    );
    assert!(plan.auth_dir_changed && !plan.force_auth_refresh);
    assert_eq!(plan.affected_oauth_providers, ["codex"]);
}

#[test]
fn model_hashes_are_sensitive_to_routing_fields() {
    let model = |alias: &str| CodexModel {
        name: "m".into(),
        alias: alias.into(),
        ..Default::default()
    };
    assert_eq!(compute_codex_models_hash(&[]), "");
    assert_ne!(
        compute_codex_models_hash(&[model("a")]),
        compute_codex_models_hash(&[model("b")])
    );
    let forced = CodexModel {
        force_mapping: true,
        ..model("a")
    };
    assert_ne!(
        compute_codex_models_hash(&[model("a")]),
        compute_codex_models_hash(&[forced])
    );
    let thinking = CodexModel {
        thinking: Some(ThinkingSupport {
            levels: vec!["high".into()],
            ..Default::default()
        }),
        ..model("a")
    };
    assert_ne!(
        compute_codex_models_hash(&[model("a")]),
        compute_codex_models_hash(&[thinking])
    );
    // Routing order and duplicates are preserved in the routing hash, normalised in the summary.
    assert_ne!(
        compute_codex_models_hash(&[model("a"), model("b")]),
        compute_codex_models_hash(&[model("b"), model("a")])
    );
    assert_eq!(
        summarize_codex_models(&[model("a"), model("b")]),
        summarize_codex_models(&[model("b"), model("a"), model("a")])
    );
}

#[tokio::test]
async fn watcher_publishes_reloaded_snapshots_and_ignores_noise() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "port: 8000\nauth-dir: ~/auths\n").unwrap();
    let initial = Arc::new(load_config(&path).unwrap());
    let watcher = ConfigWatcher::start(&path, Arc::clone(&initial), None)
        .await
        .unwrap();
    let mut rx = watcher.subscribe();
    assert_eq!(watcher.current().port, 8000);

    // A burst of writes is coalesced into one reload with the final content.
    for port in [8001, 8002, 8003] {
        std::fs::write(&path, format!("port: {port}\nauth-dir: ~/auths\n")).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), rx.changed())
        .await
        .expect("reload timed out")
        .unwrap();
    let snapshot = Arc::clone(&rx.borrow_and_update());
    assert_eq!(snapshot.port, 8003);
    assert!(
        !snapshot.auth_dir.starts_with('~'),
        "auth-dir is resolved: {}",
        snapshot.auth_dir
    );
    assert!(!Arc::ptr_eq(&snapshot, &initial));

    // Rewriting identical content, and a config that fails validation, publish nothing.
    std::fs::write(&path, "port: 8003\nauth-dir: ~/auths\n").unwrap();
    assert!(
        !watcher.reload_now().await,
        "unchanged content must be skipped by the hash gate"
    );
    std::fs::write(&path, "trusted-proxies: [definitely-not-an-ip]\n").unwrap();
    assert!(
        !watcher.reload_now().await,
        "invalid config must not replace the snapshot"
    );
    assert_eq!(watcher.current().port, 8003);

    // Fixing the file recovers; an explicit reload works without waiting for fs events.
    std::fs::write(&path, "port: 8100\n").unwrap();
    assert!(watcher.reload_now().await);
    assert_eq!(watcher.current().port, 8100);
}

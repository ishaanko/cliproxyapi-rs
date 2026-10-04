//! Port of Go `config_v8_auth_index_test.go` (issue 6287): `GET /v8/management/config` injects
//! the live credential index into `api-keys` entries as `auth_index`, and the transient field is
//! never persisted. Each scenario of the Go test is its own test here.

mod common;

use axum::http::Method;
use common::Harness;
use common::harness;
use cpa_auth::Auth;
use cpa_runtime::service::synth::{SynthesisContext, synthesize_config_auths};
use serde_json::Value;

const GET: Method = Method::GET;
const PUT: Method = Method::PUT;
const PATCH: Method = Method::PATCH;

/// The credentials the config synthesizes, each with its `auth-index`. With `register` they are
/// also made live in the conductor; without, the index falls back to the synthesized one.
async fn synthesized(h: &Harness, register: bool) -> Vec<Auth> {
    let cfg = h.load();
    let ctx = SynthesisContext {
        config: &cfg,
        auth_dir: &cfg.auth_dir,
        now: chrono::Utc::now(),
    };
    let mut out = Vec::new();
    for auth in synthesize_config_auths(&ctx).unwrap() {
        let mut auth = if register {
            h.manager.register(auth).await.unwrap()
        } else {
            auth
        };
        auth.ensure_index();
        out.push(auth);
    }
    out
}

async fn get_json(h: &Harness, path: &str) -> Value {
    let body = h
        .expect(GET, &format!("/v8/management/config/{path}"), "", 200)
        .await;
    serde_json::from_str(&body).unwrap()
}

/// `auth_index` of `groups[group].keys[key]`, `None` when absent.
fn key_index(groups: &Value, group: usize, key: usize) -> Option<String> {
    groups[group]["keys"][key]["auth_index"]
        .as_str()
        .map(str::to_string)
}

fn with_version(body: &str) -> String {
    format!("config-version: 8\nport: 8317\n{body}")
}

#[tokio::test]
async fn api_keys_expose_auth_index_and_never_persist_it() {
    let raw = with_version(
        "api-keys:
  codex:
    - name: codex-group
      base-url: https://api.openai.invalid
      keys:
        - api-key: sk-codex-1
        - api-key: sk-codex-2
  claude:
    - name: claude-group
      base-url: https://api.anthropic.invalid
      keys:
        - api-key: sk-claude-1
  openai-compatibility:
    - name: compat-provider
      base-url: https://api.compat.invalid
      keys:
        - api-key: sk-compat-1
    - name: keyless-provider
      base-url: https://api.keyless.invalid
      keys: []
",
    );
    let h = harness(&raw);
    let mut expected = std::collections::HashMap::new();
    for auth in synthesized(&h, true).await {
        let api_key = auth.attributes.get("api_key").cloned().unwrap_or_default();
        let name = auth
            .attributes
            .get("compat_name")
            .cloned()
            .unwrap_or_else(|| auth.provider.clone());
        let tag = if api_key.is_empty() { "keyless".to_string() } else { api_key };
        expected.insert(format!("{name}:{tag}"), auth.index.clone());
    }

    let codex = get_json(&h, "api-keys/codex").await;
    assert_eq!(codex.as_array().unwrap().len(), 1);
    for (i, key) in ["sk-codex-1", "sk-codex-2"].iter().enumerate() {
        assert_eq!(key_index(&codex, 0, i), Some(expected[&format!("codex:{key}")].clone()));
    }

    let all = get_json(&h, "api-keys").await;
    assert_eq!(
        all["claude"][0]["keys"][0]["auth_index"].as_str(),
        Some(expected["claude:sk-claude-1"].as_str())
    );
    assert_eq!(all["openai-compatibility"].as_array().unwrap().len(), 2);
    assert_eq!(
        all["openai-compatibility"][1]["auth_index"].as_str(),
        Some(expected["keyless-provider:keyless"].as_str()),
        "keyless group carries a group-level auth_index"
    );

    let root = get_json(&h, "").await;
    assert_eq!(
        root["api-keys"]["codex"][0]["keys"][0]["auth_index"].as_str(),
        Some(expected["codex:sk-codex-1"].as_str())
    );

    let yaml = h
        .expect(GET, "/v8/management/config.yaml", "", 200)
        .await;
    assert!(!yaml.contains("auth_index"), "config.yaml output must not contain auth_index");

    // A PUT of the injected JSON never persists the transient field.
    h.expect(PUT, "/v8/management/config/api-keys/codex", &codex.to_string(), 200).await;
    assert!(!h.read().contains("auth_index"), "saved config.yaml must not contain auth_index");

    let patch = r#"{"api-keys":{"claude":[{"name":"claude-group","base-url":"https://api.anthropic.invalid","keys":[{"api-key":"sk-claude-1","auth_index":"arbitrary-ignore"}]}]}}"#;
    h.expect(PATCH, "/v8/management/config", patch, 200).await;
    assert!(!h.read().contains("auth_index"), "saved config.yaml after PATCH must not contain auth_index");

    // Without live credentials the index falls back to the synthesized one.
    let bare = harness(&raw);
    let mut by_key = std::collections::HashMap::new();
    for auth in synthesized(&bare, false).await {
        by_key.insert(auth.attributes.get("api_key").cloned().unwrap_or_default(), auth.index.clone());
    }
    let codex = get_json(&bare, "api-keys/codex").await;
    assert_eq!(key_index(&codex, 0, 0), Some(by_key["sk-codex-1"].clone()));
}

/// Registers the synthesized credentials, then checks `expect` against a GET of `path`.
async fn assert_indexes(raw: &str, register: bool, path: &str, check: impl FnOnce(&[Auth], &Value)) {
    let h = harness(raw);
    let auths = synthesized(&h, register).await;
    let groups = get_json(&h, path).await;
    check(&auths, &groups);
}

#[tokio::test]
async fn meta_with_omitted_base_url_matches_the_runtime_default() {
    let raw = with_version("api-keys:\n  meta:\n    - name: meta-group\n      keys:\n        - api-key: sk-meta-default-base\n");
    assert_indexes(&raw, true, "api-keys/meta", |auths, groups| {
        assert_eq!(auths.len(), 1);
        assert_eq!(key_index(groups, 0, 0), Some(auths[0].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn explicit_empty_proxy_url_on_a_key_overrides_the_group() {
    let raw = with_version(
        "api-keys:
  vertex:
    - name: vertex-group
      base-url: https://api.vertex.invalid
      proxy-url: http://proxy.group.invalid:8080
      keys:
        - api-key: sk-vertex-inherited
        - api-key: sk-vertex-explicit-empty
          proxy-url: \"\"
",
    );
    assert_indexes(&raw, true, "api-keys/vertex", |auths, groups| {
        assert_eq!(auths.len(), 2);
        assert_eq!(key_index(groups, 0, 0), Some(auths[0].index.clone()));
        assert_eq!(key_index(groups, 0, 1), Some(auths[1].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn plugin_options_and_headers_keep_their_auth_index_fields() {
    let raw = with_version(
        "plugins:
  configs:
    custom-plugin:
      enabled: true
      options:
        auth_index: keep-this-plugin-field
api-keys:
  codex:
    - name: codex-group
      base-url: https://api.openai.invalid
      headers:
        auth_index: keep-group-header
      keys:
        - api-key: sk-codex-headers
          headers:
            auth_index: keep-key-header
",
    );
    let h = harness(&raw);
    let groups = get_json(&h, "api-keys/codex").await;
    h.expect(PUT, "/v8/management/config/api-keys/codex", &groups.to_string(), 200).await;
    let saved = h.read();
    for kept in ["keep-this-plugin-field", "keep-group-header", "keep-key-header"] {
        assert!(saved.contains(kept), "{kept} was stripped! saved config:\n{saved}");
    }
}

#[tokio::test]
async fn duplicate_vertex_keys_share_the_deduplicated_credential() {
    let raw = with_version(
        "api-keys:
  vertex:
    - name: vertex-group
      base-url: https://api.vertex.invalid
      keys:
        - api-key: sk-vertex-shared
        - api-key: sk-vertex-shared
",
    );
    assert_indexes(&raw, true, "api-keys/vertex", |auths, groups| {
        assert_eq!(auths.len(), 1, "runtime deduplicates identical vertex keys");
        assert_eq!(groups[0]["keys"].as_array().unwrap().len(), 2);
        assert_eq!(key_index(groups, 0, 0), Some(auths[0].index.clone()));
        assert_eq!(key_index(groups, 0, 1), Some(auths[0].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn unnormalized_prefix_matches_the_normalized_live_credential() {
    let raw = with_version(
        "api-keys:
  codex:
    - name: codex-team
      prefix: /team/
      base-url: https://api.openai.invalid
      keys:
        - api-key: sk-codex-team
",
    );
    assert_indexes(&raw, true, "api-keys/codex", |auths, groups| {
        assert_eq!(auths.len(), 1);
        assert_eq!(key_index(groups, 0, 0), Some(auths[0].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn gemini_duplicate_keys_map_to_their_deduplicated_credentials() {
    let raw = with_version(
        "api-keys:
  gemini:
    - name: gemini-group
      base-url: https://api.gemini.invalid
      keys:
        - api-key: sk-gemini-a
        - api-key: sk-gemini-a
        - api-key: sk-gemini-b
",
    );
    assert_indexes(&raw, true, "api-keys/gemini", |auths, groups| {
        assert_eq!(auths.len(), 2, "A and B");
        let index_of = |key: &str| {
            auths
                .iter()
                .find(|a| a.attributes.get("api_key").map(String::as_str) == Some(key))
                .map(|a| a.index.clone())
                .unwrap()
        };
        let (a, b) = (index_of("sk-gemini-a"), index_of("sk-gemini-b"));
        assert_ne!(a, b);
        assert_eq!(groups[0]["keys"].as_array().unwrap().len(), 3);
        assert_eq!(key_index(groups, 0, 0), Some(a.clone()));
        assert_eq!(key_index(groups, 0, 1), Some(a));
        assert_eq!(key_index(groups, 0, 2), Some(b));
    })
    .await;
}

#[tokio::test]
async fn openai_compatibility_mixed_empty_key_and_invalid_group() {
    let raw = with_version(
        "api-keys:
  openai-compatibility:
    - name: invalid-group-no-base
      keys:
        - api-key: sk-invalid
    - name: mixed-group
      base-url: https://api.mixed.invalid
      keys:
        - api-key: \"\"
        - api-key: sk-compat-b
",
    );
    assert_indexes(&raw, true, "api-keys/openai-compatibility", |auths, groups| {
        assert_eq!(auths.len(), 2);
        let index_of = |key: &str| {
            auths
                .iter()
                .find(|a| a.attributes.get("api_key").map_or("", String::as_str) == key)
                .map(|a| a.index.clone())
                .unwrap()
        };
        let (empty, b) = (index_of(""), index_of("sk-compat-b"));
        assert_ne!(empty, b);
        assert_eq!(key_index(groups, 0, 0), None, "group without base-url has no index");
        assert_eq!(key_index(groups, 1, 0), Some(empty));
        assert_eq!(key_index(groups, 1, 1), Some(b));
    })
    .await;
}

#[tokio::test]
async fn gemini_base_url_only_entry_gets_an_index() {
    let raw = with_version(
        "api-keys:
  gemini:
    - name: gemini-emulator
      base-url: http://emulator.invalid:8080
      keys:
        - api-key: \"\"
",
    );
    assert_indexes(&raw, false, "api-keys/gemini", |auths, groups| {
        assert_eq!(auths.len(), 1);
        assert!(!auths[0].index.is_empty());
        assert_eq!(key_index(groups, 0, 0), Some(auths[0].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn openai_compatibility_empty_keys_with_different_proxies_stay_distinct() {
    let raw = with_version(
        "api-keys:
  openai-compatibility:
    - name: multi-empty-group
      base-url: https://api.multi.invalid
      keys:
        - api-key: \"\"
          proxy-url: http://proxy1.invalid:8080
        - api-key: \"\"
          proxy-url: http://proxy2.invalid:8080
",
    );
    assert_indexes(&raw, false, "api-keys/openai-compatibility", |auths, groups| {
        assert_eq!(auths.len(), 2);
        assert_ne!(auths[0].index, auths[1].index);
        assert_eq!(key_index(groups, 0, 0), Some(auths[0].index.clone()));
        assert_eq!(key_index(groups, 0, 1), Some(auths[1].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn gemini_empty_keys_dedupe_by_proxy_url() {
    let raw = with_version(
        "api-keys:
  gemini:
    - name: gemini-proxy-group
      base-url: http://emulator.invalid:8080
      keys:
        - api-key: \"\"
          proxy-url: http://proxy1.invalid:8080
        - api-key: \"\"
          proxy-url: http://proxy1.invalid:8080
        - api-key: \"\"
          proxy-url: http://proxy2.invalid:8080
",
    );
    assert_indexes(&raw, true, "api-keys/gemini", |auths, groups| {
        assert_eq!(auths.len(), 2, "P1 and P2");
        assert_ne!(auths[0].index, auths[1].index);
        assert_eq!(key_index(groups, 0, 0), Some(auths[0].index.clone()));
        assert_eq!(key_index(groups, 0, 1), Some(auths[0].index.clone()), "reuses P1, not P2");
        assert_eq!(key_index(groups, 0, 2), Some(auths[1].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn identical_openai_compatibility_groups_do_not_collide() {
    let raw = with_version(
        "api-keys:
  openai-compatibility:
    - name: shared-name
      base-url: https://api.shared.invalid
      keys:
        - api-key: \"\"
          proxy-url: http://proxyA.invalid:8080
    - name: shared-name
      base-url: https://api.shared.invalid
      keys:
        - api-key: \"\"
          proxy-url: http://proxyB.invalid:8080
",
    );
    assert_indexes(&raw, true, "api-keys/openai-compatibility", |auths, groups| {
        assert_eq!(auths.len(), 2);
        assert_ne!(auths[0].index, auths[1].index);
        assert_eq!(groups.as_array().unwrap().len(), 2);
        assert_eq!(key_index(groups, 0, 0), Some(auths[0].index.clone()));
        assert_eq!(key_index(groups, 1, 0), Some(auths[1].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn gemini_empty_keys_dedupe_by_prefix() {
    let raw = with_version(
        "api-keys:
  gemini:
    - name: gemini-prefix-group
      base-url: http://emulator.invalid:8080
      keys:
        - api-key: \"\"
          prefix: /team-a/
        - api-key: \"\"
          prefix: /team-a/
        - api-key: \"\"
          prefix: /team-b/
",
    );
    assert_indexes(&raw, true, "api-keys/gemini", |auths, groups| {
        assert_eq!(auths.len(), 2);
        assert_ne!(auths[0].index, auths[1].index);
        assert_eq!(key_index(groups, 0, 0), Some(auths[0].index.clone()));
        assert_eq!(key_index(groups, 0, 1), Some(auths[0].index.clone()), "reuses A, not B");
        assert_eq!(key_index(groups, 0, 2), Some(auths[1].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn unnamed_openai_compatibility_groups_expose_their_index() {
    let raw = with_version(
        "api-keys:
  openai-compatibility:
    - base-url: https://api.unnamed-keyless.invalid
      keys: []
    - base-url: https://api.unnamed-keyed.invalid
      keys:
        - api-key: sk-unnamed-key
",
    );
    assert_indexes(&raw, true, "api-keys/openai-compatibility", |auths, groups| {
        assert_eq!(auths.len(), 2);
        assert_ne!(auths[0].index, auths[1].index);
        assert_eq!(groups.as_array().unwrap().len(), 2);
        assert_eq!(groups[0]["auth_index"].as_str(), Some(auths[0].index.as_str()));
        assert_eq!(key_index(groups, 1, 0), Some(auths[1].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn claude_empty_key_entry_is_skipped_by_position() {
    let raw = with_version(
        "api-keys:
  claude:
    - name: claude-group
      keys:
        - {}
        - api-key: sk-claude-real-a
        - api-key: sk-claude-real-b
",
    );
    assert_indexes(&raw, true, "api-keys/claude", |auths, groups| {
        assert_eq!(auths.len(), 2, "the empty entry without base-url is skipped");
        assert_ne!(auths[0].index, auths[1].index);
        assert_eq!(groups[0]["keys"].as_array().unwrap().len(), 3);
        assert_eq!(key_index(groups, 0, 0), None);
        assert_eq!(key_index(groups, 0, 1), Some(auths[0].index.clone()));
        assert_eq!(key_index(groups, 0, 2), Some(auths[1].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn codex_empty_keys_with_different_models_are_not_deduplicated() {
    let raw = with_version(
        "api-keys:
  codex:
    - name: codex-models-group
      base-url: https://api.openai.invalid
      keys:
        - api-key: \"\"
          models:
            - name: gpt-5
              alias: gpt-5
        - api-key: \"\"
          models:
            - name: gpt-6
              alias: gpt-6
",
    );
    assert_indexes(&raw, false, "api-keys/codex", |auths, groups| {
        assert_eq!(auths.len(), 2);
        assert_ne!(auths[0].index, auths[1].index);
        assert_eq!(key_index(groups, 0, 0), Some(auths[0].index.clone()));
        assert_eq!(key_index(groups, 0, 1), Some(auths[1].index.clone()));
    })
    .await;
}

#[tokio::test]
async fn meta_skips_empty_and_dca_keys() {
    let raw = with_version(
        "api-keys:
  meta:
    - name: meta-filtered-group
      keys:
        - api-key: \"\"
        - api-key: dca:some-oauth-token
        - api-key: sk-meta-real-a
        - api-key: sk-meta-real-b
",
    );
    assert_indexes(&raw, false, "api-keys/meta", |auths, groups| {
        assert_eq!(auths.len(), 2);
        assert_ne!(auths[0].index, auths[1].index);
        assert_eq!(groups[0]["keys"].as_array().unwrap().len(), 4);
        assert_eq!(key_index(groups, 0, 0), None);
        assert_eq!(key_index(groups, 0, 1), None);
        assert_eq!(key_index(groups, 0, 2), Some(auths[0].index.clone()));
        assert_eq!(key_index(groups, 0, 3), Some(auths[1].index.clone()));
    })
    .await;
}

//! Ports of the Go shared-upstream and historical-alias config tests (`shared_upstream_test.go`,
//! `config_v8_comments_test.go`, `config_v8_save_layout_test.go`).

use cpa_config::*;
use serde_yaml_ng::Value;

fn yaml(text: &str) -> Value {
    serde_yaml_ng::from_str(text).unwrap()
}

fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |v, k| v.as_mapping()?.get(k))
}

/// The shared upstream fields a scoped view must always expose.
fn shared_view(c: &Config) -> Vec<String> {
    let h = &c.claude_header_defaults;
    let x = &c.codex;
    vec![
        c.auth_dir.clone(),
        c.auth_auto_refresh_workers.to_string(),
        x.disable_codex_cloaking.to_string(),
        x.stream_bootstrap_buffering.to_string(),
        x.stream_bootstrap_timeout.clone(),
        x.orphan_delegation_compatibility.to_string(),
        x.model_level_cooling.to_string(),
        x.response_steering.to_string(),
        c.claude.model_level_cooling.to_string(),
        c.claude_code.disable_cloaking_model_list.to_string(),
        c.disable_claude_cloak_mode.to_string(),
        h.user_agent.clone(),
        h.package_version.clone(),
        h.runtime_version.clone(),
        h.os.clone(),
        h.arch.clone(),
        h.timeout.clone(),
        h.timezone.clone(),
        (h.stabilize_device_profile == Some(true)).to_string(),
        c.xai.inject_x_search.to_string(),
    ]
}

#[test]
fn shared_upstream_config_survives_migration_and_snapshots() {
    let legacy = "auth-dir: client-auth
auth-auto-refresh-workers: 3
codex: {disable-codex-cloaking: true, stream-bootstrap-buffering: true, stream-bootstrap-timeout: 10s, orphan-delegation-compatibility: true, model-level-cooling: true, response-steering: true}
claude: {model-level-cooling: true}
claude-code: {disable-cloaking-model-list: true}
disable-claude-cloak-mode: true
claude-header-defaults: {user-agent: client-agent, package-version: 1.2.3, runtime-version: v22.1.0, os: Linux, arch: arm64, timeout: '123', timezone: Asia/Shanghai, stabilize-device-profile: true}
xai: {inject-x-search: true}
oauth: {auth-dir: client-auth, auth-auto-refresh-workers: 3, providers: {codex: {header-defaults: {user-agent: oauth-agent, beta-features: oauth-beta}}}}
api-keys: {codex: [{base-url: https://example.invalid, keys: [{api-key: test-key, disable-codex-cloaking: false}]}]}
";
    let canonical = "upstream:
  codex: {disable-codex-cloaking: true, stream-bootstrap-buffering: true, stream-bootstrap-timeout: 10s, orphan-delegation-compatibility: true, model-level-cooling: true, response-steering: true}
  claude:
    model-level-cooling: true
    disable-cloaking-model-list: true
    disable-claude-cloak-mode: true
    header-defaults: {user-agent: client-agent, package-version: 1.2.3, runtime-version: v22.1.0, os: Linux, arch: arm64, timeout: '123', timezone: Asia/Shanghai, stabilize-device-profile: true}
  xai: {inject-x-search: true}
oauth: {auth-dir: client-auth, auth-auto-refresh-workers: 3, providers: {codex: {header-defaults: {user-agent: oauth-agent, beta-features: oauth-beta}}}}
api-keys: {codex: [{base-url: https://example.invalid, keys: [{api-key: test-key, disable-codex-cloaking: false}]}]}
";
    let historical = "oauth:
  auth-dir: client-auth
  auth-auto-refresh-workers: 3
  providers:
    codex: {disable-codex-cloaking: true, stream-bootstrap-buffering: true, stream-bootstrap-timeout: 10s, orphan-delegation-compatibility: true, model-level-cooling: true, response-steering: true, header-defaults: {user-agent: oauth-agent, beta-features: oauth-beta}}
    claude:
      model-level-cooling: true
      claude-code: {disable-cloaking-model-list: true}
      disable-claude-cloak-mode: true
      header-defaults: {user-agent: client-agent, package-version: 1.2.3, runtime-version: v22.1.0, os: Linux, arch: arm64, timeout: '123', timezone: Asia/Shanghai, stabilize-device-profile: true}
    xai: {inject-x-search: true}
api-keys: {codex: [{base-url: https://example.invalid, keys: [{api-key: test-key, disable-codex-cloaking: false}]}]}
";
    let expected: Vec<String> = [
        "client-auth",
        "3",
        "true",
        "true",
        "10s",
        "true",
        "true",
        "true",
        "true",
        "true",
        "true",
        "client-agent",
        "1.2.3",
        "v22.1.0",
        "Linux",
        "arm64",
        "123",
        "Asia/Shanghai",
        "true",
        "true",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    for (name, raw) in [
        ("legacy", legacy),
        ("upstream", canonical),
        ("historical OAuth", historical),
    ] {
        let mut cfg = parse_config_bytes(raw.as_bytes()).unwrap();
        let (migrated, _) = normalize_config_layout(raw.as_bytes(), true).unwrap();
        validate_v8_config(&migrated).unwrap_or_else(|e| panic!("{name}: {e}"));
        let doc = yaml(std::str::from_utf8(&migrated).unwrap());
        assert!(
            at(&doc, "oauth.auth-dir").is_some()
                && at(&doc, "upstream.claude.header-defaults.timezone").is_some()
                && at(&doc, "oauth.providers.codex.header-defaults.user-agent").is_some(),
            "{name}: migration did not separate shared upstream fields from OAuth headers"
        );
        for (old, _) in v8_alias_paths() {
            assert!(
                at(&doc, old).is_none(),
                "{name}: historical alias {old} survived migration"
            );
        }
        let snapshot = serde_yaml_ng::to_string(&cfg.to_yaml_value().unwrap()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, raw).unwrap();
        save_config_preserve_comments(&path, &mut cfg, true).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        let migrated = String::from_utf8(migrated).unwrap();
        for (what, text) in [
            ("snapshot", &snapshot),
            ("migrated", &migrated),
            ("saved", &saved),
        ] {
            let restored = parse_config_bytes(text.as_bytes()).unwrap();
            for scoped in [restored.clone(), restored.for_api_key().into_owned()] {
                assert_eq!(
                    shared_view(&scoped),
                    expected,
                    "{name}/{what} changed shared configuration"
                );
            }
            assert!(
                restored.codex_key.len() == 1
                    && restored.codex_key[0].disable_codex_cloaking == Some(false),
                "{name}/{what}: explicit key override lost"
            );
            assert!(
                restored
                    .for_api_key()
                    .codex_header_defaults
                    .user_agent
                    .is_empty()
                    && restored.codex_header_defaults.user_agent == "oauth-agent",
                "{name}/{what}: OAuth-only headers lost their scope"
            );
        }
    }
}

#[test]
fn shared_upstream_presence_wins_over_historical_aliases() {
    for value in ["false", "null"] {
        let raw = format!(
            "codex: {{response-steering: true}}\noauth: {{providers: {{codex: {{response-steering: true}}, claude: {{header-defaults: {{user-agent: historical-agent, stabilize-device-profile: true}}}}}}}}\nupstream: {{codex: {{response-steering: {value}}}, claude: {{header-defaults: {{user-agent: '', stabilize-device-profile: {value}}}}}}}\n"
        );
        for migrate in [false, true] {
            let (data, _) = normalize_config_layout(raw.as_bytes(), migrate).unwrap();
            let cfg = parse_config_bytes(&data).unwrap();
            assert!(
                !cfg.codex.response_steering
                    && cfg.claude_header_defaults.user_agent.is_empty()
                    && cfg.auth_auto_refresh_workers == 0
                    && cfg.claude_header_defaults.stabilize_device_profile != Some(true),
                "{value}/migrate={migrate}: historical alias overrode an explicit shared zero value"
            );
            if migrate {
                validate_v8_config(&data).unwrap();
            }
        }
    }
}

#[test]
fn shared_upstream_empty_historical_containers() {
    for (historical, current) in [
        (
            "oauth.providers.claude.header-defaults",
            "upstream.claude.header-defaults",
        ),
        ("oauth.providers.claude.claude-code", "upstream.claude"),
        ("oauth.providers.claude", "upstream.claude"),
        ("oauth.providers.xai", "upstream.xai"),
    ] {
        for value in ["{}", "null"] {
            let raw = historical
                .rsplit('.')
                .fold(value.to_string(), |acc, key| format!("{{{key}: {acc}}}"));
            let (migrated, _) = normalize_config_layout(raw.as_bytes(), true).unwrap();
            validate_v8_config(&migrated).unwrap_or_else(|e| panic!("{historical}/{value}: {e}"));
            let doc = yaml(std::str::from_utf8(&migrated).unwrap());
            assert!(
                at(&doc, historical).is_none() && at(&doc, current).is_some(),
                "{historical}/{value}: empty historical provider container was not migrated:\n{}",
                String::from_utf8_lossy(&migrated)
            );
        }
    }
}

#[test]
fn moved_comments_survive_v0_saves() {
    let raw = "# DOCUMENT HEAD
config-version: 8
oauth: # OAUTH INLINE
  providers: # PROVIDERS INLINE
    xai: # PROVIDER INLINE
      # FIELD HEAD
      inject-x-search: true # FIELD INLINE

      # FIELD FOOT

    # PROVIDER FOOT
server:
  port: 8317 # UNRELATED INLINE
# DOCUMENT FOOT
";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, raw).unwrap();
    let mut cfg = load_config(&path).unwrap();
    for port in [8318, 8319, 8320] {
        cfg.port = port;
        save_config_preserve_comments(&path, &mut cfg, false).unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        validate_v8_config(data.as_bytes())
            .unwrap_or_else(|e| panic!("invalid migrated document: {e}\n{data}"));
        for marker in [
            "DOCUMENT HEAD",
            "OAUTH INLINE",
            "PROVIDERS INLINE",
            "PROVIDER INLINE",
            "FIELD HEAD",
            "FIELD INLINE",
            "FIELD FOOT",
            "PROVIDER FOOT",
            "UNRELATED INLINE",
            "DOCUMENT FOOT",
        ] {
            assert_eq!(
                data.matches(marker).count(),
                1,
                "comment {marker:?} after save:\n{data}"
            );
        }
    }
}

#[test]
fn migration_carries_provider_key_foot_comments() {
    let raw = "oauth:\n    providers:\n        xai:\n            inject-x-search: true\n            # FIELD KEY FOOT\n        # PROVIDER KEY FOOT\n";
    let (data, _) = normalize_config_layout(raw.as_bytes(), true).unwrap();
    let data = String::from_utf8(data).unwrap();
    for marker in ["PROVIDER KEY FOOT", "FIELD KEY FOOT"] {
        assert_eq!(
            data.matches(marker).count(),
            1,
            "migration changed {marker}:\n{data}"
        );
    }
}

#[test]
fn is_v8_config_layout_detection() {
    for (name, raw, v8) in [
        (
            "legacy",
            "request-retry: 3\ncodex: {response-steering: true}\n",
            false,
        ),
        (
            "legacy common roots",
            "routing: {strategy: fill-first}\nplugins: {configs: {example: {enabled: true}}}\nquota-exceeded: {switch-project: true}\nclient: {codex: {enable-apply-patch: true}}\napi-keys: [client-key]\n",
            false,
        ),
        (
            "legacy optimize alias",
            "codex: {optimize-multi-agent-v2: true}\n",
            false,
        ),
        ("declared v8", "config-version: 8\nrequest-retry: 3\n", true),
        (
            "historical v8",
            "oauth: {providers: {codex: {response-steering: true}}}\n",
            true,
        ),
        ("empty v8", "server: {}\n", true),
        (
            "latest v8",
            "upstream: {xai: {inject-x-search: false}}\n",
            true,
        ),
        ("grouped credentials", "api-keys: {}\n", true),
        (
            "partial v8 retry",
            "request-retry: 3\nrouting: {retry: {max-retry-credentials: 2}}\n",
            true,
        ),
        ("empty v8 retry", "routing: {retry: {}}\n", true),
        (
            "historical client",
            "providers: {codex: {optimize-multi-agent-v2: true}}\n",
            true,
        ),
        (
            "latest client",
            "client: {codex: {optimize-multi-agent-v2: false}}\n",
            true,
        ),
    ] {
        assert_eq!(is_v8_config_layout(&yaml(raw)), v8, "{name}");
    }
}

#[test]
fn v0_save_upgrades_historical_v8_layout() {
    for version in ["", "config-version: 8\n"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let raw = format!(
            "{version}# Preserve provider settings\noauth: {{providers: {{codex: {{response-steering: true, header-defaults: {{user-agent: oauth-agent}}}}, xai: {{inject-x-search: true}}}}}}\n"
        );
        std::fs::write(&path, raw).unwrap();
        let mut cfg = load_config(&path).unwrap();
        cfg.port = 8318;
        cfg.home.enabled = true;
        save_config_preserve_comments(&path, &mut cfg, false).unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        validate_v8_config(data.as_bytes())
            .unwrap_or_else(|e| panic!("v0 saved legacy/mixed fields: {e}\n{data}"));
        let doc = yaml(&data);
        assert!(
            at(&doc, "upstream.codex.response-steering").is_some()
                && at(&doc, "upstream.xai.inject-x-search").is_some()
                && at(&doc, "server.port").and_then(Value::as_i64) == Some(8318),
            "v0 save did not restore canonical provider fields and updated port:\n{data}"
        );
        let api = cfg.for_api_key();
        assert!(
            api.codex.response_steering
                && api.xai.inject_x_search
                && api.codex_header_defaults.user_agent.is_empty()
                && cfg.home.enabled,
            "v0 save changed shared/OAuth scope or runtime-only state"
        );
        assert!(
            data.contains("# Preserve provider settings"),
            "v0 save lost provider comments:\n{data}"
        );
    }
}

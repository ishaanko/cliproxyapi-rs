//! Ports of the Go v8 layout tests (`config_v8_test.go`): precedence, migration, validation and
//! the shipped example file.

use std::path::{Path, PathBuf};

use cpa_config::*;
use serde_yaml_ng::Value;

fn example_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config.example.yaml")
}

fn write(dir: &tempfile::TempDir, text: &str) -> PathBuf {
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, text).unwrap();
    path
}

fn yaml(text: &str) -> Value {
    serde_yaml_ng::from_str(text).unwrap()
}

fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |v, k| v.as_mapping()?.get(k))
}

#[test]
fn example_loads_validates_and_round_trips() {
    let example = std::fs::read_to_string(example_path()).unwrap();
    let active = parse_config_bytes(example.as_bytes()).unwrap();
    assert_eq!(active.port, 8317);
    assert_eq!(active.api_keys.len(), 3);
    assert_eq!(active.request_retry, 3);
    assert!(active.quota_exceeded.antigravity_credits);
    validate_v8_config(example.as_bytes()).unwrap();
    assert!(
        !normalize_config_layout(example.as_bytes(), false)
            .unwrap()
            .1
    );
    for count in [
        active.gemini_key.len(),
        active.codex_key.len(),
        active.claude_key.len(),
        active.vertex_compat_api_key.len(),
        active.xai_key.len(),
        active.meta_key.len(),
        active.interactions_key.len(),
        active.openai_compatibility.len(),
    ] {
        assert_eq!(
            count, 0,
            "placeholder upstream credentials must remain commented"
        );
    }

    // Validate the provider examples exactly as operators uncomment them.
    let text = example.replace("\r\n", "\n");
    let block = text.split_once("# BEGIN API KEY EXAMPLES\n").unwrap().1;
    let block = block.split_once("# END API KEY EXAMPLES").unwrap().0;
    let mut uncommented = String::new();
    for line in block
        .trim_end_matches('\n')
        .split('\n')
        .filter(|l| !l.is_empty())
    {
        let rest = line
            .strip_prefix('#')
            .expect("examples must stay commented");
        uncommented.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        uncommented.push('\n');
    }
    let data = format!("{text}\n{uncommented}");
    validate_v8_config(data.as_bytes()).unwrap();
    let mut cfg = parse_config_bytes(data.as_bytes()).unwrap();
    assert_eq!(
        (
            cfg.gemini_key.len(),
            cfg.codex_key.len(),
            cfg.claude_key.len(),
            cfg.vertex_compat_api_key.len(),
            cfg.xai_key.len(),
            cfg.meta_key.len(),
            cfg.interactions_key.len(),
            cfg.openai_compatibility.len()
        ),
        (3, 1, 2, 1, 1, 1, 1, 1)
    );
    assert!(cfg.quota_exceeded.antigravity_credits && !cfg.quota_exceeded.switch_project);

    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, &data);
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let reloaded = load_config(&path).unwrap();
    assert_eq!(
        cfg.to_yaml_value().unwrap(),
        reloaded.to_yaml_value().unwrap(),
        "runtime values changed by save"
    );
    let saved = yaml(&std::fs::read_to_string(&path).unwrap());
    let groups = at(&saved, "api-keys.gemini")
        .and_then(Value::as_sequence)
        .expect("gemini groups");
    assert_eq!(groups.len(), 2);
    assert_eq!(
        at(&groups[0], "name").and_then(Value::as_str),
        Some("gemini-1")
    );
}

#[test]
fn presence_precedence_and_conflict_cleanup() {
    // (name, raw yaml, request-retry, disable-cooling, client keys, file must stay untouched)
    let cases = [
        (
            "legacy",
            "request-retry: 4\ndisable-cooling: true\napi-keys: [old]\n",
            4,
            true,
            1,
            true,
        ),
        (
            "mixed explicit zero",
            "request-retry: 4\ndisable-cooling: true\napi-keys: [old]\nrouting:\n  retry: {request-retry: 0}\n  cooldown: {disable-cooling: false}\naccess: {api-keys: []}\n",
            0,
            false,
            0,
            false,
        ),
        (
            "version does not force migration",
            "config-version: 8\nrequest-retry: 4\ndisable-cooling: true\napi-keys: [old]\n",
            4,
            true,
            1,
            true,
        ),
        (
            "partial new block",
            "request-retry: 4\ndisable-cooling: true\napi-keys: [old]\nrouting: {retry: {max-retry-credentials: 2}}\n",
            4,
            true,
            1,
            true,
        ),
    ];
    for (name, raw, retry, cooling, keys, unchanged) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, raw);
        let cfg = load_config(&path).unwrap();
        assert_eq!(
            (cfg.request_retry, cfg.disable_cooling, cfg.api_keys.len()),
            (retry, cooling, keys),
            "{name}"
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        if unchanged {
            assert_eq!(saved, raw, "{name}: legacy config was rewritten");
        } else {
            let doc = yaml(&saved);
            for legacy in ["request-retry", "api-keys", "disable-cooling"] {
                assert!(
                    doc.as_mapping().unwrap().get(legacy).is_none(),
                    "{name}: conflicting {legacy} remains"
                );
            }
        }
    }
}

#[test]
fn key_groups_inherit_and_override() {
    for provider in [
        "gemini",
        "interactions",
        "vertex",
        "codex",
        "claude",
        "xai",
        "meta",
    ] {
        let raw = format!(
            "request-retry: 9\napi-keys:\n  {provider}:\n    - name: shared
      base-url: https://example.invalid
      priority: 7
      prefix: group
      proxy-url: direct
      headers: {{X-Group: yes}}
      models: [{{name: model, alias: alias}}]
      excluded-models: [blocked]
      disable-cooling: true
      request-retry: 3
      keys:
        - api-key: inherited
          priority: null
          headers: null
          disable-cooling: null
          request-retry: null
        - api-key: overridden
          weight: 0
          priority: 0
          prefix: ''
          proxy-url: ''
          headers: {{}}
          models: []
          excluded-models: []
          disable-cooling: false
          request-retry: 0
        - api-key: global-retry
          request-retry: -1
"
        );
        let cfg = parse_config_bytes(raw.as_bytes()).unwrap();
        let value = cfg.to_yaml_value().unwrap();
        let family = match provider {
            "gemini" | "interactions" | "vertex" | "codex" | "claude" | "xai" | "meta" => {
                format!("{provider}-api-key")
            }
            _ => unreachable!(),
        };
        let keys = at(&value, &family)
            .and_then(Value::as_sequence)
            .expect("keys");
        assert_eq!(
            keys.len(),
            3,
            "{provider}: group did not expand to three keys"
        );
        let num = |v: &Value, k: &str| at(v, k).and_then(Value::as_i64);
        // Null inherits the group value.
        assert_eq!(num(&keys[0], "priority"), Some(7), "{provider}");
        assert_eq!(num(&keys[0], "request-retry"), Some(3), "{provider}");
        assert_eq!(
            at(&keys[0], "disable-cooling").and_then(Value::as_bool),
            Some(true),
            "{provider}"
        );
        assert!(at(&keys[0], "headers").is_some(), "{provider}");
        // Explicit zero values override.
        assert_eq!(num(&keys[1], "request-retry"), Some(0), "{provider}");
        assert_eq!(
            at(&keys[1], "disable-cooling").and_then(Value::as_bool),
            Some(false),
            "{provider}"
        );
        assert_eq!(num(&keys[1], "weight"), Some(0), "{provider}");
        assert_eq!(num(&keys[2], "request-retry"), Some(-1), "{provider}");
        for name in ["headers", "models", "excluded-models"] {
            let empty = at(&keys[1], name).is_none_or(|v| {
                v.as_mapping().is_some_and(|m| m.is_empty())
                    || v.as_sequence().is_some_and(|s| s.is_empty())
            });
            assert!(empty, "{provider}: empty {name} inherited the group value");
        }
    }
}

#[test]
fn migration_preserves_legacy_semantics() {
    let raw = "host: 127.0.0.1
port: 8317
api-keys: [client]
request-retry: 0
ws-auth: false
quota-exceeded: {switch-project: true, switch-preview-model: true, antigravity-credits: true}
codex-api-key:
  - api-key: a
    base-url: https://example.invalid
    headers: {X-Test: first}
    request-retry: 0
    disable-cooling: false
  - api-key: b
    base-url: https://example.invalid
    headers: {X-Test: second}
    request-retry: -1
openai-compatibility:
  - name: compatible
    base-url: https://example.invalid
    api-key-entries: [{api-key: a, weight: 0}, {api-key: b, proxy-url: direct}]
";
    let before = parse_config_bytes(raw.as_bytes()).unwrap();
    let (migrated, _) = normalize_config_layout(raw.as_bytes(), true).unwrap();
    validate_v8_config(&migrated).unwrap();
    let mut after = parse_config_bytes(&migrated).unwrap();
    // Scope metadata is intentionally added when fields move under oauth.providers.
    after.oauth_only_fields.clear();
    assert_eq!(before, after, "migration changed effective config");
    let doc = yaml(std::str::from_utf8(&migrated).unwrap());
    assert_eq!(
        at(&doc, "api-keys.codex")
            .and_then(Value::as_sequence)
            .map(Vec::len),
        Some(2)
    );
    assert!(after.quota_exceeded.switch_project && after.quota_exceeded.switch_preview_model);
}

#[test]
fn migration_comments_unknown_sections_and_stays_idempotent() {
    let raw = "home:\n  enabled: true\n  host: ignored.example\nenable-gemini-cli-endpoint: true\nforgotten-setting:\n  items: [first, second]\nproxy-url: old\n";
    let (unchanged, changed) = normalize_config_layout(raw.as_bytes(), false).unwrap();
    assert!(!changed && unchanged == raw.as_bytes());
    let (migrated, changed) = normalize_config_layout(raw.as_bytes(), true).unwrap();
    assert!(changed);
    let text = String::from_utf8(migrated.clone()).unwrap();
    let doc = yaml(&text);
    for key in [
        "home",
        "enable-gemini-cli-endpoint",
        "forgotten-setting",
        "proxy-url",
    ] {
        assert!(
            doc.as_mapping().unwrap().get(key).is_none(),
            "{key} stayed active"
        );
    }
    for expected in [
        "# home:",
        "#     enabled: true",
        "#     host: ignored.example",
        "# enable-gemini-cli-endpoint: true",
        "# forgotten-setting:",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in\n{text}");
    }
    validate_v8_config(&migrated).unwrap();
    let cfg = parse_config_bytes(&migrated).unwrap();
    assert_eq!(cfg.proxy_url, "old");
    assert!(!cfg.home.enabled);
    let (again, _) = normalize_config_layout(&migrated, true).unwrap();
    let again = String::from_utf8(again).unwrap();
    assert_eq!(again.matches("# home:").count(), 1, "{again}");
    assert_eq!(again.matches("# forgotten-setting:").count(), 1, "{again}");
}

#[test]
fn migration_comments_unknown_nested_fields() {
    let raw = "server: {port: 8317}\nrouting: {strategy: fill-first, session-affinity: true}\noauth:\n  providers:\n    codex:\n      disable-codex-cloaking: true\n      retired-setting: {mode: old}\n";
    let (unchanged, changed) = normalize_config_layout(raw.as_bytes(), false).unwrap();
    assert!(!changed && unchanged == raw.as_bytes());
    let (migrated, _) = normalize_config_layout(raw.as_bytes(), true).unwrap();
    validate_v8_config(&migrated).unwrap();
    let text = String::from_utf8(migrated.clone()).unwrap();
    assert!(at(&yaml(&text), "oauth.providers.codex.retired-setting").is_none());
    assert!(
        text.contains("# oauth.providers.codex.retired-setting:"),
        "{text}"
    );
    let cfg = parse_config_bytes(&migrated).unwrap();
    assert_eq!(cfg.routing.strategy, "fill-first");
    assert!(cfg.routing.session_affinity && cfg.codex.disable_codex_cloaking);
    let (again, _) = normalize_config_layout(&migrated, true).unwrap();
    assert_eq!(
        String::from_utf8(again)
            .unwrap()
            .matches("# oauth.providers.codex.retired-setting:")
            .count(),
        1
    );
}

#[test]
fn migration_warns_once_per_unknown_section() {
    use std::sync::{Arc, Mutex};
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&seen);
    set_v8_migration_warn_func(Some(Arc::new(move |section: &str, msg: &str| {
        assert!(
            msg.contains("unrecognized") && msg.contains("commented out"),
            "{msg}"
        );
        sink.lock().unwrap().push(section.to_string());
    })));
    let raw = "host: \"127.0.0.1\"\nport: 8317\nsome-obsolete-legacy-block:\n  alpha: 1\nanother-legacy-key:\n  gamma: 3\n";
    let (migrated, _) = normalize_config_layout(raw.as_bytes(), true).unwrap();
    let (_, _) = normalize_config_layout(&migrated, true).unwrap();
    let (_, _) =
        normalize_config_layout(b"host: \"127.0.0.1\"\nport: 8317\ndebug: true\n", true).unwrap();
    set_v8_migration_warn_func(None);
    // The hook is process-global and other tests migrate concurrently; look at our sections only.
    let mut sections: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.contains("legacy-block") || s.contains("legacy-key"))
        .cloned()
        .collect();
    sections.sort();
    assert_eq!(
        sections,
        ["another-legacy-key", "some-obsolete-legacy-block"]
    );
}

#[test]
fn save_comments_obsolete_sections_on_migration() {
    let dir = tempfile::tempdir().unwrap();
    let raw = "auth: {old: true}\nampcode: {old: true}\namp-upstream-url: https://old.example\namp-upstream-api-key: old-secret\ngenerative-language-api-key: old-key\nhome: {enabled: true}\nproxy-url: old\n";
    let path = write(&dir, raw);
    let mut cfg = load_config(&path).unwrap();
    save_config_preserve_comments(&path, &mut cfg, true).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    validate_v8_config(saved.as_bytes()).unwrap();
    for key in [
        "auth",
        "ampcode",
        "amp-upstream-url",
        "amp-upstream-api-key",
        "generative-language-api-key",
        "home",
    ] {
        assert!(
            saved.contains(&format!("# {key}:")),
            "obsolete setting {key} was discarded:\n{saved}"
        );
    }
    assert!(saved.contains("# amp-upstream-api-key: old-secret"));
}

#[test]
fn empty_legacy_containers_move_to_v8_paths() {
    let sections = [
        ("tls", "server.tls"),
        ("remote-management", "management"),
        ("pprof", "observability.pprof"),
        ("discovery", "server.discovery"),
        ("credential-concurrency", "credentials.concurrency"),
        ("credential-in-flight", "credentials.in-flight"),
        ("streaming", "requests.streaming"),
        ("payload", "requests.payload"),
        ("codex", "oauth.providers.codex"),
        (
            "codex-header-defaults",
            "oauth.providers.codex.header-defaults",
        ),
        ("claude", "oauth.providers.claude"),
        ("claude-code", "oauth.providers.claude.claude-code"),
        (
            "claude-header-defaults",
            "oauth.providers.claude.header-defaults",
        ),
        ("antigravity", "oauth.providers.antigravity"),
        ("xai", "oauth.providers.xai"),
        ("devin", "oauth.providers.devin"),
    ];
    for (old, current) in sections {
        for empty in ["{}", "null"] {
            let raw = format!(
                "port: 8317\nplugins: {{configs: {{sample: {{enabled: false, options: {{}}}}}}}}\n{old}: {empty}\n"
            );
            let before = parse_config_bytes(raw.as_bytes()).unwrap();
            let (unchanged, changed) = normalize_config_layout(raw.as_bytes(), false).unwrap();
            assert!(
                !changed && unchanged == raw.as_bytes(),
                "{old}/{empty}: legacy-only load changed the file"
            );
            let (migrated, _) = normalize_config_layout(raw.as_bytes(), true).unwrap();
            validate_v8_config(&migrated).unwrap_or_else(|e| panic!("{old}/{empty}: {e}"));
            let after = parse_config_bytes(&migrated).unwrap();
            assert_eq!(
                serde_yaml_ng::to_value(&before).unwrap(),
                serde_yaml_ng::to_value(&after).unwrap(),
                "{old}/{empty}: migration changed effective defaults or plugin options"
            );
            let doc = yaml(std::str::from_utf8(&migrated).unwrap());
            let moved =
                at(&doc, current).unwrap_or_else(|| panic!("{old}/{empty}: {current} missing"));
            assert!(doc.as_mapping().unwrap().get(old).is_none());
            assert!(
                moved.as_mapping().is_some_and(|m| m.is_empty()),
                "{old}/{empty}: not moved as an empty map"
            );
        }
    }
}

#[test]
fn empty_legacy_containers_keep_new_values() {
    let raw = "port: 8317
tls: null
codex: {disable-codex-cloaking: true, live-media-relay: {}}
server: {tls: {enable: true, cert: server.crt, key: server.key}}
oauth: {providers: {codex: {live-media-relay: {max-sessions: 12}}}}
";
    for migrate in [false, true] {
        let (data, _) = normalize_config_layout(raw.as_bytes(), migrate).unwrap();
        let cfg = parse_config_bytes(&data).unwrap();
        assert!(cfg.tls.enable && cfg.tls.cert == "server.crt" && cfg.tls.key == "server.key");
        assert_eq!(cfg.codex.live_media_relay.max_sessions, 12);
        assert!(cfg.codex.disable_codex_cloaking);
        let doc = yaml(std::str::from_utf8(&data).unwrap());
        assert!(at(&doc, "tls").is_none() && at(&doc, "codex.live-media-relay").is_none());
        if !migrate {
            assert!(
                at(&doc, "codex.disable-codex-cloaking").is_some(),
                "cleanup migrated a non-conflicting sibling"
            );
        }
    }
}

#[test]
fn private_ip_alias_precedence_and_migration() {
    let cases = [
        (
            "allow",
            "codex: {live-media-relay: {allow-private-remote-ips: true}}\n",
            false,
        ),
        (
            "deny",
            "codex: {live-media-relay: {allow-private-remote-ips: false}}\n",
            true,
        ),
        (
            "new wins",
            "codex: {live-media-relay: {allow-private-remote-ips: false}}\noauth: {providers: {codex: {live-media-relay: {disable-private-remote-ips: false}}}}\n",
            false,
        ),
    ];
    for (name, raw, want) in cases {
        let mut cfg = parse_config_bytes(raw.as_bytes()).unwrap();
        assert_eq!(
            cfg.codex.live_media_relay.disable_private_remote_ips, want,
            "{name}"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, raw);
        save_config_preserve_comments(&path, &mut cfg, true).unwrap();
        let after = load_config(&path).unwrap();
        assert_eq!(
            after.codex.live_media_relay.disable_private_remote_ips, want,
            "{name}: migration inverted the policy"
        );
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("allow-private-remote-ips"),
            "{name}"
        );
    }
}

#[test]
fn legacy_api_writes_are_not_shadowed_by_stale_v8_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        &dir,
        "config-version: 8\nrouting: {retry: {request-retry: 1}}\noauth: {providers: {aistudio: {ws-auth: true}}}\n",
    );
    let mut cfg = load_config(&path).unwrap();
    cfg.request_retry = 0;
    cfg.websocket_auth = false;
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let reloaded = load_config(&path).unwrap();
    assert!(reloaded.request_retry == 0 && !reloaded.websocket_auth);

    std::fs::write(
        &path,
        "config-version: 8\nrequest-retry: 5\nws-auth: true\n",
    )
    .unwrap();
    let reloaded = load_config(&path).unwrap();
    assert!(
        reloaded.request_retry == 5 && reloaded.websocket_auth,
        "manual legacy fallback failed"
    );
}

#[test]
fn invalid_v8_documents_are_rejected() {
    for raw in [
        "api-keys: {codex: [{name: a, keys: [{api-key: a, weight: 1.5}]}]}",
        "api-keys: {codex: [{name: a, keys: [{api-key: a, base-url: https://invalid}]}]}",
        "api-keys: {codex: [{name: a, keys: {api-key: a}}]}",
        "server: true",
        "config-version: 9",
        "server: {port: bad}",
    ] {
        assert!(
            parse_config_bytes(raw.as_bytes()).is_err(),
            "accepted invalid v8 config: {raw}"
        );
    }
    // Group-level weight is not a shared field; weights above the cap are load errors.
    assert!(
        parse_config_bytes(b"api-keys: {codex: [{name: a, weight: 2, keys: [{api-key: a}]}]}")
            .is_err()
    );
    let err = parse_config_bytes(b"claude-api-key: [{api-key: a, weight: 1000001}]").unwrap_err();
    assert!(
        err.to_string().contains("claude-api-key[0].weight"),
        "{err}"
    );
}

#[test]
fn v8_validation_rejects_legacy_write_layout_but_load_accepts_it() {
    for raw in [
        "debug: true",
        "server: {port: 8317}\nport: 8318",
        "api-keys: [client]",
        "codex-api-key: []",
        "codex: {}",
        "quota-exceeded: {antigravity-credits: true}",
        "home: {enabled: true}",
        "enable-gemini-cli-endpoint: true",
        "unknown-root: true",
        "<<: {debug: true}",
    ] {
        parse_config_bytes(raw.as_bytes())
            .unwrap_or_else(|e| panic!("legacy compatibility failed for {raw:?}: {e}"));
        assert!(
            validate_v8_config(raw.as_bytes()).is_err(),
            "v8 API accepted the legacy write layout: {raw}"
        );
    }
    // Unknown nested fields are rejected only by the strict v8 validator.
    assert!(validate_v8_config(b"server: {port: 8317, bogus: 1}").is_err());
    validate_v8_config(b"server: {port: 8317}").unwrap();
}

#[test]
fn secret_is_hashed_at_the_v8_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "management: {secret-key: test-secret}\n");
    let cfg = load_config(&path).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(looks_like_bcrypt(&cfg.remote_management.secret_key));
    assert!(
        !saved.contains("test-secret") && !saved.contains("remote-management"),
        "{saved}"
    );
}

#[test]
fn secret_hash_resolves_references_and_keeps_comments() {
    const SECRET: &str = "test-management-reference-secret";
    let cases = [
        (
            "v8 alias",
            format!(
                "defaults: &management\n  secret-key: {SECRET}\n  allow-remote: true\nmanagement: *management\nother: *management\n"
            ),
            "management",
            true,
        ),
        (
            "v8 merge",
            format!(
                "defaults: &management\n  secret-key: {SECRET}\n  allow-remote: true\nmanagement: {{<<: *management, allow-remote: false}}\nother: *management\n"
            ),
            "management",
            false,
        ),
        (
            "v8 scalar alias",
            format!(
                "password: &password {SECRET}\nmanagement: {{secret-key: *password, allow-remote: true}}\n"
            ),
            "management",
            true,
        ),
        (
            "v8 root merge",
            format!(
                "defaults: &root\n  management: {{secret-key: {SECRET}, allow-remote: true}}\n<<: *root\n"
            ),
            "management",
            true,
        ),
        (
            "v8 wins legacy",
            format!(
                "remote-management: {{secret-key: stale-secret, allow-remote: false}}\ndefaults: &management {{secret-key: {SECRET}, allow-remote: true}}\nmanagement: *management\n"
            ),
            "management",
            true,
        ),
        (
            "legacy alias",
            format!(
                "defaults: &management\n  secret-key: {SECRET}\n  allow-remote: true\nremote-management: *management\nother: *management\n"
            ),
            "remote-management",
            true,
        ),
    ];
    for (name, raw, field, allow_remote) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, &format!("{raw}# Keep this comment\nport: 8317\n"));
        let cfg = load_config(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        let doc = yaml(&saved);
        let stored = at(&doc, &format!("{field}.secret-key"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{name}: no secret at {field}"));
        assert!(
            stored == cfg.remote_management.secret_key && looks_like_bcrypt(stored),
            "{name}"
        );
        assert_eq!(cfg.remote_management.allow_remote, allow_remote, "{name}");
        assert_eq!(cfg.port, 8317, "{name}");
        assert!(
            saved.contains("# Keep this comment"),
            "{name}: comment lost:\n{saved}"
        );
        if let Some(other) = at(&doc, "other.secret-key").and_then(Value::as_str) {
            assert_eq!(
                other, SECRET,
                "{name}: hashing mutated another use of the shared anchor"
            );
        }
        if field == "management" {
            assert!(
                at(&doc, "remote-management.secret-key").is_none(),
                "{name}: conflicting legacy secret left"
            );
        }
        let reloaded = load_config(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            saved,
            "{name}: second load rewrote the file"
        );
        assert_eq!(
            reloaded.remote_management.secret_key, cfg.remote_management.secret_key,
            "{name}"
        );
    }
}

#[test]
fn legacy_client_keys_do_not_overwrite_upstream_groups() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        &dir,
        "api-keys: {codex: [{name: upstream, base-url: 'https://example.invalid', keys: [{api-key: upstream-key}]}]}\n",
    );
    let mut cfg = load_config(&path).unwrap();
    cfg.api_keys = vec!["client-key".to_string()];
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let cfg = load_config(&path).unwrap();
    assert_eq!(cfg.api_keys, ["client-key"]);
    assert_eq!(cfg.codex_key.len(), 1);
}

#[test]
fn aliases_and_merge_keys_expand_before_migration() {
    let raw = "routing: &routing
  strategy: round-robin
  retry: {request-retry: 0}
codex-api-key:
  - &key
    api-key: first
    base-url: https://example.invalid
    request-retry: 0
  - <<: *key
    api-key: second
api-keys:
  gemini:
    - &upstream
      name: first
      base-url: https://example.invalid
      request-retry: 2
      keys: [{api-key: one}]
    - <<: *upstream
      name: second
      keys: [{api-key: two, request-retry: 0}]
";
    let before = parse_config_bytes(raw.as_bytes()).unwrap();
    let (migrated, _) = normalize_config_layout(raw.as_bytes(), true).unwrap();
    let after = parse_config_bytes(&migrated).unwrap();
    assert_eq!(before, after);
    assert_eq!((after.codex_key.len(), after.gemini_key.len()), (2, 2));
    assert_eq!(
        (
            after.gemini_key[0].request_retry,
            after.gemini_key[1].request_retry
        ),
        (Some(2), Some(0))
    );
}

#[test]
fn explicit_zero_retry_override_survives_save() {
    for raw in [
        "request-retry: 3\napi-keys: {codex: [{name: upstream, base-url: 'https://example.invalid', keys: [{api-key: upstream-key}]}]}\n",
        "request-retry: 3\ncodex-api-key: [{base-url: 'https://example.invalid', api-key: upstream-key}]\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, raw);
        let mut cfg = load_config(&path).unwrap();
        cfg.codex_key[0].request_retry = Some(0);
        save_config_preserve_comments(&path, &mut cfg, false).unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(
            cfg.codex_key[0].request_retry,
            Some(0),
            "new zero retry override was discarded"
        );
    }
}

#[test]
fn null_routing_means_defaults() {
    for raw in [
        "port: 8317\nrouting: null\n",
        "port: 8317\nrouting: ~\n",
        "port: 8317\nrouting:\n",
        "port: 8317\nrouting: null\nrequest-retry: 3\n",
        "server: {port: 8317}\nrouting: null\n",
    ] {
        let cfg = parse_config_bytes(raw.as_bytes()).unwrap_or_else(|e| panic!("{raw:?}: {e}"));
        assert_eq!(cfg.port, 8317);
        assert_eq!(cfg.routing, RoutingConfig::default());
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, raw);
        let mut loaded = load_config(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            raw,
            "loading legacy null routing rewrote the file"
        );
        save_config_preserve_comments(&path, &mut loaded, true).unwrap();
        let reloaded = load_config(&path).unwrap();
        assert_eq!(
            (&loaded.routing, loaded.request_retry),
            (&reloaded.routing, reloaded.request_retry),
            "{raw:?}"
        );
    }
    for raw in [
        "routing: false",
        "routing: []",
        "routing: {retry: false}",
        "server: null",
    ] {
        assert!(
            parse_config_bytes(raw.as_bytes()).is_err(),
            "accepted invalid container: {raw}"
        );
    }
}

#[test]
fn comments_follow_entries_when_v8_lists_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        &dir,
        "access:
  api-keys:
    - k1 # first
    - k2 # second
    - k3 # third
api-keys:
  gemini:
    # group one
    - name: one
      base-url: https://a.invalid
      keys:
        - api-key: A1 # a1
    # bee
    - name: two
      base-url: https://b.invalid # b url
      keys:
        # key head
        - api-key: B1 # b1
",
    );
    let mut cfg = load_config(&path).unwrap();
    cfg.api_keys = vec!["k1".into(), "k3".into()];
    cfg.gemini_key.remove(0);
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    // List items keep their own comments; nothing of the removed entries survives.
    assert!(
        saved.contains("- k1 # first") && saved.contains("- k3 # third"),
        "{saved}"
    );
    assert!(
        !saved.contains("# second") && !saved.contains("# group one") && !saved.contains("# a1"),
        "{saved}"
    );
    // The rebuilt group takes over the comments of the group that moved up.
    assert!(
        saved.contains("# bee\n# key head\n    - name: gemini-1"),
        "{saved}"
    );
    assert!(
        saved.contains("base-url: https://b.invalid # b url"),
        "{saved}"
    );
    assert!(saved.contains("api-key: B1 # b1"), "{saved}");
    let reloaded = load_config(&path).unwrap();
    assert_eq!((reloaded.api_keys.len(), reloaded.gemini_key.len()), (2, 1));

    // Unchanged groups keep every comment, including group-only ones.
    let path = write(
        &dir,
        "api-keys:\n  gemini:\n    # group one\n    - name: one # nm\n      base-url: https://a.invalid\n      keys:\n        - api-key: A1 # a1\n",
    );
    let mut cfg = load_config(&path).unwrap();
    cfg.debug = true;
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(
        saved.contains("# group one")
            && saved.contains("name: one # nm")
            && saved.contains("A1 # a1"),
        "{saved}"
    );
}

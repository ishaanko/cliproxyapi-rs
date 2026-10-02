//! Ports of the Go config tests that pin defaults, parsing, sanitising, scoping and saving
//! behaviour (everything except the v8 layout tests, which live in `v8_layout.rs`).

use std::path::PathBuf;

use cpa_config::*;
use serde_yaml_ng::Value;

fn write(dir: &tempfile::TempDir, text: &str) -> PathBuf {
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, text).unwrap();
    path
}

fn parse(text: &str) -> Config {
    parse_config_bytes(text.as_bytes()).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

// --- defaults and fallbacks -------------------------------------------------------------------

#[test]
fn code_defaults_apply_before_decode() {
    let cfg = parse("{}");
    // Pre-set in code (docs/survey/conductor-config.md 15.1).
    assert_eq!(cfg.host, "");
    assert_eq!(cfg.error_logs_max_files, 10);
    assert_eq!(cfg.redis_usage_queue_retention_seconds, 60);
    assert!(cfg.websocket_auth);
    assert_eq!(
        cfg.pprof,
        PprofConfig {
            enable: false,
            addr: "127.0.0.1:8316".into()
        }
    );
    assert_eq!(cfg.discovery.service_type, "_ai-gateway._tcp");
    assert_eq!(
        cfg.discovery.subtypes,
        [
            "_chat-completions",
            "_responses",
            "_messages",
            "_generate-content",
            "_interactions"
        ]
    );
    assert_eq!(
        cfg.remote_management.panel_github_repository,
        "https://github.com/router-for-me/Cli-Proxy-API-Management-Center"
    );
    assert_eq!(
        cfg.credential_in_flight,
        CredentialInFlightConfig::parse_defaults()
    );
    assert_eq!(cfg.plugins.dir, "plugins");
    assert!(cfg.plugins.configs.is_empty());
    // Zero values in code, resolved at their use sites rather than from the sample file.
    assert_eq!(
        (
            cfg.port,
            cfg.request_retry,
            cfg.max_retry_credentials,
            cfg.max_retry_interval
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(cfg.routing.strategy, "");
    assert_eq!(cfg.auth_dir, "");
    assert_eq!(
        cfg.disable_image_generation,
        DisableImageGenerationMode::Off
    );
    // Lifecycle defaults for Home-owned concurrency settings.
    let cc = &cfg.credential_concurrency;
    assert_eq!(cc.cpa_heartbeat_timeout, GoDuration::from_secs(3));
    assert_eq!(cc.release_flush_interval, GoDuration::from_millis(250));
    assert_eq!(cc.max_limit, 1_000_000);
}

#[test]
fn nested_presets_survive_partial_sections() {
    let cfg = parse(
        "pprof: {enable: true}\ndiscovery: {enabled: true}\nremote-management: {allow-remote: true}\ncredential-in-flight: {stale-after: 12s}\n",
    );
    assert!(cfg.pprof.enable && cfg.pprof.addr == "127.0.0.1:8316");
    assert!(cfg.discovery.enabled && cfg.discovery.service_type == "_ai-gateway._tcp");
    assert!(
        cfg.remote_management.allow_remote
            && !cfg.remote_management.panel_github_repository.is_empty()
    );
    assert_eq!(cfg.credential_in_flight.stale_after, "12s");
    assert_eq!(cfg.credential_in_flight.snapshot_interval, "2s");
    // Explicit null/blank values are treated like absent keys for scalars and structs.
    let cfg = parse("host: null\npprof: null\nport: 9000\n");
    assert_eq!((cfg.host.as_str(), cfg.port), ("", 9000));
    assert_eq!(cfg.pprof.addr, "127.0.0.1:8316");
}

#[test]
fn clamps_and_trimming() {
    let cfg = parse(
        "logs-max-total-size-mb: -5\nerror-logs-max-files: -1\nredis-usage-queue-retention-seconds: 99999\nmax-retry-credentials: -3\npprof: {addr: '  '}\nremote-management: {panel-github-repository: '   '}\n",
    );
    assert_eq!(cfg.logs_max_total_size_mb, 0);
    assert_eq!(cfg.error_logs_max_files, 10);
    assert_eq!(cfg.redis_usage_queue_retention_seconds, 3600);
    assert_eq!(cfg.max_retry_credentials, 0);
    assert_eq!(cfg.pprof.addr, "127.0.0.1:8316");
    assert!(
        cfg.remote_management
            .panel_github_repository
            .starts_with("https://github.com/")
    );
    assert_eq!(
        parse("redis-usage-queue-retention-seconds: 0").redis_usage_queue_retention_seconds,
        60
    );
}

#[test]
fn optional_load_falls_back_to_an_empty_config() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.yaml");
    let file = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    let cases: [(&str, PathBuf); 4] = [
        ("missing", missing.clone()),
        ("empty", file("empty.yaml", "")),
        ("whitespace", file("space.yaml", " \t\n\r ")),
        ("invalid", file("invalid.yaml", ":")),
    ];
    for (name, path) in cases {
        let cfg = load_config_optional(&path, true).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            cfg.credential_in_flight,
            CredentialInFlightConfig::parse_defaults(),
            "{name}"
        );
        cfg.credential_in_flight.validate().unwrap();
        // The fallback keeps the empty-config zero values, not the parse-time presets.
        assert!(
            cfg.error_logs_max_files == 0 && !cfg.websocket_auth,
            "{name}"
        );
        assert_eq!(
            cfg.credential_concurrency,
            CredentialConcurrencyConfig::default(),
            "{name}"
        );
    }
    assert!(load_config(&missing).is_err());
    assert!(
        load_config_optional(dir.path(), true).is_ok(),
        "a directory counts as missing when optional"
    );
}

// --- yaml.v3 decoding quirks -------------------------------------------------------------------

#[test]
fn scalars_decode_like_yaml_v3() {
    // YAML 1.1 bool spellings are accepted for bool fields; floats truncate into int fields;
    // any scalar becomes the text of a string field (numeric client keys are common).
    let cfg = parse(
        "debug: yes\nws-auth: no\nlogging-to-file: On\ncommercial-mode: OFF\nport: 8317.9\napi-keys: [12345, true, 1.5]\ngemini-api-key: [{api-key: 42, prefix: 7}]\n",
    );
    assert!(cfg.debug && !cfg.websocket_auth && cfg.logging_to_file && !cfg.commercial_mode);
    assert_eq!(cfg.port, 8317);
    assert_eq!(cfg.api_keys, ["12345", "true", "1.5"]);
    assert_eq!(
        (
            cfg.gemini_key[0].api_key.as_str(),
            cfg.gemini_key[0].prefix.as_str()
        ),
        ("42", "7")
    );
    // ...but strings are never coerced into numbers, and "true" in quotes is not a bool.
    for bad in [
        "port: '8317'",
        "debug: 'true'",
        "debug: 1",
        "request-retry: three",
        "gemini-api-key: {a: 1}",
    ] {
        assert!(parse_config_bytes(bad.as_bytes()).is_err(), "{bad}");
    }
    // Null scalars and list items are skipped; only the first document is read.
    let cfg = parse(
        "host: null\ntrusted-proxies: [null, 10.0.0.1]\ncodex-api-key: [null]\n---\nport: 2\n",
    );
    assert_eq!(
        (
            cfg.host.as_str(),
            cfg.trusted_proxies.as_slice(),
            cfg.codex_key.len(),
            cfg.port
        ),
        ("", ["10.0.0.1".to_string()].as_slice(), 0, 0)
    );
    // Plugin instances keep their raw tree even when host-owned fields are odd.
    let cfg = parse(
        "plugins: {configs: {a: null, b: {enabled: yes, priority: 2.0}, c: {enabled: null}}}\n",
    );
    assert_eq!(cfg.plugins.configs["a"].enabled, None);
    assert_eq!(
        (
            cfg.plugins.configs["b"].enabled,
            cfg.plugins.configs["b"].priority
        ),
        (Some(true), 2)
    );
    assert_eq!(cfg.plugins.configs["c"].enabled, Some(false));
    assert_eq!(parse("plugins: {configs: null}").plugins.configs.len(), 0);
}

// --- validation --------------------------------------------------------------------------------

#[test]
fn trusted_proxies() {
    let cfg = parse("trusted-proxies:\n  - 192.0.2.0/24\n  - 2001:db8::1\n");
    assert_eq!(cfg.trusted_proxies, ["192.0.2.0/24", "2001:db8::1"]);
    for bad in [
        "[not-an-ip]",
        "[' 10.0.0.1']",
        "['10.0.0.0/33']",
        "['10.0.0.1/']",
        "['']",
    ] {
        assert!(
            parse_config_bytes(format!("trusted-proxies: {bad}").as_bytes()).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn api_key_weight_validation() {
    for (weight, valid) in [
        ("-1", true),
        ("1000000", true),
        ("1.5", false),
        ("1000001", false),
        ("9223372036854775808", false),
        ("'3'", false),
    ] {
        let raw = format!("gemini-api-key:\n  - api-key: key\n    weight: {weight}\n");
        assert_eq!(
            parse_config_bytes(raw.as_bytes()).is_ok(),
            valid,
            "weight {weight}"
        );
    }
}

#[test]
fn zero_weight_survives_save() {
    let mut cfg =
        parse("xai-api-key:\n  - api-key: key\n    base-url: https://api.x.ai/v1\n    weight: 0\n");
    assert_eq!(cfg.xai_key[0].weight, Some(0));
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        &dir,
        "xai-api-key:\n  - api-key: key\n    base-url: https://api.x.ai/v1\n",
    );
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("weight: 0")
    );
}

#[test]
fn credential_concurrency_defaults_only_missing_fields() {
    let got = CredentialConcurrencyConfig::default().with_defaults();
    assert_eq!(
        (
            got.lifecycle_config_revision,
            got.observation_barrier_revision
        ),
        (0, 0)
    );
    assert_eq!(got.cpa_heartbeat_timeout, GoDuration::from_secs(3));
    assert_eq!(got.busy_retry_max, GoDuration::from_secs(1));
    got.validate_lifecycle(GoDuration::from_secs(20)).unwrap();
    assert!(
        got.validate_lifecycle(GoDuration::from_secs(2)).is_err(),
        "timing invariant"
    );

    let base = "credential-concurrency:\n  cpa-cancel-bound: 5s\n  reclaim-grace: 5s\n  cleanup-interval: 5s\n";
    for (name, extra) in [
        (
            "explicit zero revision",
            "  lifecycle-config-revision: 0\n  cpa-heartbeat-timeout: 3s\n",
        ),
        (
            "explicit zero duration",
            "  lifecycle-config-revision: 1\n  cpa-heartbeat-timeout: 0s\n",
        ),
        (
            "explicit null duration",
            "  lifecycle-config-revision: 1\n  cpa-heartbeat-timeout: null\n",
        ),
        (
            "negative barrier",
            "  lifecycle-config-revision: 1\n  observation-barrier-revision: -1\n  cpa-heartbeat-timeout: 3s\n",
        ),
    ] {
        let cfg = parse(&format!("{base}{extra}"));
        assert!(
            cfg.credential_concurrency
                .validate_lifecycle(GoDuration::from_secs(20))
                .is_err(),
            "{name}: explicit invalid lifecycle value accepted"
        );
    }
    // Limiter bounds.
    let ok = || CredentialConcurrencyConfig::default().with_defaults();
    for mutate in [
        (|c: &mut CredentialConcurrencyConfig| {
            c.release_flush_interval = GoDuration::from_secs(1);
            c.release_max_backoff = GoDuration::from_millis(500);
        }) as fn(&mut CredentialConcurrencyConfig),
        |c| c.busy_retry_min = GoDuration(1_500_000),
        |c| c.max_limit = 1_000_001,
    ] {
        let mut cfg = ok();
        mutate(&mut cfg);
        assert!(cfg.validate().is_err());
    }
    let mut overflow = ok();
    overflow.lifecycle_config_revision = 1;
    overflow.cpa_heartbeat_timeout = GoDuration(i64::MAX);
    overflow.cpa_cancel_bound = GoDuration(1);
    assert!(
        overflow
            .validate_lifecycle(GoDuration::from_secs(1))
            .is_err()
    );
}

#[test]
fn credential_in_flight_bounds() {
    let with = |every: &str, stale: &str| {
        let mut cfg = CredentialInFlightConfig::parse_defaults();
        cfg.snapshot_interval = every.into();
        cfg.stale_after = stale.into();
        cfg.validate().is_ok()
    };
    assert!(with("1s", "3s"));
    assert!(!with("1s", "2999999999ns"));
    assert!(!with(
        &GoDuration(i64::MAX / 2).to_string(),
        &GoDuration(i64::MAX).to_string()
    ));
    let mut cfg = CredentialInFlightConfig::parse_defaults();
    cfg.stale_after = "5s".into();
    assert!(cfg.validate().is_err());
    cfg = CredentialInFlightConfig::parse_defaults();
    cfg.max_revision_bytes = 16 * 1024 * 1024 + 1;
    assert!(cfg.validate().is_err());
    cfg = CredentialInFlightConfig::parse_defaults();
    cfg.max_part_bytes = i64::MAX;
    assert!(cfg.validate().is_err(), "overflow-safe part bound");
    assert!(parse_config_bytes(b"credential-in-flight: {stale-after: 5s}").is_err());
}

#[test]
fn codex_live_media_relay_validation() {
    assert!(
        parse_config_bytes(b"codex: {live-media-relay: {enabled: true, public-ip: nope}}").is_err()
    );
    assert!(
        parse_config_bytes(b"codex: {live-media-relay: {enabled: true, udp-port-min: 4000}}")
            .is_err()
    );
    assert!(parse_config_bytes(b"codex: {live-media-relay: {enabled: true, udp-port-min: 4000, udp-port-max: 4010, max-sessions: 8}}").is_err());
    assert!(
        parse_config_bytes(
            b"codex: {live-media-relay: {enabled: true, ice-servers: [{urls: ['http://x']}]}}"
        )
        .is_err()
    );
    let ok = parse(
        "codex: {live-media-relay: {enabled: true, udp-port-min: 4000, udp-port-max: 4100, max-sessions: 8, ice-servers: [{urls: ['stun:stun.example:3478']}]}}",
    );
    assert_eq!(ok.codex.live_media_relay.effective_max_sessions(), 8);
    assert!(parse_config_bytes(b"codex: {live-media-relay: {allow-private-remote-ips: true, disable-private-remote-ips: true}}").is_err());
}

#[test]
fn codex_stream_bootstrap_timeout_forms() {
    let timeout = |raw: &str| {
        parse(&format!("codex: {{stream-bootstrap-timeout: '{raw}'}}"))
            .codex
            .stream_bootstrap_timeout_duration()
    };
    assert_eq!(timeout("20s"), GoDuration::from_secs(20));
    assert_eq!(timeout("15"), GoDuration::from_secs(15));
    for off in [
        "0",
        "none",
        "Off",
        "unlimited",
        "never",
        "disabled",
        "junk",
        "-5s",
    ] {
        assert_eq!(timeout(off), GoDuration(0), "{off}");
    }
}

// --- image generation mode ---------------------------------------------------------------------

#[test]
fn disable_image_generation_modes() {
    for (raw, want) in [
        ("false", DisableImageGenerationMode::Off),
        ("true", DisableImageGenerationMode::All),
        ("chat", DisableImageGenerationMode::Chat),
        ("passthrough", DisableImageGenerationMode::Passthrough),
        ("'TRUE'", DisableImageGenerationMode::All),
        ("off", DisableImageGenerationMode::Off),
        ("1", DisableImageGenerationMode::All),
    ] {
        assert_eq!(
            parse(&format!("disable-image-generation: {raw}")).disable_image_generation,
            want,
            "{raw}"
        );
    }
    assert!(parse_config_bytes(b"disable-image-generation: sometimes").is_err());
    let value = serde_yaml_ng::to_value(DisableImageGenerationMode::Chat).unwrap();
    assert_eq!(value, Value::String("chat".into()));
}

// --- plugins -----------------------------------------------------------------------------------

#[test]
fn plugin_sections_parse_and_normalize() {
    let cfg = parse("plugins: {}\n");
    assert!(!cfg.plugins.enabled && cfg.plugins.dir == "plugins" && cfg.plugins.configs.is_empty());

    let cfg = parse(
        "plugins:
  store-sources:
    - ' https://community.example/registry.json '
    - ''
  store-auth:
    - match: ' https://plugins.example.com/ '
      apply-to: [registry, artifact, registry]
      type: bearer
      token-env: ' CLIPROXY_PLUGIN_STORE_TOKEN '
    - match: ''
      type: bearer
  auth-revision: 42
",
    );
    assert_eq!(
        cfg.plugins.store_sources,
        ["https://community.example/registry.json"]
    );
    assert_eq!(cfg.plugins.store_auth.len(), 1);
    let auth = &cfg.plugins.store_auth[0];
    assert_eq!(
        (
            auth.r#match.as_str(),
            auth.kind.as_str(),
            auth.token_env.as_str()
        ),
        (
            "https://plugins.example.com/",
            "bearer",
            "CLIPROXY_PLUGIN_STORE_TOKEN"
        )
    );
    assert_eq!(auth.apply_to, ["registry", "artifact"]);
    assert_eq!(cfg.plugins.auth_revision, 42);
}

#[test]
fn plugin_dir_expands_leading_tilde() {
    // Read HOME as the loader does; no environment mutation needed.
    let home = std::env::var("HOME").expect("HOME is set");
    let cfg = parse("plugins:\n  dir: \"~/.cli-proxy-api/plugins\"\n");
    assert_eq!(cfg.plugins.dir, format!("{home}/.cli-proxy-api/plugins"));
    assert_eq!(
        resolve_auth_dir("").unwrap(),
        PathBuf::from(format!("{home}/.cli-proxy-api"))
    );
    assert_eq!(resolve_auth_dir("~").unwrap(), PathBuf::from(&home));
    assert_eq!(
        resolve_auth_dir("/tmp//a/../b").unwrap(),
        PathBuf::from("/tmp/b")
    );
}

#[test]
fn plugin_instances_keep_raw_yaml() {
    let cfg = parse(
        "plugins:\n  configs:\n    sample: {}\n    other:\n      enabled: true\n      priority: 7\n      nested: {k: v}\n",
    );
    let sample = &cfg.plugins.configs["sample"];
    assert_eq!((sample.enabled, sample.priority), (Some(false), 0));
    assert_eq!(
        sample.raw,
        serde_yaml_ng::from_str::<Value>("{}").unwrap(),
        "no host defaults leak into the raw subtree"
    );
    let other = &cfg.plugins.configs["other"];
    assert_eq!((other.enabled, other.priority), (Some(true), 7));
    assert_eq!(other.raw["nested"]["k"], Value::String("v".into()));
    // Round-trips verbatim.
    let out = serde_yaml_ng::to_value(&cfg.plugins).unwrap();
    assert_eq!(
        out["configs"]["other"]["nested"]["k"],
        Value::String("v".into())
    );
}

#[test]
fn saving_prunes_default_plugins_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "debug: true\n");
    let mut cfg = Config {
        debug: true,
        plugins: PluginsConfig {
            dir: "plugins".into(),
            ..PluginsConfig::default()
        },
        ..Config::default()
    };
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("plugins"), "{text}");
}

#[test]
fn saving_plugin_configs_replaces_the_subtree() {
    let plugin = |yaml: &str| {
        PluginInstanceConfig::from_yaml(serde_yaml_ng::from_str(yaml).unwrap()).unwrap()
    };
    let dir = tempfile::tempdir().unwrap();

    // Zero values, false booleans and empty collections must be kept; stale keys must go.
    let path = write(
        &dir,
        "plugins:\n  enabled: true\n  configs:\n    sample:\n      name: initial\n      stale_key: gone\n      nested:\n        old_child: gone\n",
    );
    let mut cfg = Config::default();
    cfg.plugins.enabled = true;
    cfg.plugins.dir = "plugins".into();
    cfg.plugins.configs.insert("sample".into(), plugin("name: updated\nenabled: false\ntimeout: 0\nempty_str: ''\nempty_list: []\nnested:\n  new_child: keep_me\n"));
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let saved: Value = serde_yaml_ng::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let sample = &saved["plugins"]["configs"]["sample"];
    assert_eq!(sample["name"], Value::String("updated".into()));
    assert_eq!(sample["enabled"], Value::Bool(false));
    assert_eq!(sample["timeout"], Value::Number(0.into()));
    assert_eq!(sample["empty_str"], Value::String(String::new()));
    assert_eq!(sample["empty_list"], Value::Sequence(vec![]));
    assert!(sample.get("stale_key").is_none() && sample["nested"].get("old_child").is_none());
    assert_eq!(
        sample["nested"]["new_child"],
        Value::String("keep_me".into())
    );

    // Sequence items follow the new order.
    let path = write(
        &dir,
        "plugins:\n  configs:\n    sample:\n      vision_models:\n        - name: model-a\n          old_prop: stale-a\n        - name: model-b\n          old_prop: stale-b\n",
    );
    let mut cfg = Config::default();
    cfg.plugins.dir = "plugins".into();
    cfg.plugins.configs.insert(
        "sample".into(),
        plugin("vision_models:\n  - name: model-b\n    extra_b: x\n  - name: model-a\n"),
    );
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.find("model-b").unwrap() < text.find("model-a").unwrap(),
        "{text}"
    );
    assert!(
        !text.contains("old_prop") && text.contains("extra_b"),
        "{text}"
    );
}

// --- sanitisers --------------------------------------------------------------------------------

#[test]
fn xai_and_meta_keys_follow_codex_shape() {
    let cfg = parse(
        "xai-api-key:
  - api-key: ' xai-key '
    priority: 3
    weight: 5
    prefix: ' team-xai '
    base-url: ' https://api.x.ai/v1 '
    websockets: true
    alpha-search: true
    proxy-url: ' http://proxy.local '
    headers: {X-Custom: value}
    models: [{name: grok-4.5, alias: grok-latest, display-name: Grok Latest, force-mapping: true}]
    excluded-models: [' grok-3-* ']
    disable-cooling: true
    request-retry: 0
  - api-key: dropped
    base-url: ' '
meta-api-key:
  - {}
  - api-key: '   '
  - base-url: https://api.meta.ai/v1
  - headers: {X-Trace: placeholder}
  - api-key: ' LLM|valid '
  - api-key: 'dca:requires-oauth-storage'
",
    );
    assert_eq!(cfg.xai_key.len(), 1);
    let key = &cfg.xai_key[0];
    assert_eq!(key.api_key, " xai-key ", "api-key keeps its original value");
    assert_eq!(
        (key.priority, key.weight, key.prefix.as_str()),
        (3, Some(5), "team-xai")
    );
    assert_eq!(key.base_url, "https://api.x.ai/v1");
    assert_eq!(
        key.proxy_url, " http://proxy.local ",
        "proxy-url keeps its original value"
    );
    assert!(
        key.websockets && !key.alpha_search,
        "alpha-search is Codex-only"
    );
    assert_eq!(
        (key.disable_cooling, key.request_retry),
        (Some(true), Some(0))
    );
    assert_eq!(key.excluded_models, ["grok-3-*"]);
    assert_eq!(key.models[0].display_name, "Grok Latest");
    assert_eq!(
        cfg.meta_key.len(),
        1,
        "only the valid API key survives (DCA tokens need OAuth storage)"
    );
    assert_eq!(
        (
            cfg.meta_key[0].api_key.as_str(),
            cfg.meta_key[0].base_url.as_str()
        ),
        ("LLM|valid", "https://api.meta.ai/v1")
    );
}

#[test]
fn credential_lists_are_sanitised() {
    let cfg = parse(
        "gemini-api-key:
  - {api-key: ' k1 ', prefix: '/team/', headers: {' A ': ' b ', C: ''}, excluded-models: [' X ', x, '']}
  - {api-key: k1, prefix: team, headers: {A: b}}
  - {api-key: '', base-url: ''}
  - {api-key: '', base-url: ' https://only-base.invalid '}
vertex-api-key:
  - {api-key: v, base-url: ' https://v.invalid ', models: [{name: a, alias: b}, {name: a}, {alias: b}]}
  - {api-key: v, base-url: https://v.invalid}
  - {api-key: '  '}
openai-compatibility:
  - {name: ' keep ', base-url: ' https://oai.invalid ', prefix: 'a/b', headers: {X: ' y '}}
  - {name: dropped, base-url: ' '}
codex-api-key:
  - {api-key: c, base-url: ''}
  - {api-key: c, base-url: https://codex.invalid, prefix: ' p '}
",
    );
    assert_eq!(
        cfg.gemini_key.len(),
        2,
        "duplicates and empty entries are dropped"
    );
    assert_eq!(cfg.gemini_key[0].api_key, "k1");
    assert_eq!(cfg.gemini_key[0].prefix, "team");
    assert_eq!(cfg.gemini_key[0].headers.len(), 1);
    assert_eq!(cfg.gemini_key[0].headers["A"], "b");
    assert_eq!(cfg.gemini_key[0].excluded_models, ["x"]);
    assert_eq!(cfg.gemini_key[1].base_url, "https://only-base.invalid");
    assert_eq!(cfg.vertex_compat_api_key.len(), 1);
    assert_eq!(cfg.vertex_compat_api_key[0].models.len(), 1);
    assert_eq!(cfg.openai_compatibility.len(), 1);
    assert_eq!(cfg.openai_compatibility[0].name, "keep");
    assert_eq!(
        cfg.openai_compatibility[0].prefix, "",
        "a prefix with an inner slash is rejected"
    );
    assert_eq!(cfg.openai_compatibility[0].headers["X"], "y");
    assert_eq!(cfg.codex_key.len(), 1);
    assert_eq!(cfg.codex_key[0].prefix, "p");
}

#[test]
fn claude_keys_normalise_cloak_and_fingerprint() {
    let cfg = parse(
        "claude-api-key:
  - {api-key: a, fingerprint-profile: ' OAuth-CLI ', cloak: {mode: ' always ', sensitive-words: [' w ', '']}}
  - {api-key: b, fingerprint-profile: ' Weird '}
  - {api-key: c, fingerprint-profile: CLAUDE-CODE-CLI}
",
    );
    assert_eq!(cfg.claude_key[0].fingerprint_profile, "claude-code-cli");
    let cloak = cfg.claude_key[0].cloak.as_ref().unwrap();
    assert_eq!(
        (cloak.mode.as_str(), cloak.sensitive_words.as_slice()),
        ("always", ["w".to_string()].as_slice())
    );
    assert_eq!(
        cfg.claude_key[1].fingerprint_profile, "Weird",
        "unrecognised values are preserved (trimmed)"
    );
    assert_eq!(cfg.claude_key[2].fingerprint_profile, "claude-code-cli");
    assert_eq!(normalize_claude_fingerprint_profile("nope"), ("", false));
    assert!(validate_claude_fingerprint_profile("nope").is_err());
}

#[test]
fn oauth_maps_are_normalised() {
    let cfg = parse(
        "oauth-excluded-models: {' Codex ': [' A ', a, B], empty: [], '': [x]}
oauth-model-alias:
  Codex:
    - {name: gpt-5, alias: g5, fork: true, display-name: ' G5 '}
    - {name: gpt-5, alias: G5}
    - {name: same, alias: SAME}
    - {name: '', alias: x}
  ghost: []
oauth-settings:
  Codex:
    - {name: m, max-context-length: 1}
    - {name: ' M ', max-context-length: 2}
    - {name: m, alias: a, max-context-length: 3}
    - {name: ''}
oauth-request-scoped-errors:
  Codex:
    - {status: 400, match: [' x ', ''], action: ' STOP '}
    - {status: 0, match: [x], action: stop}
    - {status: 500, action: stop}
    - {status: 500, match: [x], action: ''}
",
    );
    assert_eq!(cfg.oauth_excluded_models.len(), 1);
    assert_eq!(cfg.oauth_excluded_models["codex"], ["a", "b"]);
    let aliases = &cfg.oauth_model_alias["codex"];
    assert_eq!(aliases.len(), 1);
    assert!(aliases[0].fork && aliases[0].display_name == "G5");
    assert!(!cfg.oauth_model_alias.contains_key("ghost"));
    let settings = &cfg.oauth_settings["codex"];
    // Later entries win for the same name/alias pair; order is preserved.
    assert_eq!(
        settings
            .iter()
            .map(|s| (s.alias.as_str(), s.max_context_length))
            .collect::<Vec<_>>(),
        [("", 2), ("a", 3)]
    );
    let rules = &cfg.oauth_request_scoped_errors["codex"];
    assert_eq!(rules.len(), 1);
    assert_eq!(
        (
            rules[0].status,
            rules[0].action.as_str(),
            rules[0].r#match.as_slice()
        ),
        (400, "stop", ["x".to_string()].as_slice())
    );
}

#[test]
fn oauth_model_setting_resolution() {
    let settings = vec![
        OAuthModelSetting {
            name: "gpt-5".into(),
            alias: String::new(),
            max_context_length: 10,
        },
        OAuthModelSetting {
            name: "gpt-5".into(),
            alias: "fast".into(),
            max_context_length: 20,
        },
        OAuthModelSetting {
            name: "gpt-5".into(),
            alias: String::new(),
            max_context_length: 30,
        },
    ];
    // An alias match on the requested ID wins; otherwise the later name match overrides.
    assert_eq!(
        resolve_oauth_model_setting(&settings, "FAST", "", "")
            .unwrap()
            .max_context_length,
        20
    );
    assert_eq!(
        resolve_oauth_model_setting(&settings, "gpt-5", "", "")
            .unwrap()
            .max_context_length,
        30
    );
    assert_eq!(
        resolve_oauth_model_setting(&settings, "x", "gpt-5", "")
            .unwrap()
            .max_context_length,
        30
    );
    assert!(resolve_oauth_model_setting(&settings, "other", "", "").is_none());
}

#[test]
fn payload_raw_rules_with_invalid_json_are_dropped() {
    let cfg = parse(
        "payload:
  default-raw:
    - {models: [{name: m}], params: {a: '{\"ok\": true}', b: 5}}
    - {models: [{name: m}], params: {a: 'not json'}}
    - {models: [{name: m}], params: {a: '   '}}
    - {models: [{name: m}], params: {}}
  override-raw:
    - {models: [{name: m}], params: {a: '[1, 2]'}}
    - {models: [{name: m}], params: {a: '{broken'}}
  default:
    - {models: [{name: m}], params: {a: 'not json is fine here', nothing: null}}
",
    );
    assert_eq!(cfg.payload.default_raw.len(), 1);
    assert_eq!(cfg.payload.override_raw.len(), 1);
    assert_eq!(
        cfg.payload.default[0].params["nothing"],
        Value::Null,
        "null params are data"
    );
}

#[test]
fn model_entries_parse_all_fields() {
    let cfg = parse(
        "openai-compatibility:
  - name: p
    base-url: https://p.invalid
    disabled: true
    support-prompt-cache-key: true
    api-key-entries: [{api-key: k, weight: 2, proxy-url: direct}]
    models:
      - name: m
        alias: a
        display-name: M
        max-context-length: 4096
        force-mapping: true
        image: true
        input-modalities: [text, image]
        output-modalities: [text]
        is-compat: true
        use-max-completion-tokens: true
        thinking: {min: 1, max: 100, zero-allowed: true, dynamic-allowed: true, levels: [low, high]}
codex-api-key:
  - base-url: https://c.invalid
    api-key: k
    models: [{name: m, alias: a, is-compat: true, support-configuration-update: true, max-context-length: 9}]
",
    );
    let p = &cfg.openai_compatibility[0];
    assert!(p.disabled && p.support_prompt_cache_key);
    assert_eq!(
        (
            p.api_key_entries[0].weight,
            p.api_key_entries[0].proxy_url.as_str()
        ),
        (Some(2), "direct")
    );
    let m = &p.models[0];
    assert_eq!(
        (
            m.max_context_length,
            m.force_mapping,
            m.image,
            m.is_compat,
            m.use_max_completion_tokens
        ),
        (4096, true, true, true, true)
    );
    assert_eq!(
        (m.input_modalities.len(), m.output_modalities.len()),
        (2, 1)
    );
    let t = m.thinking.as_ref().unwrap();
    assert_eq!(
        (
            t.min,
            t.max,
            t.zero_allowed,
            t.dynamic_allowed,
            t.levels.len()
        ),
        (1, 100, true, true, 2)
    );
    let cm = &cfg.codex_key[0].models[0];
    assert!(cm.is_compat && cm.support_configuration_update && cm.max_context_length == 9);
}

#[test]
fn remote_management_and_home_helpers() {
    let cfg = parse(
        "remote-management:\n  allow-remote: true\n  base-url: \"https://proxy.example.com\"\n",
    );
    assert_eq!(cfg.remote_management.base_url, "https://proxy.example.com");
    assert_eq!(
        (
            normalize_home_port(0),
            normalize_home_port(-4),
            normalize_home_port(9000)
        ),
        (8317, 8317, 9000)
    );
    assert!(
        !looks_like_bcrypt("a\u{e9}\u{e9}-secret"),
        "multi-byte prefixes must not panic"
    );
    assert!(
        looks_like_bcrypt("$2a$10$abc")
            && looks_like_bcrypt("$2y$10$abc")
            && !looks_like_bcrypt("$2x$10$abc")
            && !looks_like_bcrypt("$2a$")
    );
    // Unknown fields are ignored on load (Go decodes non-strictly).
    parse("totally-unknown: 1\nserver: {nested-unknown: true}\n");
}

#[test]
fn header_defaults_are_trimmed() {
    let cfg = parse(
        "codex-header-defaults: {user-agent: ' ua ', beta-features: ' b '}\nclaude-header-defaults: {user-agent: ' c ', os: ' linux ', stabilize-device-profile: true}\n",
    );
    assert_eq!(
        (
            cfg.codex_header_defaults.user_agent.as_str(),
            cfg.codex_header_defaults.beta_features.as_str()
        ),
        ("ua", "b")
    );
    assert_eq!(
        (
            cfg.claude_header_defaults.user_agent.as_str(),
            cfg.claude_header_defaults.os.as_str()
        ),
        ("c", "linux")
    );
    assert_eq!(
        cfg.claude_header_defaults.stabilize_device_profile,
        Some(true)
    );
}

// --- client.codex options ----------------------------------------------------------------------

#[test]
fn client_codex_options() {
    for (raw, want) in [
        ("config-version: 8\n", false),
        ("client: {codex: {optimize-multi-agent-v2: false}}\n", false),
        (
            "client: {codex: {optimize-multi-agent-v2: true, enable-apply-patch: true}}\n",
            true,
        ),
        ("client: {codex: {optimize-multi-agent-v2: null}}\n", false),
    ] {
        validate_v8_config(raw.as_bytes()).unwrap_or_else(|e| panic!("{raw:?}: {e}"));
        assert_eq!(
            parse(raw).client.codex.optimize_multi_agent_v2,
            want,
            "{raw:?}"
        );
    }
    for raw in [
        "client: {codex: {optimize-multi-agent-v2: invalid}}",
        "client: {codex: {optimize-multi-agent-v2: 1}}",
        "client: false",
        "client: {codex: false}",
        "client: {codex: {optimize-multi-agent-v2: true, optimize-multi-agent-v2: false}}",
    ] {
        assert!(
            validate_v8_config(raw.as_bytes()).is_err(),
            "accepted invalid config {raw:?}"
        );
    }
}

#[test]
fn client_codex_historical_spellings() {
    let old_paths = [
        "oauth.providers.codex.optimize-multi-agent-v2",
        "providers.codex.optimize-multi-agent-v2",
        "codex.optimize-multi-agent-v2",
    ];
    for (name, raw, want) in [
        ("flat", "codex: {optimize-multi-agent-v2: true}\n", true),
        (
            "providers",
            "providers: {codex: {optimize-multi-agent-v2: true}}\n",
            true,
        ),
        (
            "oauth",
            "oauth: {providers: {codex: {optimize-multi-agent-v2: true}}}\n",
            true,
        ),
        (
            "old false",
            "providers: {codex: {optimize-multi-agent-v2: false}}\n",
            false,
        ),
        (
            "client false wins",
            "client: {codex: {optimize-multi-agent-v2: false}}\nproviders: {codex: {optimize-multi-agent-v2: true}}\noauth: {providers: {codex: {optimize-multi-agent-v2: true}}}\ncodex: {optimize-multi-agent-v2: true}\n",
            false,
        ),
        (
            "client true wins",
            "client: {codex: {optimize-multi-agent-v2: true}}\noauth: {providers: {codex: {optimize-multi-agent-v2: false}}}\n",
            true,
        ),
        (
            "client null wins",
            "client: {codex: {optimize-multi-agent-v2: null}}\nproviders: {codex: {optimize-multi-agent-v2: true}}\n",
            false,
        ),
        (
            "oauth wins old conflicts",
            "oauth: {providers: {codex: {optimize-multi-agent-v2: false}}}\nproviders: {codex: {optimize-multi-agent-v2: true}}\ncodex: {optimize-multi-agent-v2: true}\n",
            false,
        ),
        (
            "providers wins flat",
            "providers: {codex: {optimize-multi-agent-v2: false}}\ncodex: {optimize-multi-agent-v2: true}\n",
            false,
        ),
        (
            "aliases",
            "client: &client {codex: {optimize-multi-agent-v2: false}}\nproviders: {<<: *client}\n",
            false,
        ),
    ] {
        let cfg = parse(raw);
        assert_eq!(cfg.client.codex.optimize_multi_agent_v2, want, "{name}");
        assert!(
            cfg.oauth_only_fields
                .iter()
                .all(|f| !f.contains("optimize")),
            "{name}: historical client field marked OAuth-only"
        );
        let (migrated, _) = normalize_config_layout(raw.as_bytes(), true).unwrap();
        validate_v8_config(&migrated)
            .unwrap_or_else(|e| panic!("{name}: {e}\n{}", String::from_utf8_lossy(&migrated)));
        assert_eq!(
            parse_config_bytes(&migrated)
                .unwrap()
                .client
                .codex
                .optimize_multi_agent_v2,
            want,
            "{name}: migration changed the value"
        );
        let (again, changed) = normalize_config_layout(&migrated, false).unwrap();
        assert!(
            !changed && again == migrated,
            "{name}: normalized config not stable"
        );
    }
    for path in old_paths {
        let nested = path
            .rsplit('.')
            .fold(String::from("true"), |acc, key| format!("{{{key}: {acc}}}"));
        assert!(
            validate_v8_config(nested.as_bytes()).is_err(),
            "v8 write accepted historical path {path}"
        );
        let (unchanged, changed) = normalize_config_layout(nested.as_bytes(), false).unwrap();
        assert!(
            !changed && unchanged == nested.as_bytes(),
            "legacy-only normalization rewrote {path}"
        );
    }
}

#[test]
fn client_codex_historical_field_keeps_comments_through_migrating_saves() {
    for old in ["oauth.providers.codex", "providers.codex", "codex"] {
        let indent = |depth: usize| "  ".repeat(depth);
        let mut raw = String::new();
        let parts: Vec<&str> = old.split('.').collect();
        for (depth, key) in parts.iter().enumerate() {
            raw.push_str(&format!("{}{key}:\n", indent(depth)));
        }
        raw.push_str(&format!("{}# keep optimize heading\n", indent(parts.len())));
        raw.push_str(&format!(
            "{}optimize-multi-agent-v2: true # keep optimize comment\n",
            indent(parts.len())
        ));
        raw.push_str("client:\n  codex:\n    enable-apply-patch: true\n");
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, &raw);
        let mut cfg = load_config(&path).unwrap();
        for enabled in [true, false, true] {
            cfg.client.codex.optimize_multi_agent_v2 = enabled;
            save_config_preserve_comments(&path, &mut cfg, true).unwrap();
            let data = std::fs::read_to_string(&path).unwrap();
            validate_v8_config(data.as_bytes()).unwrap_or_else(|e| panic!("{old}: {e}\n{data}"));
            let loaded = load_config(&path).unwrap();
            assert!(
                loaded.client.codex.optimize_multi_agent_v2 == enabled
                    && loaded.client.codex.enable_apply_patch,
                "{old}"
            );
            assert!(
                data.contains("keep optimize comment") && data.contains("keep optimize heading"),
                "{old} (enabled={enabled}):\n{data}"
            );
        }
    }
}

// --- OAuth scope -------------------------------------------------------------------------------

#[test]
fn oauth_scope_survives_snapshots_and_saves() {
    let raw = "codex: {stream-bootstrap-buffering: true}
oauth:
  providers:
    codex: {disable-codex-cloaking: true, model-level-cooling: true}
    claude:
      disable-claude-cloak-mode: true
      header-defaults: {user-agent: oauth-agent}
    xai: {inject-x-search: true}
api-keys:
  codex:
    - name: api
      base-url: https://example.invalid
      keys: [{api-key: test-key, disable-codex-cloaking: true}]
";
    let mut cfg = parse(raw);
    let snapshot = serde_yaml_ng::to_string(&cfg.to_yaml_value().unwrap()).unwrap();
    let decoded = parse(&snapshot);
    assert_eq!(cfg.oauth_only_fields, decoded.oauth_only_fields);
    for (name, value) in [
        ("parsed", &cfg),
        ("cloned", &cfg.clone()),
        ("snapshot", &decoded),
    ] {
        let api = value.for_api_key();
        assert!(
            !api.codex.disable_codex_cloaking
                && !api.codex.model_level_cooling
                && !api.disable_claude_cloak_mode
                && api.claude_header_defaults.user_agent.is_empty()
                && !api.xai.inject_x_search,
            "{name}: API-key view inherited OAuth-only settings"
        );
        assert!(
            api.codex.stream_bootstrap_buffering
                && api.codex_key[0].disable_codex_cloaking == Some(true),
            "{name}: API-key view lost a legacy setting or explicit key override"
        );
        assert!(
            value.codex.disable_codex_cloaking
                && value.xai.inject_x_search
                && value.claude_header_defaults.user_agent == "oauth-agent",
            "{name}: API-key view mutated the shared OAuth configuration"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, raw);
    cfg.debug = true;
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let reloaded = load_config(&path).unwrap();
    assert!(
        !reloaded.for_api_key().codex.disable_codex_cloaking
            && reloaded.codex.disable_codex_cloaking,
        "save/reload lost OAuth scope"
    );
}

#[test]
fn legacy_globals_keep_their_api_key_fallback() {
    let cfg = parse("codex: {disable-codex-cloaking: true}\ndisable-claude-cloak-mode: true\n");
    assert!(matches!(cfg.for_api_key(), std::borrow::Cow::Borrowed(_)));
    assert!(
        cfg.for_api_key().codex.disable_codex_cloaking
            && cfg.for_api_key().disable_claude_cloak_mode
    );
}

#[test]
fn oauth_scope_is_published_after_a_saving_migration() {
    for migrate in [false, true] {
        let raw = "codex: {disable-codex-cloaking: true, response-steering: true}\nxai: {inject-x-search: true}\n";
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, raw);
        let mut cfg = parse(raw);
        cfg.home.enabled = true;
        cfg.codex_response_steering = true;
        let mut before = cfg.clone();
        save_config_preserve_comments(&path, &mut cfg, migrate).unwrap();
        let disk = load_config(&path).unwrap();
        assert_eq!(
            cfg.oauth_only_fields, disk.oauth_only_fields,
            "migrate={migrate}: runtime and disk scopes differ"
        );
        let scoped = cfg.for_api_key();
        assert_ne!(scoped.codex.disable_codex_cloaking, migrate);
        assert_ne!(scoped.xai.inject_x_search, migrate);
        // Runtime-only state and values are untouched by the save.
        before.oauth_only_fields = cfg.oauth_only_fields.clone();
        assert_eq!(before, cfg, "migrate={migrate}");
    }
}

// --- saving ------------------------------------------------------------------------------------

#[test]
fn claude_cloak_updates_persist() {
    let initial = "claude-api-key:\n  - api-key: sk-ant-test\n    cloak:\n      mode: always\n      strict-mode: true\n      cache-user-id: true\n";
    let key = |cloak: CloakConfig| Config {
        claude_key: vec![ClaudeKey {
            api_key: "sk-ant-test".into(),
            cloak: Some(cloak),
            ..ClaudeKey::default()
        }],
        ..Config::default()
    };
    let dir = tempfile::tempdir().unwrap();

    // Clearing mode, strict-mode and cache-user-id removes them but keeps an explicit cloak block.
    let path = write(&dir, initial);
    let mut cfg = key(CloakConfig::default());
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    for stale in ["mode: always", "strict-mode: true", "cache-user-id: true"] {
        assert!(!text.contains(stale), "{stale} remained:\n{text}");
    }
    let cloak = load_config(&path).unwrap().claude_key[0]
        .cloak
        .clone()
        .expect("explicit cloak survives");
    assert!(cloak.mode.is_empty() && !cloak.strict_mode && cloak.cache_user_id != Some(true));

    // An explicit false is preserved for pointer-backed booleans.
    let path = write(&dir, initial);
    let mut cfg = key(CloakConfig {
        mode: "auto".into(),
        cache_user_id: Some(false),
        ..CloakConfig::default()
    });
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("mode: auto")
            && text.contains("cache-user-id: false")
            && !text.contains("strict-mode: true"),
        "{text}"
    );
    let cloak = load_config(&path).unwrap().claude_key[0]
        .cloak
        .clone()
        .unwrap();
    assert_eq!(
        (cloak.mode.as_str(), cloak.cache_user_id),
        ("auto", Some(false))
    );
}

#[test]
fn save_keeps_comments_order_and_does_not_add_defaults_for_zero_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        &dir,
        "# keep me\nport: 9000 # inline\n\n# section\nrequest-retry: 2\n",
    );
    let mut cfg = load_config(&path).unwrap();
    cfg.request_retry = 0; // existing keys are updated even to zero
    cfg.debug = false; // new zero keys are not added
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep me\nport: 9000 # inline"), "{text}");
    assert!(text.contains("# section\nrequest-retry: 0"), "{text}");
    assert!(!text.contains("debug:"), "{text}");
    assert!(text.find("port").unwrap() < text.find("request-retry").unwrap());
}

#[test]
fn load_hashes_a_plaintext_management_secret_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(&dir, "remote-management:\n  secret-key: hunter2 # keep\n");
    let cfg = load_config(&path).unwrap();
    assert!(looks_like_bcrypt(&cfg.remote_management.secret_key));
    assert!(bcrypt::verify("hunter2", &cfg.remote_management.secret_key).unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("# keep") && !text.contains("hunter2"),
        "{text}"
    );
    // parse_config_bytes hashes in memory only.
    let parsed = parse("remote-management: {secret-key: plain}");
    assert!(looks_like_bcrypt(&parsed.remote_management.secret_key));
}

#[test]
fn dotenv_loading_does_not_override_existing_variables() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        !load_dotenv(dir.path()).unwrap(),
        "missing .env is not an error"
    );
    std::fs::write(
        dir.path().join(".env"),
        "CPA_CONFIG_TEST_NEW=from-file\nCPA_CONFIG_TEST_QUOTED=\"a b\"\nexport CPA_CONFIG_TEST_EXPORTED=1\nHOME=/should/not/override\n",
    )
    .unwrap();
    let home = std::env::var("HOME").unwrap();
    assert!(load_dotenv(dir.path()).unwrap());
    assert_eq!(std::env::var("CPA_CONFIG_TEST_NEW").unwrap(), "from-file");
    assert_eq!(std::env::var("CPA_CONFIG_TEST_QUOTED").unwrap(), "a b");
    assert_eq!(std::env::var("CPA_CONFIG_TEST_EXPORTED").unwrap(), "1");
    assert_eq!(std::env::var("HOME").unwrap(), home);
}

#[test]
fn serialisation_matches_the_legacy_layout() {
    let cfg = parse("port: 8317\nclaude-api-key: [{api-key: a, models: [{name: n, alias: x}]}]\n");
    let value = serde_yaml_ng::to_value(&cfg).unwrap();
    let map = value.as_mapping().unwrap();
    for key in [
        "port",
        "host",
        "tls",
        "remote-management",
        "claude-api-key",
        "ws-auth",
        "pprof",
        "credential-in-flight",
    ] {
        assert!(
            map.contains_key(key),
            "{key} missing from serialised config"
        );
    }
    for omitted in [
        "oauth-excluded-models",
        "gpt-image-2-base-model",
        "antigravity-signature-cache-enabled",
        "nonstream-keepalive-interval",
    ] {
        assert!(!map.contains_key(omitted), "{omitted} should be omitempty");
    }
    // Runtime-only state never serialises.
    assert!(!map.contains_key("home") && !map.contains_key("oauth_only_fields"));
    assert_eq!(
        value["credential-concurrency"]["cpa-heartbeat-timeout"],
        Value::String("3s".into())
    );
}

// --- source text of scalars ---------------------------------------------------------------------

#[test]
fn string_fields_keep_the_source_text_of_scalars() {
    // yaml.v3 decodes a scalar into a string field using its source text; resolving it first
    // would silently change secrets (`True` -> `true`, `1.50` -> `1.5`, `0o7` -> `7`).
    let cfg = parse(
        "api-keys: [True, 1.50, 12e4, 0o7, 0x1F, 1_000, +5, .5]\ngemini-api-key: [{api-key: 0o7, prefix: True, base-url: 1.50}]\n",
    );
    assert_eq!(
        cfg.api_keys,
        ["True", "1.50", "12e4", "0o7", "0x1F", "1_000", "+5", ".5"]
    );
    let key = &cfg.gemini_key[0];
    assert_eq!(
        (
            key.api_key.as_str(),
            key.prefix.as_str(),
            key.base_url.as_str()
        ),
        ("0o7", "True", "1.50")
    );
    // Quoted scalars are strings already.
    assert_eq!(
        parse("api-keys: ['0o7', \"1.50\"]").api_keys,
        ["0o7", "1.50"]
    );
}

#[test]
fn integer_literals_follow_yaml_v3() {
    let cfg = parse(
        "request-retry: 0o10\nport: 0x1F\nmax-retry-interval: 1_000\nmax-retry-credentials: 010\n",
    );
    assert_eq!(
        (
            cfg.request_retry,
            cfg.port,
            cfg.max_retry_interval,
            cfg.max_retry_credentials
        ),
        (8, 31, 1000, 8)
    );
    parse("gemini-api-key: [{api-key: k, weight: 0x10}]");
    assert!(parse_config_bytes(b"gemini-api-key: [{api-key: k, weight: 1.50}]").is_err());
    // The version must literally be 8.
    for bad in [
        "config-version: +8",
        "config-version: 0x8",
        "config-version: 8.0",
        "config-version: '8'",
    ] {
        assert!(parse_config_bytes(bad.as_bytes()).is_err(), "{bad}");
    }
    parse("config-version: 8");
}

#[test]
fn saving_keeps_the_written_form_of_opaque_scalars() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        &dir,
        "plugins:\n  configs:\n    p:\n      version: 1.10\n      flag: True\n      note: |\n        line\n        # not a comment\n",
    );
    let mut cfg = load_config(&path).unwrap();
    cfg.debug = true;
    save_config_preserve_comments(&path, &mut cfg, false).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("version: 1.10") && text.contains("flag: True"),
        "{text}"
    );
    assert!(text.contains("# not a comment"), "{text}");
    // Migration must not treat `#` lines inside block scalars as comments to re-indent.
    let (migrated, _) = normalize_config_layout(text.as_bytes(), true).unwrap();
    let again = parse_config_bytes(&migrated).unwrap();
    assert_eq!(
        again.plugins.configs["p"].raw["note"],
        Value::String("line\n# not a comment\n".into())
    );
}

// --- durations and live relay flags -------------------------------------------------------------

#[test]
fn durations_reject_bare_numbers() {
    // yaml.v3 decodes only strings into time.Duration: a bare 0 is an int and fails, "0" parses.
    assert!(parse_config_bytes(b"credential-concurrency: {cpa-heartbeat-timeout: 0}").is_err());
    assert!(parse_config_bytes(b"credential-concurrency: {cpa-heartbeat-timeout: 5}").is_err());
    let cfg =
        parse("credential-concurrency: {lifecycle-config-revision: 1, cpa-heartbeat-timeout: '0'}");
    assert_eq!(
        cfg.credential_concurrency.cpa_heartbeat_timeout,
        GoDuration(0)
    );
}

#[test]
fn private_ip_flags_decode_like_go() {
    let disabled = |yaml: &str| {
        parse(yaml)
            .codex
            .live_media_relay
            .disable_private_remote_ips
    };
    assert!(!disabled(
        "codex: {live-media-relay: {allow-private-remote-ips: yes}}"
    ));
    assert!(disabled(
        "codex: {live-media-relay: {allow-private-remote-ips: n}}"
    ));
    assert!(
        disabled("codex: {live-media-relay: {allow-private-remote-ips: null}}"),
        "null decodes to false"
    );
    assert!(disabled(
        "oauth: {providers: {codex: {live-media-relay: {disable-private-remote-ips: on}}}}"
    ));
    // Plain-field errors are reported before flag errors.
    let both_bad = b"oauth: {providers: {codex: {live-media-relay: {max-sessions: x, disable-private-remote-ips: maybe}}}}";
    let err = parse_config_bytes(both_bad).unwrap_err();
    assert!(
        !err.to_string().contains("disable-private-remote-ips"),
        "{err}"
    );
    assert!(
        parse_config_bytes(b"codex: {live-media-relay: {allow-private-remote-ips: maybe}}")
            .is_err()
    );
    // A huge session count must not overflow the port-range check.
    let big = format!(
        "codex: {{live-media-relay: {{enabled: true, max-sessions: {}, udp-port-min: 1, udp-port-max: 2}}}}",
        i64::MAX
    );
    assert!(parse_config_bytes(big.as_bytes()).is_err());
}

// --- JSON view and thinking ---------------------------------------------------------------------

#[test]
fn json_view_hides_secrets_and_uses_go_json_names() {
    let cfg = parse(
        "host: 0.0.0.0
port: 9000
auth-dir: /srv/auth
remote-management:
  allow-remote: true
  secret-key: '$2a$10$abcdefghijklmnopqrstuuabcdefghijklmnopqrstuuabcdefghi'
codex:
  live-media-relay:
    enabled: true
    ice-servers: [{urls: ['turn:t:3478'], username: u, credential: c}]
plugins:
  store-auth: [{match: https://p/, apply-to: [registry], type: bearer, token-env: T}]
  configs: {a: {enabled: true, priority: 3, secret: s}, b: {}}
claude-api-key:
  - api-key: a
    models: [{name: m, alias: x, thinking: {min: 1, zero-allowed: true, dynamic-allowed: true, levels: [low]}}]
payload:
  default: [{models: [{name: m}], params: {thinking: {zero-allowed: 1}}}]
",
    );
    let json = cfg.to_json_value().unwrap();
    let text = json.to_string();
    for secret in [
        "0.0.0.0",
        "9000",
        "/srv/auth",
        "abcdefghijklmnop",
        "\"remote-management\"",
        "\"u\"",
        "\"c\"",
        "secret",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    for hidden in ["host", "port", "auth-dir", "remote-management"] {
        assert!(json.get(hidden).is_none(), "{hidden}");
    }
    assert_eq!(
        json["credential-concurrency"]["cpa-heartbeat-timeout"], 3_000_000_000i64,
        "durations are integer nanoseconds"
    );
    assert_eq!(json["plugins"]["store-auth"][0]["apply_to"][0], "registry");
    assert_eq!(json["plugins"]["store-auth"][0]["token_env"], "T");
    assert_eq!(
        json["plugins"]["configs"]["a"],
        serde_json::json!({"enabled": true, "priority": 3})
    );
    assert_eq!(
        json["plugins"]["configs"]["b"],
        serde_json::json!({"enabled": false})
    );
    let thinking = &json["claude-api-key"][0]["models"][0]["thinking"];
    assert_eq!(
        thinking,
        &serde_json::json!({"min": 1, "zero_allowed": true, "dynamic_allowed": true, "levels": ["low"]})
    );
    assert!(
        json["payload"]["default"][0]["params"]["thinking"]
            .get("zero-allowed")
            .is_some(),
        "free-form params are untouched"
    );
    assert_eq!(json["disable-image-generation"], false);
}

#[test]
fn thinking_uses_the_registry_type_with_kebab_yaml() {
    let cfg = parse(
        "claude-api-key: [{api-key: a, models: [{name: m, thinking: {min: 1, max: 9, zero-allowed: true, dynamic_allowed: true, levels: [low]}}]}]",
    );
    let thinking: &cpa_core::registry::ThinkingSupport =
        cfg.claude_key[0].models[0].thinking.as_ref().unwrap();
    assert_eq!(
        (
            thinking.min,
            thinking.max,
            thinking.zero_allowed,
            thinking.dynamic_allowed
        ),
        (1, 9, true, true)
    );
    let value = serde_yaml_ng::to_value(&cfg).unwrap();
    let written = &value["claude-api-key"][0]["models"][0]["thinking"];
    assert_eq!(written["zero-allowed"], Value::Bool(true));
    assert!(written.get("zero_allowed").is_none());
}

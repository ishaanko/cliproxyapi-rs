//! Ports of the Go `config_v8_upstream_test.go` and `config_v8_compatibility_test.go`: shared
//! upstream settings and historical v8 aliases through the management config API.

mod common;

use axum::http::Method;
use common::harness;
use serde_json::{Value, json};

const GET: Method = Method::GET;
const PUT: Method = Method::PUT;
const PATCH: Method = Method::PATCH;
const DELETE: Method = Method::DELETE;

fn config_uri(path: &str) -> String {
    format!("/v8/management/config/{path}")
}

#[tokio::test]
async fn shared_upstream_round_trip() {
    let raw = "oauth: {providers: {codex: {stream-bootstrap-buffering: true, header-defaults: {user-agent: oauth-agent}}}}\n";
    let h = harness(raw);
    let body = h
        .expect(GET, &config_uri("upstream/codex/stream-bootstrap-buffering"), "", 200)
        .await;
    assert_eq!(body.trim(), "true", "historical shared field was not exposed at upstream");
    assert_eq!(h.read(), raw, "GET rewrote the historical document");

    h.expect(PUT, &config_uri("upstream/codex/stream-bootstrap-buffering"), "false", 200).await;
    h.expect(PUT, &config_uri("oauth/auth-auto-refresh-workers"), "3", 200).await;
    h.expect(
        PATCH,
        &config_uri("upstream/claude"),
        r#"{"header-defaults":{"timezone":"Asia/Singapore","stabilize-device-profile":false}}"#,
        200,
    )
    .await;
    let loaded = h.load();
    assert!(
        !loaded.codex.stream_bootstrap_buffering
            && loaded.auth_auto_refresh_workers == 3
            && loaded.claude_header_defaults.timezone == "Asia/Singapore"
            && loaded.for_api_key().claude_header_defaults.timezone == "Asia/Singapore",
        "management write lost shared upstream values"
    );
    assert!(
        loaded.codex_header_defaults.user_agent == "oauth-agent"
            && loaded.for_api_key().codex_header_defaults.user_agent.is_empty(),
        "management write changed OAuth-only header scope"
    );
    let before = h.read();
    h.expect(
        PUT,
        &config_uri("oauth/providers/codex/stream-bootstrap-buffering"),
        r#""invalid""#,
        422,
    )
    .await;
    assert_eq!(h.read(), before, "rejected historical-path write changed the document");
    h.expect(DELETE, &config_uri("upstream/claude"), "", 200).await;
    assert!(!h.expect(GET, &config_uri("upstream/claude"), "", 404).await.is_empty());
    assert!(h.load().claude_header_defaults.timezone.is_empty());
}

#[tokio::test]
async fn historical_field_paths() {
    for (historical, current, value) in [
        ("oauth/providers/codex/disable-codex-cloaking", "upstream/codex/disable-codex-cloaking", "true"),
        ("oauth/providers/codex/stream-bootstrap-buffering", "upstream/codex/stream-bootstrap-buffering", "true"),
        ("oauth/providers/codex/stream-bootstrap-timeout", "upstream/codex/stream-bootstrap-timeout", r#""10s""#),
        ("oauth/providers/codex/orphan-delegation-compatibility", "upstream/codex/orphan-delegation-compatibility", "true"),
        ("oauth/providers/codex/model-level-cooling", "upstream/codex/model-level-cooling", "true"),
        ("oauth/providers/codex/response-steering", "upstream/codex/response-steering", "true"),
        ("oauth/providers/claude/model-level-cooling", "upstream/claude/model-level-cooling", "true"),
        ("oauth/providers/claude/disable-claude-cloak-mode", "upstream/claude/disable-claude-cloak-mode", "true"),
        ("oauth/providers/claude/header-defaults/user-agent", "upstream/claude/header-defaults/user-agent", r#""test-agent""#),
        ("oauth/providers/claude/header-defaults/package-version", "upstream/claude/header-defaults/package-version", r#""0.2.0""#),
        ("oauth/providers/claude/header-defaults/runtime-version", "upstream/claude/header-defaults/runtime-version", r#""v22""#),
        ("oauth/providers/claude/header-defaults/os", "upstream/claude/header-defaults/os", r#""Windows""#),
        ("oauth/providers/claude/header-defaults/arch", "upstream/claude/header-defaults/arch", r#""amd64""#),
        ("oauth/providers/claude/header-defaults/timeout", "upstream/claude/header-defaults/timeout", r#""300""#),
        ("oauth/providers/claude/header-defaults/timezone", "upstream/claude/header-defaults/timezone", r#""Asia/Shanghai""#),
        ("oauth/providers/claude/header-defaults/stabilize-device-profile", "upstream/claude/header-defaults/stabilize-device-profile", "true"),
        ("oauth/providers/claude/claude-code/disable-cloaking-model-list", "upstream/claude/disable-cloaking-model-list", "false"),
        ("oauth/providers/xai/inject-x-search", "upstream/xai/inject-x-search", "true"),
        ("oauth/providers/codex/optimize-multi-agent-v2", "client/codex/optimize-multi-agent-v2", "true"),
    ] {
        let h = harness("server: {port: 8317}\n");
        h.expect(PUT, &config_uri(current), value, 200).await;
        let got = h.expect(GET, &config_uri(historical), "", 200).await;
        assert_eq!(got.trim(), value, "{historical}: historical GET");
        h.expect(PATCH, &config_uri(historical), value, 200).await;
        h.expect(PUT, &config_uri(historical), "null", 200).await;
        let got = h.expect(GET, &config_uri(current), "", 200).await;
        assert_eq!(got.trim(), "null", "{historical}: historical PUT lost explicit null");
        h.expect(DELETE, &config_uri(historical), "", 200).await;
        h.expect(GET, &config_uri(current), "", 404).await;
        let port = h.expect(GET, &config_uri("server/port"), "", 200).await;
        assert_eq!(port.trim(), "8317", "{historical}: historical DELETE changed unrelated settings");
        cpa_config::validate_v8_config(h.read().as_bytes())
            .unwrap_or_else(|e| panic!("{historical}: saved historical aliases: {e}"));
    }
}

#[tokio::test]
async fn historical_provider_subtrees() {
    // (name, method, path, body, steering, buffering, optimize, agent)
    for (name, method, path, body, steering, buffering, optimize, agent) in [
        ("patch old provider", PATCH, "oauth/providers/codex", r#"{"response-steering":false,"header-defaults":{"user-agent":"updated"}}"#, false, true, true, "updated"),
        ("replace old provider", PUT, "oauth/providers/codex", r#"{"response-steering":true}"#, true, false, false, ""),
        ("delete old provider", DELETE, "oauth/providers/codex", "", false, false, false, ""),
        ("replace shared provider", PUT, "upstream/codex", r#"{"response-steering":false}"#, false, false, true, "oauth-agent"),
    ] {
        let raw = "upstream: {codex: {response-steering: true, stream-bootstrap-buffering: true}, xai: {inject-x-search: true}}\noauth: {providers: {codex: {header-defaults: {user-agent: oauth-agent}}}}\nclient: {codex: {optimize-multi-agent-v2: true, enable-apply-patch: true}}\n";
        let h = harness(raw);
        let view = h.expect(GET, &config_uri("oauth/providers/codex"), "", 200).await;
        let provider: Value = serde_json::from_str(&view).unwrap();
        assert!(
            provider["response-steering"] == json!(true) && provider["optimize-multi-agent-v2"] == json!(true),
            "{name}: historical provider view omitted shared/client values: {view}"
        );
        assert_eq!(h.read(), raw, "{name}: historical GET changed the file");
        h.expect(method, &config_uri(path), body, 200).await;
        let loaded = h.load();
        assert!(
            loaded.codex.response_steering == steering
                && loaded.codex.stream_bootstrap_buffering == buffering
                && loaded.client.codex.optimize_multi_agent_v2 == optimize
                && loaded.codex_header_defaults.user_agent == agent,
            "{name}: historical subtree mutation did not preserve replacement/merge semantics"
        );
        assert!(
            loaded.xai.inject_x_search
                && loaded.client.codex.enable_apply_patch
                && loaded.for_api_key().codex_header_defaults.user_agent.is_empty(),
            "{name}: historical subtree mutation changed unrelated settings or OAuth scope"
        );
        cpa_config::validate_v8_config(h.read().as_bytes())
            .unwrap_or_else(|e| panic!("{name}: saved historical provider layout: {e}"));
    }
}

#[tokio::test]
async fn historical_configuration_bodies() {
    // (name, method, route, body, steering, buffering)
    for (name, method, route, body, steering, buffering) in [
        ("patch old body", PATCH, "config", r#"{"oauth":{"providers":{"codex":{"response-steering":false}}}}"#, false, true),
        ("canonical false wins", PATCH, "config", r#"{"oauth":{"providers":{"codex":{"response-steering":true}}},"upstream":{"codex":{"response-steering":false}}}"#, false, true),
        ("canonical null wins", PATCH, "config", r#"{"oauth":{"providers":{"codex":{"response-steering":true}}},"upstream":{"codex":{"response-steering":null}}}"#, false, true),
        ("replace old JSON", PUT, "config", r#"{"oauth":{"providers":{"codex":{"response-steering":true}}}}"#, true, false),
        ("replace old YAML", PUT, "config.yaml", "# Keep this comment\noauth: {providers: {codex: {response-steering: false}}}\n", false, false),
    ] {
        let h = harness("upstream: {codex: {response-steering: true, stream-bootstrap-buffering: true}}\n");
        h.expect(method, &format!("/v8/management/{route}"), body, 200).await;
        let data = h.read();
        cpa_config::validate_v8_config(data.as_bytes())
            .unwrap_or_else(|e| panic!("{name}: saved historical request body: {e}"));
        assert!(data.contains("config-version: 8"), "{name}: historical v8 request did not save the latest version");
        let loaded = h.load();
        assert!(
            loaded.codex.response_steering == steering && loaded.codex.stream_bootstrap_buffering == buffering,
            "{name}: historical body lost effective values:\n{data}"
        );
        if route == "config.yaml" {
            assert!(data.contains("# Keep this comment"), "YAML alias normalization lost comments");
        }
    }
}

#[tokio::test]
async fn legacy_config_yaml_saves_by_submitted_version() {
    // (name, body, steering, v8)
    for (name, body, steering, v8) in [
        ("legacy", "# Keep this comment\ncodex: {response-steering: true}\n", true, false),
        ("historical v8", "# Keep this comment\noauth: {providers: {codex: {response-steering: true}}}\n", true, true),
        ("latest v8", "# Keep this comment\nconfig-version: 8\nupstream: {codex: {response-steering: false}}\n", false, true),
        ("mixed", "# Keep this comment\nrequest-retry: 3\nupstream: {codex: {response-steering: false}}\n", false, true),
    ] {
        let h = harness("server: {port: 8317}\n");
        h.expect(PUT, "/v0/management/config.yaml", body, 200).await;
        let data = h.read();
        if !v8 {
            assert_eq!(data, body, "{name}: v0 changed the submitted layout");
        } else {
            assert!(
                cpa_config::validate_v8_config(data.as_bytes()).is_ok() && data.contains("config-version: 8"),
                "{name}: v0 did not normalize the submitted v8 document:\n{data}"
            );
        }
        assert!(data.contains("# Keep this comment"), "{name}: v0 YAML write lost comments");
        assert_eq!(h.load().for_api_key().codex.response_steering, steering, "{name}");
    }
}

#[tokio::test]
async fn v0_setter_saves_by_existing_version() {
    for (name, raw, v8) in [
        ("legacy", "request-retry: 3\ncodex: {response-steering: true}\nxai: {inject-x-search: true}\nrouting: {strategy: fill-first}\n", false),
        ("historical v8", "config-version: 8\noauth: {providers: {codex: {response-steering: true, header-defaults: {user-agent: oauth-agent}}, xai: {inject-x-search: true}}}\n", true),
        ("unversioned v8", "oauth: {providers: {codex: {response-steering: true}, xai: {inject-x-search: true}}}\n", true),
        ("latest v8", "config-version: 8\nupstream: {codex: {response-steering: true}, xai: {inject-x-search: true}}\nrouting: {retry: {request-retry: 3}}\n", true),
    ] {
        let h = harness(raw);
        let agent = h.load().codex_header_defaults.user_agent;
        h.expect(PUT, "/v0/management/request-retry", r#"{"value":2}"#, 200).await;
        let data = h.read();
        if v8 {
            assert!(
                cpa_config::validate_v8_config(data.as_bytes()).is_ok() && data.contains("config-version: 8"),
                "{name}: v0 saved historical/legacy paths in a v8 file:\n{data}"
            );
        } else {
            assert!(
                !data.contains("config-version:") && !data.contains("upstream:"),
                "{name}: v0 migrated a legacy-only file:\n{data}"
            );
        }
        let loaded = h.load();
        assert!(
            loaded.request_retry == 2
                && loaded.for_api_key().codex.response_steering
                && loaded.for_api_key().xai.inject_x_search
                && loaded.codex_header_defaults.user_agent == agent,
            "{name}: v0 save changed effective settings"
        );
    }
}

#[tokio::test]
async fn historical_null_container_patch() {
    let raw = "upstream: {claude: {model-level-cooling: true, header-defaults: {user-agent: agent, timezone: UTC, stabilize-device-profile: true}}}\n";
    let h = harness(raw);
    h.expect(
        PATCH,
        "/v8/management/config",
        r#"{"oauth":{"providers":{"claude":{"header-defaults":null}}}}"#,
        200,
    )
    .await;
    let loaded = h.load();
    assert!(
        loaded.claude_header_defaults.user_agent.is_empty()
            && loaded.claude_header_defaults.timezone.is_empty()
            && loaded.claude.model_level_cooling,
        "historical null container failed to reset defaults or changed cooling"
    );
}

#[tokio::test]
async fn v0_repeated_saves_keep_legacy_client_alias() {
    let raw = "# LEGACY DOCUMENT\ncodex:\n  # OPTIMIZE HEAD\n  optimize-multi-agent-v2: true # OPTIMIZE INLINE\n  response-steering: true\nrequest-retry: 3\n";
    let h = harness(raw);
    for body in [r#"{"value":2}"#, r#"{"value":1}"#, r#"{"value":0}"#] {
        h.expect(PUT, "/v0/management/request-retry", body, 200).await;
        let data = h.read();
        assert!(
            !data.contains("config-version:")
                && !data.contains("upstream:")
                && !data.contains("client:")
                && data.contains("optimize-multi-agent-v2: true"),
            "v0 save changed the legacy layout: {data}"
        );
        for marker in ["LEGACY DOCUMENT", "OPTIMIZE HEAD", "OPTIMIZE INLINE"] {
            assert_eq!(data.matches(marker).count(), 1, "legacy comment {marker}: {data}");
        }
        let loaded = h.load();
        assert!(
            loaded.client.codex.optimize_multi_agent_v2 && loaded.for_api_key().codex.response_steering,
            "v0 save changed effective settings"
        );
    }
}

#[tokio::test]
async fn v8_field_updates_keep_comments() {
    for method in [PUT, PATCH] {
        for path in [
            "oauth/providers/codex/response-steering",
            "upstream/codex/response-steering",
        ] {
            let raw = "oauth:\n  providers:\n    codex: # PROVIDER INLINE\n      # FIELD HEAD\n      response-steering: true # FIELD INLINE\n\n      # FIELD FOOT\n";
            let h = harness(raw);
            for body in ["false", "null", "true"] {
                h.expect(method.clone(), &config_uri(path), body, 200).await;
                let data = h.read();
                for marker in ["PROVIDER INLINE", "FIELD HEAD", "FIELD INLINE", "FIELD FOOT"] {
                    assert_eq!(
                        data.matches(marker).count(),
                        1,
                        "{method} {path} ({body}) changed {marker}:\n{data}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn client_multi_agent_migration() {
    for legacy in [
        "codex: {optimize-multi-agent-v2: true}\n",
        "providers: {codex: {optimize-multi-agent-v2: true}}\n",
        "oauth: {providers: {codex: {optimize-multi-agent-v2: true}}}\n",
    ] {
        let raw = format!("{legacy}client: {{codex: {{enable-apply-patch: true}}}}\n");
        let h = harness(&raw);
        let canonical = config_uri("client/codex/optimize-multi-agent-v2");
        let got = h.expect(GET, &canonical, "", 200).await;
        assert_eq!(got.trim(), "true", "{legacy}: GET canonical value");
        h.expect(PUT, &canonical, r#""invalid""#, 422).await;
        assert_eq!(h.read(), raw, "{legacy}: GET or rejected write changed legacy file");
        for old in ["providers/codex", "oauth/providers/codex", "codex"] {
            let url = config_uri(&format!("{old}/optimize-multi-agent-v2"));
            h.expect(PUT, &url, "true", 200).await;
            let got = h.expect(GET, &url, "", 200).await;
            assert_eq!(got.trim(), "true", "{legacy}: historical client path {old}");
        }
        for enabled in [false, true] {
            h.expect(PUT, &canonical, &enabled.to_string(), 200).await;
            let data = h.read();
            cpa_config::validate_v8_config(data.as_bytes())
                .unwrap_or_else(|e| panic!("{legacy}: invalid saved config: {e}"));
            let loaded = h.load();
            assert!(
                loaded.client.codex.optimize_multi_agent_v2 == enabled && loaded.client.codex.enable_apply_patch,
                "{legacy}: management update changed client settings"
            );
        }
        h.expect(DELETE, &canonical, "", 200).await;
        let loaded = h.load();
        assert!(
            !loaded.client.codex.optimize_multi_agent_v2 && loaded.client.codex.enable_apply_patch,
            "{legacy}: delete did not restore default false or changed apply_patch"
        );
    }
}

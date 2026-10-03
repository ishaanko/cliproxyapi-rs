//! Ports of Go `registry_test.go`.

use crate::registry::*;

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn valid_plugin() -> Plugin {
    Plugin {
        id: "sample-provider".into(),
        name: "Sample Provider".into(),
        description: "Adds sample provider support.".into(),
        author: "author-name".into(),
        version: "0.1.0".into(),
        repository: "https://github.com/author-name/cliproxy-sample-provider-plugin".into(),
        ..Default::default()
    }
}

fn direct_plugin(url: &str) -> Plugin {
    Plugin {
        version: "0.2.0".into(),
        repository: String::new(),
        install: InstallPlan {
            kind: INSTALL_TYPE_DIRECT.into(),
            artifacts: vec![Artifact {
                goos: "linux".into(),
                goarch: "amd64".into(),
                url: url.into(),
                sha256: SHA.into(),
                size: 0,
            }],
        },
        ..valid_plugin()
    }
}

#[test]
fn parse_registry_validates_registry() {
    let registry = parse_registry(
        br#"{
        "schema_version": 1,
        "plugins": [{
            "id": "sample-provider",
            "name": "Sample Provider",
            "description": "Adds sample provider support.",
            "author": "author-name",
            "version": "0.1.0",
            "repository": "https://github.com/author-name/cliproxy-sample-provider-plugin",
            "logo": "https://example.com/logo.png",
            "homepage": "https://github.com/author-name/cliproxy-sample-provider-plugin",
            "license": "MIT",
            "tags": ["provider"]
        }]
    }"#,
    )
    .expect("parse");
    let plugin = registry.plugin_by_id("sample-provider").expect("plugin present");
    assert_eq!(plugin.version, "0.1.0");
}

#[test]
fn parse_registry_normalizes_plugin_fields() {
    let registry = parse_registry(
        br#"{
        "schema_version": 1,
        "plugins": [{
            "id": " sample-provider ",
            "name": " Sample Provider ",
            "description": " Adds sample provider support. ",
            "author": " author-name ",
            "version": " 0.1.0 ",
            "repository": " https://github.com/author-name/cliproxy-sample-provider-plugin ",
            "logo": " https://example.com/logo.png ",
            "homepage": " https://github.com/author-name/cliproxy-sample-provider-plugin ",
            "license": " MIT ",
            "tags": [" provider "]
        }]
    }"#,
    )
    .expect("parse");
    let plugin = registry.plugin_by_id("sample-provider").expect("plugin present");
    assert_eq!(plugin.id, "sample-provider");
    assert_eq!(plugin.version, "0.1.0");
    assert_eq!(plugin.repository, "https://github.com/author-name/cliproxy-sample-provider-plugin");
    assert_eq!(plugin.name, "Sample Provider");
    assert_eq!(plugin.tags[0], "provider");
}

#[test]
fn validate_registry_allows_missing_version() {
    let mut plugin = valid_plugin();
    plugin.version.clear();
    validate_registry(&Registry { schema_version: 1, plugins: vec![plugin] }).expect("missing version allowed");
}

#[test]
fn parse_registry_supports_direct_install() {
    let registry = parse_registry(
        br#"{
        "schema_version": 2,
        "plugins": [{
            "id": "sample-provider",
            "name": "Sample Provider",
            "description": "Adds sample provider support.",
            "author": "author-name",
            "version": "0.2.0",
            "auth_required": true,
            "install": {
                "type": "direct",
                "artifacts": [{
                    "goos": "windows",
                    "goarch": "x64",
                    "url": "https://downloads.example/sample-provider.zip",
                    "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                }]
            },
            "versions": [{
                "version": "0.1.0",
                "install": {
                    "type": "direct",
                    "artifacts": [{
                        "goos": "linux",
                        "goarch": "aarch64",
                        "url": "https://downloads.example/sample-provider-0.1.0.zip",
                        "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    }]
                }
            }]
        }]
    }"#,
    )
    .expect("parse");
    let plugin = registry.plugin_by_id("sample-provider").expect("plugin present");
    assert_eq!(plugin_install_type(plugin), INSTALL_TYPE_DIRECT);
    assert!(plugin.auth_required);
    assert_eq!(plugin.versions.len(), 1);
    assert_eq!(plugin.versions[0].version, "0.1.0");
    assert_eq!(
        plugin_platforms(plugin),
        vec![
            Platform { goos: "windows".into(), goarch: "amd64".into() },
            Platform { goos: "linux".into(), goarch: "arm64".into() },
        ]
    );
    let artifacts = plugin_artifacts(plugin);
    assert_eq!(artifacts.len(), 2);
    assert_eq!((artifacts[0].goos.as_str(), artifacts[0].goarch.as_str()), ("windows", "amd64"));
    assert_eq!((artifacts[1].goos.as_str(), artifacts[1].goarch.as_str()), ("linux", "arm64"));
}

#[test]
fn validate_registry_rejects_invalid_direct_install() {
    let plugin = direct_plugin("https://downloads.example/sample.zip?token=secret");
    let err = validate_registry(&Registry { schema_version: SCHEMA_VERSION_V2, plugins: vec![plugin] })
        .expect_err("rejected");
    assert!(err.to_string().contains("sensitive query"), "{err}");
}

#[test]
fn validate_registry_rejects_direct_install_in_schema_v1() {
    let plugin = direct_plugin("https://downloads.example/sample.zip");
    let err =
        validate_registry(&Registry { schema_version: SCHEMA_VERSION, plugins: vec![plugin] }).expect_err("rejected");
    assert!(err.to_string().contains("schema_version 2"), "{err}");
}

#[test]
fn validate_registry_rejects_invalid_entries() {
    type Mutate = fn(&mut Registry);
    let cases: Vec<(&str, Mutate, &str)> = vec![
        ("schema version", |r| r.schema_version = 3, "unsupported schema_version"),
        ("missing required field", |r| r.plugins[0].name.clear(), "missing required field name"),
        (
            "duplicate id",
            |r| {
                let dup = r.plugins[0].clone();
                r.plugins.push(dup);
            },
            "duplicate plugin id",
        ),
        ("invalid id", |r| r.plugins[0].id = "../sample-provider".into(), "invalid plugin id"),
        ("v-prefixed version", |r| r.plugins[0].version = "v0.1.0".into(), "invalid plugin version"),
        (
            "invalid repository",
            |r| r.plugins[0].repository = "https://example.com/author/repo".into(),
            "repository must be",
        ),
    ];
    for (name, mutate, want) in cases {
        let mut registry = Registry { schema_version: 1, plugins: vec![valid_plugin()] };
        mutate(&mut registry);
        let err = validate_registry(&registry).expect_err(name);
        assert!(err.to_string().contains(want), "{name}: {err}");
    }
}

#[test]
fn normalize_sources_appends_urls_to_default_source() {
    let sources = normalize_sources(&[" https://community.example/registry.json ".to_string()]).expect("sources");
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].id, DEFAULT_SOURCE_ID);
    assert_eq!(sources[0].url, DEFAULT_REGISTRY_URL);
    assert_eq!(sources[1].id, source_id("https://community.example/registry.json"));
    assert_eq!(sources[1].name, "community.example");
    assert_eq!(sources[1].url, "https://community.example/registry.json");
}

#[test]
fn normalize_sources_skips_duplicates() {
    let sources = normalize_sources(&[
        DEFAULT_REGISTRY_URL.to_string(),
        "https://community.example/registry.json".to_string(),
        "https://community.example/registry.json".to_string(),
    ])
    .expect("sources");
    assert_eq!(sources.len(), 2, "{sources:?}");
}

#[test]
fn github_repository_parts_rejects_non_repository_urls() {
    for repository in [
        "http://github.com/owner/repo",
        "https://github.com/owner",
        "https://github.com/owner/repo/issues",
        "https://github.com/owner/repo.git",
        "https://github.com/owner/repo?tab=readme",
    ] {
        assert!(github_repository_parts(repository).is_err(), "{repository}");
    }
}

#[test]
fn plugin_artifacts_includes_version_artifacts() {
    let mut plugin = direct_plugin("https://downloads.example/sample-provider.zip");
    plugin.install.artifacts[0].goos = "windows".into();
    plugin.install.artifacts[0].goarch = "x64".into();
    plugin.versions = vec![Version {
        version: "0.3.0".into(),
        install: InstallPlan {
            kind: INSTALL_TYPE_DIRECT.into(),
            artifacts: vec![Artifact {
                goos: "linux".into(),
                goarch: "aarch64".into(),
                url: "https://downloads.example/sample-provider-0.3.0.zip".into(),
                sha256: SHA.into(),
                size: 0,
            }],
        },
    }];
    let artifacts = plugin_artifacts(&plugin);
    assert_eq!(artifacts.len(), 2);
    assert_eq!(artifacts[0].goarch, "amd64");
    assert_eq!(artifacts[1].goarch, "arm64");
}

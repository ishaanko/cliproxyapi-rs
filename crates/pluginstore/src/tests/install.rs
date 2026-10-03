//! Ports of Go `install_test.go`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

use crate::auth::*;
use crate::error::{Context, Error};
use crate::github::{Client, ReleaseAsset};
use crate::install::{InstallOptions, install_archive, plugin_extension, runtime_goarch, runtime_goos};
use crate::manifest::Manifest;
use crate::registry::*;
use crate::testutil::*;

fn test_plugin() -> Plugin {
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

fn options(root: &Path, goos: &str, goarch: &str) -> InstallOptions {
    InstallOptions {
        plugins_dir: root.to_string_lossy().into_owned(),
        goos: goos.into(),
        goarch: goarch.into(),
        ..Default::default()
    }
}

fn target(root: &Path, goos: &str, goarch: &str, file: &str) -> PathBuf {
    root.join(goos).join(goarch).join(file)
}

fn write_existing(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, content).expect("write existing");
}

fn direct_plugin(version: &str, url: &str, archive: &[u8]) -> Plugin {
    Plugin {
        version: version.into(),
        install: InstallPlan {
            kind: INSTALL_TYPE_DIRECT.into(),
            artifacts: vec![Artifact {
                goos: "linux".into(),
                goarch: "amd64".into(),
                url: url.into(),
                sha256: hex::encode(Sha256::digest(archive)),
                size: 0,
            }],
        },
        repository: String::new(),
        ..test_plugin()
    }
}

#[tokio::test]
async fn install_does_not_report_loaded_lock_for_network_failures() {
    let cases = [("windows", true), ("windows", false), ("linux", true), ("darwin", true)];
    for (goos, loaded) in cases {
        let client = Client { http_client: Some(failing_doer()), ..Default::default() };
        let dir = tempfile::tempdir().expect("tempdir");
        let mut opts = options(dir.path(), goos, "amd64");
        opts.plugin_loaded = Some(Arc::new(move || loaded));
        let err = client.install(&Context::background(), &test_plugin(), &opts).await.expect_err("install fails");
        assert!(!err.is_loaded_plugin_locked(), "{goos}/{loaded}: {err}");
    }
}

#[test]
fn install_archive_blocks_loaded_windows_plugin_before_write() {
    let root = tempfile::tempdir().expect("tempdir");
    write_existing(&target(root.path(), "windows", "amd64", "sample-provider-v0.1.0.dll"), "old");
    let mut opts = options(root.path(), "windows", "amd64");
    opts.plugin_loaded = Some(Arc::new(|| true));
    let err = install_archive(&make_zip(&[("sample-provider.dll", "library-data")]), &test_plugin(), &opts)
        .expect_err("locked");
    assert!(matches!(err, Error::LoadedPluginLocked), "{err}");
}

#[test]
fn install_archive_prepares_loaded_windows_plugin_before_write() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = target(root.path(), "windows", "amd64", "sample-provider-v0.1.0.dll");
    write_existing(&path, "old");
    let loaded = Arc::new(AtomicBool::new(true));
    let prepared = Arc::new(AtomicBool::new(false));
    let mut opts = options(root.path(), "windows", "amd64");
    let loaded_probe = loaded.clone();
    opts.plugin_loaded = Some(Arc::new(move || loaded_probe.load(Ordering::SeqCst)));
    let (loaded_flag, prepared_flag) = (loaded.clone(), prepared.clone());
    opts.before_write = Some(Arc::new(move || {
        prepared_flag.store(true, Ordering::SeqCst);
        loaded_flag.store(false, Ordering::SeqCst);
        Ok(())
    }));
    let result =
        install_archive(&make_zip(&[("sample-provider.dll", "new")]), &test_plugin(), &opts).expect("install");
    assert!(prepared.load(Ordering::SeqCst), "BeforeWrite was not called");
    assert!(result.overwritten);
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "new");
}

#[test]
fn install_archive_skips_identical_loaded_windows_plugin() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = target(root.path(), "windows", "amd64", "sample-provider-v0.1.0.dll");
    write_existing(&path, "same");
    let before_write_called = Arc::new(AtomicBool::new(false));
    let mut opts = options(root.path(), "windows", "amd64");
    opts.plugin_loaded = Some(Arc::new(|| true));
    let called = before_write_called.clone();
    opts.before_write = Some(Arc::new(move || {
        called.store(true, Ordering::SeqCst);
        Err(Error::msg("before write should not run"))
    }));
    let result =
        install_archive(&make_zip(&[("sample-provider.dll", "same")]), &test_plugin(), &opts).expect("install");
    assert!(!before_write_called.load(Ordering::SeqCst), "BeforeWrite was called for identical artifact");
    assert!(result.overwritten && result.skipped);
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "same");
}

#[test]
fn install_archive_writes_platform_plugin() {
    let root = tempfile::tempdir().expect("tempdir");
    let result = install_archive(
        &make_zip(&[("README.md", "ignored"), ("sample-provider.dylib", "library-data")]),
        &test_plugin(),
        &options(root.path(), "darwin", "arm64"),
    )
    .expect("install");
    let want = target(root.path(), "darwin", "arm64", "sample-provider-v0.1.0.dylib");
    assert_eq!(PathBuf::from(&result.path), want);
    assert_eq!(std::fs::read_to_string(&want).expect("read"), "library-data");
}

#[test]
fn install_archive_reports_overwrite() {
    let root = tempfile::tempdir().expect("tempdir");
    write_existing(&target(root.path(), "darwin", "arm64", "sample-provider-v0.1.0.dylib"), "old");
    let result = install_archive(
        &make_zip(&[("sample-provider.dylib", "new")]),
        &test_plugin(),
        &options(root.path(), "darwin", "arm64"),
    )
    .expect("install");
    assert!(result.overwritten);
}

#[test]
fn install_archive_overwrites_runtime_selected_plugin() {
    let root = tempfile::tempdir().expect("tempdir");
    let (goos, goarch) = (runtime_goos(), runtime_goarch());
    let existing = target(
        root.path(),
        &goos,
        &goarch,
        &format!("sample-provider-v0.1.0{}", plugin_extension(&goos)),
    );
    write_existing(&existing, "old");
    let result = install_archive(
        &make_zip(&[(&format!("sample-provider{}", plugin_extension(&goos)), "new")]),
        &test_plugin(),
        &options(root.path(), &goos, &goarch),
    )
    .expect("install");
    assert_eq!(PathBuf::from(&result.path), existing);
    assert!(result.overwritten);
    assert_eq!(std::fs::read_to_string(&existing).expect("read"), "new");
}

#[test]
fn install_archive_rejects_unsafe_archives() {
    let cases: Vec<(&str, Vec<(&str, &str)>, &str)> = vec![
        ("zip slip", vec![("../sample-provider.dylib", "library")], "escapes archive root"),
        ("absolute path", vec![("/sample-provider.dylib", "library")], "is absolute"),
        ("nested target", vec![("nested/sample-provider.dylib", "library")], "zip root"),
        ("extension mismatch", vec![("sample-provider.so", "library")], "sample-provider.dylib"),
        ("filename mismatch", vec![("other.dylib", "library")], "sample-provider.dylib"),
        ("missing target", vec![("README.md", "library")], "does not contain"),
        (
            "multiple targets",
            vec![("sample-provider.dylib", "library"), ("copy.dylib", "library")],
            "sample-provider.dylib",
        ),
    ];
    for (name, files, want) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = install_archive(&make_zip(&files), &test_plugin(), &options(dir.path(), "darwin", "arm64"))
            .expect_err(name);
        assert!(err.to_string().contains(want), "{name}: {err}");
    }
}

fn release_entries(
    tag_url: &str,
    tag_name: &str,
    version: &str,
    goos: &str,
    archive: &[u8],
    with_api_urls: bool,
) -> Vec<(String, Vec<u8>)> {
    let archive_name = format!("sample-provider_{version}_{goos}_{}.zip", if goos == "darwin" { "arm64" } else { "amd64" });
    let api = |n: u32| {
        if with_api_urls {
            format!(
                r#""url": "https://api.github.com/repos/author-name/cliproxy-sample-provider-plugin/releases/assets/{n}","#
            )
        } else {
            String::new()
        }
    };
    let release = format!(
        r#"{{"tag_name": "{tag_name}", "assets": [
            {{{} "name": "{archive_name}", "browser_download_url": "https://downloads.example/{archive_name}"}},
            {{{} "name": "checksums.txt", "browser_download_url": "https://downloads.example/checksums.txt"}}
        ]}}"#,
        api(1),
        api(2)
    );
    vec![
        (tag_url.to_string(), release.into_bytes()),
        (format!("https://downloads.example/{archive_name}"), archive.to_vec()),
        (
            "https://downloads.example/checksums.txt".to_string(),
            format!("{}  {archive_name}\n", hex::encode(Sha256::digest(archive))).into_bytes(),
        ),
    ]
}

#[tokio::test]
async fn install_uses_latest_release_version() {
    let root = tempfile::tempdir().expect("tempdir");
    let archive = make_zip(&[("sample-provider.dylib", "library-data")]);
    let client = Client {
        http_client: Some(MapDoer::arc(release_entries(
            "https://api.github.com/repos/author-name/cliproxy-sample-provider-plugin/releases/latest",
            "v0.2.0",
            "0.2.0",
            "darwin",
            &archive,
            true,
        ))),
        ..Default::default()
    };
    let result = client
        .install(&Context::background(), &test_plugin(), &options(root.path(), "darwin", "arm64"))
        .await
        .expect("install");
    assert_eq!(result.version, "0.2.0");
    assert_eq!(result.install_type, INSTALL_TYPE_GITHUB_RELEASE);
    assert_eq!(result.release_tag, "v0.2.0");
    let path = target(root.path(), "darwin", "arm64", "sample-provider-v0.2.0.dylib");
    assert_eq!(std::fs::read_to_string(path).expect("read"), "library-data");
}

const ASSET_API_URL: &str = "https://api.github.com/repos/author-name/cliproxy-sample-provider-plugin/releases/assets/1";

#[tokio::test]
async fn download_asset_falls_back_to_api_url_when_browser_url_empty() {
    let client = Client {
        http_client: Some(MapDoer::arc(vec![(ASSET_API_URL.to_string(), b"artifact-data".to_vec())])),
        ..Default::default()
    };
    let data = client
        .download_asset(
            &Context::background(),
            &ReleaseAsset {
                name: "sample-provider_0.2.0_darwin_arm64.zip".into(),
                api_url: ASSET_API_URL.into(),
                ..Default::default()
            },
        )
        .await
        .expect("download");
    assert_eq!(data, b"artifact-data");
}

fn asset_with_both_urls() -> ReleaseAsset {
    ReleaseAsset {
        name: "sample-provider_0.2.0_darwin_arm64.zip".into(),
        api_url: ASSET_API_URL.into(),
        browser_download_url: "https://downloads.example/sample-provider.zip".into(),
    }
}

#[tokio::test]
async fn download_asset_uses_api_url_when_auth_matches_artifact() {
    let client = Client {
        http_client: Some(auth_checking_doer(ASSET_API_URL, "Bearer secret-token", b"artifact-data".to_vec())),
        env: Some(env_fn(&[("PLUGIN_STORE_TOKEN", "secret-token")])),
        auth: vec![AuthConfig {
            match_url: "https://api.github.com/repos/author-name/cliproxy-sample-provider-plugin/releases/".into(),
            apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
            kind: AUTH_TYPE_BEARER.into(),
            token_env: "PLUGIN_STORE_TOKEN".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let data = client.download_asset(&Context::background(), &asset_with_both_urls()).await.expect("download");
    assert_eq!(data, b"artifact-data");
}

#[tokio::test]
async fn download_asset_uses_api_url_when_resolved_auth_matches_artifact() {
    let client = Client {
        http_client: Some(auth_checking_doer(ASSET_API_URL, "Bearer temporary-token", b"artifact-data".to_vec())),
        resolved_auth: vec![ResolvedAuthConfig {
            match_url: "https://api.github.com/repos/author-name/cliproxy-sample-provider-plugin/releases/".into(),
            apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
            kind: AUTH_TYPE_GITHUB_TOKEN.into(),
            token: Secret::from("temporary-token"),
            ..Default::default()
        }],
        ..Default::default()
    };
    let data = client.download_asset(&Context::background(), &asset_with_both_urls()).await.expect("download");
    assert_eq!(data, b"artifact-data");
}

#[tokio::test]
async fn download_asset_uses_browser_url_with_unrelated_auth() {
    let browser_url = "https://downloads.example/sample-provider.zip";
    let client = Client {
        http_client: Some(MapDoer::arc(vec![(browser_url.to_string(), b"artifact-data".to_vec())])),
        env: Some(env_fn(&[("PLUGIN_STORE_TOKEN", "secret-token")])),
        auth: vec![AuthConfig {
            match_url: "https://registry.example/".into(),
            apply_to: vec![REQUEST_KIND_REGISTRY.into()],
            kind: AUTH_TYPE_BEARER.into(),
            token_env: "PLUGIN_STORE_TOKEN".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let data = client.download_asset(&Context::background(), &asset_with_both_urls()).await.expect("download");
    assert_eq!(data, b"artifact-data");
}

#[tokio::test]
async fn install_version_uses_pinned_release_tag() {
    let root = tempfile::tempdir().expect("tempdir");
    let archive = make_zip(&[("sample-provider.so", "library-data")]);
    let client = Client {
        http_client: Some(MapDoer::arc(release_entries(
            "https://api.github.com/repos/author-name/cliproxy-sample-provider-plugin/releases/tags/v0.3.0",
            "v0.3.0",
            "0.3.0",
            "linux",
            &archive,
            false,
        ))),
        ..Default::default()
    };
    let result = client
        .install_version(&Context::background(), &test_plugin(), "v0.3.0", "0.3.0", &options(root.path(), "linux", "amd64"))
        .await
        .expect("install version");
    assert_eq!(result.version, "0.3.0");
    let path = target(root.path(), "linux", "amd64", "sample-provider-v0.3.0.so");
    assert_eq!(std::fs::read_to_string(path).expect("read"), "library-data");
}

#[tokio::test]
async fn install_manifest_resolves_direct_artifacts_from_source() {
    let root = tempfile::tempdir().expect("tempdir");
    let archive = make_zip(&[("sample-provider.so", "library-data")]);
    let checksum = hex::encode(Sha256::digest(&archive));
    let registry_url = "https://registry.example/registry.json";
    let artifact_url = "https://downloads.example/sample-provider_0.4.0_linux_amd64.zip";
    let latest_url = "https://downloads.example/sample-provider_0.5.0_linux_amd64.zip";
    let registry = format!(
        r#"{{
        "schema_version": 2,
        "plugins": [{{
            "id": "sample-provider", "name": "Sample Provider",
            "description": "Adds sample provider support.", "author": "author-name", "version": "0.5.0",
            "install": {{"type": "direct", "artifacts": [{{"goos": "linux", "goarch": "amd64", "url": "{latest_url}", "sha256": "{checksum}"}}]}},
            "versions": [{{"version": "0.4.0", "install": {{"type": "direct", "artifacts": [{{"goos": "linux", "goarch": "amd64", "url": "{artifact_url}", "sha256": "{checksum}"}}]}}}}]
        }}]
    }}"#
    );
    let client = Client {
        http_client: Some(MapDoer::arc(vec![
            (registry_url.to_string(), registry.into_bytes()),
            (artifact_url.to_string(), archive),
        ])),
        ..Default::default()
    };
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION_V2,
        id: "sample-provider".into(),
        version: "0.4.0".into(),
        source_url: registry_url.into(),
        install: InstallPlan { kind: INSTALL_TYPE_DIRECT.into(), artifacts: Vec::new() },
        ..Default::default()
    };
    let result = client
        .install_manifest(&Context::background(), &manifest, &options(root.path(), "linux", "amd64"))
        .await
        .expect("install manifest");
    assert_eq!((result.install_type.as_str(), result.version.as_str()), (INSTALL_TYPE_DIRECT, "0.4.0"));
    let path = target(root.path(), "linux", "amd64", "sample-provider-v0.4.0.so");
    assert_eq!(std::fs::read_to_string(path).expect("read"), "library-data");
}

#[tokio::test]
async fn install_direct_downloads_matching_artifact_with_bearer_auth() {
    let root = tempfile::tempdir().expect("tempdir");
    let archive = make_zip(&[("sample-provider.so", "library-data")]);
    let url = "https://downloads.example/private/sample-provider_0.4.0_linux_amd64.zip";
    let client = Client {
        http_client: Some(auth_checking_doer(url, "Bearer secret-token", archive.clone())),
        env: Some(env_fn(&[("PLUGIN_STORE_TOKEN", "secret-token")])),
        auth: vec![AuthConfig {
            match_url: "https://downloads.example/private/".into(),
            apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
            kind: AUTH_TYPE_BEARER.into(),
            token_env: "PLUGIN_STORE_TOKEN".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let plugin = direct_plugin("0.4.0", url, &archive);
    let result = client
        .install(&Context::background(), &plugin, &options(root.path(), "linux", "amd64"))
        .await
        .expect("install");
    assert_eq!((result.install_type.as_str(), result.version.as_str()), (INSTALL_TYPE_DIRECT, "0.4.0"));
    let path = target(root.path(), "linux", "amd64", "sample-provider-v0.4.0.so");
    assert_eq!(std::fs::read_to_string(path).expect("read"), "library-data");
}

#[tokio::test]
async fn install_direct_rejects_checksum_mismatch() {
    let archive = make_zip(&[("sample-provider.so", "library-data")]);
    let url = "https://downloads.example/sample-provider.zip";
    let client = Client {
        http_client: Some(MapDoer::arc(vec![(url.to_string(), archive.clone())])),
        ..Default::default()
    };
    let mut plugin = direct_plugin("0.4.0", url, &archive);
    plugin.install.artifacts[0].sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into();
    let dir = tempfile::tempdir().expect("tempdir");
    let err = client
        .install(&Context::background(), &plugin, &options(dir.path(), "linux", "amd64"))
        .await
        .expect_err("mismatch");
    assert!(err.to_string().contains("checksum mismatch"), "{err}");
}

#[tokio::test]
async fn install_rejects_invalid_latest_release_tag() {
    let client = Client {
        http_client: Some(MapDoer::arc(vec![(
            "https://api.github.com/repos/author-name/cliproxy-sample-provider-plugin/releases/latest".to_string(),
            br#"{"tag_name": "latest", "assets": []}"#.to_vec(),
        )])),
        ..Default::default()
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let err = client
        .install(&Context::background(), &test_plugin(), &options(dir.path(), "darwin", "arm64"))
        .await
        .expect_err("invalid tag");
    assert!(err.to_string().contains("invalid release tag"), "{err}");
}

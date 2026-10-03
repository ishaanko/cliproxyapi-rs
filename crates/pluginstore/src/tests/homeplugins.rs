//! Ports of Go `internal/homeplugins/sync_test.go` and `network_scope_test.go`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use cpa_config::{Config, PluginInstanceConfig};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::auth::*;
use crate::error::{Context, Error};
use crate::github::Client;
use crate::home_sync::PluginSyncItem;
use crate::homeplugins::*;
use crate::http::{HttpDoer, HttpResponse};
use crate::install::{plugin_extension, runtime_goarch, runtime_goos};
use crate::manifest::Manifest;
use crate::registry::*;
use crate::testutil::*;

/// Runtime fake: tracks busy state and unload calls; the contextual variant is opt-in.
#[derive(Default)]
struct FakeRuntime {
    busy: Mutex<bool>,
    unloaded: Mutex<Vec<String>>,
    contextual: bool,
    unload_ctx: Mutex<Option<Context>>,
}

impl FakeRuntime {
    fn busy() -> Arc<Self> {
        Arc::new(Self { busy: Mutex::new(true), ..Default::default() })
    }

    fn contextual() -> Arc<Self> {
        Arc::new(Self { busy: Mutex::new(true), contextual: true, ..Default::default() })
    }
}

impl PluginRuntime for FakeRuntime {
    fn plugin_busy(&self, _id: &str) -> bool {
        *self.busy.lock()
    }

    fn unload_plugin(&self, id: &str) -> bool {
        self.unloaded.lock().push(id.to_string());
        *self.busy.lock() = false;
        true
    }

    fn unload_plugin_context(&self, ctx: &Context, id: &str) -> Option<bool> {
        if !self.contextual {
            return None;
        }
        *self.unload_ctx.lock() = Some(ctx.clone());
        Some(self.unload_plugin(id))
    }
}

struct Inspector(HashMap<&'static str, bool>);

impl PluginLoadInspector for Inspector {
    fn plugin_registered(&self, id: &str) -> bool {
        self.0.get(id).copied().unwrap_or(false)
    }
}

/// Factory returning clients bound to a fixed in-memory doer.
struct TestFactory(Arc<dyn HttpDoer>);

impl ClientFactory for TestFactory {
    fn plugin_store_client(&self, _cfg: &Config) -> Client {
        Client { http_client: Some(self.0.clone()), ..Default::default() }
    }

    fn resolved_plugin_store_client(
        &self,
        _cfg: &Config,
        auth: Vec<ResolvedAuthConfig>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Client {
        Client {
            http_client: Some(self.0.clone()),
            resolved_auth: auth,
            resolved_auth_expires_at: expires_at,
            ..Default::default()
        }
    }
}

const SAMPLE_STORE_YAML: &str = "
enabled: true
store:
  id: sample
  name: Sample
  description: Adds sample support.
  author: owner
  version: 0.2.0
  release-tag: v0.2.0
  repository: https://github.com/owner/sample-plugin
";

fn plugin_config(text: &str) -> PluginInstanceConfig {
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).expect("yaml");
    PluginInstanceConfig::from_yaml(value).expect("plugin config")
}

fn config_with(root: &Path, configs: Vec<(&str, PluginInstanceConfig)>) -> Config {
    let mut cfg = Config::default();
    cfg.home.enabled = true;
    cfg.plugins.enabled = true;
    cfg.plugins.dir = root.to_string_lossy().into_owned();
    for (id, item) in configs {
        cfg.plugins.configs.insert(id.to_string(), item);
    }
    cfg
}

fn sync_test_config(root: &Path) -> Config {
    config_with(root, vec![("sample", plugin_config(SAMPLE_STORE_YAML))])
}

fn plugin_test_path(root: &Path, goos: &str, goarch: &str, id: &str, version: &str) -> PathBuf {
    let mut name = id.trim().to_string();
    if !version.trim().is_empty() {
        name.push_str(&format!("-v{}", version.trim()));
    }
    root.join(goos).join(goarch).join(format!("{name}{}", plugin_extension(goos)))
}

fn windows() -> Platform {
    Platform { goos: "windows".into(), goarch: "amd64".into() }
}

/// Release metadata plus archive and checksum for `sample` 0.2.0 on windows/amd64.
fn sample_release_doer(archive: &[u8]) -> Arc<dyn HttpDoer> {
    let archive_name = "sample_0.2.0_windows_amd64.zip";
    let release = format!(
        r#"{{"tag_name": "v0.2.0", "assets": [
            {{"name": "{archive_name}", "browser_download_url": "https://downloads.example/{archive_name}"}},
            {{"name": "checksums.txt", "browser_download_url": "https://downloads.example/checksums.txt"}}
        ]}}"#
    );
    MapDoer::arc(vec![
        ("https://api.github.com/repos/owner/sample-plugin/releases/tags/v0.2.0".to_string(), release.into_bytes()),
        (format!("https://downloads.example/{archive_name}"), archive.to_vec()),
        (
            "https://downloads.example/checksums.txt".to_string(),
            format!("{}  {archive_name}\n", hex::encode(Sha256::digest(archive))).into_bytes(),
        ),
    ])
}

fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, content).expect("write");
}

#[tokio::test]
async fn sync_platform_installs_manifest_artifact() {
    let root = tempfile::tempdir().expect("tempdir");
    let archive = make_zip(&[("sample.dll", "library-data")]);
    let factory = TestFactory(sample_release_doer(&archive));
    let (_, err) = sync_platform_with_report_using(
        &factory,
        &Context::background(),
        &sync_test_config(root.path()),
        None,
        &windows(),
    )
    .await;
    assert!(err.is_none(), "{err:?}");
    let target = plugin_test_path(root.path(), "windows", "amd64", "sample", "0.2.0");
    assert_eq!(std::fs::read_to_string(target).expect("read target"), "library-data");
}

#[tokio::test]
async fn sync_resolved_with_report_uses_temporary_auth_and_clears_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let library_name = format!("sample{}", plugin_extension(&runtime_goos()));
    let archive = make_zip(&[(&library_name, "library-data")]);
    let authenticated = Arc::new(Mutex::new(false));
    let seen = authenticated.clone();
    let body = archive.clone();
    let factory = TestFactory(fn_doer(move |request| {
        if request.headers.get("Authorization") != "Bearer temporary-token" {
            return Ok(HttpResponse::from_bytes(401, crate::http::Headers::new(), "unauthorized"));
        }
        *seen.lock() = true;
        Ok(HttpResponse::from_bytes(200, crate::http::Headers::new(), body.clone()))
    }));
    let mut items = vec![PluginSyncItem {
        manifest: Manifest {
            schema_version: SCHEMA_VERSION_V2,
            id: "sample".into(),
            version: "1.0.0".into(),
            install: InstallPlan {
                kind: INSTALL_TYPE_DIRECT.into(),
                artifacts: vec![Artifact {
                    goos: runtime_goos(),
                    goarch: runtime_goarch(),
                    url: "https://downloads.example/private/sample.zip".into(),
                    sha256: hex::encode(Sha256::digest(&archive)),
                    size: archive.len() as i64,
                }],
            },
            ..Default::default()
        },
        auth: vec![ResolvedAuthConfig {
            match_url: "https://downloads.example/private/".into(),
            apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
            kind: AUTH_TYPE_BEARER.into(),
            token: Secret::from("temporary-token"),
            ..Default::default()
        }],
    }];
    let cfg = config_with(root.path(), vec![("sample", plugin_config("enabled: true"))]);
    let installed: HashMap<String, String> = HashMap::from([("sample".to_string(), "0.9.0".to_string())]);

    let (report, err) = sync_resolved_with_report_using(
        &factory,
        &Context::background(),
        &cfg,
        &mut items,
        Some(Utc::now() + Duration::minutes(1)),
        &installed,
        None,
    )
    .await;
    assert!(err.is_none(), "{err:?}");
    assert!(*authenticated.lock() && report.ok, "report={report:?}");
    assert_eq!(report.plugins.len(), 1);
    assert_eq!(report.plugins[0].version, "1.0.0");
    assert!(items[0].auth.is_empty(), "sync item retained auth references");
    let target = plugin_test_path(root.path(), &runtime_goos(), &runtime_goarch(), "sample", "1.0.0");
    assert_eq!(std::fs::read_to_string(target).expect("installed plugin"), "library-data");
}

fn unchanged_config(root: &Path, version: &str, repository: &str) -> Config {
    config_with(
        root,
        vec![(
            "sample",
            plugin_config(&format!(
                "
enabled: true
store:
  id: sample
  name: Sample
  description: Adds sample support.
  author: owner
  version: {version}
  release-tag: v{version}
  repository: {repository}
"
            )),
        )],
    )
}

#[tokio::test]
async fn sync_resolved_with_report_includes_unchanged_installed_plugins() {
    let root = tempfile::tempdir().expect("tempdir");
    let target = plugin_test_path(root.path(), &runtime_goos(), &runtime_goarch(), "sample", "1.0.0");
    write_file(&target, "plugin");
    let cfg = unchanged_config(root.path(), "1.0.0", "https://github.com/owner/sample-plugin");
    let installed = HashMap::from([("sample".to_string(), "1.0.0".to_string())]);

    let (mut report, err) = sync_resolved_with_report_using(
        &TestFactory(MapDoer::arc(Vec::new())),
        &Context::background(),
        &cfg,
        &mut [],
        Some(Utc::now() + Duration::minutes(1)),
        &installed,
        None,
    )
    .await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(report.plugins.len(), 1);
    let status = report.plugins[0].clone();
    assert_eq!((status.id.as_str(), status.install_status.as_str()), ("sample", PLUGIN_INSTALL_STATUS_SKIPPED));
    assert_eq!(PathBuf::from(&status.path), target);
    assert_eq!(status.release_tag, "v1.0.0");
    assert_eq!(status.repository, "https://github.com/owner/sample-plugin");
    assert_eq!(status.install_type, INSTALL_TYPE_GITHUB_RELEASE);
    assert!(mark_load_results(&mut report, Some(&Inspector(HashMap::new()))).is_some());
    assert_eq!(report.plugins[0].load_status, PLUGIN_LOAD_STATUS_FAILED);
}

#[tokio::test]
async fn sync_resolved_with_report_does_not_mix_installed_and_configured_metadata() {
    let root = tempfile::tempdir().expect("tempdir");
    let target = plugin_test_path(root.path(), &runtime_goos(), &runtime_goarch(), "sample", "1.0.0");
    write_file(&target, "plugin");
    let cfg = unchanged_config(root.path(), "2.0.0", "https://github.com/owner/sample-plugin-v2");
    let installed = HashMap::from([("sample".to_string(), "1.0.0".to_string())]);

    let (report, err) = sync_resolved_with_report_using(
        &TestFactory(MapDoer::arc(Vec::new())),
        &Context::background(),
        &cfg,
        &mut [],
        Some(Utc::now() + Duration::minutes(1)),
        &installed,
        None,
    )
    .await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(report.plugins.len(), 1);
    let status = &report.plugins[0];
    assert_eq!(status.version, "1.0.0");
    assert_eq!(PathBuf::from(&status.path), target);
    assert!(status.release_tag.is_empty() && status.repository.is_empty() && status.install_type.is_empty());
}

#[test]
fn installed_versions_uses_plugin_files_on_disk() {
    let root = tempfile::tempdir().expect("tempdir");
    write_file(&plugin_test_path(root.path(), &runtime_goos(), &runtime_goarch(), "sample", "2.3.4"), "plugin");
    let cfg = config_with(root.path(), vec![("sample", PluginInstanceConfig::default())]);
    let versions = installed_versions(&cfg).expect("versions");
    assert_eq!(versions.get("sample").map(String::as_str), Some("2.3.4"));
}

#[tokio::test]
async fn sync_platform_with_report_records_successful_install() {
    let root = tempfile::tempdir().expect("tempdir");
    let archive = make_zip(&[("sample.dll", "library-data")]);
    let factory = TestFactory(sample_release_doer(&archive));
    let (report, err) = sync_platform_with_report_using(
        &factory,
        &Context::background(),
        &sync_test_config(root.path()),
        None,
        &windows(),
    )
    .await;
    assert!(err.is_none(), "{err:?}");
    assert!(report.ok && report.status == PLUGIN_TASK_STATUS_OK && report.phase == PLUGIN_TASK_PHASE_INSTALL);
    assert_eq!(report.plugins.len(), 1);
    let plugin = &report.plugins[0];
    assert_eq!(plugin.id, "sample");
    assert_eq!(plugin.install_status, PLUGIN_INSTALL_STATUS_INSTALLED);
    assert_eq!(plugin.version, "0.2.0");
    assert_eq!(PathBuf::from(&plugin.path), plugin_test_path(root.path(), "windows", "amd64", "sample", "0.2.0"));
}

#[tokio::test]
async fn sync_platform_with_report_records_skipped_identical_artifact() {
    let root = tempfile::tempdir().expect("tempdir");
    let target = root.path().join("windows").join("amd64").join("sample-v0.2.0.dll");
    write_file(&target, "library-data");
    let archive = make_zip(&[("sample.dll", "library-data")]);
    let factory = TestFactory(sample_release_doer(&archive));
    let (report, err) = sync_platform_with_report_using(
        &factory,
        &Context::background(),
        &sync_test_config(root.path()),
        None,
        &windows(),
    )
    .await;
    assert!(err.is_none(), "{err:?}");
    assert!(report.ok && report.plugins.len() == 1, "{report:?}");
    let plugin = &report.plugins[0];
    assert_eq!(plugin.install_status, PLUGIN_INSTALL_STATUS_SKIPPED);
    assert!(plugin.skipped);
    assert_eq!(PathBuf::from(&plugin.path), target);
}

#[tokio::test]
async fn sync_platform_skips_identical_busy_plugin() {
    let root = tempfile::tempdir().expect("tempdir");
    let target = root.path().join("windows").join("amd64").join("sample-v0.2.0.dll");
    write_file(&target, "library-data");
    let archive = make_zip(&[("sample.dll", "library-data")]);
    let factory = TestFactory(sample_release_doer(&archive));
    let runtime = FakeRuntime::busy();
    let (_, err) = sync_platform_with_report_using(
        &factory,
        &Context::background(),
        &sync_test_config(root.path()),
        Some(runtime.clone()),
        &windows(),
    )
    .await;
    assert!(err.is_none(), "{err:?}");
    assert!(runtime.unloaded.lock().is_empty(), "UnloadPlugin calls = {:?}", runtime.unloaded.lock());
    assert_eq!(std::fs::read_to_string(target).expect("read"), "library-data");
}

#[tokio::test]
async fn sync_platform_skips_config_without_manifest() {
    let root = tempfile::tempdir().expect("tempdir");
    let cfg = config_with(root.path(), vec![("sample", plugin_config("enabled: true"))]);
    let linux = Platform { goos: "linux".into(), goarch: "amd64".into() };
    let (_, err) = sync_platform_with_report_using(
        &TestFactory(MapDoer::arc(Vec::new())),
        &Context::background(),
        &cfg,
        None,
        &linux,
    )
    .await;
    assert!(err.is_none(), "{err:?}");
}

#[tokio::test]
async fn sync_platform_with_report_records_invalid_manifest() {
    let root = tempfile::tempdir().expect("tempdir");
    let cfg = config_with(root.path(), vec![("sample", plugin_config("\nenabled: true\nstore:\n  id: sample\n"))]);
    let linux = Platform { goos: "linux".into(), goarch: "amd64".into() };
    let (report, err) = sync_platform_with_report_using(
        &TestFactory(MapDoer::arc(Vec::new())),
        &Context::background(),
        &cfg,
        None,
        &linux,
    )
    .await;
    assert!(err.is_some(), "want invalid manifest");
    assert!(!report.ok && report.status == PLUGIN_TASK_STATUS_ERROR && report.plugins.len() == 1);
    let plugin = &report.plugins[0];
    assert_eq!(plugin.id, "sample");
    assert_eq!(plugin.install_status, PLUGIN_INSTALL_STATUS_FAILED);
    assert!(plugin.error.contains("invalid store manifest"), "{}", plugin.error);
}

fn status(id: &str, install_status: &str, error: &str) -> PluginInstallStatus {
    PluginInstallStatus {
        id: id.into(),
        install_status: install_status.into(),
        error: error.into(),
        ..Default::default()
    }
}

#[test]
fn mark_load_results_fails_when_installed_plugin_did_not_load() {
    let mut report = completed_sync_report(&Platform::default(), None);
    report.plugins = vec![status("sample", PLUGIN_INSTALL_STATUS_INSTALLED, "")];
    let err = mark_load_results(&mut report, Some(&Inspector(HashMap::new()))).expect("load failure");
    assert!(err.to_string().contains("installed but not loaded"));
    assert!(!report.ok && report.status == PLUGIN_TASK_STATUS_ERROR && report.phase == PLUGIN_TASK_PHASE_LOAD);
    assert_eq!(report.plugins[0].load_status, PLUGIN_LOAD_STATUS_FAILED);
    assert!(report.plugins[0].error.contains("installed but not loaded"));
}

#[test]
fn mark_load_results_preserves_install_failure() {
    let mut report = completed_sync_report(&Platform::default(), Some(&Error::msg("boom")));
    report.status = PLUGIN_TASK_STATUS_ERROR.into();
    report.ok = false;
    report.error = String::new();
    report.plugins = vec![status("sample", PLUGIN_INSTALL_STATUS_FAILED, "install boom")];
    let err = mark_load_results(&mut report, Some(&Inspector(HashMap::from([("sample", true)]))));
    assert!(err.is_some(), "install failure must remain fatal");
    assert!(!report.ok && report.status == PLUGIN_TASK_STATUS_ERROR);
    assert_eq!(report.plugins[0].load_status, PLUGIN_INSTALL_STATUS_SKIPPED);
}

#[test]
fn mark_load_results_preserves_global_sync_failure() {
    let mut report = completed_sync_report(
        &Platform { goos: "linux".into(), goarch: "amd64".into() },
        Some(&Error::msg("home plugins: plugin sync response expired")),
    );
    report.plugins.push(status("installed", PLUGIN_INSTALL_STATUS_INSTALLED, ""));
    let err = mark_load_results(&mut report, Some(&Inspector(HashMap::from([("installed", true)]))))
        .expect("preserved sync expiry");
    assert!(err.to_string().contains("plugin sync response expired"));
    assert!(!report.ok && report.status == PLUGIN_TASK_STATUS_ERROR && report.phase == PLUGIN_TASK_PHASE_LOAD);
    assert!(report.error.contains("plugin sync response expired"));
    assert_eq!(report.plugins[0].load_status, PLUGIN_LOAD_STATUS_LOADED);
}

#[test]
fn completed_sync_report_cases() {
    let linux = Platform { goos: "linux".into(), goarch: "amd64".into() };
    let ok = completed_sync_report(&linux, None);
    assert!(ok.ok && ok.task == PLUGIN_TASK_NAME && ok.finished_at.is_some());
    let failure = Error::msg("home plugins: inspect installed plugins: access denied");
    let report = completed_sync_report(&linux, Some(&failure));
    assert!(!report.ok && report.finished_at.is_some());
    assert_eq!(report.status, PLUGIN_TASK_STATUS_ERROR);
    assert_eq!(report.error, failure.to_string());
}

fn current_plugin_dir(root: &Path) -> PathBuf {
    root.join(runtime_goos()).join(runtime_goarch())
}

#[test]
fn delete_with_report_removes_current_platform_plugin() {
    let root = tempfile::tempdir().expect("tempdir");
    let target = current_plugin_dir(root.path()).join(format!("sample{}", plugin_extension(&runtime_goos())));
    write_file(&target, "library-data");
    let runtime = FakeRuntime::busy();
    let report = delete_with_report(
        &Context::background(),
        &sync_test_config(root.path()),
        Some(runtime.as_ref()),
        42,
        "sample",
    );
    assert!(report.ok && report.task_id == 42 && report.task == PLUGIN_DELETE_TASK_NAME);
    assert_eq!(report.phase, PLUGIN_TASK_PHASE_DELETE);
    assert_eq!(*runtime.unloaded.lock(), vec!["sample".to_string()]);
    assert_eq!(report.plugins.len(), 1);
    assert_eq!(report.plugins[0].install_status, PLUGIN_INSTALL_STATUS_DELETED);
    assert_eq!(PathBuf::from(&report.plugins[0].path), target);
    assert!(!target.exists());
}

#[test]
fn delete_with_report_removes_all_current_platform_plugin_versions() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = current_plugin_dir(root.path());
    let extension = plugin_extension(&runtime_goos());
    let older = dir.join(format!("sample-v0.2.0{extension}"));
    let newer = dir.join(format!("sample-v0.3.0{extension}"));
    let other = dir.join(format!("other-v0.3.0{extension}"));
    for path in [&older, &newer, &other] {
        write_file(path, "library-data");
    }
    let runtime = FakeRuntime::busy();
    let report = delete_with_report(
        &Context::background(),
        &sync_test_config(root.path()),
        Some(runtime.as_ref()),
        43,
        "sample",
    );
    assert!(report.ok, "{report:?}");
    assert_eq!(*runtime.unloaded.lock(), vec!["sample".to_string()]);
    assert_eq!(report.plugins[0].install_status, PLUGIN_INSTALL_STATUS_DELETED);
    assert_eq!(PathBuf::from(&report.plugins[0].path), newer, "representative target");
    assert!(!older.exists() && !newer.exists());
    assert!(other.exists(), "other plugin must be retained");
}

#[test]
fn delete_with_report_stops_before_unload_when_context_canceled() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = plugin_test_path(root.path(), &runtime_goos(), &runtime_goarch(), "sample", "1.0.0");
    write_file(&path, "plugin");
    let runtime = FakeRuntime::contextual();
    let ctx = Context::background();
    ctx.cancel();
    let report = delete_with_report(&ctx, &sync_test_config(root.path()), Some(runtime.as_ref()), 44, "sample");
    assert!(!report.ok && report.error.contains("context canceled"), "{report:?}");
    assert!(runtime.unload_ctx.lock().is_none() && runtime.unloaded.lock().is_empty());
    assert!(path.exists(), "canceled delete removed plugin artifact");
}

#[test]
fn delete_with_report_uses_contextual_unload() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = plugin_test_path(root.path(), &runtime_goos(), &runtime_goarch(), "sample", "1.0.0");
    write_file(&path, "plugin");
    let runtime = FakeRuntime::contextual();
    let ctx = Context::background();
    let report = delete_with_report(&ctx, &sync_test_config(root.path()), Some(runtime.as_ref()), 45, "sample");
    assert!(report.ok, "{report:?}");
    let seen = runtime.unload_ctx.lock().clone().expect("contextual unload used");
    assert!(seen.same_as(&ctx));
    assert_eq!(*runtime.unloaded.lock(), vec!["sample".to_string()]);
}

#[test]
fn delete_with_report_missing_plugin_is_success() {
    let root = tempfile::tempdir().expect("tempdir");
    let report = delete_with_report(&Context::background(), &sync_test_config(root.path()), None, 7, "missing");
    assert!(report.ok && report.status == PLUGIN_TASK_STATUS_OK);
    assert_eq!(report.plugins.len(), 1);
    assert_eq!(report.plugins[0].install_status, PLUGIN_INSTALL_STATUS_MISSING);
}

#[tokio::test]
async fn plugin_store_clients_share_proxy_cooldown() {
    // A proxy that must never be contacted: the seeded cooldown has to short-circuit.
    let proxy_calls = Arc::new(Mutex::new(0u32));
    let counter = proxy_calls.clone();
    let proxy = spawn_server(move |_, _| {
        *counter.lock() += 1;
        (502, vec![], Vec::new())
    })
    .await;
    let auth = vec![ResolvedAuthConfig {
        match_url: "https://api.github.com/".into(),
        kind: AUTH_TYPE_GITHUB_TOKEN.into(),
        token: Secret::from("home_plugin_network_scope_test"),
        ..Default::default()
    }];
    let expires_at = Some(Utc::now() + Duration::hours(1));
    let plugin = Plugin { repository: "https://github.com/test/home-network-scope".into(), ..Default::default() };
    // Seed the process-wide cooldown used by management and Home clients.
    let seed = crate::sdk::new_client_with_resolved_auth_expiry(
        Some(fn_doer(|_| {
            Ok(HttpResponse::from_bytes(429, headers(&[("Retry-After", "3600")]), "limited"))
        })),
        "",
        auth.clone(),
        expires_at,
    )
    .with_network_scope(&proxy);
    let err = seed.fetch_latest_release(&Context::background(), &plugin).await.expect_err("seed cooldown");
    assert!(err.rate_limit().is_some(), "{err}");

    let mut cfg = Config::default();
    cfg.proxy_url = format!(" {proxy} ");
    let client = DefaultClientFactory.resolved_plugin_store_client(&cfg, auth.clone(), expires_at);
    let err = client
        .fetch_latest_release(&Context::background(), &plugin)
        .await
        .expect_err("Home client must inherit the proxy cooldown");
    assert!(err.rate_limit().is_some(), "{err}");
    assert_eq!(*proxy_calls.lock(), 0, "cooldown made proxy requests");

    let key = crate::ratelimit::github_rate_limit_key(
        "https://api.github.com/",
        &proxy,
        &{
            let mut h = crate::http::Headers::new();
            h.set("Authorization", "Bearer home_plugin_network_scope_test");
            h
        },
        true,
    );
    crate::ratelimit::DEFAULT_GITHUB_RATE_LIMITER.remove_entry(&key);
}

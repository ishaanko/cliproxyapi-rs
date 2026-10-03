//! Plugin install: release/direct resolution, archive verification and extraction of the
//! platform library (Go `install.go`).

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::checksum::{parse_checksums, verify_checksum};
use crate::direct::{select_artifact, verify_artifact_checksum};
use crate::error::{Context, Error, Result};
use crate::errf;
use crate::github::{Client, Release, release_version, select_release_assets};
use crate::manifest::Manifest;
use crate::registry::{
    INSTALL_TYPE_DIRECT, INSTALL_TYPE_GITHUB_RELEASE, InstallPlan, Plugin, normalize_goarch, normalize_goos,
    normalize_install_plan, normalize_version, plugin_install_type, valid_plugin_id, valid_plugin_version,
    validate_install_plan, validate_plugin,
};

/// Where and for which platform to install. Closures mirror Go's callbacks.
#[derive(Clone, Default)]
pub struct InstallOptions {
    pub plugins_dir: String,
    pub goos: String,
    pub goarch: String,
    /// Reports whether the plugin's dynamic library is currently loaded by the running
    /// host. Windows installs are rejected only when they would overwrite an existing
    /// target file while it returns true.
    pub plugin_loaded: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Runs after the archive has been downloaded and verified, but before an existing
    /// target plugin file is replaced.
    pub before_write: Option<Arc<dyn Fn() -> Result<()> + Send + Sync>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallResult {
    pub id: String,
    pub version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub release_tag: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub install_type: String,
    pub path: String,
    pub overwritten: bool,
    pub skipped: bool,
}

/// Go `runtime.GOOS` for the host.
pub fn runtime_goos() -> String {
    match std::env::consts::OS {
        "macos" => "darwin".to_string(),
        other => other.to_string(),
    }
}

/// Go `runtime.GOARCH` for the host.
pub fn runtime_goarch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        "x86" => "386".to_string(),
        other => other.to_string(),
    }
}

impl Client {
    /// Installs the plugin: direct plugins from their pinned plan, GitHub plugins from
    /// the latest release (whose tag becomes the installed version).
    pub async fn install(&self, ctx: &Context, plugin: &Plugin, options: &InstallOptions) -> Result<InstallResult> {
        validate_plugin(plugin)?;
        let options = normalize_install_options(options);
        let mut plugin = plugin.clone();
        if plugin_install_type(&plugin) == INSTALL_TYPE_DIRECT {
            plugin.version = normalize_version(&plugin.version);
            let plan = plugin.install.clone();
            return self.install_direct(ctx, &plugin, &plan, &options).await;
        }
        let release = self.fetch_latest_release(ctx, &plugin).await?;
        let latest_version = release_version(&release)?;
        plugin.version = latest_version.clone();
        self.install_release(ctx, &plugin, &release, &latest_version, &options).await
    }

    /// Installs from a pinned manifest (as stored in config or sent by Home).
    pub async fn install_manifest(
        &self,
        ctx: &Context,
        manifest: &Manifest,
        options: &InstallOptions,
    ) -> Result<InstallResult> {
        manifest.validate()?;
        let options = normalize_install_options(options);
        match manifest.install_type().as_str() {
            INSTALL_TYPE_DIRECT => {
                let plugin = self.direct_plugin_from_manifest(ctx, manifest).await?;
                let plan = plugin.install.clone();
                self.install_direct(ctx, &plugin, &plan, &options).await
            }
            INSTALL_TYPE_GITHUB_RELEASE => {
                self.install_version(ctx, &manifest.plugin(), &manifest.release_tag, &manifest.version, &options)
                    .await
            }
            _ => Err(errf!("unsupported install type {:?}", manifest.install.kind)),
        }
    }

    /// Installs a plugin artifact from a fixed release tag/version.
    pub async fn install_version(
        &self,
        ctx: &Context,
        plugin: &Plugin,
        release_tag: &str,
        version: &str,
        options: &InstallOptions,
    ) -> Result<InstallResult> {
        validate_plugin(plugin)?;
        let options = normalize_install_options(options);
        let version = normalize_version(version);
        if !valid_plugin_version(&version) {
            return Err(errf!("invalid plugin version {version:?}"));
        }
        let mut release_tag = release_tag.trim().to_string();
        if release_tag.is_empty() {
            release_tag = version.clone();
        }
        let release = self.fetch_release_by_tag(ctx, plugin, &release_tag).await?;
        let resolved = release_version(&release)?;
        if resolved != version {
            return Err(errf!("release tag {release_tag:?} resolved version {resolved:?}, want {version:?}"));
        }
        let mut plugin = plugin.clone();
        plugin.version = version.clone();
        self.install_release(ctx, &plugin, &release, &version, &options).await
    }

    async fn install_release(
        &self,
        ctx: &Context,
        plugin: &Plugin,
        release: &Release,
        version: &str,
        options: &InstallOptions,
    ) -> Result<InstallResult> {
        let (archive_asset, checksum_asset) =
            select_release_assets(release, &plugin.id, &plugin.version, &options.goos, &options.goarch)?;
        let archive_data = self
            .download_asset(ctx, &archive_asset)
            .await
            .map_err(|err| err.wrap(format!("download {}", archive_asset.name)))?;
        let checksum_data = self
            .download_asset(ctx, &checksum_asset)
            .await
            .map_err(|err| err.wrap("download checksums.txt"))?;
        let checksums = parse_checksums(&checksum_data)?;
        verify_checksum(&archive_asset.name, &archive_data, &checksums)?;
        let mut plugin = plugin.clone();
        plugin.version = version.to_string();
        let mut result = install_archive(&archive_data, &plugin, options)?;
        result.install_type = INSTALL_TYPE_GITHUB_RELEASE.to_string();
        result.release_tag = release.tag_name.trim().to_string();
        Ok(result)
    }

    /// Installs the platform artifact of a direct install plan.
    pub async fn install_direct(
        &self,
        ctx: &Context,
        plugin: &Plugin,
        plan: &InstallPlan,
        options: &InstallOptions,
    ) -> Result<InstallResult> {
        let mut plugin = plugin.clone();
        plugin.id = plugin.id.trim().to_string();
        plugin.version = normalize_version(&plugin.version);
        if !valid_plugin_id(&plugin.id) {
            return Err(errf!("invalid plugin id {:?}", plugin.id));
        }
        if !valid_plugin_version(&plugin.version) {
            return Err(errf!("invalid plugin version {:?}", plugin.version));
        }
        let mut plan = normalize_install_plan(plan);
        plan.kind = INSTALL_TYPE_DIRECT.to_string();
        validate_install_plan(&plan)?;
        let options = normalize_install_options(options);
        let artifact = select_artifact(&plan, &options.goos, &options.goarch)?;
        let archive_data = self
            .download_artifact(ctx, &artifact)
            .await
            .map_err(|err| err.wrap("download artifact"))?;
        verify_artifact_checksum(&artifact, &archive_data)?;
        let mut result = install_archive(&archive_data, &plugin, &options)?;
        result.install_type = INSTALL_TYPE_DIRECT.to_string();
        Ok(result)
    }

    /// Resolves a direct manifest without pinned artifacts against its source registry.
    async fn direct_plugin_from_manifest(&self, ctx: &Context, manifest: &Manifest) -> Result<Plugin> {
        let mut plugin = manifest.plugin();
        plugin.version = normalize_version(&manifest.version);
        plugin.install = normalize_install_plan(&plugin.install);
        plugin.install.kind = INSTALL_TYPE_DIRECT.to_string();
        if !plugin.install.artifacts.is_empty() {
            return Ok(plugin);
        }
        let mut source_url = manifest.source_url.trim().to_string();
        if source_url.is_empty() {
            source_url = self.registry_url.trim().to_string();
        }
        if source_url.is_empty() {
            return Err(errf!("direct install manifest missing source-url"));
        }
        let mut source_client = self.clone();
        source_client.registry_url = source_url;
        let registry = source_client
            .fetch_registry(ctx)
            .await
            .map_err(|err| err.wrap("fetch direct install source"))?;
        let Some(resolved) = registry.plugin_by_id(&manifest.id) else {
            return Err(errf!("direct install plugin {:?} not found in source", manifest.id.trim()));
        };
        if plugin_install_type(resolved) != INSTALL_TYPE_DIRECT {
            return Err(errf!(
                "direct install plugin {:?} resolved as {:?}",
                manifest.id.trim(),
                plugin_install_type(resolved)
            ));
        }
        direct_plugin_version(resolved, &manifest.id, &manifest.version)
    }
}

/// Picks the registry entry (current or historical) for the exact version.
fn direct_plugin_version(plugin: &Plugin, id: &str, version: &str) -> Result<Plugin> {
    let id = id.trim();
    let version = normalize_version(version);
    let mut plugin = plugin.clone();
    if normalize_version(&plugin.version) == version {
        plugin.version = version.clone();
        plugin.install = normalize_install_plan(&plugin.install);
        plugin.install.kind = INSTALL_TYPE_DIRECT.to_string();
        validate_install_plan(&plugin.install)
            .map_err(|err| err.wrap(format!("direct install plugin {id:?} version {version:?}")))?;
        return Ok(plugin);
    }
    let candidates = plugin.versions.clone();
    for candidate in candidates {
        if normalize_version(&candidate.version) != version {
            continue;
        }
        plugin.version = version.clone();
        plugin.install = normalize_install_plan(&candidate.install);
        if plugin.install.kind.is_empty() {
            plugin.install.kind = INSTALL_TYPE_DIRECT.to_string();
        }
        if plugin.install.kind != INSTALL_TYPE_DIRECT {
            return Err(errf!(
                "direct install plugin {id:?} version {version:?} resolved as {:?}",
                plugin.install.kind
            ));
        }
        validate_install_plan(&plugin.install)
            .map_err(|err| err.wrap(format!("direct install plugin {id:?} version {version:?}")))?;
        return Ok(plugin);
    }
    Err(errf!("direct install plugin {id:?} version {version:?} not found in source"))
}

/// Extracts the platform library from the zip and writes it to
/// `<plugins-dir>/<goos>/<goarch>/<id>-v<version><ext>`. Identical existing files are
/// left alone and reported as skipped.
pub fn install_archive(archive_data: &[u8], plugin: &Plugin, options: &InstallOptions) -> Result<InstallResult> {
    let options = normalize_install_options(options);
    let id = plugin.id.trim().to_string();
    if !valid_plugin_id(&id) {
        return Err(errf!("invalid plugin id {:?}", plugin.id));
    }
    let version = normalize_version(&plugin.version);
    if !valid_plugin_version(&version) {
        return Err(errf!("invalid plugin version {:?}", plugin.version));
    }
    let mut archive =
        zip::ZipArchive::new(Cursor::new(archive_data)).map_err(|err| errf!("open zip: {err}"))?;

    let (library_data, mode) = read_target_library(&mut archive, &id, &version, &options.goos)?;

    let target_path = install_target_path(&options, &id, &version)?;
    let target_text = target_path.to_string_lossy().into_owned();
    let mut overwritten = false;
    match std::fs::metadata(&target_path) {
        Ok(_) => overwritten = true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(errf!("stat target plugin: {err}")),
    }
    if overwritten {
        let existing = std::fs::read(&target_path).map_err(|err| errf!("read target plugin: {err}"))?;
        if existing == library_data {
            return Ok(InstallResult {
                id,
                version,
                path: target_text,
                overwritten: true,
                skipped: true,
                ..Default::default()
            });
        }
    }
    // Re-check immediately before replacing an existing file: the same version may
    // have been loaded while the archive was being downloaded and verified.
    if overwritten {
        if let Some(before_write) = &options.before_write {
            before_write().map_err(|err| err.wrap("prepare plugin write"))?;
        }
    }
    if overwritten && loaded_plugin_install_blocked(&options) {
        return Err(Error::LoadedPluginLocked);
    }
    write_file_atomic(&target_path, &library_data, mode)?;
    Ok(InstallResult { id, version, path: target_text, overwritten, ..Default::default() })
}

fn install_target_path(options: &InstallOptions, id: &str, version: &str) -> Result<PathBuf> {
    let version = normalize_version(version);
    if !valid_plugin_version(&version) {
        return Err(errf!("invalid plugin version {version:?}"));
    }
    let joined = Path::new(&options.plugins_dir)
        .join(&options.goos)
        .join(&options.goarch)
        .join(versioned_plugin_file_name(id, &version, &options.goos));
    Ok(PathBuf::from(cpa_config::clean_path(&joined.to_string_lossy())))
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;

/// Finds the single library entry named `<id><ext>` or `<id>-v<version><ext>` at the
/// zip root, validating every entry name, and returns its bytes and permission bits.
fn read_target_library(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    id: &str,
    version: &str,
    goos: &str,
) -> Result<(Vec<u8>, u32)> {
    let target_name = format!("{}{}", id.trim(), plugin_extension(goos));
    let versioned_target_name = versioned_plugin_file_name(id, version, goos);
    let mut target: Option<usize> = None;
    let mut target_mode = 0;
    for index in 0..archive.len() {
        let file = archive.by_index_raw(index).map_err(|err| errf!("open zip: {err}"))?;
        let name = file.name().to_string();
        let cleaned = clean_zip_name(&name)?;
        let unix_mode = file.unix_mode();
        let mode_type = unix_mode.map_or(0, |mode| mode & S_IFMT);
        if name.ends_with('/') || mode_type == S_IFDIR {
            continue;
        }
        if mode_type != 0 && mode_type != S_IFREG {
            return Err(errf!("zip entry {name} is not a regular file"));
        }
        if !has_dynamic_library_extension(&cleaned) {
            continue;
        }
        if cleaned != target_name && cleaned != versioned_target_name {
            let base = cleaned.rsplit('/').next().unwrap_or(&cleaned);
            if base == target_name || base == versioned_target_name {
                return Err(errf!("target dynamic library must be at zip root"));
            }
            return Err(errf!("dynamic library filename must be {target_name} or {versioned_target_name}"));
        }
        if target.is_some() {
            return Err(errf!("zip contains multiple target dynamic libraries"));
        }
        target = Some(index);
        target_mode = unix_mode.map_or(0, |mode| mode & 0o777);
    }
    let Some(index) = target else {
        return Err(errf!("zip does not contain {target_name}"));
    };
    let mut handle = archive.by_index(index).map_err(|err| errf!("open {target_name}: {err}"))?;
    let mut data = Vec::new();
    handle.read_to_end(&mut data).map_err(|err| errf!("read {target_name}: {err}"))?;
    let mode = if target_mode == 0 { 0o755 } else { target_mode };
    Ok((data, mode))
}

fn versioned_plugin_file_name(id: &str, version: &str, goos: &str) -> String {
    format!("{}-v{}{}", id.trim(), normalize_version(version), plugin_extension(goos))
}

/// Validates an archive entry name and returns its cleaned form.
fn clean_zip_name(name: &str) -> Result<String> {
    if name.trim().is_empty() {
        return Err(errf!("zip entry has empty name"));
    }
    if name.contains('\\') {
        return Err(errf!("zip entry {name} uses backslash path separators"));
    }
    if name.starts_with('/') {
        return Err(errf!("zip entry {name} is absolute"));
    }
    let cleaned = cpa_config::clean_path(name);
    if cleaned == "." || cleaned == ".." || cleaned.starts_with("../") {
        return Err(errf!("zip entry {name} escapes archive root"));
    }
    Ok(cleaned)
}

fn has_dynamic_library_extension(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".dylib") || lower.ends_with(".so") || lower.ends_with(".dll")
}

/// Library extension for the target OS (`.dylib`, `.dll`, otherwise `.so`).
pub(crate) fn plugin_extension(goos: &str) -> &'static str {
    match goos.trim().to_lowercase().as_str() {
        "darwin" | "mac" | "macos" | "osx" => ".dylib",
        "windows" => ".dll",
        _ => ".so",
    }
}

/// Writes through a temp file in the target directory, then renames into place.
fn write_file_atomic(target_path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;

    let target_dir = target_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(target_dir).map_err(|err| errf!("create plugin directory: {err}"))?;

    let base = target_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (temp_path, mut temp) = create_temp_file(target_dir, &base)?;
    let mut guard = TempGuard { path: temp_path.clone(), armed: true };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|err| errf!("chmod temp plugin file: {err}"))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    temp.write_all(data).map_err(|err| errf!("write temp plugin file: {err}"))?;
    temp.sync_all().map_err(|err| errf!("sync temp plugin file: {err}"))?;
    drop(temp);
    if let Err(rename_err) = std::fs::rename(&temp_path, target_path) {
        if cfg!(windows) {
            if let Err(err) = std::fs::remove_file(target_path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    return Err(errf!("remove old plugin file: {err}"));
                }
            }
            return match std::fs::rename(&temp_path, target_path) {
                Ok(()) => {
                    guard.armed = false;
                    Ok(())
                }
                Err(err) => Err(errf!("install plugin file: {err}")),
            };
        }
        return Err(errf!("install plugin file: {rename_err}"));
    }
    guard.armed = false;
    Ok(())
}

/// Removes the temp file on early return.
struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Err(err) = std::fs::remove_file(&self.path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(error = %err, "failed to remove temp plugin file");
                }
            }
        }
    }
}

/// Creates `.<name>.tmp-<random>` exclusively in `dir`.
fn create_temp_file(dir: &Path, base: &str) -> Result<(PathBuf, std::fs::File)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let suffix = format!("{:x}{:x}{:x}", std::process::id(), nanos, COUNTER.fetch_add(1, Ordering::Relaxed));
        let path = dir.join(format!(".{base}.tmp-{suffix}"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(errf!("create temp plugin file: {err}")),
        }
    }
    Err(errf!("create temp plugin file: too many attempts"))
}

fn loaded_plugin_install_blocked(options: &InstallOptions) -> bool {
    options.goos.eq_ignore_ascii_case("windows")
        && options.plugin_loaded.as_ref().is_some_and(|loaded| loaded())
}

fn normalize_install_options(options: &InstallOptions) -> InstallOptions {
    let mut options = options.clone();
    options.plugins_dir = options.plugins_dir.trim().to_string();
    if options.plugins_dir.is_empty() {
        options.plugins_dir = "plugins".to_string();
    }
    options.goos = options.goos.trim().to_string();
    if options.goos.is_empty() {
        options.goos = runtime_goos();
    }
    options.goarch = options.goarch.trim().to_string();
    if options.goarch.is_empty() {
        options.goarch = runtime_goarch();
    }
    options.goos = normalize_goos(&options.goos);
    options.goarch = normalize_goarch(&options.goarch);
    options
}

//! Management control panel asset (`management.html`) lifecycle (Go: internal/managementasset):
//! locating the static directory, downloading the latest release asset with sha256 verification,
//! falling back to the default page, and the periodic background updater.

use std::path::Path;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use cpa_auth::singleflight::SingleFlight;
use cpa_config::Config;
use cpa_misc::httpfetch;
use parking_lot::{Mutex, RwLock};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

const DEFAULT_MANAGEMENT_RELEASE_URL: &str =
    "https://api.github.com/repos/router-for-me/Cli-Proxy-API-Management-Center/releases/latest";
const DEFAULT_MANAGEMENT_FALLBACK_URL: &str = "https://cpamc.router-for.me/";
const MANAGEMENT_ASSET_NAME: &str = "management.html";
const HTTP_USER_AGENT: &str = "CLIProxyAPI-management-updater";
const MANAGEMENT_SYNC_MIN_INTERVAL: Duration = Duration::from_secs(30);
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(3 * 60 * 60);
const MAX_ASSET_DOWNLOAD_SIZE: u64 = 50 << 20;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// The control panel asset filename (`ManagementFileName`).
pub const MANAGEMENT_FILE_NAME: &str = MANAGEMENT_ASSET_NAME;

static LAST_UPDATE_CHECK: Mutex<Option<Instant>> = Mutex::new(None);
static CURRENT_CONFIG: RwLock<Option<Arc<Config>>> = RwLock::new(None);
static SCHEDULER_ONCE: Once = Once::new();
static SCHEDULER_CONFIG_PATH: RwLock<String> = RwLock::new(String::new());
static SINGLE_FLIGHT: std::sync::LazyLock<SingleFlight<()>> = std::sync::LazyLock::new(SingleFlight::default);

/// Stores the latest configuration snapshot for management asset decisions.
pub fn set_current_config(cfg: Option<Arc<Config>>) {
    *CURRENT_CONFIG.write() = cfg;
}

/// Keeps the snapshot in sync with a config watch channel (the server calls
/// `SetCurrentConfig` on every reload). Returns when the channel closes.
pub fn follow_config(mut rx: tokio::sync::watch::Receiver<Arc<Config>>) -> tokio::task::JoinHandle<()> {
    set_current_config(Some(rx.borrow().clone()));
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            set_current_config(Some(rx.borrow().clone()));
        }
    })
}

/// Launches a background task that periodically ensures the management asset is up to date.
/// It respects the disable-control-panel flag on every iteration and picks up hot-reloaded
/// configurations through [`set_current_config`]. Only the first call starts the task.
pub fn start_auto_updater(cancel: CancellationToken, config_file_path: &str) {
    let config_file_path = config_file_path.trim();
    if config_file_path.is_empty() {
        tracing::debug!("management asset auto-updater skipped: empty config path");
        return;
    }
    *SCHEDULER_CONFIG_PATH.write() = config_file_path.to_string();
    SCHEDULER_ONCE.call_once(|| match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(run_auto_updater(cancel));
        }
        Err(_) => tracing::warn!("management asset auto-updater not started: no async runtime"),
    });
}

async fn run_auto_updater(cancel: CancellationToken) {
    let run_once = || async {
        let cfg = CURRENT_CONFIG.read().clone();
        if let Some(reason) = auto_update_skip_reason(cfg.as_deref()) {
            tracing::debug!("management asset auto-updater skipped: {reason}");
            return;
        }
        let Some(cfg) = cfg else { return };
        let config_path = SCHEDULER_CONFIG_PATH.read().clone();
        let static_dir = static_dir(&config_path);
        ensure_latest_management_html(&static_dir, &cfg.proxy_url, &cfg.remote_management.panel_github_repository).await;
    };

    run_once().await;
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + UPDATE_CHECK_INTERVAL, UPDATE_CHECK_INTERVAL);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => run_once().await,
        }
    }
}

/// Why the auto-updater iteration is skipped, or `None` when it should run.
fn auto_update_skip_reason(cfg: Option<&Config>) -> Option<&'static str> {
    let Some(cfg) = cfg else {
        return Some("config not yet available");
    };
    if cfg.home.enabled {
        return Some("cluster mode enabled");
    }
    if cfg.remote_management.disable_control_panel {
        return Some("control panel disabled");
    }
    if cfg.remote_management.disable_auto_update_panel {
        return Some("disable-auto-update-panel is enabled");
    }
    None
}

fn new_http_client(proxy_url: &str) -> reqwest::Client {
    cpa_auth::http::build_client(proxy_url.trim(), Some(HTTP_TIMEOUT)).unwrap_or_else(|e| {
        tracing::error!("failed to build management asset http client: {e}");
        reqwest::Client::new()
    })
}

#[derive(Debug, Default, Deserialize)]
struct ReleaseAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    digest: String,
}

#[derive(Debug, Default, Deserialize)]
struct ReleaseResponse {
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

// filepath helpers (Unix semantics)

fn go_clean(path: &str) -> String {
    cpa_core::util::filepath_clean(path)
}

fn go_join(base: &str, elem: &str) -> String {
    match (base.is_empty(), elem.is_empty()) {
        (true, true) => String::new(),
        (true, false) => go_clean(elem),
        (false, true) => go_clean(base),
        _ => go_clean(&format!("{base}/{elem}")),
    }
}

fn go_dir(path: &str) -> String {
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "",
    };
    let cleaned = go_clean(dir);
    if cleaned.is_empty() { ".".to_string() } else { cleaned }
}

fn go_base(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// Resolution of `MANAGEMENT_STATIC_PATH` shared by [`static_dir`] and [`file_path`]: the cleaned
/// override and whether it names the asset file itself.
fn static_override(override_path: &str) -> Option<(String, bool)> {
    let trimmed = override_path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let cleaned = go_clean(trimmed);
    let is_file = go_base(&cleaned).eq_ignore_ascii_case(MANAGEMENT_ASSET_NAME);
    Some((cleaned, is_file))
}

fn static_dir_from(override_path: &str, writable_path: &str, config_file_path: &str) -> String {
    if let Some((cleaned, is_file)) = static_override(override_path) {
        return if is_file { go_dir(&cleaned) } else { cleaned };
    }
    if !writable_path.is_empty() {
        return go_join(writable_path, "static");
    }
    let config_file_path = config_file_path.trim();
    if config_file_path.is_empty() {
        return String::new();
    }
    let mut base = go_dir(config_file_path);
    if Path::new(config_file_path).metadata().is_ok_and(|m| m.is_dir()) {
        base = config_file_path.to_string();
    }
    go_join(&base, "static")
}

fn env_trimmed(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_string()
}

/// The directory that stores the management control panel asset: `MANAGEMENT_STATIC_PATH`, else
/// `<WRITABLE_PATH>/static`, else `static` next to the config file.
pub fn static_dir(config_file_path: &str) -> String {
    static_dir_from(&env_trimmed("MANAGEMENT_STATIC_PATH"), &cpa_core::util::writable_path(), config_file_path)
}

/// The path to the management control panel asset; empty when it cannot be resolved.
pub fn file_path(config_file_path: &str) -> String {
    if let Some((cleaned, is_file)) = static_override(&env_trimmed("MANAGEMENT_STATIC_PATH")) {
        return if is_file { cleaned } else { go_join(&cleaned, MANAGEMENT_FILE_NAME) };
    }
    let dir = static_dir(config_file_path);
    if dir.is_empty() {
        return String::new();
    }
    go_join(&dir, MANAGEMENT_FILE_NAME)
}

/// Where [`ensure_latest_management_html`] fetches from; overridable for tests.
struct Sources {
    /// Replaces the URL derived from `panel-github-repository`.
    release_url: Option<String>,
    fallback_url: String,
}

/// Checks the latest management.html asset and updates the local copy when needed. Concurrent
/// sync attempts for the same file coalesce (and attempts within 30 seconds of the last one are
/// skipped). Returns whether the asset exists after the attempt.
pub async fn ensure_latest_management_html(static_dir: &str, proxy_url: &str, panel_repository: &str) -> bool {
    ensure_latest(static_dir, proxy_url, panel_repository, Sources { release_url: None, fallback_url: DEFAULT_MANAGEMENT_FALLBACK_URL.to_string() }, true).await
}

async fn ensure_latest(static_dir: &str, proxy_url: &str, panel_repository: &str, sources: Sources, throttle: bool) -> bool {
    let static_dir = static_dir.trim();
    if static_dir.is_empty() {
        tracing::debug!("management asset sync skipped: empty static directory");
        return false;
    }
    let local_path = go_join(static_dir, MANAGEMENT_ASSET_NAME);

    let flight_dir = static_dir.to_string();
    let flight_path = local_path.clone();
    let proxy_url = proxy_url.to_string();
    let panel_repository = panel_repository.to_string();
    let _ = SINGLE_FLIGHT
        .run(&local_path, move || async move {
            sync_once(&flight_dir, &flight_path, &proxy_url, &panel_repository, sources, throttle).await;
            Ok(())
        })
        .await;

    tokio::fs::metadata(&local_path).await.is_ok()
}

async fn sync_once(static_dir: &str, local_path: &str, proxy_url: &str, panel_repository: &str, sources: Sources, throttle: bool) {
    {
        let mut last = LAST_UPDATE_CHECK.lock();
        let now = Instant::now();
        if throttle && let Some(prev) = *last {
            let since = now.duration_since(prev);
            if since < MANAGEMENT_SYNC_MIN_INTERVAL {
                tracing::debug!(
                    "management asset sync skipped by throttle: last attempt {:?} ago (interval {:?})",
                    Duration::from_secs(since.as_secs()),
                    MANAGEMENT_SYNC_MIN_INTERVAL
                );
                return;
            }
        }
        *last = Some(now);
    }

    let mut local_file_missing = false;
    if let Err(e) = tokio::fs::metadata(local_path).await {
        if e.kind() == std::io::ErrorKind::NotFound {
            local_file_missing = true;
        } else {
            tracing::debug!("failed to stat local management asset: {e}");
        }
    }

    if let Err(e) = tokio::fs::create_dir_all(static_dir).await {
        tracing::warn!("failed to prepare static directory for management asset: {e}");
        return;
    }
    let release_url = sources.release_url.clone().unwrap_or_else(|| resolve_release_url(panel_repository));
    let client = new_http_client(proxy_url);

    let local_hash = match file_sha256(local_path).await {
        Ok(h) => h,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("failed to read local management asset hash: {e}");
            }
            String::new()
        }
    };

    let token = cpa_core::util::resolve_github_token();
    let (asset, remote_hash) = match fetch_latest_asset(&client, &release_url, &token).await {
        Ok(v) => v,
        Err(e) => {
            if local_file_missing {
                tracing::warn!("failed to fetch latest management release information, trying fallback page: {e}");
                ensure_fallback_management_html(&client, local_path, &sources.fallback_url).await;
            } else {
                tracing::warn!("failed to fetch latest management release information: {e}");
            }
            return;
        }
    };

    if !remote_hash.is_empty() && !local_hash.is_empty() && remote_hash.eq_ignore_ascii_case(&local_hash) {
        tracing::debug!("management asset is already up to date");
        return;
    }

    let (data, downloaded_hash) = match download_asset(&client, &asset.browser_download_url).await {
        Ok(v) => v,
        Err(e) => {
            if local_file_missing {
                tracing::warn!("failed to download management asset, trying fallback page: {e}");
                ensure_fallback_management_html(&client, local_path, &sources.fallback_url).await;
            } else {
                tracing::warn!("failed to download management asset: {e}");
            }
            return;
        }
    };

    if !remote_hash.is_empty() && !remote_hash.eq_ignore_ascii_case(&downloaded_hash) {
        tracing::error!("management asset digest mismatch: expected {remote_hash} got {downloaded_hash} — aborting update for safety");
        return;
    }

    if let Err(e) = atomic_write_file(local_path, data).await {
        tracing::warn!("failed to update management asset on disk: {e}");
        return;
    }
    tracing::info!("management asset updated successfully (hash={downloaded_hash})");
}

async fn ensure_fallback_management_html(client: &reqwest::Client, local_path: &str, fallback_url: &str) -> bool {
    let (data, downloaded_hash) = match download_asset(client, fallback_url).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to download fallback management control panel page: {e}");
            return false;
        }
    };

    tracing::warn!(
        "management asset downloaded from fallback URL without digest verification (hash={downloaded_hash}) — enable verified GitHub updates by keeping disable-auto-update-panel set to false"
    );

    if let Err(e) = atomic_write_file(local_path, data).await {
        tracing::warn!("failed to persist fallback management control panel page: {e}");
        return false;
    }
    tracing::info!("management asset updated from fallback page successfully (hash={downloaded_hash})");
    true
}

/// The releases API URL for a `panel-github-repository` value (GitHub repo or API URL); anything
/// else resolves to the default release URL.
fn resolve_release_url(repo: &str) -> String {
    let repo = repo.trim();
    if repo.is_empty() {
        return DEFAULT_MANAGEMENT_RELEASE_URL.to_string();
    }
    let Ok(mut parsed) = url::Url::parse(repo) else {
        return DEFAULT_MANAGEMENT_RELEASE_URL.to_string();
    };
    let Some(host) = parsed.host_str().map(str::to_lowercase) else {
        return DEFAULT_MANAGEMENT_RELEASE_URL.to_string();
    };
    let path = parsed.path().trim_end_matches('/').to_string();

    if host == "api.github.com" {
        let path = if path.to_lowercase().ends_with("/releases/latest") { path } else { format!("{path}/releases/latest") };
        parsed.set_path(&path);
        return parsed.to_string();
    }

    if host == "github.com" {
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            let repo_name = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
            return format!("https://api.github.com/repos/{}/{repo_name}/releases/latest", parts[0]);
        }
    }

    DEFAULT_MANAGEMENT_RELEASE_URL.to_string()
}

/// Fetches the latest release and returns its `management.html` asset plus the digest hash
/// (lowercase, without the algorithm prefix; empty when the release has none).
async fn fetch_latest_asset(client: &reqwest::Client, release_url: &str, github_token: &str) -> Result<(ReleaseAsset, String), String> {
    let release_url = if release_url.trim().is_empty() { DEFAULT_MANAGEMENT_RELEASE_URL } else { release_url };

    let bearer = format!("Bearer {github_token}");
    let mut headers = vec![("Accept", "application/vnd.github+json"), ("User-Agent", HTTP_USER_AGENT)];
    if !github_token.is_empty() {
        headers.push(("Authorization", &bearer));
    }

    let data = httpfetch::get_bytes(client, release_url, &headers, 0)
        .await
        .map_err(|e| format!("fetch release: {e}"))?;
    let release: ReleaseResponse = serde_json::from_slice(&data).map_err(|e| format!("decode release response: {e}"))?;

    for asset in release.assets {
        if asset.name.eq_ignore_ascii_case(MANAGEMENT_ASSET_NAME) {
            let remote_hash = parse_digest(&asset.digest);
            return Ok((asset, remote_hash));
        }
    }
    Err(format!("management asset {MANAGEMENT_ASSET_NAME} not found in latest release"))
}

async fn download_asset(client: &reqwest::Client, download_url: &str) -> Result<(Vec<u8>, String), String> {
    if download_url.trim().is_empty() {
        return Err("empty download url".into());
    }
    let data = httpfetch::get_bytes(client, download_url, &[("User-Agent", HTTP_USER_AGENT)], MAX_ASSET_DOWNLOAD_SIZE)
        .await
        .map_err(|e| format!("download asset: {e}"))?;
    let hash = hex::encode(Sha256::digest(&data));
    Ok((data, hash))
}

async fn file_sha256(path: &str) -> std::io::Result<String> {
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Writes through a `management-*.html` temp file in the target directory and renames it into
/// place (mode 0644), so readers never see a partial file.
async fn atomic_write_file(path: &str, data: Vec<u8>) -> std::io::Result<()> {
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let dir = Path::new(&path).parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let mut tmp = tempfile::Builder::new().prefix("management-").suffix(".html").tempfile_in(dir)?;
        tmp.write_all(&data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o644))?;
        }
        tmp.persist(&path).map(|_| ()).map_err(|e| e.error)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// `sha256:ABC` -> `abc`; empty for a blank digest.
fn parse_digest(digest: &str) -> String {
    let digest = digest.trim();
    if digest.is_empty() {
        return String::new();
    }
    let digest = match digest.find(':') {
        Some(idx) => &digest[idx + 1..],
        None => digest,
    };
    digest.trim().to_lowercase()
}

#[cfg(test)]
mod tests;

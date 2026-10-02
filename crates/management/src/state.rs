//! Shared dependencies of the management handlers.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use cpa_auth::{Auth, FileTokenStore, OAuthSessions, Store};
use cpa_config::Config;
use cpa_runtime::conductor::Manager;
use cpa_runtime::usage::UsageTracker;
use parking_lot::Mutex;
use tokio::sync::watch;

/// Called after the management API wrote the config file so the running server picks the change
/// up without waiting for the file watcher (typically `ConfigWatcher::reload_now`).
pub type ReloadHook = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Values for the `X-CPA-*` response headers (Go: `buildinfo`).
#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub version: String,
    pub commit: String,
    pub build_date: String,
}

impl Default for BuildInfo {
    fn default() -> Self {
        Self {
            version: cpa_auth::client_version(),
            commit: "none".into(),
            build_date: "unknown".into(),
        }
    }
}

/// The slice of the conductor the management API uses. `cpa_runtime::conductor::Manager`
/// implements it; tests substitute an in-memory registry.
#[async_trait::async_trait]
pub trait AuthRegistry: Send + Sync {
    /// Snapshot of all credentials with live status.
    fn list(&self) -> Vec<Auth>;
    fn get(&self, id: &str) -> Option<Auth>;
    /// Insert or update (and persist) a credential, returning the stored record.
    async fn update(&self, auth: Auth) -> Result<Auth, String>;
    async fn remove(&self, id: &str);
    /// Exchange the credential's refresh token now and store the result (Go: `ForceRefreshAuth`).
    async fn force_refresh_auth(&self, id: &str) -> Result<Auth, String>;
}

#[async_trait::async_trait]
impl AuthRegistry for Manager {
    fn list(&self) -> Vec<Auth> {
        Manager::list(self)
    }

    fn get(&self, id: &str) -> Option<Auth> {
        Manager::get(self, id)
    }

    async fn update(&self, auth: Auth) -> Result<Auth, String> {
        Manager::update(self, auth).await.map_err(|e| e.message)
    }

    async fn remove(&self, id: &str) {
        Manager::remove(self, id).await
    }

    async fn force_refresh_auth(&self, id: &str) -> Result<Auth, String> {
        Manager::force_refresh_auth(self, id).await.map_err(|e| e.message)
    }
}

/// Failed-login accounting of one client IP.
#[derive(Default)]
pub(crate) struct AttemptInfo {
    pub count: u32,
    pub blocked_until: Option<Instant>,
    pub last_activity: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct Shared {
    pub attempts: Mutex<HashMap<String, AttemptInfo>>,
    pub last_purge: Mutex<Option<Instant>>,
    /// SHA-256 of the last management key that verified, with the bcrypt hash it matched.
    pub verified: Mutex<Option<([u8; 32], String)>>,
    /// Serializes config file mutations (Go: `Handler.mu`).
    pub config_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Everything the management API needs, passed explicitly. Cheap to clone.
#[derive(Clone)]
pub struct ManagementState {
    /// Path of the YAML config file the management API reads and writes.
    pub config_path: PathBuf,
    /// Live config snapshots (hot reload publishes new ones).
    pub config: watch::Receiver<Arc<Config>>,
    /// Credential manager: listing, live status, update and removal.
    pub manager: Arc<Manager>,
    /// What the handlers actually call; defaults to `manager`.
    pub(crate) registry: Arc<dyn AuthRegistry>,
    /// Auth-file store rooted at `auth-dir`.
    pub store: Arc<FileTokenStore>,
    /// OAuth session registry; must be the one `login` was built with.
    pub oauth: Arc<OAuthSessions>,
    /// Starts OAuth logins and persists their credentials.
    pub login: cpa_auth::Manager,
    pub usage: Arc<UsageTracker>,
    /// Directory holding `main.log`, rotated logs, `error-*.log` and request logs.
    pub log_dir: PathBuf,
    /// `MANAGEMENT_PASSWORD`: plaintext secret that also forces remote access on.
    pub(crate) env_secret: Option<String>,
    /// Runtime password accepted from local clients only (TUI embedded mode).
    pub(crate) local_password: Option<String>,
    pub(crate) build: BuildInfo,
    pub(crate) reload: Option<ReloadHook>,
    pub(crate) shared: Arc<Shared>,
}

impl ManagementState {
    /// Reads `MANAGEMENT_PASSWORD` once, like the Go handler constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config_path: impl Into<PathBuf>,
        config: watch::Receiver<Arc<Config>>,
        manager: Arc<Manager>,
        store: Arc<FileTokenStore>,
        oauth: Arc<OAuthSessions>,
        login: cpa_auth::Manager,
        usage: Arc<UsageTracker>,
        log_dir: impl Into<PathBuf>,
    ) -> Self {
        let env_secret = std::env::var("MANAGEMENT_PASSWORD")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let log_dir = log_dir.into();
        let log_dir = if log_dir.is_absolute() {
            log_dir
        } else {
            std::path::absolute(&log_dir).unwrap_or(log_dir)
        };
        Self {
            config_path: config_path.into(),
            config,
            registry: manager.clone(),
            manager,
            store,
            oauth,
            login,
            usage,
            log_dir,
            env_secret,
            local_password: None,
            build: BuildInfo::default(),
            reload: None,
            shared: Arc::new(Shared::default()),
        }
    }

    /// Replaces the credential registry the handlers talk to (tests).
    pub fn with_registry(mut self, registry: Arc<dyn AuthRegistry>) -> Self {
        self.registry = registry;
        self
    }

    /// Overrides `MANAGEMENT_PASSWORD` (None disables it).
    pub fn with_env_secret(mut self, secret: Option<String>) -> Self {
        self.env_secret = secret
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self
    }

    /// Password accepted from `127.0.0.1` / `::1` clients only.
    pub fn with_local_password(mut self, password: impl Into<String>) -> Self {
        self.local_password = Some(password.into()).filter(|p| !p.is_empty());
        self
    }

    pub fn with_build_info(mut self, build: BuildInfo) -> Self {
        self.build = build;
        self
    }

    pub fn with_reload_hook(mut self, hook: ReloadHook) -> Self {
        self.reload = Some(hook);
        self
    }

    pub(crate) fn cfg(&self) -> Arc<Config> {
        self.config.borrow().clone()
    }

    pub(crate) async fn reload_config(&self) {
        if let Some(hook) = &self.reload {
            hook().await;
        }
    }

    /// The auth directory: the store's base dir, else the resolved `auth-dir` setting.
    pub(crate) fn auth_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = self.store.base_dir() {
            return Some(dir);
        }
        let cfg = self.cfg();
        cpa_config::resolve_auth_dir(&cfg.auth_dir)
            .ok()
            .filter(|d| !d.as_os_str().is_empty())
    }

    /// Port the server listens on (Go falls back to 8317 where it needs one).
    pub(crate) fn server_port(&self) -> u16 {
        let port = self.cfg().port;
        if port > 0 { port as u16 } else { 8317 }
    }

    /// `http(s)://127.0.0.1:<port><path>`: where provider redirects land on this server.
    pub(crate) fn loopback_url(&self, path: &str) -> String {
        let scheme = if self.cfg().tls.enable {
            "https"
        } else {
            "http"
        };
        format!("{scheme}://127.0.0.1:{}{path}", self.server_port())
    }
}

/// Absolute, lexically cleaned path (Go: `filepath.Abs`).
pub(crate) fn abs_path(p: &Path) -> PathBuf {
    cpa_auth::util::clean_path(&std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
}

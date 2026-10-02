//! Shared server state. The service wiring (config watcher, executors, store) is owned by the
//! caller; the server only needs these handles.

use std::sync::Arc;
use std::time::Duration;

use cpa_auth::{OAuthSessions, Store};
use cpa_config::Config;
use cpa_runtime::conductor::Manager;
use cpa_runtime::usage::UsageTracker;
use tokio::sync::watch;

use crate::models::{ModelCatalog, RegistryModelCatalog};
use crate::reqlog::RequestLogger;

/// Build identity reported by `X-CPA-*` headers and the CLI banner.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub version: String,
    pub commit: String,
    pub build_date: String,
}

impl Default for BuildInfo {
    fn default() -> Self {
        BuildInfo {
            version: "dev".into(),
            commit: "none".into(),
            build_date: "unknown".into(),
        }
    }
}

/// Endpoint `GET /keep-alive` guarded by the local management password (TUI supervision).
#[derive(Clone)]
pub struct KeepAlive {
    pub password: String,
    pub heartbeat: tokio::sync::mpsc::Sender<()>,
}

#[derive(Clone)]
pub struct AppState {
    /// Live config; every request reads the current snapshot.
    pub config: watch::Receiver<Arc<Config>>,
    pub manager: Arc<Manager>,
    pub auth_store: Arc<dyn Store>,
    pub oauth_sessions: Arc<OAuthSessions>,
    pub usage: Arc<UsageTracker>,
    /// Model listing computation behind `/v1/models` and `/v1beta/models`.
    pub models: Arc<dyn ModelCatalog>,
    pub build: BuildInfo,
    /// Active when the config still contains template API keys (decided at startup).
    pub example_api_key_safe_mode: bool,
    pub keep_alive: Option<KeepAlive>,
    /// Request log files (`request-log`); `None` in commercial mode.
    pub request_logger: Option<Arc<RequestLogger>>,
    /// Releases per-session executor resources when a client websocket ends
    /// (`AuthManager.CloseExecutionSession`); the service wiring plugs in the executors' hook.
    pub close_execution_session: Arc<dyn Fn(&str) + Send + Sync>,
}

impl AppState {
    pub fn new(
        config: watch::Receiver<Arc<Config>>,
        manager: Arc<Manager>,
        auth_store: Arc<dyn Store>,
        oauth_sessions: Arc<OAuthSessions>,
        usage: Arc<UsageTracker>,
    ) -> Self {
        AppState {
            config,
            manager,
            auth_store,
            oauth_sessions,
            usage,
            models: Arc::new(RegistryModelCatalog),
            build: BuildInfo::default(),
            example_api_key_safe_mode: false,
            keep_alive: None,
            request_logger: None,
            close_execution_session: Arc::new(|_| {}),
        }
    }

    /// Current config snapshot.
    pub fn cfg(&self) -> Arc<Config> {
        self.config.borrow().clone()
    }
}

/// Config-derived handler settings (Go: `SDKConfig` accessors in handlers.go).
#[derive(Debug, Clone, Copy, Default)]
pub struct HandlerSettings {
    /// `streaming.keepalive-seconds` (SSE comments and WebSocket pings); zero disables.
    pub stream_keepalive: Duration,
    /// `nonstream-keepalive-interval`; zero disables.
    pub nonstream_keepalive: Duration,
    pub bootstrap_retries: u32,
    pub passthrough_headers: bool,
}

impl HandlerSettings {
    pub fn from_config(cfg: &Config) -> Self {
        let secs = |v: i64| if v > 0 { Duration::from_secs(v as u64) } else { Duration::ZERO };
        HandlerSettings {
            stream_keepalive: secs(cfg.streaming.keepalive_seconds),
            nonstream_keepalive: secs(cfg.nonstream_keepalive_interval),
            bootstrap_retries: cfg.streaming.bootstrap_retries.max(0) as u32,
            passthrough_headers: cfg.passthrough_headers,
        }
    }
}

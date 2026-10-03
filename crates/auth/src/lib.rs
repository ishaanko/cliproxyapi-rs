//! Credentials for CLIProxyAPI: the `Auth` record, provider token file formats, the file store and
//! watcher, and OAuth login/refresh flows. Port of `internal/auth/**`, `sdk/auth` and the data model
//! of `sdk/cliproxy/auth`; auth dirs written by the Go app load unchanged.
//!
//! Layout:
//! - [`types`], [`credmeta`]: `Auth`, status, attributes/metadata, index and expiry derivation.
//! - [`storage`]: per-provider credential file structs (exact Go JSON shapes).
//! - [`store`], [`watcher`]: `Store` trait, `FileTokenStore`, notify-based change events.
//! - [`claude`], [`codex`], [`antigravity`], [`xai`], [`kimi`], [`vertex`], [`devin`], [`meta`]:
//!   provider endpoints, token exchange/refresh and `Auth` record construction.
//! - [`login`], [`manager`]: `LoginSession` (CLI and management API) and the login manager.
//! - [`sessions`]: OAuth session registry and callback handoff behind the management endpoints.
//!
//! Driving a login:
//!
//! ```ignore
//! let manager = Manager::new(Arc::new(FileTokenStore::with_dir(auth_dir)));
//!
//! // CLI: prints / opens the URL, waits for the callback, saves the credential file.
//! let outcome = manager.login(Provider::Codex, LoginOptions::cli()).await?;
//!
//! // Management API: start, hand the URL to the UI, poll, optionally feed a pasted callback.
//! let session = manager.start_login(Provider::Claude, LoginOptions::management("")).await?;
//! respond_json(session.start_info().to_json());                    // {"status":"ok","url","state"}
//! let (http_status, body) = manager.sessions().poll_status(&state); // wait | ok | error
//! let (http_status, body) = manager.sessions().handle_oauth_callback(Some(auth_dir), &req).await;
//! ```
//!
//! Known gaps vs the Go app: no TLS ClientHello fingerprinting (Claude uses a uTLS Firefox profile
//! in Go; here plain rustls, see [`http`]), no `compress` (LZW) content-encoding on Claude OAuth
//! responses, and no git / postgres / object-store token stores.

use std::sync::RwLock;

pub mod antigravity;
pub mod browser;
pub mod callback_server;
pub mod claude;
pub mod codex;
pub mod credmeta;
pub mod devin;
#[cfg(test)]
mod e2e_tests;
pub mod error;
#[cfg(test)]
mod flow_tests;
pub mod http;
pub mod jwt;
pub mod kimi;
pub mod login;
pub mod manager;
pub mod meta;
pub mod oauth;
pub mod pkce;
pub mod plugin_parser;
mod redact;
pub mod refresh;
pub mod retry;
pub mod sessions;
pub mod singleflight;
pub mod storage;
pub mod store;
#[cfg(test)]
mod testutil;
pub mod types;
pub mod util;
pub mod vertex;
pub mod watcher;
pub mod xai;

pub use error::{AuthFlowError, Result};
pub use login::{LoginOptions, LoginOutcome, LoginSession, LoginStart, LoginStatus, Provider};
pub use manager::Manager;
pub use refresh::refresh_auth;
pub use sessions::OAuthSessions;
pub use storage::TokenStorage;
pub use store::{FileTokenStore, SaveOptions, Store};
pub use types::{Auth, Status};
pub use watcher::{AuthFileEvent, AuthWatcher, WatchOptions};

static CLIENT_VERSION: RwLock<String> = RwLock::new(String::new());

/// Build version reported in `X-Msh-Version` (Go `buildinfo.Version`); `dev` until set.
pub fn client_version() -> String {
    match CLIENT_VERSION.read() {
        Ok(v) if !v.is_empty() => v.clone(),
        _ => "dev".to_string(),
    }
}

/// Sets the version string sent to providers that want one. Call once at startup.
pub fn set_client_version(version: &str) {
    if let Ok(mut v) = CLIENT_VERSION.write() {
        *v = version.to_string();
    }
}

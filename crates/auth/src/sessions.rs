//! In-process OAuth session store and callback handoff used by the management API
//! (internal/api/handlers/management/oauth_sessions.go, oauth_callback.go).
//!
//! A session is registered per login `state`. It is *pending* until it completes, fails (a message
//! is stored) or is cancelled (entry removed). Provider redirects and manually pasted callbacks
//! reach a running login either through the session's in-process inbox or, when no inbox exists,
//! through the `.oauth-<provider>-<state>.oauth` callback file in the auth dir (Go's handoff).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::oauth::validate_oauth_state;

pub const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
pub const COMPLETED_SESSION_TTL: Duration = Duration::from_secs(60);

/// Redirect payload delivered to a waiting login: `code` + `state`, or `error`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackPayload {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub error: String,
}

/// Snapshot of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub provider: String,
    /// Empty while pending; the failure message once failed.
    pub status: String,
    pub completed: bool,
}

struct Session {
    provider: String,
    status: String,
    completed: bool,
    expires_at: Instant,
    inbox: Option<mpsc::UnboundedSender<CallbackPayload>>,
}

/// Session registry. One instance per server; cheap to share behind an `Arc`.
pub struct OAuthSessions {
    ttl: Duration,
    completed_ttl: Duration,
    sessions: Mutex<HashMap<String, Session>>,
}

impl Default for OAuthSessions {
    fn default() -> Self {
        Self::new(SESSION_TTL)
    }
}

impl OAuthSessions {
    pub fn new(ttl: Duration) -> Self {
        let ttl = if ttl.is_zero() { SESSION_TTL } else { ttl };
        Self {
            ttl,
            completed_ttl: COMPLETED_SESSION_TTL.min(ttl),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn purge(map: &mut HashMap<String, Session>, now: Instant) {
        map.retain(|_, s| now <= s.expires_at);
    }

    /// `Register`: a pending session for `state`, replacing any previous one.
    pub fn register(&self, state: &str, provider: &str) {
        self.register_inner(state, provider, None);
    }

    /// Registers a session whose callbacks are delivered to `inbox` in-process.
    pub fn register_with_inbox(
        &self,
        state: &str,
        provider: &str,
        inbox: mpsc::UnboundedSender<CallbackPayload>,
    ) {
        self.register_inner(state, provider, Some(inbox));
    }

    fn register_inner(
        &self,
        state: &str,
        provider: &str,
        inbox: Option<mpsc::UnboundedSender<CallbackPayload>>,
    ) {
        let state = state.trim();
        let provider = provider.trim().to_lowercase();
        if state.is_empty() || provider.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut map = self.sessions.lock();
        Self::purge(&mut map, now);
        map.insert(
            state.to_string(),
            Session {
                provider,
                status: String::new(),
                completed: false,
                expires_at: now + self.ttl,
                inbox,
            },
        );
    }

    /// `SetError`: ignored for unknown or completed sessions; empty message becomes
    /// "Authentication failed". Refreshes the TTL.
    pub fn set_error(&self, state: &str, message: &str) {
        let state = state.trim();
        if state.is_empty() {
            return;
        }
        let message = match message.trim() {
            "" => "Authentication failed",
            m => m,
        };
        let now = Instant::now();
        let mut map = self.sessions.lock();
        Self::purge(&mut map, now);
        if let Some(s) = map.get_mut(state).filter(|s| !s.completed) {
            s.status = message.to_string();
            s.inbox = None;
            s.expires_at = now + self.ttl;
        }
    }

    /// `Complete`: marks done and keeps the entry for one minute so pollers see `ok`.
    pub fn complete(&self, state: &str) {
        let state = state.trim();
        if state.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut map = self.sessions.lock();
        Self::purge(&mut map, now);
        if let Some(s) = map.get_mut(state).filter(|s| !s.completed) {
            s.status.clear();
            s.completed = true;
            s.inbox = None;
            s.expires_at = now + self.completed_ttl;
        }
    }

    /// `CompleteProvider`: completes every pending session of a provider (a credential for it was
    /// saved elsewhere). Returns how many were completed.
    pub fn complete_provider(&self, provider: &str) -> usize {
        let provider = provider.trim().to_lowercase();
        if provider.is_empty() {
            return 0;
        }
        let now = Instant::now();
        let mut map = self.sessions.lock();
        Self::purge(&mut map, now);
        let mut n = 0;
        for s in map
            .values_mut()
            .filter(|s| !s.completed && s.provider.eq_ignore_ascii_case(&provider))
        {
            s.status.clear();
            s.completed = true;
            s.inbox = None;
            s.expires_at = now + self.completed_ttl;
            n += 1;
        }
        n
    }

    pub fn get(&self, state: &str) -> Option<SessionInfo> {
        let now = Instant::now();
        let mut map = self.sessions.lock();
        Self::purge(&mut map, now);
        map.get(state.trim()).map(|s| SessionInfo {
            provider: s.provider.clone(),
            status: s.status.clone(),
            completed: s.completed,
        })
    }

    /// Pending: exists, not completed, no error status, and (when given) matching provider.
    pub fn is_pending(&self, state: &str, provider: &str) -> bool {
        let provider = provider.trim().to_lowercase();
        let now = Instant::now();
        let mut map = self.sessions.lock();
        Self::purge(&mut map, now);
        let Some(s) = map.get(state.trim()) else {
            return false;
        };
        if s.completed || !s.status.is_empty() {
            return false;
        }
        provider.is_empty() || s.provider.eq_ignore_ascii_case(&provider)
    }

    /// `Cancel`: removes only a pending session. Returns whether one was removed.
    pub fn cancel(&self, state: &str) -> bool {
        let state = state.trim();
        if state.is_empty() {
            return false;
        }
        let now = Instant::now();
        let mut map = self.sessions.lock();
        Self::purge(&mut map, now);
        match map.get(state) {
            Some(s) if !s.completed && s.status.is_empty() => {
                map.remove(state);
                true
            }
            _ => false,
        }
    }

    /// `GetAuthStatus` for built-in sessions: `(http status, body)`.
    pub fn poll_status(&self, state: &str) -> (u16, Value) {
        let state = state.trim();
        if state.is_empty() {
            return (200, json!({"status": "ok"}));
        }
        if validate_oauth_state(state).is_err() {
            return (400, json!({"status": "error", "error": "invalid state"}));
        }
        match self.get(state) {
            None => (
                200,
                json!({"status": "error", "error": "unknown or expired state"}),
            ),
            Some(s) if s.completed => (200, json!({"status": "ok"})),
            Some(s) if !s.status.is_empty() => (200, json!({"status": "error", "error": s.status})),
            Some(_) => (200, json!({"status": "wait"})),
        }
    }

    /// `CancelAuthSession` endpoint body: `(http status, body)`.
    pub fn cancel_status(&self, state: &str) -> (u16, Value) {
        let state = state.trim();
        if state.is_empty() {
            return (400, json!({"status": "error", "error": "missing state"}));
        }
        if validate_oauth_state(state).is_err() {
            return (400, json!({"status": "error", "error": "invalid state"}));
        }
        (
            200,
            json!({"status": "ok", "cancelled": self.cancel(state)}),
        )
    }

    /// Hands a callback to the login behind `state`. Returns false when the session has no inbox.
    fn deliver(&self, state: &str, payload: CallbackPayload) -> bool {
        let map = self.sessions.lock();
        match map.get(state).and_then(|s| s.inbox.as_ref()) {
            Some(tx) => tx.send(payload).is_ok(),
            None => false,
        }
    }

    /// `WriteOAuthCallbackFileForPendingSession` generalized: delivers in-process when the session
    /// has an inbox, else writes the callback file. Errors with `NotPending` unless the session is
    /// pending for the (normalized) provider.
    pub async fn submit_callback(
        &self,
        auth_dir: Option<&Path>,
        provider: &str,
        state: &str,
        code: &str,
        error: &str,
    ) -> Result<(), CallbackError> {
        let canonical =
            normalize_callback_provider(provider).ok_or(CallbackError::UnsupportedProvider)?;
        if !self.is_pending(state, &canonical) {
            return Err(CallbackError::NotPending);
        }
        let payload = CallbackPayload {
            code: code.trim().into(),
            state: state.trim().into(),
            error: error.trim().into(),
        };
        if self.deliver(state.trim(), payload.clone()) {
            return Ok(());
        }
        let dir = auth_dir
            .filter(|d| !d.as_os_str().is_empty())
            .ok_or_else(|| CallbackError::Io("auth dir is empty".into()))?
            .to_path_buf();
        let state = state.to_string();
        tokio::task::spawn_blocking(move || write_callback_file(&dir, &canonical, &state, &payload))
            .await
            .map_err(|e| CallbackError::Io(e.to_string()))?
            .map(|_| ())
            .map_err(CallbackError::Io)
    }

    /// `handleOAuthCallback`: validation and error mapping of the manual / redirect callback
    /// endpoints. `(http status, body)`; success is `200 {"status":"ok"}` (callback accepted, not
    /// necessarily finished).
    pub async fn handle_oauth_callback(
        &self,
        auth_dir: Option<&Path>,
        req: &CallbackRequest,
    ) -> (u16, Value) {
        let err = |status: u16, msg: &str| (status, json!({"status": "error", "error": msg}));
        let mut state = req.state.trim().to_string();
        let mut code = req.code.trim().to_string();
        let mut err_msg = req.error.trim().to_string();

        let redirect = req.redirect_url.trim();
        if !redirect.is_empty() {
            let Ok(u) = url::Url::parse(redirect) else {
                return err(400, "invalid redirect_url");
            };
            let q = |k: &str| {
                u.query_pairs()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| v.trim().to_string())
                    .unwrap_or_default()
            };
            if state.is_empty() {
                state = q("state");
            }
            if code.is_empty() {
                code = q("code");
            }
            if err_msg.is_empty() {
                err_msg = q("error");
                if err_msg.is_empty() {
                    err_msg = q("error_description");
                }
            }
        }

        if state.is_empty() {
            return err(400, "state is required");
        }
        if validate_oauth_state(&state).is_err() {
            return err(400, "invalid state");
        }
        if code.is_empty() && err_msg.is_empty() {
            return err(400, "code or error is required");
        }
        let Some(session) = self.get(&state) else {
            return err(404, "unknown or expired state");
        };
        if session.completed {
            return err(409, "oauth flow is already completed");
        }
        let provider = if req.provider.trim().is_empty() {
            session.provider.clone()
        } else {
            req.provider.trim().to_string()
        };
        let Some(canonical) = normalize_callback_provider(&provider) else {
            return err(400, "unsupported provider");
        };
        if !session.status.is_empty() {
            return err(409, &session.status);
        }
        if !session.provider.eq_ignore_ascii_case(&canonical) {
            return err(400, "provider does not match state");
        }
        match self
            .submit_callback(auth_dir, &canonical, &state, &code, &err_msg)
            .await
        {
            Ok(()) => (200, json!({"status": "ok"})),
            Err(CallbackError::NotPending) => match self.get(&state) {
                Some(s) if !s.status.is_empty() => err(409, &s.status),
                _ => err(409, "oauth flow is not pending"),
            },
            Err(e) => {
                tracing::error!("failed to persist oauth callback: {e}");
                err(500, "failed to persist oauth callback")
            }
        }
    }
}

/// Body of the manual callback endpoints.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CallbackRequest {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub redirect_url: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub error: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CallbackError {
    #[error("oauth session is not pending")]
    NotPending,
    #[error("unsupported oauth provider")]
    UnsupportedProvider,
    #[error("{0}")]
    Io(String),
}

/// `NormalizeOAuthProvider`: canonical session provider for built-in flows (Claude is `anthropic`).
pub fn normalize_oauth_provider(provider: &str) -> Option<&'static str> {
    match provider.trim().to_lowercase().as_str() {
        "anthropic" | "claude" => Some("anthropic"),
        "codex" | "openai" => Some("codex"),
        "antigravity" | "anti-gravity" => Some("antigravity"),
        "xai" | "x-ai" | "x.ai" | "grok" => Some("xai"),
        "devin" | "cognition" => Some("devin"),
        "meta" | "muse" => Some("meta"),
        _ => None,
    }
}

/// Callback providers are the built-in ones plus kimi sessions (their own names) and
/// plugin-style `[a-z0-9-]+` names.
pub fn normalize_callback_provider(provider: &str) -> Option<String> {
    if let Some(p) = normalize_oauth_provider(provider) {
        return Some(p.to_string());
    }
    let t = provider.trim().to_lowercase();
    if t.is_empty()
        || !t
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return None;
    }
    Some(t)
}

/// `<auth-dir>/.oauth-<provider>-<state>.oauth`.
pub fn callback_file_path(auth_dir: &Path, canonical_provider: &str, state: &str) -> PathBuf {
    auth_dir.join(format!(".oauth-{canonical_provider}-{state}.oauth"))
}

/// Atomically publishes the callback file (temp file + rename), dir mode 0700.
pub fn write_callback_file(
    auth_dir: &Path,
    canonical_provider: &str,
    state: &str,
    payload: &CallbackPayload,
) -> Result<PathBuf, String> {
    if validate_oauth_state(state).is_err() {
        return Err("invalid oauth state".into());
    }
    crate::storage::mkdir_all_private(auth_dir)
        .map_err(|e| format!("create oauth callback dir: {e}"))?;
    let data =
        serde_json::to_vec(payload).map_err(|e| format!("marshal oauth callback payload: {e}"))?;
    let final_path = callback_file_path(auth_dir, canonical_provider, state);
    let tmp = auth_dir.join(format!(".oauth-callback-{}", crate::util::random_hex(8)));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &final_path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("publish oauth callback file: {e}"));
    }
    Ok(final_path)
}

/// Reads and removes a callback file if present.
pub fn take_callback_file(
    auth_dir: &Path,
    canonical_provider: &str,
    state: &str,
) -> Option<CallbackPayload> {
    let path = callback_file_path(auth_dir, canonical_provider, state);
    let data = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(serde_json::from_slice(&data).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_pending_error_complete_cancel() {
        let s = OAuthSessions::default();
        s.register("st1", "Anthropic");
        assert!(s.is_pending("st1", "anthropic"));
        assert!(s.is_pending("st1", ""));
        assert!(!s.is_pending("st1", "codex"));
        assert_eq!(s.poll_status("st1"), (200, json!({"status": "wait"})));

        s.set_error("st1", "  ");
        assert_eq!(
            s.poll_status("st1").1,
            json!({"status": "error", "error": "Authentication failed"})
        );
        assert!(!s.is_pending("st1", ""));
        assert!(!s.cancel("st1"), "errored sessions cannot be cancelled");

        s.register("st2", "codex");
        s.complete("st2");
        s.set_error("st2", "late");
        assert_eq!(s.poll_status("st2").1, json!({"status": "ok"}));

        s.register("st3", "xai");
        assert!(s.cancel("st3"));
        assert_eq!(
            s.poll_status("st3").1,
            json!({"status": "error", "error": "unknown or expired state"})
        );
        assert_eq!(s.poll_status("").1, json!({"status": "ok"}));
        assert_eq!(s.poll_status("bad/state").0, 400);
    }

    #[test]
    fn complete_provider_only_touches_pending_matching_sessions() {
        let s = OAuthSessions::default();
        s.register("a", "codex");
        s.register("b", "codex");
        s.register("c", "xai");
        assert_eq!(s.complete_provider("CODEX"), 2);
        assert!(s.is_pending("c", "xai"));
        assert_eq!(s.complete_provider("codex"), 0);
    }

    #[test]
    fn expired_sessions_are_purged() {
        let s = OAuthSessions::new(Duration::from_millis(20));
        s.register("old", "codex");
        std::thread::sleep(Duration::from_millis(40));
        assert!(s.get("old").is_none());
    }

    #[tokio::test]
    async fn callback_endpoint_error_mapping() {
        let s = OAuthSessions::default();
        let dir = tempfile::tempdir().unwrap();
        let req = |provider: &str, state: &str, code: &str| CallbackRequest {
            provider: provider.into(),
            state: state.into(),
            code: code.into(),
            ..Default::default()
        };
        async fn call(
            s: &OAuthSessions,
            dir: &std::path::Path,
            r: &CallbackRequest,
        ) -> (u16, Value) {
            s.handle_oauth_callback(Some(dir), r).await
        }

        assert_eq!(call(&s, dir.path(), &req("", "", "c")).await.0, 400);
        assert_eq!(
            call(&s, dir.path(), &req("", "bad/state", "c")).await.1["error"],
            "invalid state"
        );
        assert_eq!(
            call(&s, dir.path(), &req("", "st", "")).await.1["error"],
            "code or error is required"
        );
        assert_eq!(call(&s, dir.path(), &req("", "st", "c")).await.0, 404);

        s.register("st", "anthropic");
        assert_eq!(
            call(&s, dir.path(), &req("openai", "st", "c")).await.1["error"],
            "provider does not match state"
        );
        assert_eq!(
            call(&s, dir.path(), &req("!!", "st", "c")).await.1["error"],
            "unsupported provider"
        );

        // No inbox: falls back to the callback file, which the waiter reads and removes.
        assert_eq!(
            call(&s, dir.path(), &req("claude", "st", " the-code ")).await,
            (200, json!({"status": "ok"}))
        );
        let got = take_callback_file(dir.path(), "anthropic", "st").unwrap();
        assert_eq!(
            got,
            CallbackPayload {
                code: "the-code".into(),
                state: "st".into(),
                error: String::new()
            }
        );
        assert!(take_callback_file(dir.path(), "anthropic", "st").is_none());

        // redirect_url fills the blanks.
        let r = CallbackRequest {
            redirect_url: "http://localhost:54545/callback?code=zz&state=st".into(),
            ..Default::default()
        };
        assert_eq!(call(&s, dir.path(), &r).await.0, 200);

        s.set_error("st", "Bad request");
        assert_eq!(
            call(&s, dir.path(), &req("", "st", "c")).await,
            (409, json!({"status": "error", "error": "Bad request"}))
        );
        s.register("done", "codex");
        s.complete("done");
        assert_eq!(
            call(&s, dir.path(), &req("", "done", "c")).await.1["error"],
            "oauth flow is already completed"
        );
    }

    #[tokio::test]
    async fn inbox_receives_callbacks_in_process() {
        let s = OAuthSessions::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        s.register_with_inbox("st", "codex", tx);
        s.submit_callback(None, "codex", "st", "c1", "")
            .await
            .unwrap();
        assert_eq!(rx.recv().await.unwrap().code, "c1");
        assert!(matches!(
            s.submit_callback(None, "xai", "st", "c", "").await,
            Err(CallbackError::NotPending)
        ));
    }
}

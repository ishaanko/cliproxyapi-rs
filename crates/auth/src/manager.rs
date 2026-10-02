//! Login manager: persists freshly logged-in credentials (sdk/auth/manager.go and the management
//! `saveTokenRecord`) and wires logins to a store and the OAuth session registry.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::claude::find_matching_legacy_credential;
use crate::credmeta::merge_existing_auth_metadata;
use crate::error::{AuthFlowError, Result};
use crate::login::{LoginOptions, LoginOutcome, LoginSession, Provider, start_login};
use crate::sessions::OAuthSessions;
use crate::store::{SaveOptions, Store};
use crate::types::Auth;

/// `PostAuthHook`: may adjust a record (e.g. inject metadata) after login, before it is persisted.
pub type PostAuthHook = Arc<dyn Fn(&mut Auth) -> std::result::Result<(), String> + Send + Sync>;

/// Persists a login result: merges operator fields from the existing file, migrates a legacy Claude
/// credential, runs the post-auth hook and saves with creation intent. Returns the saved path.
pub fn save_login_record(
    store: &dyn Store,
    hook: Option<&PostAuthHook>,
    record: &mut Auth,
) -> Result<Option<PathBuf>> {
    merge_existing_file_metadata(store, record);
    let legacy = find_matching_legacy_credential(store, record)?;
    if let Some(legacy) = &legacy {
        merge_existing_auth_metadata(record, &legacy.metadata);
    }
    if let Some(hook) = hook {
        hook(record).map_err(|e| AuthFlowError::other(format!("post-auth hook failed: {e}")))?;
    }
    let saved = store
        .save(
            record,
            SaveOptions {
                creation_intent: true,
            },
        )
        .map_err(|e| AuthFlowError::Storage(e.to_string()))?;
    if let Some(legacy) = legacy {
        if saved.is_none() {
            return Err(AuthFlowError::other(
                "canonical Claude credential was not persisted; legacy credential retained",
            ));
        }
        let mut legacy_id = legacy.id.trim().to_string();
        if legacy_id.is_empty() {
            legacy_id = legacy.file_name.trim().to_string();
        }
        store.delete(&legacy_id).map_err(|e| {
            AuthFlowError::other(format!(
                "canonical Claude credential saved but legacy credential cleanup failed: {e}"
            ))
        })?;
    }
    Ok(saved)
}

/// Keeps operator-set fields (proxy_url, priority, prefix, ...) of an existing file with the same
/// name across a re-login.
fn merge_existing_file_metadata(store: &dyn Store, record: &mut Auth) {
    let Some(dir) = store.base_dir() else { return };
    let mut target = record.file_name.trim().to_string();
    if target.is_empty() {
        target = record.id.clone();
    }
    if target.is_empty() {
        return;
    }
    let path: PathBuf = if Path::new(&target).is_absolute() {
        PathBuf::from(&target)
    } else {
        dir.join(&target)
    };
    let Ok(raw) = std::fs::read(&path) else {
        return;
    };
    if raw.is_empty() {
        return;
    }
    if let Ok(Value::Object(existing)) = serde_json::from_slice::<Value>(&raw) {
        merge_existing_auth_metadata(record, &existing);
    }
}

/// Drives logins for every provider against one store and session registry.
#[derive(Clone)]
pub struct Manager {
    store: Arc<dyn Store>,
    sessions: Arc<OAuthSessions>,
    hook: Option<PostAuthHook>,
}

impl Manager {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self {
            store,
            sessions: Arc::new(OAuthSessions::default()),
            hook: None,
        }
    }

    /// Shares an existing session registry (the management API owns one for polling).
    pub fn with_sessions(mut self, sessions: Arc<OAuthSessions>) -> Self {
        self.sessions = sessions;
        self
    }

    pub fn with_post_auth_hook(mut self, hook: PostAuthHook) -> Self {
        self.hook = Some(hook);
        self
    }

    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    pub fn sessions(&self) -> &Arc<OAuthSessions> {
        &self.sessions
    }

    /// Saves a record the way a login does (merge, legacy migration, hook, create).
    pub fn save_record(&self, record: &mut Auth) -> Result<Option<PathBuf>> {
        save_login_record(self.store.as_ref(), self.hook.as_ref(), record)
    }

    /// Starts a login without waiting for it: returns the URL and state immediately; poll with
    /// [`LoginSession::poll`] or [`OAuthSessions::poll_status`]. This is the management API path.
    pub async fn start_login(
        &self,
        provider: Provider,
        opts: LoginOptions,
    ) -> Result<LoginSession> {
        start_login(
            self.store.clone(),
            self.sessions.clone(),
            self.hook.clone(),
            provider,
            opts,
        )
        .await
    }

    /// CLI login: starts the flow, prints (or opens) the URL, waits for completion and saves.
    pub async fn login(&self, provider: Provider, opts: LoginOptions) -> Result<LoginOutcome> {
        let no_browser = opts.no_browser;
        let session = self.start_login(provider, opts).await?;
        crate::login::announce(&session, no_browser);
        let outcome = session.wait().await?;
        println!("{} authentication successful", provider.display_name());
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::credential_file_name;
    use crate::storage::{ClaudeTokenStorage, TokenStorage};
    use crate::store::FileTokenStore;
    use serde_json::json;

    fn claude_record(email: &str, org: &str) -> Auth {
        let name = credential_file_name(email, org, "");
        let mut a = Auth::new(name, "claude");
        a.storage = Some(TokenStorage::Claude(ClaudeTokenStorage {
            access_token: "new-at".into(),
            email: email.into(),
            organization_uuid: org.into(),
            ..Default::default()
        }));
        a.metadata.insert("email".into(), email.into());
        if !org.is_empty() {
            a.metadata.insert("organization_uuid".into(), org.into());
        }
        a
    }

    #[test]
    fn re_login_keeps_operator_fields_and_overwrites_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTokenStore::with_dir(dir.path());
        let mut first = claude_record("a@b.c", "");
        first.metadata.insert("proxy_url".into(), "http://p".into());
        first.metadata.insert("priority".into(), json!(5));
        save_login_record(&store, None, &mut first).unwrap();

        let mut second = claude_record("a@b.c", "");
        let path = save_login_record(&store, None, &mut second)
            .unwrap()
            .unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["proxy_url"], "http://p");
        assert_eq!(saved["priority"], 5);
        assert_eq!(saved["access_token"], "new-at");
    }

    #[test]
    fn legacy_email_only_claude_file_is_migrated_into_hashed_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTokenStore::with_dir(dir.path());
        // Legacy credential: email-only name, same org uuid recorded inside.
        std::fs::write(
            dir.path().join("claude-a@b.c.json"),
            r#"{"type":"claude","email":"a@b.c","organization_uuid":"ORG-1","access_token":"old","prefix":"team","note":"mine"}"#,
        )
        .unwrap();

        let mut record = claude_record("a@b.c", "ORG-1");
        let saved = save_login_record(&store, None, &mut record)
            .unwrap()
            .unwrap();
        assert_ne!(saved, dir.path().join("claude-a@b.c.json"));
        assert!(
            !dir.path().join("claude-a@b.c.json").exists(),
            "legacy file must be deleted"
        );
        let body: Value = serde_json::from_slice(&std::fs::read(&saved).unwrap()).unwrap();
        assert_eq!(body["prefix"], "team");
        assert_eq!(body["note"], "mine");
        assert_eq!(body["access_token"], "new-at");
    }

    #[test]
    fn post_auth_hook_runs_and_can_veto() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTokenStore::with_dir(dir.path());
        let tag: PostAuthHook = Arc::new(|a: &mut Auth| {
            a.metadata.insert("tagged".into(), true.into());
            Ok(())
        });
        let mut r = claude_record("x@y.z", "");
        let p = save_login_record(&store, Some(&tag), &mut r)
            .unwrap()
            .unwrap();
        assert!(
            std::fs::read_to_string(p)
                .unwrap()
                .contains("\"tagged\":true")
        );

        let veto: PostAuthHook = Arc::new(|_: &mut Auth| Err("nope".into()));
        let mut r2 = claude_record("q@y.z", "");
        let err = save_login_record(&store, Some(&veto), &mut r2).unwrap_err();
        assert!(err.to_string().contains("post-auth hook failed: nope"));
    }
}

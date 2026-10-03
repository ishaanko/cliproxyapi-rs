//! Git-backed token store (Go: internal/store/gitstore.go).
//!
//! Config and auth files live in a git repository (`auths/`, `config/config.yaml`) that is
//! cloned next to the configured auth dir. Every change is committed as a single parentless
//! commit, so history stays squashed, and pushed with force-with-lease semantics. Pulls that
//! diverge are reconciled against local edits and a corrupt repository is rebuilt by re-cloning.
//! libgit2 (`git2`) stands in for go-git; see `ops` for the primitives.

mod err;
mod gc;
mod ops;
mod recovery;
#[cfg(test)]
mod tests;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use cpa_auth::Auth;
use cpa_auth::credmeta::{
    Metadata, apply_custom_headers_from_metadata, normalize_credential_metadata, validate_auth_weight,
};
use cpa_auth::store::{SaveOptions, Store, StoreError};
use cpa_auth::types::{ATTRIBUTE_PATH, ATTRIBUTE_SOURCE_BACKEND, AUTH_SOURCE_GIT, Status};
use cpa_auth::util::{clean_path, marshal_compact};
use cpa_runtime::service::StorePersister;
use git2::{Repository, Signature};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use self::err::{AUTH_REQUIRED, EMPTY_REMOTE, GitErr, NON_FAST_FORWARD, R, REF_NOT_FOUND, UNSTAGED_CHANGES, UP_TO_DATE};
use self::ops::{
    BasicAuth, checkout_branch, clone_into, create_and_checkout_branch, fetch_origin, head_ref, init_repo,
    pull, reset_index_to_head, resolve_remote_default_branch, restore_head_and_index, restore_missing_tracked_files,
    set_branch_tracking, tree_snapshot, verify_repository_head, worktree_dirty_paths,
};
use self::recovery::{RecoveryCtx, os_rename, reconcile_remote_worktree, recover_repository};
use crate::common::{backend_err, json_equal, mkdir_all_private, rel_path, write_file_private};

/// Minimum time between garbage collection runs.
const GC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

#[derive(Default)]
struct Dirs {
    base_dir: String,
    repo_dir: String,
    config_dir: String,
}

/// Persists token records and auth metadata using git as the backing storage.
pub struct GitTokenStore {
    /// Serializes every repository operation (Go `mu`).
    mu: Mutex<()>,
    dirs: RwLock<Dirs>,
    remote: String,
    branch: String,
    username: String,
    password: String,
    last_gc: Mutex<Option<Instant>>,
}

fn into_store_err(e: GitErr) -> StoreError {
    backend_err(e.into_message())
}

fn failed_with_recovery(prefix: &str, original: &GitErr, recovery: &GitErr) -> GitErr {
    GitErr::msg(format!("{prefix}: {original}; recovery failed: {recovery}"))
}

impl GitTokenStore {
    /// `NewGitTokenStore`. When `branch` is non-empty, clone/pull/push target that branch instead
    /// of the remote default.
    pub fn new(remote: &str, username: &str, password: &str, branch: &str) -> Self {
        Self {
            mu: Mutex::new(()),
            dirs: RwLock::new(Dirs::default()),
            remote: remote.to_string(),
            branch: branch.trim().to_string(),
            username: username.to_string(),
            password: password.to_string(),
            last_gc: Mutex::new(None),
        }
    }

    /// `SetBaseDir`: the auth directory; the repository root is its parent.
    pub fn set_base_dir(&self, dir: &str) {
        let _guard = self.mu.lock();
        let clean = dir.trim();
        let mut dirs = self.dirs.write();
        if clean.is_empty() {
            *dirs = Dirs::default();
            return;
        }
        let abs = std::path::absolute(clean).map(|p| clean_path(&p)).unwrap_or_else(|_| PathBuf::from(clean));
        let repo_dir = match abs.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => abs.clone(),
        };
        dirs.config_dir = repo_dir.join("config").to_string_lossy().into_owned();
        dirs.repo_dir = repo_dir.to_string_lossy().into_owned();
        dirs.base_dir = abs.to_string_lossy().into_owned();
    }

    /// `AuthDir`: the directory used for auth persistence ("" when unset).
    pub fn auth_dir(&self) -> PathBuf {
        PathBuf::from(self.base_dir_snapshot())
    }

    /// `ConfigPath`: the managed config file ("" path when unset).
    pub fn config_path(&self) -> PathBuf {
        let dirs = self.dirs.read();
        if dirs.config_dir.is_empty() {
            return PathBuf::new();
        }
        Path::new(&dirs.config_dir).join("config.yaml")
    }

    /// `EnsureRepository`: clones or opens and syncs the local working tree.
    pub fn ensure_repository(&self) -> Result<(), StoreError> {
        let _guard = self.mu.lock();
        self.ensure_repository_locked().map_err(into_store_err)
    }

    /// `PersistConfig`: commits and pushes the managed config file.
    pub fn persist_config(&self) -> Result<(), StoreError> {
        let _guard = self.mu.lock();
        self.persist_config_locked().map_err(into_store_err)
    }

    fn base_dir_snapshot(&self) -> String {
        self.dirs.read().base_dir.clone()
    }

    fn repo_dir_snapshot(&self) -> String {
        self.dirs.read().repo_dir.clone()
    }

    /// `gitClientOptions`: basic auth, with the user defaulting to `git`.
    fn git_client_options(&self) -> Option<BasicAuth> {
        if self.username.is_empty() && self.password.is_empty() {
            return None;
        }
        let user = if self.username.is_empty() { "git" } else { &self.username };
        Some(BasicAuth { user: user.to_string(), pass: self.password.clone() })
    }

    /// `relativeToRepo`: `path` relative to the repository root, rejecting paths outside it.
    fn relative_to_repo(&self, path: &Path) -> R<String> {
        let repo_dir = self.repo_dir_snapshot();
        if repo_dir.is_empty() {
            return Err(GitErr::msg("git token store: repository path not configured"));
        }
        let abs_repo = std::path::absolute(&repo_dir)
            .map(|p| clean_path(&p))
            .map_err(|e| GitErr::msg(format!("git token store: resolve repository path: {e}")))?;
        let abs_path = std::path::absolute(path)
            .map(|p| clean_path(&p))
            .map_err(|e| GitErr::msg(format!("git token store: resolve path: {e}")))?;
        let rel = rel_path(&abs_repo, &abs_path)
            .ok_or_else(|| GitErr::msg("git token store: relative path: no common root"))?;
        let rel = rel.to_string_lossy().into_owned();
        if rel == ".." || rel.starts_with(&format!("..{}", std::path::MAIN_SEPARATOR)) {
            return Err(GitErr::msg("git token store: path outside repository"));
        }
        Ok(rel)
    }

    fn ensure_repository_locked(&self) -> R<()> {
        let (repo_dir, base_dir, config_dir) = {
            let mut dirs = self.dirs.write();
            if self.remote.is_empty() {
                return Err(GitErr::msg("git token store: remote not configured"));
            }
            if dirs.base_dir.is_empty() {
                return Err(GitErr::msg("git token store: base directory not configured"));
            }
            if dirs.repo_dir.is_empty() {
                let base = Path::new(&dirs.base_dir);
                let repo = match base.parent() {
                    Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                    _ => base.to_path_buf(),
                };
                dirs.repo_dir = repo.to_string_lossy().into_owned();
            }
            if dirs.config_dir.is_empty() {
                dirs.config_dir = Path::new(&dirs.repo_dir).join("config").to_string_lossy().into_owned();
            }
            (
                PathBuf::from(&dirs.repo_dir),
                PathBuf::from(&dirs.base_dir),
                PathBuf::from(&dirs.config_dir),
            )
        };
        let auth = self.git_client_options();
        let auth = auth.as_ref();
        let ctx = RecoveryCtx { remote: &self.remote, branch: &self.branch, auth };

        let mut init_paths: Vec<&str> = Vec::new();
        match fs::metadata(repo_dir.join(".git")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                init_paths = self.clone_or_init(&repo_dir, auth)?;
            }
            Err(e) => return Err(GitErr::msg(format!("git token store: stat repo: {e}"))),
            Ok(_) => self.sync_existing_repository(&ctx, &repo_dir, auth)?,
        }
        disable_git_commit_signing(&repo_dir)?;
        mkdir_all_private(&base_dir).map_err(|e| GitErr::msg(format!("git token store: create auth dir: {e}")))?;
        mkdir_all_private(&config_dir).map_err(|e| GitErr::msg(format!("git token store: create config dir: {e}")))?;
        if !init_paths.is_empty() {
            let paths: Vec<String> = init_paths.iter().map(|p| p.to_string()).collect();
            self.commit_and_push_with_options("Initialize git token store", true, &paths)?;
        }
        Ok(())
    }

    /// Fresh checkout: clone the remote, or when it is empty init a repository with placeholder
    /// files that the caller commits and pushes. Returns the paths to commit.
    fn clone_or_init(&self, repo_dir: &Path, auth: Option<&BasicAuth>) -> R<Vec<&'static str>> {
        mkdir_all_private(repo_dir).map_err(|e| GitErr::msg(format!("git token store: create repo dir: {e}")))?;
        match clone_into(repo_dir, &self.remote, &self.branch, auth) {
            Ok(_) => Ok(Vec::new()),
            Err(e) if e.is(EMPTY_REMOTE) => {
                let _ = fs::remove_dir_all(repo_dir.join(".git"));
                let repo = init_repo(repo_dir).map_err(|e| e.wrap("git token store: init empty repo"))?;
                if !self.branch.is_empty() {
                    repo.set_head(&format!("refs/heads/{}", self.branch)).map_err(|e| {
                        GitErr::from(e).wrap(format!("git token store: set head to branch {}", self.branch))
                    })?;
                }
                if repo.find_remote("origin").is_err() {
                    repo.remote("origin", &self.remote)
                        .map_err(|e| GitErr::from(e).wrap("git token store: configure remote"))?;
                }
                let auth_dir = repo_dir.join("auths");
                let config_dir = repo_dir.join("config");
                mkdir_all_private(&auth_dir)
                    .map_err(|e| GitErr::msg(format!("git token store: create auth dir: {e}")))?;
                mkdir_all_private(&config_dir)
                    .map_err(|e| GitErr::msg(format!("git token store: create config dir: {e}")))?;
                ensure_empty_file(&auth_dir.join(".gitkeep"))
                    .map_err(|e| GitErr::msg(format!("git token store: create auth placeholder: {e}")))?;
                ensure_empty_file(&config_dir.join(".gitkeep"))
                    .map_err(|e| GitErr::msg(format!("git token store: create config placeholder: {e}")))?;
                Ok(vec!["auths/.gitkeep", "config/.gitkeep"])
            }
            Err(e) => Err(e.wrap("git token store: clone remote")),
        }
    }

    /// Existing checkout: verify, follow the right branch, pull and repair.
    fn sync_existing_repository(&self, ctx: &RecoveryCtx<'_>, repo_dir: &Path, auth: Option<&BasicAuth>) -> R<()> {
        let mut repo =
            Repository::open(repo_dir).map_err(|e| GitErr::from(e).wrap("git token store: open repo"))?;
        let reopen = || {
            Repository::open(repo_dir).map_err(|e| GitErr::from(e).wrap("git token store: open recovered repo"))
        };

        if let Err(verify) = verify_repository_head(&repo) {
            let prefix = "git token store: verify repository before pull";
            if !verify.is_corruption() {
                return Err(verify.wrap(prefix));
            }
            if let Err(rec) = recover_repository(ctx, repo_dir, Some(repo), None, None, &os_rename) {
                return Err(failed_with_recovery(prefix, &verify, &rec));
            }
            repo = reopen()?;
        }

        if !self.branch.is_empty() {
            self.checkout_configured_branch(&repo, auth)?;
        } else if let Err(e) = checkout_remote_default_branch(&repo, auth)
            && !should_fallback_to_current_branch(&repo, &e)
        {
            return Err(e.wrap("git token store: checkout remote default"));
        }

        let pre_head = head_ref(&repo).map_err(|e| e.wrap("git token store: get head before pull"))?;
        let pre_tree = match &pre_head {
            Some(h) => Some(
                tree_snapshot(&repo, h.oid).map_err(|e| e.wrap("git token store: inspect head before pull"))?,
            ),
            None => None,
        };
        let dirty = worktree_dirty_paths(&repo)
            .map_err(|e| e.wrap("git token store: inspect worktree before pull"))?;

        let mut repo = Some(repo);
        let mut recovered = false;
        // Runs recovery from the pre-pull baseline, consuming the open repository handle.
        let recover = |repo: &mut Option<Repository>| {
            recover_repository(ctx, repo_dir, repo.take(), pre_tree.clone(), Some(dirty.clone()), &os_rename)
        };
        let pulled = match repo.as_ref() {
            Some(r) => pull(r, auth, &self.branch),
            None => Ok(()),
        };
        if let Err(pull_err) = pulled {
            if pull_err.is(UP_TO_DATE) {
                let reset = repo.as_ref().map(reset_index_to_head).unwrap_or(Ok(()));
                if let Err(reset_err) = reset {
                    let prefix = "git token store: repair index after up-to-date pull";
                    if !reset_err.is_corruption() {
                        return Err(reset_err.wrap(prefix));
                    }
                    if let Err(rec) = recover(&mut repo) {
                        return Err(failed_with_recovery(prefix, &reset_err, &rec));
                    }
                    recovered = true;
                }
            } else if pull_err.is(UNSTAGED_CHANGES) || pull_err.is(NON_FAST_FORWARD) {
                let Some(base) = &pre_head else {
                    return Err(GitErr::msg("git token store: reconcile pull without a local branch"));
                };
                let reconciled = match repo.as_ref() {
                    Some(r) => reconcile_remote_worktree(r, repo_dir, base, &dirty),
                    None => Ok(()),
                };
                if let Err(rec_err) = reconciled {
                    let prefix = "git token store: reconcile remote changes";
                    if !rec_err.is_corruption() {
                        return Err(rec_err.wrap(prefix));
                    }
                    if let Err(rec) = recover(&mut repo) {
                        return Err(failed_with_recovery(prefix, &rec_err, &rec));
                    }
                    recovered = true;
                }
            } else if pull_err.is(AUTH_REQUIRED) || pull_err.is(EMPTY_REMOTE) {
                // Ignore authentication prompts and empty remote references on initial sync.
            } else if pull_err.is(REF_NOT_FOUND) {
                if !self.branch.is_empty() {
                    return Err(pull_err.wrap("git token store: pull"));
                }
                // Ignore missing references only when following the remote default branch.
            } else if pull_err.is_corruption() {
                if let Err(rec) = recover(&mut repo) {
                    return Err(failed_with_recovery("git token store: pull", &pull_err, &rec));
                }
                recovered = true;
            } else {
                return Err(pull_err.wrap("git token store: pull"));
            }
        }

        if !recovered && let Some(r) = repo.as_ref() {
            if let Err(verify) = verify_repository_head(r) {
                let prefix = "git token store: verify repository after pull";
                if !verify.is_corruption() {
                    return Err(verify.wrap(prefix));
                }
                if let Err(rec) = recover(&mut repo) {
                    return Err(failed_with_recovery(prefix, &verify, &rec));
                }
                recovered = true;
            }
        }
        if !recovered && let Some(r) = repo.as_ref() {
            restore_missing_tracked_files(r, repo_dir)
                .map_err(|e| e.wrap("git token store: restore tracked worktree files"))?;
        }
        Ok(())
    }

    /// `checkoutConfiguredBranch`: switch to `branch`, creating it from `origin/<branch>` when it
    /// does not exist locally.
    fn checkout_configured_branch(&self, repo: &Repository, auth: Option<&BasicAuth>) -> R<()> {
        let branch_ref = format!("refs/heads/{}", self.branch);
        match head_ref(repo) {
            Ok(Some(h)) if h.name == branch_ref => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(e.wrap("git token store: get head")),
        }
        let checkout_err = match checkout_branch(repo, &branch_ref) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        match repo.find_reference(&branch_ref) {
            Ok(_) => Err(checkout_err.wrap(format!("git token store: checkout branch {}", self.branch))),
            Err(e) => {
                let e = GitErr::from(e);
                if !e.is(REF_NOT_FOUND) {
                    return Err(e.wrap(format!("git token store: inspect branch {}", self.branch)));
                }
                self.checkout_configured_remote_tracking_branch(repo, &branch_ref, auth)
                    .map_err(|e| e.wrap(format!("git token store: checkout branch {}", self.branch)))
            }
        }
    }

    fn checkout_configured_remote_tracking_branch(
        &self,
        repo: &Repository,
        branch_ref: &str,
        auth: Option<&BasicAuth>,
    ) -> R<()> {
        let remote_ref = format!("refs/remotes/origin/{}", self.branch);
        let oid = match ref_oid(repo, &remote_ref) {
            Err(e) if e.is(REF_NOT_FOUND) => {
                fetch_origin(repo, auth).map_err(|e| e.wrap("sync remote refs"))?;
                ref_oid(repo, &remote_ref)?
            }
            other => other?,
        };
        create_and_checkout_branch(repo, branch_ref, oid)?;
        set_branch_tracking(repo, &self.branch, branch_ref)
            .map_err(|e| GitErr::from(e).wrap("git token store: set branch config"))
    }

    fn persist_config_locked(&self) -> R<()> {
        self.ensure_repository_locked()?;
        let config_path = self.config_path();
        if config_path.as_os_str().is_empty() {
            return Err(GitErr::msg("git token store: config path not configured"));
        }
        match fs::metadata(&config_path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(GitErr::msg(format!("git token store: stat config: {e}"))),
        }
        let rel = self.relative_to_repo(&config_path)?;
        self.commit_and_push_with_options("Update config", false, &[rel])
    }

    /// `commitAndPushLocked` / `commitAndPushInitialLocked`: stage exactly `rel_paths`, commit,
    /// squash the branch to that single commit and push. `allow_missing_remote` permits a
    /// branch-creating push when no remote-tracking ref exists yet (initial setup).
    fn commit_and_push_with_options(&self, message: &str, allow_missing_remote: bool, rel_paths: &[String]) -> R<()> {
        let repo_dir = self.repo_dir_snapshot();
        if repo_dir.is_empty() {
            return Err(GitErr::msg("git token store: repository path not configured"));
        }
        let repo = Repository::open(&repo_dir).map_err(|e| GitErr::from(e).wrap("git token store: open repo"))?;
        let managed =
            normalize_managed_paths(rel_paths).map_err(|e| e.wrap("git token store: validate commit paths"))?;
        if managed.is_empty() {
            return Ok(());
        }

        let base_ref = head_ref(&repo).map_err(|e| e.wrap("git token store: get base head"))?;
        if base_ref.is_some() {
            reset_index_to_head(&repo).map_err(|e| e.wrap("git token store: reset index before commit"))?;
        }

        if !stage_managed_paths(&repo, Path::new(&repo_dir), &managed)? {
            return Ok(());
        }

        let message = if message.trim().is_empty() { "Update auth store" } else { message };
        let signature = Signature::now("CLIProxyAPI", "cliproxy@local")
            .map_err(|e| GitErr::from(e).wrap("git token store: commit"))?;
        let Some(commit_oid) = commit_index(&repo, message, &signature)? else {
            return Ok(());
        };
        if let Some(base) = &base_ref
            && let Err(validate) = validate_managed_tree_changes(&repo, base.oid, commit_oid, &managed)
        {
            let validate = validate.wrap("git token store: validate commit tree");
            return Err(match restore_head_and_index(&repo, base) {
                Ok(()) => validate,
                Err(e) => validate.join(e.wrap("git token store: restore head after rejected commit")),
            });
        }
        let committed = head_ref(&repo)
            .map_err(|e| e.wrap("git token store: get committed head"))?
            .ok_or_else(|| GitErr::kind(REF_NOT_FOUND).wrap("git token store: get committed head"))?;
        rewrite_head_as_single_commit(&repo, &committed.name, commit_oid, message, &signature)?;
        if let Err(push) = self.push_repository_locked(&repo, Path::new(&repo_dir), allow_missing_remote) {
            let Some(base) = &base_ref else { return Err(push) };
            return Err(match restore_head_and_index(&repo, base) {
                Ok(()) => push,
                Err(e) => push.join(e.wrap("git token store: restore head after rejected push")),
            });
        }
        Ok(())
    }

    fn push_repository_locked(&self, repo: &Repository, repo_dir: &Path, allow_missing_remote: bool) -> R<()> {
        let Some(head) = head_ref(repo).map_err(|e| e.wrap("git token store: get head for push"))? else {
            return Ok(());
        };
        if !head.is_branch() {
            return Err(GitErr::msg(format!("git token store: head {} is not a branch", head.name)));
        }
        let auth = self.git_client_options();
        ops::push_branch(repo, auth.as_ref(), &head, allow_missing_remote)?;
        self.maybe_run_gc(repo_dir);
        Ok(())
    }

    /// Best-effort housekeeping at most every `GC_INTERVAL`, after a successful push.
    fn maybe_run_gc(&self, repo_dir: &Path) {
        let now = Instant::now();
        {
            let mut last = self.last_gc.lock();
            if last.is_some_and(|t| now.duration_since(t) < GC_INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        if let Err(e) = gc::run(repo_dir) {
            tracing::warn!("git token store: gc: {e}");
        }
    }

    /// Forces the next push to run GC (tests).
    #[cfg(test)]
    fn reset_gc_timer(&self) {
        *self.last_gc.lock() = None;
    }

    /// `PersistAuthFiles`: commits and pushes watcher-reported paths.
    fn persist_auth_files_locked(&self, message: &str, paths: &[String]) -> R<()> {
        let mut filtered = Vec::with_capacity(paths.len());
        for p in paths {
            let trimmed = p.trim();
            if trimmed.is_empty() {
                continue;
            }
            filtered.push(self.relative_to_repo(Path::new(trimmed))?);
        }
        if filtered.is_empty() {
            return Ok(());
        }
        let message = if message.trim().is_empty() { "Sync watcher updates" } else { message };

        // Inspect watcher removals before ensure_repository restores missing tracked files so an
        // unexpected filesystem event remains distinguishable from Delete.
        match fs::metadata(Path::new(&self.repo_dir_snapshot()).join(".git")) {
            Ok(_) => {
                if self.guard_watcher_auth_removal_locked(message, &filtered)? {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(GitErr::msg(format!(
                    "git token store: stat repository before watcher removal guard: {e}"
                )));
            }
        }
        self.ensure_repository_locked()?;
        if self.guard_watcher_auth_removal_locked(message, &filtered)? {
            return Ok(());
        }
        self.commit_and_push_with_options(message, false, &filtered)
    }

    /// `guardWatcherAuthRemovalLocked`: for `Remove auth ...` messages, refuses to drop a tracked
    /// auth the watcher saw vanish. `Ok(true)` means the event was handled (nothing to commit).
    fn guard_watcher_auth_removal_locked(&self, message: &str, rel_paths: &[String]) -> R<bool> {
        if !message.trim().starts_with("Remove auth ") {
            return Ok(false);
        }
        let repo_dir = self.repo_dir_snapshot();
        if repo_dir.is_empty() {
            return Err(GitErr::msg("git token store: repository path not configured"));
        }
        let repo = Repository::open(&repo_dir)
            .map_err(|e| GitErr::from(e).wrap("git token store: open repo for watcher removal guard"))?;
        let Some(head) = head_ref(&repo).map_err(|e| e.wrap("git token store: inspect head for watcher removal guard"))?
        else {
            return Ok(true);
        };
        let tree = repo
            .find_commit(head.oid)
            .map_err(|e| GitErr::from(e).wrap("git token store: inspect commit for watcher removal guard"))?
            .tree()
            .map_err(|e| GitErr::from(e).wrap("git token store: inspect tree for watcher removal guard"))?;

        let mut has_existing_path = false;
        for rel in rel_paths {
            let clean = clean_path(Path::new(rel)).to_string_lossy().replace('\\', "/");
            match fs::metadata(Path::new(&repo_dir).join(&clean)) {
                Ok(_) => {
                    has_existing_path = true;
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(GitErr::msg(format!(
                        "git token store: stat watcher removal path {clean}: {e}"
                    )));
                }
            }
            match tree.get_path(Path::new(&clean)) {
                Ok(entry) if entry.kind() == Some(git2::ObjectType::Blob) => {
                    return Err(GitErr::msg(format!(
                        "git token store: refusing watcher-originated removal of tracked auth {clean}; use an explicit delete"
                    )));
                }
                Ok(_) => {}
                Err(e) if e.code() == git2::ErrorCode::NotFound => {}
                Err(e) => {
                    return Err(GitErr::from(e)
                        .wrap(format!("git token store: inspect watcher removal path {clean}")));
                }
            }
        }
        if has_existing_path {
            return Ok(false);
        }
        // An explicit Delete already removed the path from HEAD; the subsequent watcher event is
        // redundant and safe to ignore.
        Ok(true)
    }

    fn resolve_auth_path(&self, auth: &Auth) -> Result<PathBuf, StoreError> {
        let p = auth.attr(ATTRIBUTE_PATH);
        let p = p.trim();
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
        let file_name = auth.file_name.trim();
        if !file_name.is_empty() {
            if Path::new(file_name).is_absolute() {
                return Ok(PathBuf::from(file_name));
            }
            let dir = self.base_dir_snapshot();
            if !dir.is_empty() {
                return Ok(Path::new(&dir).join(file_name));
            }
            return Ok(PathBuf::from(file_name));
        }
        if auth.id.is_empty() {
            return Err(backend_err("auth filestore: missing id"));
        }
        if Path::new(&auth.id).is_absolute() {
            return Ok(PathBuf::from(&auth.id));
        }
        let dir = self.base_dir_snapshot();
        if dir.is_empty() {
            return Err(backend_err("auth filestore: directory not configured"));
        }
        Ok(Path::new(&dir).join(&auth.id))
    }

    fn resolve_delete_path(&self, id: &str) -> Result<PathBuf, StoreError> {
        if id.contains(std::path::MAIN_SEPARATOR) || Path::new(id).is_absolute() {
            return Ok(PathBuf::from(id));
        }
        let dir = self.base_dir_snapshot();
        if dir.is_empty() {
            return Err(backend_err("auth filestore: directory not configured"));
        }
        Ok(Path::new(&dir).join(id))
    }

    /// `readAuthFile`: `Ok(None)` for empty files.
    fn read_auth_file(&self, path: &Path, base_dir: &Path) -> Result<Option<Auth>, String> {
        let data = fs::read(path).map_err(|e| format!("read file: {e}"))?;
        if data.is_empty() {
            return Ok(None);
        }
        let mut metadata: Metadata = match serde_json::from_slice::<Value>(&data) {
            Ok(Value::Object(m)) => m,
            Ok(_) => return Err("unmarshal auth json: not an object".into()),
            Err(e) => return Err(format!("unmarshal auth json: {e}")),
        };
        normalize_credential_metadata(&mut metadata);
        let mut probe = Auth::default();
        probe.metadata = metadata.clone();
        validate_auth_weight(&probe)?;
        let mut provider = str_value(&metadata, "type").to_string();
        if provider.is_empty() {
            provider = "unknown".into();
        }
        let mtime: chrono::DateTime<chrono::Utc> = fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|e| format!("stat file: {e}"))?
            .into();
        let id = id_for(path, base_dir);
        let mut auth = Auth::default();
        auth.id = id.clone();
        auth.provider = provider;
        auth.file_name = id;
        auth.label = label_for(&metadata);
        auth.status = Status::Active;
        auth.created_at = Some(mtime);
        auth.updated_at = Some(mtime);
        auth.attributes.insert(ATTRIBUTE_PATH.into(), path.to_string_lossy().into_owned());
        auth.attributes.insert(ATTRIBUTE_SOURCE_BACKEND.into(), AUTH_SOURCE_GIT.into());
        let email = str_value(&metadata, "email");
        if !email.is_empty() {
            auth.attributes.insert("email".into(), email.to_string());
        }
        let disabled = metadata.get("disabled").and_then(Value::as_bool).unwrap_or(false);
        auth.metadata = metadata;
        apply_custom_headers_from_metadata(&mut auth);
        if disabled {
            auth.disabled = true;
            auth.status = Status::Disabled;
        }
        Ok(Some(auth))
    }
}

fn str_value<'a>(metadata: &'a Metadata, key: &str) -> &'a str {
    metadata.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `labelFor`: label, else email, else project_id (untrimmed, like Go's git store).
fn label_for(metadata: &Metadata) -> String {
    ["label", "email", "project_id"]
        .iter()
        .map(|k| str_value(metadata, k))
        .find(|v| !v.is_empty())
        .unwrap_or("")
        .to_string()
}

/// `idFor`: path relative to the base dir, else the full path.
fn id_for(path: &Path, base_dir: &Path) -> String {
    if base_dir.as_os_str().is_empty() {
        return path.to_string_lossy().into_owned();
    }
    rel_path(base_dir, path).unwrap_or_else(|| path.to_path_buf()).to_string_lossy().into_owned()
}

/// Recursive walk in lexical order that does not follow symlinks (Go `filepath.WalkDir`).
fn walk_json_files(dir: &Path, visit: &mut dyn FnMut(&Path)) -> std::io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_json_files(&path, visit)?;
        } else if entry.file_name().to_string_lossy().to_lowercase().ends_with(".json") {
            visit(&path);
        }
    }
    Ok(())
}

impl Store for GitTokenStore {
    /// `List`: every auth JSON under the auth dir after syncing the repository, skipping files
    /// that are unreadable, empty or invalid.
    fn list(&self) -> Result<Vec<Auth>, StoreError> {
        let _guard = self.mu.lock();
        self.ensure_repository_locked().map_err(into_store_err)?;
        let dir = self.base_dir_snapshot();
        if dir.is_empty() {
            return Err(backend_err("auth filestore: directory not configured"));
        }
        let base = PathBuf::from(&dir);
        let mut entries = Vec::new();
        walk_json_files(&base, &mut |path| {
            if let Ok(Some(auth)) = self.read_auth_file(path, &base) {
                entries.push(auth);
            }
        })
        .map_err(|e| backend_err(e.to_string()))?;
        Ok(entries)
    }

    /// `Save`: writes the credential file, then commits and pushes it.
    fn save(&self, auth: &mut Auth, opts: SaveOptions) -> Result<Option<PathBuf>, StoreError> {
        normalize_credential_metadata(&mut auth.metadata);
        validate_auth_weight(auth).map_err(|e| backend_err(format!("auth filestore: {e}")))?;

        let _guard = self.mu.lock();
        let path = self.resolve_auth_path(auth)?;
        if path.as_os_str().is_empty() {
            return Err(backend_err(format!("auth filestore: missing file path attribute for {}", auth.id)));
        }

        // Runtime updates must not recreate a disabled credential whose source file was
        // deliberately removed. Login and migration callers mark the save when creating a
        // missing disabled credential is intentional.
        if auth.disabled && !opts.creation_intent && !path.exists() {
            return Ok(None);
        }

        self.ensure_repository_locked().map_err(into_store_err)?;
        let rel_path = self.relative_to_repo(&path).map_err(into_store_err)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            mkdir_all_private(dir).map_err(|e| backend_err(format!("auth filestore: create dir failed: {e}")))?;
        }

        if let Some(storage) = auth.storage.clone() {
            auth.metadata.insert("disabled".into(), Value::Bool(auth.disabled));
            storage.save_to_file(&path, &auth.metadata)?;
        } else if !auth.metadata.is_empty() {
            auth.metadata.insert("disabled".into(), Value::Bool(auth.disabled));
            write_metadata_only(&path, &auth.metadata)?;
        } else {
            return Err(backend_err(format!("auth filestore: nothing to persist for {}", auth.id)));
        }

        auth.attributes.insert(ATTRIBUTE_PATH.into(), path.to_string_lossy().into_owned());
        auth.attributes.insert(ATTRIBUTE_SOURCE_BACKEND.into(), AUTH_SOURCE_GIT.into());
        if auth.file_name.trim().is_empty() {
            auth.file_name = auth.id.clone();
        }

        let message_id = if auth.id.trim().is_empty() {
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            auth.id.clone()
        };
        self.commit_and_push_with_options(&format!("Update auth {}", message_id.trim()), false, &[rel_path])
            .map_err(into_store_err)?;
        Ok(Some(path))
    }

    /// `Delete`: removes the auth file and commits the removal.
    fn delete(&self, id: &str) -> Result<(), StoreError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(backend_err("auth filestore: id is empty"));
        }
        let _guard = self.mu.lock();
        let path = self.resolve_delete_path(id)?;
        self.ensure_repository_locked().map_err(into_store_err)?;
        let rel = self.relative_to_repo(&path).map_err(into_store_err)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(backend_err(format!("auth filestore: delete failed: {e}"))),
        }
        self.commit_and_push_with_options(&format!("Delete auth {id}"), false, &[rel])
            .map_err(into_store_err)
    }

    fn base_dir(&self) -> Option<PathBuf> {
        let d = self.base_dir_snapshot();
        if d.is_empty() { None } else { Some(PathBuf::from(d)) }
    }
}

impl StorePersister for GitTokenStore {
    fn persist_config(&self) -> Result<(), String> {
        GitTokenStore::persist_config(self).map_err(|e| e.to_string())
    }

    /// No-ops when there are no paths.
    fn persist_auth_files(&self, message: &str, paths: &[String]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let _guard = self.mu.lock();
        self.persist_auth_files_locked(message, paths).map_err(GitErr::into_message)
    }
}

/// Metadata-only write: temp file plus rename, skipped when semantically equal to the file.
fn write_metadata_only(path: &Path, metadata: &Metadata) -> Result<(), StoreError> {
    let raw = marshal_compact(&Value::Object(metadata.clone()))
        .map_err(|e| backend_err(format!("auth filestore: marshal metadata failed: {e}")))?;
    match fs::read(path) {
        Ok(existing) if json_equal(&existing, raw.as_bytes()) => return Ok(()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(backend_err(format!("auth filestore: read existing failed: {e}"))),
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    write_file_private(&tmp, raw.as_bytes())
        .map_err(|e| backend_err(format!("auth filestore: write temp failed: {e}")))?;
    fs::rename(&tmp, path).map_err(|e| backend_err(format!("auth filestore: rename failed: {e}")))
}

fn ensure_empty_file(path: &Path) -> std::io::Result<()> {
    match fs::metadata(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_file_private(path, b""),
        Err(e) => Err(e),
    }
}

/// `disableGitCommitSigning`: `commit.gpgsign = false` in the repository config.
fn disable_git_commit_signing(repo_dir: &Path) -> R<()> {
    let repo = Repository::open(repo_dir)
        .map_err(|e| GitErr::from(e).wrap("git token store: open repository config"))?;
    let mut cfg = repo
        .config()
        .and_then(|c| c.open_level(git2::ConfigLevel::Local))
        .map_err(|e| GitErr::from(e).wrap("git token store: get repository config"))?;
    cfg.set_bool("commit.gpgsign", false)
        .map_err(|e| GitErr::from(e).wrap("git token store: disable commit signing"))
}

/// `ref_oid`: the commit a (possibly symbolic) reference resolves to.
fn ref_oid(repo: &Repository, name: &str) -> R<git2::Oid> {
    let r = repo.find_reference(name)?.resolve()?;
    r.target().ok_or_else(|| GitErr::msg(format!("reference {name} has no target")))
}

/// `shouldFallbackToCurrentBranch`: auth or empty-remote failures keep the current branch when
/// the repository already has a commit checked out.
fn should_fallback_to_current_branch(repo: &Repository, err: &GitErr) -> bool {
    (err.is(AUTH_REQUIRED) || err.is(EMPTY_REMOTE)) && matches!(head_ref(repo), Ok(Some(_)))
}

/// `checkoutRemoteDefaultBranch`: follow the branch the remote HEAD points at, creating a
/// tracking branch when it does not exist locally.
fn checkout_remote_default_branch(repo: &Repository, auth: Option<&BasicAuth>) -> R<()> {
    let resolved = resolve_remote_default_branch(repo, auth)?;
    let branch_ref = resolved.name;
    if let Ok(Some(h)) = head_ref(repo)
        && h.name == branch_ref
    {
        return Ok(());
    }
    if repo.find_reference(&branch_ref).is_ok() {
        return checkout_branch(repo, &branch_ref).map_err(|e| e.wrap(format!("checkout branch {branch_ref}")));
    }
    let short = branch_ref.strip_prefix("refs/heads/").unwrap_or(&branch_ref).to_string();
    let remote_ref = format!("refs/remotes/origin/{short}");
    let mut oid = resolved.oid;
    match ref_oid(repo, &remote_ref) {
        Ok(o) => oid = Some(o),
        Err(e) if e.is(REF_NOT_FOUND) => {}
        Err(e) => return Err(e.wrap(format!("checkout remote default: remote ref {remote_ref}"))),
    }
    let Some(oid) = oid else {
        return Err(GitErr::msg(format!("checkout remote default: remote ref {remote_ref} not found")));
    };
    create_and_checkout_branch(repo, &branch_ref, oid).map_err(|e| e.wrap(format!("checkout create branch {branch_ref}")))?;
    set_branch_tracking(repo, &short, &branch_ref).map_err(|e| GitErr::from(e).wrap("git token store: set branch config"))
}

/// `normalizeManagedPaths`: trimmed, cleaned, de-duplicated repository-relative paths.
fn normalize_managed_paths(paths: &[String]) -> R<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(paths.len());
    for path in paths {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        let clean = clean_path(Path::new(trimmed)).to_string_lossy().replace('\\', "/");
        if clean == "." || clean == ".." || clean.starts_with("../") || Path::new(trimmed).is_absolute() {
            return Err(GitErr::msg(format!("path {path:?} is not a repository-relative file")));
        }
        if !out.contains(&clean) {
            out.push(clean);
        }
    }
    Ok(out)
}

/// Adds existing managed paths to the index and removes vanished tracked ones. Returns whether
/// anything was staged.
fn stage_managed_paths(repo: &Repository, repo_dir: &Path, managed: &[String]) -> R<bool> {
    let mut index = repo.index().map_err(|e| GitErr::from(e).wrap("git token store: open index"))?;
    let mut added = false;
    for rel in managed {
        match fs::symlink_metadata(repo_dir.join(rel)) {
            Ok(md) => {
                let res = if md.is_dir() {
                    index.add_all([rel.as_str()], git2::IndexAddOption::DEFAULT, None)
                } else {
                    index.add_path(Path::new(rel))
                };
                res.map_err(|e| GitErr::from(e).wrap(format!("git token store: add {rel}")))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if index.get_path(Path::new(rel), 0).is_none() {
                    continue;
                }
                index
                    .remove_path(Path::new(rel))
                    .map_err(|e| GitErr::from(e).wrap(format!("git token store: remove {rel}")))?;
            }
            Err(e) => return Err(GitErr::msg(format!("git token store: add {rel}: {e}"))),
        }
        added = true;
    }
    index.write().map_err(|e| GitErr::from(e).wrap("git token store: write index"))?;
    Ok(added)
}

/// Commits the index on HEAD. `None` when the tree equals the parent's (go-git `ErrEmptyCommit`).
fn commit_index(repo: &Repository, message: &str, signature: &Signature<'_>) -> R<Option<git2::Oid>> {
    let wrap = |e: git2::Error| GitErr::from(e).wrap("git token store: commit");
    let mut index = repo.index().map_err(wrap)?;
    let parent = match head_ref(repo).map_err(|e| e.wrap("git token store: commit"))? {
        Some(h) => Some(repo.find_commit(h.oid).map_err(wrap)?),
        None => None,
    };
    if parent.is_none() && index.is_empty() {
        return Ok(None);
    }
    let tree_oid = index.write_tree().map_err(wrap)?;
    if parent.as_ref().is_some_and(|p| p.tree_id() == tree_oid) {
        return Ok(None);
    }
    let tree = repo.find_tree(tree_oid).map_err(wrap)?;
    let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
    let oid = repo.commit(Some("HEAD"), signature, signature, message, &tree, &parents).map_err(wrap)?;
    Ok(Some(oid))
}

/// `validateManagedTreeChanges`: the commit may only change the requested paths.
fn validate_managed_tree_changes(repo: &Repository, base: git2::Oid, commit: git2::Oid, managed: &[String]) -> R<()> {
    let base_snap = tree_snapshot(repo, base).map_err(|e| e.wrap("inspect base tree"))?;
    let cand_snap = tree_snapshot(repo, commit).map_err(|e| e.wrap("inspect candidate tree"))?;
    for changed in ops::changed_tree_paths(&base_snap, &cand_snap) {
        if !is_managed_tree_path(&changed, managed) {
            return Err(GitErr::msg(format!("unexpected indexed change outside requested paths: {changed}")));
        }
    }
    Ok(())
}

fn is_managed_tree_path(path: &str, managed: &[String]) -> bool {
    let clean = clean_path(Path::new(path)).to_string_lossy().replace('\\', "/");
    managed
        .iter()
        .any(|m| clean == *m || clean.strip_prefix(m.as_str()).is_some_and(|r| r.starts_with('/')))
}

/// `rewriteHeadAsSingleCommit`: replaces the branch tip with a parentless commit of the same
/// tree, leaving history squashed.
fn rewrite_head_as_single_commit(
    repo: &Repository,
    branch: &str,
    commit: git2::Oid,
    message: &str,
    signature: &Signature<'_>,
) -> R<()> {
    let tree = repo
        .find_commit(commit)
        .and_then(|c| c.tree())
        .map_err(|e| GitErr::from(e).wrap("git token store: inspect head commit"))?;
    let squashed = repo
        .commit(None, signature, signature, message, &tree, &[])
        .map_err(|e| GitErr::from(e).wrap("git token store: write squashed commit"))?;
    repo.reference(branch, squashed, true, "gitstore: squash history")
        .map_err(|e| GitErr::from(e).wrap("git token store: update branch reference"))?;
    Ok(())
}

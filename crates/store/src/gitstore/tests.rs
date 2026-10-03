//! Port of internal/store/gitstore_test.go. Remotes are local bare repositories built with git2.
//!
//! Not ported: tests that need Go-only injection points. `TestRecoverRepositoryCloseFailures*`
//! and `...RecoveredCloseFailure...` inject failing repository `Close` functions (libgit2 handles
//! close on drop, nothing to fail); `TestGitTokenStoreDisabledLoginReachesTokenStorage` and
//! `TestGitTokenStoreSaveRetryAfterLeaseConflict...` use a callback `TokenStorage` that runs
//! code mid-save, which the closed `TokenStorage` enum cannot express (their lease-rejection and
//! retry halves are covered by `rejects_stale_force_push`).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use cpa_auth::store::{SaveOptions, Store};
use cpa_auth::types::ATTRIBUTE_PATH;
use cpa_runtime::service::StorePersister;
use git2::{Oid, Repository, RepositoryInitOptions, Signature, Time};
use tempfile::TempDir;

use super::ops::verify_repository_head;
use super::recovery::{RecoveryCtx, install_recovered_git_directory, recover_repository};
use super::*;

const MARKER: &str = "branch.txt";

fn sig() -> Signature<'static> {
    Signature::new("CLIProxyAPI", "cliproxy@local", &Time::new(1_711_929_600, 0)).unwrap()
}

/// Commits `branch.txt` on `branch` directly in the bare remote, keeping other tree entries.
/// A new branch starts from `from` (the default branch tip).
fn commit_marker(repo: &Repository, branch: &str, contents: &str, message: &str, from: Option<Oid>) -> Oid {
    let refname = format!("refs/heads/{branch}");
    let parent = repo.find_reference(&refname).ok().and_then(|r| r.target()).or(from);
    let parent = parent.map(|p| repo.find_commit(p).unwrap());
    let base_tree = parent.as_ref().map(|p| p.tree().unwrap());
    let mut tb = repo.treebuilder(base_tree.as_ref()).unwrap();
    tb.insert(MARKER, repo.blob(contents.as_bytes()).unwrap(), 0o100644).unwrap();
    let tree = repo.find_tree(tb.write().unwrap()).unwrap();
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(Some(&refname), &sig(), &sig(), message, &tree, &parents).unwrap()
}

/// `setupGitRemoteRepository`: bare `remote.git` with the given branches, HEAD on the default.
fn setup_remote(root: &Path, default_branch: &str, branches: &[(&str, &str)]) -> PathBuf {
    let dir = root.join("remote.git");
    let mut opts = RepositoryInitOptions::new();
    opts.bare(true).initial_head(default_branch);
    let repo = Repository::init_opts(&dir, &opts).unwrap();
    let default_contents = branches.iter().find(|(n, _)| *n == default_branch).unwrap().1;
    let tip = commit_marker(&repo, default_branch, default_contents, "seed default branch", None);
    for (name, contents) in branches.iter().filter(|(n, _)| *n != default_branch) {
        commit_marker(&repo, name, contents, &format!("seed branch {name}"), Some(tip));
    }
    dir
}

fn advance_remote_branch(remote: &Path, branch: &str, contents: &str, message: &str) {
    let repo = Repository::open(remote).unwrap();
    commit_marker(&repo, branch, contents, message, None);
}

/// `advanceRemoteBranchFromNewBranch`: a new branch forked from master.
fn create_remote_branch(remote: &Path, branch: &str, contents: &str) {
    let repo = Repository::open(remote).unwrap();
    let master = repo.find_reference("refs/heads/master").unwrap().target().unwrap();
    commit_marker(&repo, branch, contents, &format!("create {branch}"), Some(master));
}

fn set_remote_head(remote: &Path, branch: &str) {
    Repository::open(remote).unwrap().set_head(&format!("refs/heads/{branch}")).unwrap();
}

fn remote_tree(remote: &Path, branch: &str) -> git2::Tree<'static> {
    let repo = Box::leak(Box::new(Repository::open(remote).unwrap()));
    let oid = repo.find_reference(&format!("refs/heads/{branch}")).unwrap().target().unwrap();
    repo.find_commit(oid).unwrap().tree().unwrap()
}

fn remote_has(remote: &Path, branch: &str, path: &str) -> bool {
    remote_tree(remote, branch).get_path(Path::new(path)).is_ok()
}

fn remote_file(remote: &Path, branch: &str, path: &str) -> String {
    let repo = Repository::open(remote).unwrap();
    let oid = repo.find_reference(&format!("refs/heads/{branch}")).unwrap().target().unwrap();
    let tree = repo.find_commit(oid).unwrap().tree().unwrap();
    let entry = tree.get_path(Path::new(path)).unwrap();
    String::from_utf8(repo.find_blob(entry.id()).unwrap().content().to_vec()).unwrap()
}

fn remote_branch_contents(remote: &Path, branch: &str, want: &str) {
    assert_eq!(remote_file(remote, branch, MARKER), want);
}

fn local_head_branch(repo_dir: &Path) -> String {
    let repo = Repository::open(repo_dir).unwrap();
    let head = repo.head().unwrap();
    head.name().unwrap().strip_prefix("refs/heads/").unwrap().to_string()
}

fn assert_branch_and_contents(repo_dir: &Path, branch: &str, want: &str) {
    assert_eq!(local_head_branch(repo_dir), branch);
    assert_eq!(std::fs::read_to_string(repo_dir.join(MARKER)).unwrap(), want);
}

fn assert_remote_head(remote: &Path, branch: &str) {
    let repo = Repository::open(remote).unwrap();
    let head = repo.find_reference("HEAD").unwrap();
    assert_eq!(head.symbolic_target(), Some(format!("refs/heads/{branch}").as_str()));
}

fn assert_local(path: &Path, want: &str) {
    assert_eq!(std::fs::read_to_string(path).unwrap(), want, "{}", path.display());
}

fn assert_local_json(path: &Path, key: &str, want: &str) {
    let v: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(v.get(key).and_then(Value::as_str), Some(want), "{}", path.display());
}

fn new_store(remote: &Path, branch: &str, base_dir: &Path) -> GitTokenStore {
    let s = GitTokenStore::new(remote.to_str().unwrap(), "", "", branch);
    s.set_base_dir(base_dir.to_str().unwrap());
    s
}

/// Store with its workspace at `<root>/<name>` (auth dir `<name>/auths`).
fn workspace_store(root: &Path, remote: &Path, branch: &str, name: &str) -> GitTokenStore {
    new_store(remote, branch, &root.join(name).join("auths"))
}

fn codex_auth(id: &str, token: &str) -> Auth {
    let mut a = Auth::default();
    a.id = id.into();
    a.file_name = id.into();
    a.provider = "codex".into();
    a.metadata.insert("type".into(), "codex".into());
    a.metadata.insert("access_token".into(), token.into());
    a
}

fn save(store: &GitTokenStore, id: &str, token: &str) -> PathBuf {
    store.save(&mut codex_auth(id, token), SaveOptions::default()).unwrap().unwrap()
}

fn commit_and_push(store: &GitTokenStore, message: &str, paths: &[&str]) -> Result<(), String> {
    let _g = store.mu.lock();
    let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
    store.commit_and_push_with_options(message, false, &paths).map_err(GitErr::into_message)
}

fn ensure(store: &GitTokenStore) -> Result<(), String> {
    store.ensure_repository().map_err(|e| e.to_string())
}

fn master_remote() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let remote = setup_remote(tmp.path(), "master", &[("master", "remote master branch\n")]);
    (tmp, remote)
}

fn recovery_dirs(root: &Path) -> usize {
    std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".gitstore-recovery-"))
        .count()
}

#[test]
fn uses_remote_default_branch_when_branch_not_configured() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let remote = setup_remote(
        root,
        "trunk",
        &[("trunk", "remote default branch\n"), ("release/2026", "release branch\n")],
    );
    let store = workspace_store(root, &remote, "", "workspace");
    ensure(&store).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "trunk", "remote default branch\n");

    advance_remote_branch(&remote, "trunk", "remote default branch updated\n", "advance trunk");
    advance_remote_branch(&remote, "release/2026", "release branch updated\n", "advance release");
    ensure(&store).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "trunk", "remote default branch updated\n");
    assert_remote_head(&remote, "trunk");
}

#[test]
fn uses_configured_branch_when_explicitly_set() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let remote = setup_remote(
        root,
        "trunk",
        &[("trunk", "remote default branch\n"), ("release/2026", "release branch\n")],
    );
    let store = workspace_store(root, &remote, "release/2026", "workspace");
    ensure(&store).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "release/2026", "release branch\n");

    advance_remote_branch(&remote, "trunk", "remote default branch updated\n", "advance trunk");
    advance_remote_branch(&remote, "release/2026", "release branch updated\n", "advance release");
    ensure(&store).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "release/2026", "release branch updated\n");
    assert_remote_head(&remote, "trunk");
}

#[test]
fn errors_for_missing_configured_branch() {
    let tmp = TempDir::new().unwrap();
    let remote = setup_remote(tmp.path(), "trunk", &[("trunk", "remote default branch\n")]);
    let store = workspace_store(tmp.path(), &remote, "missing-branch", "workspace");
    assert!(ensure(&store).is_err());
    assert_remote_head(&remote, "trunk");
}

#[test]
fn errors_for_missing_configured_branch_on_existing_repository_pull() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let remote = setup_remote(root, "trunk", &[("trunk", "remote default branch\n")]);
    ensure(&workspace_store(root, &remote, "", "workspace")).unwrap();

    let reopened = workspace_store(root, &remote, "missing-branch", "workspace");
    assert!(ensure(&reopened).is_err());
    assert_eq!(local_head_branch(&root.join("workspace")), "trunk");
    assert_remote_head(&remote, "trunk");
}

#[test]
fn initializes_empty_remote_using_configured_branch() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let remote = root.join("remote.git");
    Repository::init_bare(&remote).unwrap();

    let branch = "feature/gemini-fix";
    let store = workspace_store(root, &remote, branch, "workspace");
    ensure(&store).unwrap();

    assert_eq!(local_head_branch(&root.join("workspace")), branch);
    let repo = Repository::open(&remote).unwrap();
    assert!(repo.find_reference(&format!("refs/heads/{branch}")).is_ok());
    assert!(repo.find_reference("refs/heads/master").is_err());
    assert!(remote_has(&remote, branch, "auths/.gitkeep") && remote_has(&remote, branch, "config/.gitkeep"));
}

#[test]
fn existing_repo_switches_to_configured_branch() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let remote = setup_remote(
        root,
        "master",
        &[("master", "remote master branch\n"), ("develop", "remote develop branch\n")],
    );
    ensure(&workspace_store(root, &remote, "", "workspace")).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "master", "remote master branch\n");

    let reopened = workspace_store(root, &remote, "develop", "workspace");
    ensure(&reopened).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "develop", "remote develop branch\n");

    std::fs::write(root.join("workspace").join(MARKER), "local develop update\n").unwrap();
    commit_and_push(&reopened, "Update develop branch marker", &[MARKER]).unwrap();
    assert_eq!(local_head_branch(&root.join("workspace")), "develop");
    remote_branch_contents(&remote, "develop", "local develop update\n");
    remote_branch_contents(&remote, "master", "remote master branch\n");
}

#[test]
fn existing_repo_switches_to_configured_branch_created_after_clone() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    ensure(&workspace_store(root, &remote, "", "workspace")).unwrap();
    create_remote_branch(&remote, "release/2026", "release branch\n");

    let reopened = workspace_store(root, &remote, "release/2026", "workspace");
    ensure(&reopened).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "release/2026", "release branch\n");
}

#[test]
fn resets_to_remote_default_when_branch_unset() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let remote = setup_remote(
        root,
        "master",
        &[("master", "remote master branch\n"), ("develop", "remote develop branch\n")],
    );
    let pinned = workspace_store(root, &remote, "develop", "workspace");
    ensure(&pinned).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "develop", "remote develop branch\n");

    let default = workspace_store(root, &remote, "", "workspace");
    ensure(&default).unwrap();
    assert_eq!(local_head_branch(&root.join("workspace")), "master");

    std::fs::write(root.join("workspace").join(MARKER), "local master update\n").unwrap();
    commit_and_push(&default, "Update master marker", &[MARKER]).unwrap();
    remote_branch_contents(&remote, "master", "local master update\n");
}

#[test]
fn refuses_watcher_originated_auth_deletion() {
    let (tmp, remote) = master_remote();
    let store = workspace_store(tmp.path(), &remote, "", "workspace");
    ensure(&store).unwrap();
    let path = save(&store, "protected.json", "token");
    assert!(remote_has(&remote, "master", "auths/protected.json"));

    std::fs::remove_file(&path).unwrap();
    let err = store
        .persist_auth_files("Remove auth protected.json", &[path.to_string_lossy().into_owned()])
        .unwrap_err();
    assert!(err.contains("refusing watcher-originated removal"), "{err}");
    assert!(remote_has(&remote, "master", "auths/protected.json"));
}

#[test]
fn watcher_removal_no_ops_after_explicit_delete() {
    let (tmp, remote) = master_remote();
    let store = workspace_store(tmp.path(), &remote, "", "workspace");
    ensure(&store).unwrap();
    let path = save(&store, "explicit.json", "token");
    let path_str = path.to_str().unwrap();

    // Management deletes unlink the file before invoking Store::delete.
    std::fs::remove_file(&path).unwrap();
    store.delete(path_str).unwrap();
    assert!(!remote_has(&remote, "master", "auths/explicit.json"));
    store.delete(path_str).unwrap();
    assert!(!remote_has(&remote, "master", "auths/explicit.json"));

    store.persist_auth_files("Remove auth explicit.json", &[path_str.to_string()]).unwrap();
    assert!(!remote_has(&remote, "master", "auths/explicit.json"));
}

#[test]
fn repeated_delete_does_not_overwrite_remote_only_changes() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let a = workspace_store(root, &remote, "", "workspace-a");
    ensure(&a).unwrap();
    let path_a = save(&a, "a.json", "a");
    a.delete(path_a.to_str().unwrap()).unwrap();

    let b = workspace_store(root, &remote, "", "workspace-b");
    ensure(&b).unwrap();
    save(&b, "b.json", "b");
    assert!(remote_has(&remote, "master", "auths/b.json"));

    a.delete(path_a.to_str().unwrap()).unwrap();
    assert!(remote_has(&remote, "master", "auths/b.json"));
}

#[test]
fn rejects_paths_outside_repository_before_mutation() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let store = workspace_store(root, &remote, "", "workspace");
    ensure(&store).unwrap();

    let outside = root.join("outside.json");
    std::fs::write(&outside, "outside\n").unwrap();
    assert!(store.delete(outside.to_str().unwrap()).is_err());
    assert_local(&outside, "outside\n");

    let outside_save = root.join("outside-save.json");
    let mut auth = codex_auth("outside-save.json", "token");
    auth.attributes.insert(ATTRIBUTE_PATH.into(), outside_save.to_string_lossy().into_owned());
    assert!(store.save(&mut auth, SaveOptions::default()).is_err());
    assert!(!outside_save.exists());
}

#[test]
fn skips_runtime_save_of_missing_disabled_auth() {
    let (tmp, remote) = master_remote();
    let store = workspace_store(tmp.path(), &remote, "", "workspace");
    let mut auth = codex_auth("canonical-disabled.json", "token");
    auth.disabled = true;
    assert_eq!(store.save(&mut auth, SaveOptions::default()).unwrap(), None);
    assert!(!tmp.path().join("workspace").exists());

    let path = store.save(&mut auth, SaveOptions { creation_intent: true }).unwrap().unwrap();
    assert!(path.exists());
    assert!(remote_has(&remote, "master", "auths/canonical-disabled.json"));
}

#[test]
fn persist_config_drops_unrelated_staged_deletions() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let store = workspace_store(root, &remote, "", "workspace");
    ensure(&store).unwrap();
    let auth_path = save(&store, "protected.json", "token");
    let config_path = store.config_path();
    std::fs::write(&config_path, "version: one\n").unwrap();
    store.persist_config().unwrap();

    let repo = Repository::open(root.join("workspace")).unwrap();
    let mut index = repo.index().unwrap();
    index.remove_path(Path::new("auths/protected.json")).unwrap();
    index.write().unwrap();
    std::fs::remove_file(&auth_path).unwrap();
    std::fs::write(&config_path, "version: two\n").unwrap();

    store.persist_config().unwrap();
    assert!(remote_has(&remote, "master", "auths/protected.json"));
    assert_eq!(remote_file(&remote, "master", "config/config.yaml"), "version: two\n");
}

#[test]
fn persist_config_repairs_index_after_unstaged_pull() {
    let (tmp, remote) = master_remote();
    let store = workspace_store(tmp.path(), &remote, "", "workspace");
    ensure(&store).unwrap();
    std::fs::write(store.config_path(), "source: local-config\n").unwrap();
    advance_remote_branch(&remote, "master", "remote branch advanced\n", "advance remote");

    store.persist_config().unwrap();
    remote_branch_contents(&remote, "master", "remote branch advanced\n");
    assert_eq!(remote_file(&remote, "master", "config/config.yaml"), "source: local-config\n");
}

#[test]
fn persist_config_preserves_remote_only_auth_after_divergence() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let a = workspace_store(root, &remote, "", "workspace-a");
    ensure(&a).unwrap();
    let b = workspace_store(root, &remote, "", "workspace-b");
    ensure(&b).unwrap();
    save(&b, "remote-only.json", "remote");
    assert!(remote_has(&remote, "master", "auths/remote-only.json"));

    std::fs::write(a.config_path(), "source: store-a\n").unwrap();
    a.persist_config().unwrap();
    assert!(remote_has(&remote, "master", "auths/remote-only.json"));
    assert_eq!(remote_file(&remote, "master", "config/config.yaml"), "source: store-a\n");
}

#[test]
fn rejects_stale_force_push() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let a = workspace_store(root, &remote, "", "workspace-a");
    ensure(&a).unwrap();
    let b = workspace_store(root, &remote, "", "workspace-b");
    ensure(&b).unwrap();

    save(&b, "concurrent.json", "remote");
    std::fs::write(a.config_path(), "source: stale-a\n").unwrap();
    let err = commit_and_push(&a, "Update stale config", &["config/config.yaml"]);
    assert!(err.is_err(), "stale force push must be rejected");
    assert!(remote_has(&remote, "master", "auths/concurrent.json"));
    assert!(!remote_has(&remote, "master", "config/config.yaml"));

    // Retry after the lease rejection reconciles and pushes the local content.
    a.persist_config().unwrap();
    assert!(remote_has(&remote, "master", "auths/concurrent.json"));
    assert_eq!(remote_file(&remote, "master", "config/config.yaml"), "source: stale-a\n");

    save(&a, "local.json", "local");
    assert_eq!(
        remote_file(&remote, "master", "auths/local.json"),
        r#"{"access_token":"local","disabled":false,"type":"codex"}"#
    );
    assert!(remote_has(&remote, "master", "auths/concurrent.json"));
}

#[test]
fn concurrent_initialization_does_not_overwrite_created_branch() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let remote = root.join("remote.git");
    let mut opts = RepositoryInitOptions::new();
    opts.bare(true).initial_head("master");
    Repository::init_opts(&remote, &opts).unwrap();

    let workspace = root.join("workspace");
    let local = Repository::init_opts(&workspace, RepositoryInitOptions::new().initial_head("master")).unwrap();
    disable_git_commit_signing(&workspace).unwrap();
    local.remote("origin", remote.to_str().unwrap()).unwrap();
    for p in ["auths/.gitkeep", "config/.gitkeep"] {
        let full = workspace.join(p);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, b"").unwrap();
    }

    let winner_dir = root.join("winner");
    let winner = Repository::init_opts(&winner_dir, RepositoryInitOptions::new().initial_head("master")).unwrap();
    let files = [
        ("auths/remote.json", r#"{"type":"codex","access_token":"remote"}"#),
        ("config/config.yaml", "source: winner\n"),
    ];
    let mut index = winner.index().unwrap();
    for (p, c) in files {
        let full = winner_dir.join(p);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, c).unwrap();
        index.add_path(Path::new(p)).unwrap();
    }
    index.write().unwrap();
    let tree = winner.find_tree(index.write_tree().unwrap()).unwrap();
    winner.commit(Some("HEAD"), &sig(), &sig(), "Initialize complete store", &tree, &[]).unwrap();
    let mut origin = winner.remote("origin", remote.to_str().unwrap()).unwrap();
    origin.push(&["refs/heads/master:refs/heads/master"], None).unwrap();

    let store = new_store(&remote, "master", &workspace.join("auths"));
    let late = {
        let _g = store.mu.lock();
        let paths = vec!["auths/.gitkeep".to_string(), "config/.gitkeep".to_string()];
        store.commit_and_push_with_options("Initialize git token store", true, &paths)
    };
    assert!(late.is_err(), "late initialization push must be rejected");
    assert_eq!(remote_file(&remote, "master", "auths/remote.json"), files[0].1);
    assert_eq!(remote_file(&remote, "master", "config/config.yaml"), files[1].1);

    ensure(&store).unwrap();
    assert_local(&workspace.join("auths/remote.json"), files[0].1);
    assert_local(&workspace.join("config/config.yaml"), files[1].1);
}

#[test]
fn retry_restores_tracked_auth_on_up_to_date_pull() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let store = workspace_store(root, &remote, "", "workspace");
    ensure(&store).unwrap();
    let auth_path = save(&store, "retry.json", "remote");

    let repo = Repository::open(root.join("workspace")).unwrap();
    let mut index = repo.index().unwrap();
    index.remove_path(Path::new("auths/retry.json")).unwrap();
    index.write().unwrap();
    std::fs::remove_file(&auth_path).unwrap();
    repo.remote_set_url("origin", root.join("missing.git").to_str().unwrap()).unwrap();
    assert!(ensure(&store).is_err(), "unavailable remote must be a retryable failure");
    assert!(!auth_path.exists());

    repo.remote_set_url("origin", remote.to_str().unwrap()).unwrap();
    ensure(&store).unwrap();
    assert_local(&auth_path, r#"{"access_token":"remote","disabled":false,"type":"codex"}"#);
    let auths = store.list().unwrap();
    assert_eq!(auths.len(), 1);
    assert_eq!(auths[0].id, "retry.json");

    store.delete(auth_path.to_str().unwrap()).unwrap();
    assert!(!remote_has(&remote, "master", "auths/retry.json"));
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn reconciles_remote_auth_changes_around_local_config() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let owner = workspace_store(root, &remote, "", "owner");
    ensure(&owner).unwrap();
    for id in ["modified.json", "deleted.json"] {
        save(&owner, id, "old");
    }
    std::fs::write(owner.config_path(), "source: original\n").unwrap();
    owner.persist_config().unwrap();

    let a = workspace_store(root, &remote, "", "workspace-a");
    ensure(&a).unwrap();
    let b = workspace_store(root, &remote, "", "workspace-b");
    ensure(&b).unwrap();
    std::fs::write(a.config_path(), "source: local-a\n").unwrap();
    save(&b, "modified.json", "new");
    b.delete(b.auth_dir().join("deleted.json").to_str().unwrap()).unwrap();

    ensure(&a).unwrap();
    assert_local(&a.config_path(), "source: local-a\n");
    assert_local_json(&a.auth_dir().join("modified.json"), "access_token", "new");
    assert!(!a.auth_dir().join("deleted.json").exists());
}

#[test]
fn reconciles_remote_config_changes_around_local_auth() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let owner = workspace_store(root, &remote, "", "owner");
    ensure(&owner).unwrap();
    save(&owner, "local.json", "old");
    std::fs::write(owner.config_path(), "source: original\n").unwrap();
    owner.persist_config().unwrap();

    let a = workspace_store(root, &remote, "", "workspace-a");
    ensure(&a).unwrap();
    let b = workspace_store(root, &remote, "", "workspace-b");
    ensure(&b).unwrap();
    let local_auth = a.auth_dir().join("local.json");
    let local_contents = r#"{"type":"codex","access_token":"local-dirty"}"#;
    std::fs::write(&local_auth, local_contents).unwrap();
    std::fs::write(b.config_path(), "source: remote-modified\n").unwrap();
    b.persist_config().unwrap();

    ensure(&a).unwrap();
    assert_local(&a.config_path(), "source: remote-modified\n");
    assert_local(&local_auth, local_contents);

    std::fs::remove_file(b.config_path()).unwrap();
    commit_and_push(&b, "Delete config", &["config/config.yaml"]).unwrap();
    ensure(&a).unwrap();
    assert!(!a.config_path().exists());
    assert_local(&local_auth, local_contents);
}

#[test]
fn fails_closed_on_same_path_conflict() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let owner = workspace_store(root, &remote, "", "owner");
    ensure(&owner).unwrap();
    std::fs::write(owner.config_path(), "source: original\n").unwrap();
    owner.persist_config().unwrap();

    let a = workspace_store(root, &remote, "", "workspace-a");
    ensure(&a).unwrap();
    let b = workspace_store(root, &remote, "", "workspace-b");
    ensure(&b).unwrap();
    std::fs::write(a.config_path(), "source: local\n").unwrap();
    std::fs::write(b.config_path(), "source: remote\n").unwrap();
    b.persist_config().unwrap();

    let err = ensure(&a).unwrap_err();
    assert!(err.contains("conflicts with local change"), "{err}");
    assert_local(&a.config_path(), "source: local\n");
    assert_eq!(remote_file(&remote, "master", "config/config.yaml"), "source: remote\n");
}

#[test]
fn install_recovered_git_directory_retains_backup_when_restore_fails() {
    let backup = Path::new("recovery").join("corrupt.git");
    let calls = std::cell::Cell::new(0);
    let rename = move |_: &Path, _: &Path| {
        calls.set(calls.get() + 1);
        match calls.get() {
            1 => Ok(()),
            2 => Err(std::io::Error::other("install failed")),
            _ => Err(std::io::Error::other("restore failed")),
        }
    };
    let (retain, err) =
        install_recovered_git_directory(Path::new("repo/.git"), Path::new("clone/.git"), &backup, &rename)
            .unwrap_err();
    assert!(retain);
    let msg = err.to_string();
    assert!(msg.contains("install failed") && msg.contains("restore failed"), "{msg}");
    assert!(msg.contains(&backup.display().to_string()), "{msg}");
}

#[test]
fn recover_worktree_backup_rollback_failure_retains_recovery_directory() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let store = workspace_store(root, &remote, "", "workspace");
    ensure(&store).unwrap();
    let repo_dir = root.join("workspace");
    std::fs::write(repo_dir.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(repo_dir.join("z.txt"), "zeta\n").unwrap();

    let rename = |from: &Path, to: &Path| {
        let (f, t) = (from.to_string_lossy(), to.to_string_lossy());
        if f.ends_with("z.txt") && t.contains(".gitstore-recovery-") {
            return Err(std::io::Error::other("move z failed"));
        }
        if f.ends_with("a.txt") && f.contains(".gitstore-recovery-") {
            return Err(std::io::Error::other("restore a failed"));
        }
        std::fs::rename(from, to)
    };
    let ctx = RecoveryCtx { remote: remote.to_str().unwrap(), branch: "", auth: None };
    let err = recover_repository(&ctx, &repo_dir, None, None, None, &rename).unwrap_err().to_string();
    assert!(err.contains("move z failed") && err.contains("restore a failed"), "{err}");
    assert!(err.contains("backup retained at"), "{err}");
    assert_eq!(recovery_dirs(root), 1);
}

/// `removeHeadFileObject`: commits `path` locally, then deletes its loose blob so HEAD is corrupt.
fn remove_head_file_object(repo_dir: &Path, path: &str) {
    let repo = Repository::open(repo_dir).unwrap();
    std::fs::write(repo_dir.join(path), "corrupt me\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(path)).unwrap();
    index.write().unwrap();
    let blob = {
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig(), &sig(), "Add corruption marker", &tree, &[&parent]).unwrap();
        tree.get_path(Path::new(path)).unwrap().id().to_string()
    };
    std::fs::remove_file(repo_dir.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
    drop(repo);
    let reopened = Repository::open(repo_dir).unwrap();
    assert!(verify_repository_head(&reopened).unwrap_err().is_corruption());
}

/// `corruptGitRepository`: packs every object, then deletes the loose objects and the pack.
fn corrupt_git_repository(repo_dir: &Path) {
    let repo = Repository::open(repo_dir).unwrap();
    let pack_dir = repo_dir.join(".git/objects/pack");
    std::fs::create_dir_all(&pack_dir).unwrap();
    {
        let mut pb = repo.packbuilder().unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push_glob("refs/*").unwrap();
        walk.push_head().unwrap();
        pb.insert_walk(&mut walk).unwrap();
        pb.write(&pack_dir, 0o644).unwrap();
    }
    drop(repo);
    let objects = repo_dir.join(".git/objects");
    for e in std::fs::read_dir(&objects).unwrap().flatten() {
        if e.file_type().unwrap().is_dir() && e.file_name().len() == 2 {
            std::fs::remove_dir_all(e.path()).unwrap();
        }
    }
    let mut packs = 0;
    for e in std::fs::read_dir(&pack_dir).unwrap().flatten() {
        if e.path().extension().is_some_and(|x| x == "pack") {
            std::fs::remove_file(e.path()).unwrap();
            packs += 1;
        }
    }
    assert!(packs > 0, "no packfiles found to corrupt");
    let reopened = Repository::open(repo_dir).unwrap();
    assert!(verify_repository_head(&reopened).unwrap_err().is_corruption());
}

#[test]
fn corruption_recovery_uses_latest_remote_auth_tree() {
    for modification in [true, false] {
        let (tmp, remote) = master_remote();
        let root = tmp.path();
        let owner = workspace_store(root, &remote, "", "owner");
        ensure(&owner).unwrap();
        save(&owner, "victim.json", "remote-old");
        let store = workspace_store(root, &remote, "", "workspace");
        ensure(&store).unwrap();
        if modification {
            save(&owner, "victim.json", "remote-new");
        } else {
            owner.delete(owner.auth_dir().join("victim.json").to_str().unwrap()).unwrap();
        }
        remove_head_file_object(&root.join("workspace"), "corrupt-object.txt");

        ensure(&store).unwrap();
        let victim = store.auth_dir().join("victim.json");
        if modification {
            assert_local_json(&victim, "access_token", "remote-new");
        } else {
            assert!(!victim.exists());
        }
        save(&store, "unrelated.json", "local");
        assert_eq!(remote_has(&remote, "master", "auths/victim.json"), modification);
        if modification {
            assert_eq!(
                remote_file(&remote, "master", "auths/victim.json"),
                r#"{"access_token":"remote-new","disabled":false,"type":"codex"}"#
            );
        }
    }
}

#[test]
fn corruption_recovery_preserves_only_non_conflicting_local_changes() {
    let setup = || {
        let (tmp, remote) = master_remote();
        let root = tmp.path().to_path_buf();
        let owner = workspace_store(&root, &remote, "", "owner");
        ensure(&owner).unwrap();
        save(&owner, "victim.json", "remote-old");
        let store = workspace_store(&root, &remote, "", "workspace");
        ensure(&store).unwrap();
        (tmp, remote, owner, store)
    };

    let (tmp, _remote, owner, store) = setup();
    std::fs::write(store.config_path(), "source: local\n").unwrap();
    save(&owner, "victim.json", "remote-new");
    remove_head_file_object(&tmp.path().join("workspace"), "corrupt-object.txt");
    ensure(&store).unwrap();
    assert_local(&store.config_path(), "source: local\n");
    assert_local_json(&store.auth_dir().join("victim.json"), "access_token", "remote-new");

    let (tmp, remote, owner, store) = setup();
    let victim = store.auth_dir().join("victim.json");
    let local = r#"{"type":"codex","access_token":"local"}"#;
    std::fs::write(&victim, local).unwrap();
    save(&owner, "victim.json", "remote-new");
    remove_head_file_object(&tmp.path().join("workspace"), "corrupt-object.txt");
    let err = ensure(&store).unwrap_err();
    assert!(err.contains("conflicts with local change"), "{err}");
    assert_local(&victim, local);
    assert_eq!(
        remote_file(&remote, "master", "auths/victim.json"),
        r#"{"access_token":"remote-new","disabled":false,"type":"codex"}"#
    );
}

#[test]
fn full_packfile_corruption_fails_closed_with_dirty_managed_file() {
    let setup = || {
        let (tmp, remote) = master_remote();
        let store = workspace_store(tmp.path(), &remote, "", "workspace");
        ensure(&store).unwrap();
        (tmp, remote, store)
    };

    let (tmp, remote, store) = setup();
    let config = store.config_path();
    std::fs::write(&config, "source: remote\n").unwrap();
    store.persist_config().unwrap();
    std::fs::write(&config, "source: local-dirty\n").unwrap();
    corrupt_git_repository(&tmp.path().join("workspace"));
    let err = store.persist_config().unwrap_err().to_string();
    assert!(err.contains("inspect recovery baseline"), "{err}");
    assert_local(&config, "source: local-dirty\n");
    assert_eq!(remote_file(&remote, "master", "config/config.yaml"), "source: remote\n");

    let (tmp, remote, store) = setup();
    let auth_path = save(&store, "dirty.json", "remote");
    let local = r#"{"type":"codex","access_token":"local-dirty"}"#;
    std::fs::write(&auth_path, local).unwrap();
    corrupt_git_repository(&tmp.path().join("workspace"));
    let err = store.save(&mut codex_auth("unrelated.json", "unrelated"), SaveOptions::default()).unwrap_err();
    assert!(err.to_string().contains("inspect recovery baseline"), "{err}");
    assert_local(&auth_path, local);
    assert_eq!(
        remote_file(&remote, "master", "auths/dirty.json"),
        r#"{"access_token":"remote","disabled":false,"type":"codex"}"#
    );
    assert!(!remote_has(&remote, "master", "auths/unrelated.json"));
}

#[test]
fn missing_packfile_recovery_fails_closed_without_baseline() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let store = workspace_store(root, &remote, "", "workspace");
    ensure(&store).unwrap();
    let auth_path = save(&store, "recover.json", "remote");

    corrupt_git_repository(&root.join("workspace"));
    std::fs::remove_file(&auth_path).unwrap();
    let err = ensure(&store).unwrap_err();
    assert!(err.contains("inspect recovery baseline"), "{err}");
    assert!(!auth_path.exists());
    assert!(remote_has(&remote, "master", "auths/recover.json"));
}

#[test]
fn inspect_recovery_baseline_failure_leaves_repository_replaceable() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let repo_dir = root.join("workspace");
    let store = workspace_store(root, &remote, "", "workspace");
    ensure(&store).unwrap();
    corrupt_git_repository(&repo_dir);

    assert!(recovery::inspect_recovery_baseline(&repo_dir).is_err());
    std::fs::rename(repo_dir.join(".git"), repo_dir.join(".git.renamed")).unwrap();
    std::fs::rename(repo_dir.join(".git.renamed"), repo_dir.join(".git")).unwrap();
    std::fs::remove_dir_all(&repo_dir).unwrap();
    ensure(&store).unwrap();
    remote_branch_contents(&remote, "master", "remote master branch\n");
}

#[test]
fn commit_and_push_runs_gc_after_push() {
    let (tmp, remote) = master_remote();
    let root = tmp.path();
    let store = workspace_store(root, &remote, "", "workspace");
    ensure(&store).unwrap();
    for contents in ["local master update one\n", "local master update two\n"] {
        std::fs::write(root.join("workspace").join(MARKER), contents).unwrap();
        store.reset_gc_timer();
        commit_and_push(&store, "Update master marker", &[MARKER]).unwrap();
        remote_branch_contents(&remote, "master", contents);
    }
    // History is squashed: the branch tip has no parent.
    let repo = Repository::open(root.join("workspace")).unwrap();
    assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().parent_count(), 0);
    verify_repository_head(&repo).unwrap();
    // GC consolidated everything reachable into one pack and left no packed loose objects.
    let objects = root.join("workspace/.git/objects");
    let packs = std::fs::read_dir(objects.join("pack"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count();
    assert_eq!(packs, 1);
    let tip = repo.head().unwrap().peel_to_commit().unwrap().id().to_string();
    assert!(!objects.join(&tip[..2]).join(&tip[2..]).exists(), "tip must be packed, not loose");
}

#[test]
fn follows_renamed_remote_default_branch() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let remote = setup_remote(
        root,
        "master",
        &[("master", "remote master branch\n"), ("main", "remote main branch\n")],
    );
    ensure(&workspace_store(root, &remote, "", "workspace")).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "master", "remote master branch\n");

    set_remote_head(&remote, "main");
    advance_remote_branch(&remote, "main", "remote main branch updated\n", "advance main");
    ensure(&workspace_store(root, &remote, "", "workspace")).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "main", "remote main branch updated\n");
    assert_remote_head(&remote, "main");
}

/// Serves `401 Basic` to every request until the process exits.
fn auth_required_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    url
}

#[test]
fn keeps_current_branch_when_remote_default_cannot_be_resolved() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let remote = setup_remote(
        root,
        "master",
        &[("master", "remote master branch\n"), ("develop", "remote develop branch\n")],
    );
    ensure(&workspace_store(root, &remote, "develop", "workspace")).unwrap();
    assert_branch_and_contents(&root.join("workspace"), "develop", "remote develop branch\n");

    let repo = Repository::open(root.join("workspace")).unwrap();
    repo.remote_set_url("origin", &auth_required_server()).unwrap();
    drop(repo);

    ensure(&workspace_store(root, &remote, "", "workspace")).unwrap();
    assert_eq!(local_head_branch(&root.join("workspace")), "develop");

    // With credentials configured, a rejected challenge is answered once and then falls back.
    let with_auth = GitTokenStore::new(remote.to_str().unwrap(), "", "wrong-token", "");
    with_auth.set_base_dir(root.join("workspace/auths").to_str().unwrap());
    assert_eq!(with_auth.git_client_options().unwrap().user, "git");
    ensure(&with_auth).unwrap();
}

#[test]
fn commits_use_the_store_identity_and_disable_signing() {
    let (tmp, remote) = master_remote();
    let store = workspace_store(tmp.path(), &remote, "", "workspace");
    ensure(&store).unwrap();
    save(&store, "who.json", "t");

    let repo = Repository::open(tmp.path().join("workspace")).unwrap();
    let commit = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(commit.author().name(), Some("CLIProxyAPI"));
    assert_eq!(commit.author().email(), Some("cliproxy@local"));
    assert_eq!(commit.message(), Some("Update auth who.json"));
    assert!(!repo.config().unwrap().get_bool("commit.gpgsign").unwrap());
}

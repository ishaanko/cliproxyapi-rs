//! Remote reconciliation after a diverged pull and full repository recovery by re-cloning
//! (Go: reconcileRemoteWorktree, recoverRepositoryLocked and their rollback helpers).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use git2::Repository;

use super::err::{GitErr, R};
use super::ops::{
    BasicAuth, DirtySet, HeadRef, TreeSnap, changed_tree_paths, clone_into, head_ref, overlapping_dirty_path,
    reset_mixed, restore_head_and_index, tree_snapshot, verify_repository_head, worktree_dirty_paths,
};
use crate::common::{mkdir_all_private, write_file_private};

/// Directory-entry mover; tests inject failures through it.
pub(super) type RenameFn<'a> = dyn Fn(&Path, &Path) -> io::Result<()> + 'a;

pub(super) fn os_rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

/// Writes (or deletes) `paths` of `snap` in the worktree.
fn apply_tree_paths(repo: &Repository, snap: &TreeSnap, repo_dir: &Path, paths: &[String]) -> R<()> {
    for path in paths {
        let dest = repo_dir.join(path);
        let Some((oid, _)) = snap.get(path) else {
            match fs::remove_file(&dest) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(GitErr::msg(format!("remove {path}: {e}"))),
            }
            continue;
        };
        let blob = repo.find_blob(*oid).map_err(|e| GitErr::from(e).wrap(format!("read {path}")))?;
        if let Some(parent) = dest.parent() {
            mkdir_all_private(parent).map_err(|e| GitErr::msg(format!("create parent for {path}: {e}")))?;
        }
        write_file_private(&dest, blob.content()).map_err(|e| GitErr::msg(format!("write {path}: {e}")))?;
    }
    Ok(())
}

/// `reconcileRemoteWorktree`: after a non-fast-forward or unstaged pull, move the branch to the
/// remote tip while keeping local edits, failing closed when a remote change touches a dirty path.
pub(super) fn reconcile_remote_worktree(
    repo: &Repository,
    repo_dir: &Path,
    base: &HeadRef,
    dirty: &DirtySet,
) -> R<()> {
    if !base.is_branch() {
        return Err(GitErr::msg(format!("head {} is not a branch", base.name)));
    }
    let remote_name = format!("refs/remotes/origin/{}", base.short());
    let remote_oid = repo
        .find_reference(&remote_name)
        .map_err(|e| GitErr::from(e).wrap(format!("resolve remote branch {remote_name}")))?
        .target()
        .ok_or_else(|| GitErr::msg(format!("resolve remote branch {remote_name}: reference has no target")))?;
    let base_snap = tree_snapshot(repo, base.oid).map_err(|e| e.wrap("inspect pre-pull tree"))?;
    let remote_snap = tree_snapshot(repo, remote_oid).map_err(|e| e.wrap("inspect remote tree"))?;
    let changed = changed_tree_paths(&base_snap, &remote_snap);
    for path in &changed {
        if let Some(dirty_path) = overlapping_dirty_path(path, dirty) {
            let conflict = GitErr::msg(format!("remote path {path} conflicts with local change {dirty_path}"));
            return match restore_head_and_index(repo, base) {
                Ok(()) => Err(conflict),
                Err(e) => Err(conflict.join(e.wrap("restore pre-pull head after conflict"))),
            };
        }
    }

    // Pull moves HEAD before reporting unstaged changes. Return to the pre-pull tree before
    // applying only the remote changes that do not overlap local edits.
    restore_head_and_index(repo, base).map_err(|e| e.wrap("restore pre-pull head"))?;
    let rollback = |err: GitErr| match apply_tree_paths(repo, &base_snap, repo_dir, &changed) {
        Ok(()) => err,
        Err(rb) => err.join(rb.wrap("restore pre-pull worktree")),
    };
    if let Err(e) = apply_tree_paths(repo, &remote_snap, repo_dir, &changed) {
        return Err(rollback(e.wrap("apply remote worktree changes")));
    }
    if let Err(e) = repo.reference(&base.name, remote_oid, true, "gitstore: reconcile") {
        return Err(rollback(GitErr::from(e).wrap(format!("update branch {}", base.name))));
    }
    reset_mixed(repo, remote_oid).map_err(|e| e.wrap(format!("reset index to remote branch {remote_name}")))
}

/// Everything recovery needs to re-clone the configured remote.
pub(super) struct RecoveryCtx<'a> {
    pub(super) remote: &'a str,
    pub(super) branch: &'a str,
    pub(super) auth: Option<&'a BasicAuth>,
}

/// `os.MkdirTemp(parent, ".gitstore-recovery-")`.
fn make_recovery_dir(parent: &Path) -> io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    for attempt in 0..64u32 {
        let dir = parent.join(format!(".gitstore-recovery-{}-{nanos}-{attempt}", std::process::id()));
        match fs::create_dir(&dir) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
                }
                return Ok(dir);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not create a unique recovery directory"))
}

/// `recoverRepositoryLocked`: replaces a corrupt `.git` with a fresh clone of the remote while
/// preserving non-conflicting local worktree edits, with rollback on every later failure.
/// `caller` is closed first when the baseline must be inspected. `baseline` / `dirty` come from
/// before the failed operation; `None` means inspect the repository now (failing closed if it is
/// unreadable).
pub(super) fn recover_repository(
    ctx: &RecoveryCtx<'_>,
    repo_dir: &Path,
    caller: Option<Repository>,
    baseline: Option<TreeSnap>,
    dirty: Option<DirtySet>,
    rename: &RenameFn<'_>,
) -> R<()> {
    let parent = repo_dir.parent().unwrap_or(Path::new("."));
    let recovery_root = match make_recovery_dir(parent) {
        Ok(d) => d,
        Err(e) => return Err(GitErr::msg(format!("create recovery directory: {e}"))),
    };
    let mut cleanup = true;
    let res = recover_inner(ctx, repo_dir, &recovery_root, caller, baseline, dirty, rename, &mut cleanup);
    if cleanup && let Err(e) = fs::remove_dir_all(&recovery_root) {
        let cleanup_err = GitErr::msg(format!("remove recovery directory: {e}"));
        return Err(match res {
            Ok(()) => cleanup_err,
            Err(prev) => prev.join(cleanup_err),
        });
    }
    res
}

#[allow(clippy::too_many_arguments)]
fn recover_inner(
    ctx: &RecoveryCtx<'_>,
    repo_dir: &Path,
    recovery_root: &Path,
    caller: Option<Repository>,
    baseline: Option<TreeSnap>,
    dirty: Option<DirtySet>,
    rename: &RenameFn<'_>,
    cleanup: &mut bool,
) -> R<()> {
    let (baseline_repo, baseline, dirty) = match baseline {
        Some(tree) => (caller, tree, dirty.unwrap_or_default()),
        None => {
            drop(caller);
            let (repo, tree, dirty) =
                inspect_recovery_baseline(repo_dir).map_err(|e| e.wrap("inspect recovery baseline"))?;
            (Some(repo), tree, dirty)
        }
    };

    let clone_dir = recovery_root.join("clone");
    let cloned = clone_into(&clone_dir, ctx.remote, ctx.branch, ctx.auth)
        .map_err(|e| e.wrap("clone remote repository"))?;
    verify_repository_head(&cloned).map_err(|e| e.wrap("verify cloned repository"))?;
    let cloned_head = head_ref(&cloned)
        .map_err(|e| e.wrap("get cloned repository head"))?
        .ok_or_else(|| GitErr::kind(super::err::REF_NOT_FOUND).wrap("get cloned repository head"))?;
    let remote_snap = tree_snapshot(&cloned, cloned_head.oid).map_err(|e| e.wrap("inspect cloned repository tree"))?;
    let preserved = recovery_preserved_paths(&baseline, &remote_snap, &dirty)?;
    apply_recovery_local_changes(repo_dir, &clone_dir, &preserved)
        .map_err(|e| e.wrap("preserve local worktree changes"))?;
    drop(baseline_repo);
    drop(cloned);

    let backup_worktree = recovery_root.join("worktree");
    if let Err((retain, e)) = move_worktree_entries(repo_dir, &backup_worktree, rename) {
        if retain {
            *cleanup = false;
            return Err(e.wrap(format!("backup existing worktree; backup retained at {}", backup_worktree.display())));
        }
        return Err(e.wrap("backup existing worktree"));
    }
    let git_dir = repo_dir.join(".git");
    let cloned_git_dir = clone_dir.join(".git");
    let backup_git_dir = recovery_root.join("corrupt.git");
    if let Err((retain, e)) = install_recovered_git_directory(&git_dir, &cloned_git_dir, &backup_git_dir, rename) {
        if retain {
            *cleanup = false;
        }
        if let Err((_, restore)) = move_worktree_entries(&backup_worktree, repo_dir, rename) {
            *cleanup = false;
            return Err(e.join(restore.wrap(format!(
                "restore worktree; backup retained at {}",
                backup_worktree.display()
            ))));
        }
        return Err(e);
    }
    let rollback = |err: GitErr, cleanup: &mut bool| -> GitErr {
        match rollback_recovered_repository(repo_dir, &git_dir, &backup_git_dir, &backup_worktree, rename) {
            Ok(()) => err,
            Err(rb) => {
                *cleanup = false;
                err.join(rb.wrap(format!("rollback recovered repository; backup retained at {}", recovery_root.display())))
            }
        }
    };
    if let Err((_, e)) = move_worktree_entries(&clone_dir, repo_dir, rename) {
        return Err(rollback(e.wrap("install recovered worktree"), cleanup));
    }
    let verified = Repository::open(repo_dir)
        .map_err(GitErr::from)
        .and_then(|r| verify_repository_head(&r));
    if let Err(e) = verified {
        return Err(rollback(e.wrap("verify recovered repository"), cleanup));
    }
    Ok(())
}

/// `inspectRecoveryBaseline`: the open repository, its HEAD tree and its dirty paths.
pub(super) fn inspect_recovery_baseline(repo_dir: &Path) -> R<(Repository, TreeSnap, DirtySet)> {
    let repo = Repository::open(repo_dir).map_err(|e| GitErr::from(e).wrap("open repository"))?;
    let dirty = worktree_dirty_paths(&repo).map_err(|e| e.wrap("inspect worktree changes"))?;
    let head = head_ref(&repo)
        .map_err(|e| e.wrap("inspect head"))?
        .ok_or_else(|| GitErr::kind(super::err::REF_NOT_FOUND).wrap("inspect head"))?;
    let tree = tree_snapshot(&repo, head.oid).map_err(|e| e.wrap("inspect head tree"))?;
    Ok((repo, tree, dirty))
}

/// `recoveryPreservedPaths`: the dirty paths to carry over, or a conflict error when the remote
/// changed one of them since the baseline.
fn recovery_preserved_paths(baseline: &TreeSnap, remote: &TreeSnap, dirty: &DirtySet) -> R<DirtySet> {
    if dirty.is_empty() {
        return Ok(DirtySet::new());
    }
    for changed in changed_tree_paths(baseline, remote) {
        if let Some(dirty_path) = overlapping_dirty_path(&changed, dirty) {
            return Err(GitErr::msg(format!(
                "remote path {changed} conflicts with local change {dirty_path} during repository recovery"
            )));
        }
    }
    Ok(dirty.clone())
}

fn remove_all(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// `applyRecoveryLocalChanges`: copies dirty paths (or their deletion) from the old worktree
/// into the fresh clone.
fn apply_recovery_local_changes(source_dir: &Path, target_dir: &Path, paths: &DirtySet) -> R<()> {
    for path in paths {
        let source = source_dir.join(path);
        let target = target_dir.join(path);
        let info = match fs::symlink_metadata(&source) {
            Ok(i) => i,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                remove_all(&target).map_err(|e| GitErr::msg(format!("preserve deletion {path}: {e}")))?;
                continue;
            }
            Err(e) => return Err(GitErr::msg(format!("inspect local change {path}: {e}"))),
        };
        remove_all(&target).map_err(|e| GitErr::msg(format!("replace recovered path {path}: {e}")))?;
        if let Some(parent) = target.parent() {
            mkdir_all_private(parent)
                .map_err(|e| GitErr::msg(format!("create recovered parent for {path}: {e}")))?;
        }
        let ft = info.file_type();
        if ft.is_file() {
            let contents = fs::read(&source).map_err(|e| GitErr::msg(format!("read local change {path}: {e}")))?;
            write_with_mode(&target, &contents, &info)
                .map_err(|e| GitErr::msg(format!("write local change {path}: {e}")))?;
        } else if ft.is_symlink() {
            let link = fs::read_link(&source).map_err(|e| GitErr::msg(format!("read local symlink {path}: {e}")))?;
            symlink(&link, &target).map_err(|e| GitErr::msg(format!("write local symlink {path}: {e}")))?;
        } else {
            return Err(GitErr::msg(format!("local change {path} has unsupported file mode")));
        }
    }
    Ok(())
}

fn write_with_mode(target: &Path, contents: &[u8], info: &fs::Metadata) -> io::Result<()> {
    use std::io::Write as _;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        opts.mode(info.mode() & 0o777);
    }
    #[cfg(not(unix))]
    let _ = info;
    opts.open(target)?.write_all(contents)
}

#[cfg(unix)]
fn symlink(link: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(link, target)
}

#[cfg(not(unix))]
fn symlink(_link: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "symlinks are not supported"))
}

/// `moveWorktreeEntries`: moves every entry except `.git`, undoing the moves on failure. The
/// error carries whether the target still holds entries that could not be moved back.
fn move_worktree_entries(source: &Path, target: &Path, rename: &RenameFn<'_>) -> Result<(), (bool, GitErr)> {
    mkdir_all_private(target).map_err(|e| (false, GitErr::from(e)))?;
    let mut names: Vec<String> = fs::read_dir(source)
        .and_then(|rd| rd.map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned())).collect())
        .map_err(|e| (false, GitErr::from(e)))?;
    names.sort();
    let mut moved: Vec<&str> = Vec::with_capacity(names.len());
    for name in &names {
        if name == ".git" {
            continue;
        }
        if let Err(e) = rename(&source.join(name), &target.join(name)) {
            let mut err = GitErr::msg(format!("move {name}: {e}"));
            let mut retain = false;
            for done in moved.iter().rev() {
                if let Err(re) = rename(&target.join(done), &source.join(done)) {
                    retain = true;
                    err = err.join(GitErr::msg(format!("restore {done}: {re}")));
                }
            }
            return Err((retain, err));
        }
        moved.push(name);
    }
    Ok(())
}

fn remove_worktree_entries(repo_dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(repo_dir)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        remove_all(&entry.path())?;
    }
    Ok(())
}

fn rollback_recovered_repository(
    repo_dir: &Path,
    git_dir: &Path,
    backup_git_dir: &Path,
    backup_worktree: &Path,
    rename: &RenameFn<'_>,
) -> R<()> {
    remove_worktree_entries(repo_dir).map_err(|e| GitErr::msg(format!("remove recovered worktree: {e}")))?;
    rollback_recovered_git_directory(git_dir, backup_git_dir)?;
    move_worktree_entries(backup_worktree, repo_dir, rename).map_err(|(_, e)| e.wrap("restore original worktree"))
}

/// `installRecoveredGitDirectory`: swaps in the cloned `.git`, keeping the corrupt one as a
/// backup. The error flag says the backup must be retained because it could not be restored.
pub(super) fn install_recovered_git_directory(
    git_dir: &Path,
    cloned_git_dir: &Path,
    backup_git_dir: &Path,
    rename: &RenameFn<'_>,
) -> Result<(), (bool, GitErr)> {
    if let Err(e) = rename(git_dir, backup_git_dir) {
        return Err((false, GitErr::msg(format!("backup corrupt git directory: {e}"))));
    }
    if let Err(e) = rename(cloned_git_dir, git_dir) {
        let install = GitErr::msg(format!("install recovered git directory: {e}"));
        if let Err(restore) = rename(backup_git_dir, git_dir) {
            return Err((
                true,
                install.join(GitErr::msg(format!(
                    "restore corrupt git directory; backup retained at {}: {restore}",
                    backup_git_dir.display()
                ))),
            ));
        }
        return Err((false, install));
    }
    Ok(())
}

fn rollback_recovered_git_directory(git_dir: &Path, backup_git_dir: &Path) -> R<()> {
    fs::remove_dir_all(git_dir).map_err(|e| GitErr::msg(format!("remove recovered git directory: {e}")))?;
    fs::rename(backup_git_dir, git_dir).map_err(|e| GitErr::msg(format!("restore original git directory: {e}")))
}

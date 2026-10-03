//! Repository primitives the store is built from: head and tree inspection, reset / checkout,
//! fetch / pull, clone and push. Each stands in for the go-git call of the same role and keeps
//! its observable behavior (for example `pull` moves the branch before reporting unstaged
//! changes, and `fetch_origin` reports an empty remote as an error kind).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use git2::build::CheckoutBuilder;
use git2::{
    AutotagOption, Cred, CredentialType, ErrorClass, ErrorCode, FetchOptions, ObjectType,
    Oid, PushOptions, RemoteCallbacks, Repository, RepositoryInitOptions, ResetType, Status,
    StatusOptions, TreeWalkMode, TreeWalkResult,
};

use super::err::{
    EMPTY_REMOTE, GitErr, NON_FAST_FORWARD, R, REF_NOT_FOUND, UNSTAGED_CHANGES, UP_TO_DATE,
};
use crate::common::{mkdir_all_private, write_file_private};

/// Fetch refspec of the `origin` remote.
const FETCH_SPEC: &str = "+refs/heads/*:refs/remotes/origin/*";
const FILEMODE_COMMIT: i32 = 0o160000;

/// HTTP basic credentials (`gitClientOptions`).
#[derive(Debug, Clone)]
pub(super) struct BasicAuth {
    pub(super) user: String,
    pub(super) pass: String,
}

/// Callbacks answering basic-auth challenges at most once, so wrong credentials fail instead of
/// looping. Without credentials no callback is installed and a challenge yields an auth error.
pub(super) fn remote_callbacks(auth: Option<&BasicAuth>) -> RemoteCallbacks<'_> {
    let mut cb = RemoteCallbacks::new();
    if let Some(a) = auth {
        let tried = Cell::new(false);
        cb.credentials(move |_url, _user, allowed| {
            if tried.replace(true) || !allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
                return Err(git2::Error::new(ErrorCode::Auth, ErrorClass::Http, "authentication required"));
            }
            Cred::userpass_plaintext(&a.user, &a.pass)
        });
    }
    cb
}

/// The resolved HEAD: the branch ref it points at (or `HEAD` when detached) and its commit.
#[derive(Debug, Clone)]
pub(super) struct HeadRef {
    pub(super) name: String,
    pub(super) oid: Oid,
}

impl HeadRef {
    pub(super) fn is_branch(&self) -> bool {
        self.name.starts_with("refs/heads/")
    }

    /// `ReferenceName.Short()` for a branch.
    pub(super) fn short(&self) -> &str {
        self.name.strip_prefix("refs/heads/").unwrap_or(&self.name)
    }
}

/// `repo.Head()`; `None` when HEAD is unborn (go-git `ErrReferenceNotFound`).
pub(super) fn head_ref(repo: &Repository) -> R<Option<HeadRef>> {
    match repo.head() {
        Ok(r) => {
            let name = r.name().unwrap_or("HEAD").to_string();
            match r.target() {
                Some(oid) => Ok(Some(HeadRef { name, oid })),
                None => Err(GitErr::msg("head reference has no target")),
            }
        }
        Err(e) => {
            let e = GitErr::from(e);
            if e.is(REF_NOT_FOUND) { Ok(None) } else { Err(e) }
        }
    }
}

/// Flattened tree: file path to (blob id, filemode). Comparable across repositories.
pub(super) type TreeSnap = BTreeMap<String, (Oid, i32)>;
pub(super) type DirtySet = BTreeSet<String>;

pub(super) fn tree_snapshot(repo: &Repository, commit: Oid) -> R<TreeSnap> {
    let tree = repo.find_commit(commit)?.tree()?;
    let mut out = TreeSnap::new();
    tree.walk(TreeWalkMode::PreOrder, |root, entry| {
        if entry.kind() != Some(ObjectType::Tree) {
            let name = String::from_utf8_lossy(entry.name_bytes());
            out.insert(format!("{root}{name}"), (entry.id(), entry.filemode()));
        }
        TreeWalkResult::Ok
    })?;
    Ok(out)
}

/// Tree snapshot of HEAD; `None` when HEAD is unborn.
pub(super) fn head_tree_snapshot(repo: &Repository) -> R<Option<TreeSnap>> {
    match head_ref(repo)? {
        Some(h) => Ok(Some(tree_snapshot(repo, h.oid)?)),
        None => Ok(None),
    }
}

/// `changedTreePaths`: sorted paths that differ between the trees (adds, deletes and edits).
pub(super) fn changed_tree_paths(a: &TreeSnap, b: &TreeSnap) -> Vec<String> {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).cloned().collect()
}

/// `overlappingDirtyPath`.
pub(super) fn overlapping_dirty_path<'a>(path: &str, dirty: &'a DirtySet) -> Option<&'a str> {
    dirty
        .iter()
        .find(|d| {
            path == d.as_str()
                || path.strip_prefix(d.as_str()).is_some_and(|r| r.starts_with('/'))
                || d.strip_prefix(path).is_some_and(|r| r.starts_with('/'))
        })
        .map(String::as_str)
}

/// `worktreeDirtyPaths`: every path with a staged or worktree change, untracked files included.
pub(super) fn worktree_dirty_paths(repo: &Repository) -> R<DirtySet> {
    let mut o = StatusOptions::new();
    o.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .include_unmodified(false);
    let statuses = repo.statuses(Some(&mut o))?;
    let mut out = DirtySet::new();
    for e in statuses.iter() {
        let s = e.status();
        if s.is_empty() || s == Status::CURRENT || s == Status::IGNORED {
            continue;
        }
        out.insert(String::from_utf8_lossy(e.path_bytes()).into_owned());
    }
    Ok(out)
}

/// go-git `containsUnstagedChanges`: tracked files whose worktree differs from the index.
/// Untracked files do not count.
fn has_unstaged_changes(repo: &Repository) -> R<bool> {
    let mut o = StatusOptions::new();
    o.include_untracked(false).include_ignored(false).include_unmodified(false);
    let statuses = repo.statuses(Some(&mut o))?;
    let wt = Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_TYPECHANGE | Status::WT_RENAMED;
    Ok(statuses.iter().any(|e| e.status().intersects(wt)))
}

/// `resetIndexToHead`: mixed reset to HEAD; no-op on an unborn branch.
pub(super) fn reset_index_to_head(repo: &Repository) -> R<()> {
    let Some(head) = head_ref(repo)? else { return Ok(()) };
    reset_mixed(repo, head.oid)
}

pub(super) fn reset_mixed(repo: &Repository, oid: Oid) -> R<()> {
    let obj = repo.find_object(oid, None)?;
    repo.reset(&obj, ResetType::Mixed, None)?;
    Ok(())
}

/// `restoreHeadAndIndex`: point the branch back at `head` and reset the index to it.
pub(super) fn restore_head_and_index(repo: &Repository, head: &HeadRef) -> R<()> {
    repo.reference(&head.name, head.oid, true, "gitstore: restore head")?;
    reset_mixed(repo, head.oid)
}

/// Writes the tree of `oid` into the worktree and index, overwriting local edits. The baseline
/// is the current HEAD, so files that the target no longer has are removed.
fn checkout_commit_force(repo: &Repository, oid: Oid) -> R<()> {
    let obj = repo.find_object(oid, None)?;
    let mut cb = CheckoutBuilder::new();
    cb.force();
    repo.checkout_tree(&obj, Some(&mut cb))?;
    Ok(())
}

/// Shared tail of go-git `Checkout`: HEAD moves first, then a merge reset refuses to proceed
/// when tracked files have unstaged changes.
fn checkout_commit(repo: &Repository, branch_ref: &str, oid: Oid) -> R<()> {
    if has_unstaged_changes(repo)? {
        repo.set_head(branch_ref)?;
        return Err(GitErr::kind(UNSTAGED_CHANGES));
    }
    checkout_commit_force(repo, oid)?;
    repo.set_head(branch_ref)?;
    Ok(())
}

/// `worktree.Checkout(&CheckoutOptions{Branch: ref})`.
pub(super) fn checkout_branch(repo: &Repository, branch_ref: &str) -> R<()> {
    let oid = repo.find_reference(branch_ref)?.peel_to_commit()?.id();
    checkout_commit(repo, branch_ref, oid)
}

/// `worktree.Checkout(&CheckoutOptions{Branch: ref, Create: true, Hash: oid})`.
pub(super) fn create_and_checkout_branch(repo: &Repository, branch_ref: &str, oid: Oid) -> R<()> {
    if repo.find_reference(branch_ref).is_ok() {
        return Err(GitErr::msg(format!("a branch named \"{branch_ref}\" already exists")));
    }
    repo.reference(branch_ref, oid, false, "gitstore: create branch")?;
    checkout_commit(repo, branch_ref, oid)
}

/// Writes `branch.<short>.remote = origin` and `branch.<short>.merge = <ref>`.
pub(super) fn set_branch_tracking(repo: &Repository, short: &str, branch_ref: &str) -> Result<(), git2::Error> {
    let mut cfg = repo.config()?.open_level(git2::ConfigLevel::Local)?;
    cfg.set_str(&format!("branch.{short}.remote"), "origin")?;
    cfg.set_str(&format!("branch.{short}.merge"), branch_ref)
}

/// One advertised remote ref.
#[derive(Debug, Clone)]
pub(super) struct RemoteRef {
    pub(super) name: String,
    pub(super) oid: Oid,
    pub(super) symref: Option<String>,
}

pub(super) struct Fetched {
    /// Whether any tracking ref changed (go-git: not `NoErrAlreadyUpToDate`).
    pub(super) updated: bool,
    pub(super) refs: Vec<RemoteRef>,
}

/// Scratch namespace the fetch mirrors the advertised branches into. `git2::Remote::list` is
/// unsound on an empty remote (null slice), so the advertisement is read back from these refs.
const PROBE_PREFIX: &str = "refs/gitstore/probe/";
const PROBE_SPEC: &str = "+refs/heads/*:refs/gitstore/probe/*";

fn clear_probe_refs(repo: &Repository) -> R<()> {
    let names: Vec<String> = repo
        .references_glob(&format!("{PROBE_PREFIX}*"))?
        .names()
        .filter_map(|n| n.ok().map(str::to_string))
        .collect();
    for name in names {
        repo.find_reference(&name)?.delete()?;
    }
    Ok(())
}

/// Fetches the branches of `origin`, into `refs/remotes/origin/*` and
/// returns what the remote advertised (`HEAD` with its symref, then the branches). An empty
/// remote is an `EMPTY_REMOTE` error (go-git `ErrEmptyRemoteRepository`).
fn fetch_remote(repo: &Repository, auth: Option<&BasicAuth>) -> R<Fetched> {
    let mut remote = repo.find_remote("origin")?;
    clear_probe_refs(repo)?;
    let updated = Cell::new(false);
    let mut cbs = remote_callbacks(auth);
    cbs.update_tips(|name, _, _| {
        if name.starts_with("refs/remotes/origin/") {
            updated.set(true);
        }
        true
    });
    let mut fo = FetchOptions::new();
    fo.remote_callbacks(cbs).download_tags(AutotagOption::None).update_fetchhead(false);
    let fetched = remote.fetch(&[FETCH_SPEC, PROBE_SPEC], Some(&mut fo), None);
    let head_target = remote.default_branch().ok().and_then(|b| b.as_str().map(str::to_string));
    let advertised = fetched.map_err(GitErr::from).and_then(|()| {
        let mut branches = Vec::new();
        for r in repo.references_glob(&format!("{PROBE_PREFIX}*"))?.flatten() {
            if let (Some(name), Some(oid)) = (r.name(), r.target()) {
                let short = name.strip_prefix(PROBE_PREFIX).unwrap_or(name);
                branches.push(RemoteRef { name: format!("refs/heads/{short}"), oid, symref: None });
            }
        }
        Ok(branches)
    });
    clear_probe_refs(repo)?;
    let branches = advertised?;
    let mut refs = Vec::with_capacity(branches.len() + 1);
    if let Some(target) = head_target
        && let Some(b) = branches.iter().find(|b| b.name == target)
    {
        refs.push(RemoteRef { name: "HEAD".into(), oid: b.oid, symref: Some(target) });
    }
    refs.extend(branches);
    if refs.is_empty() {
        return Err(GitErr::kind(EMPTY_REMOTE));
    }
    Ok(Fetched { updated: updated.get(), refs })
}

/// Fetches all branches of `origin` into `refs/remotes/origin/*`.
pub(super) fn fetch_origin(repo: &Repository, auth: Option<&BasicAuth>) -> R<Fetched> {
    fetch_remote(repo, auth)
}

/// go-git `isFastForward`: `new` is `old` or has it as an ancestor.
fn is_fast_forward(repo: &Repository, old: Oid, new: Oid) -> R<bool> {
    repo.find_commit(new)?;
    Ok(old == new || repo.graph_descendant_of(new, old)?)
}

/// Moves the branch HEAD points at (or a detached HEAD) to `oid`.
fn update_head(repo: &Repository, oid: Oid) -> R<()> {
    let head = repo.find_reference("HEAD")?;
    match head.symbolic_target() {
        Some(target) => repo.reference(target, oid, true, "gitstore: pull")?,
        None => repo.reference("HEAD", oid, true, "gitstore: pull")?,
    };
    Ok(())
}

/// `worktree.Pull(origin, branch)` (fast-forward only). `branch` empty follows the remote HEAD.
/// `Ok` means the worktree was updated; `UP_TO_DATE`, `NON_FAST_FORWARD` and `UNSTAGED_CHANGES`
/// are returned as error kinds like in go-git.
pub(super) fn pull(repo: &Repository, auth: Option<&BasicAuth>, branch: &str) -> R<()> {
    let fetched = fetch_origin(repo, auth)?;
    let target_name = if branch.is_empty() { "HEAD".to_string() } else { format!("refs/heads/{branch}") };
    let remote_oid = fetched
        .refs
        .iter()
        .find(|r| r.name == target_name)
        .map(|r| r.oid)
        .ok_or_else(|| GitErr::kind(REF_NOT_FOUND))?;

    if let Some(head) = head_ref(repo)? {
        let head_ahead = is_fast_forward(repo, remote_oid, head.oid)?;
        if !fetched.updated && head_ahead {
            return Err(GitErr::kind(UP_TO_DATE));
        }
        if !is_fast_forward(repo, head.oid, remote_oid)? {
            return Err(GitErr::kind(NON_FAST_FORWARD));
        }
    }

    if has_unstaged_changes(repo)? {
        update_head(repo, remote_oid)?;
        return Err(GitErr::kind(UNSTAGED_CHANGES));
    }
    checkout_commit_force(repo, remote_oid)?;
    update_head(repo, remote_oid)
}

/// `normalizeRemoteBranchReference`: a local branch ref name for a branch-ish ref.
pub(super) fn normalize_remote_branch_reference(name: &str) -> Option<String> {
    if name.starts_with("refs/heads/") {
        Some(name.to_string())
    } else {
        name.strip_prefix("refs/remotes/origin/").map(|s| format!("refs/heads/{s}"))
    }
}

pub(super) struct ResolvedBranch {
    pub(super) name: String,
    pub(super) oid: Option<Oid>,
}

/// `resolveRemoteDefaultBranch`: the branch the remote HEAD points at, falling back to the local
/// `origin/HEAD` and then the first advertised branch.
pub(super) fn resolve_remote_default_branch(repo: &Repository, auth: Option<&BasicAuth>) -> R<ResolvedBranch> {
    let fetched = fetch_origin(repo, auth).map_err(|e| e.wrap("resolve remote default: sync remote refs"))?;
    for r in &fetched.refs {
        if r.name == "HEAD"
            && let Some(target) = r.symref.as_deref().and_then(normalize_remote_branch_reference)
        {
            return Ok(ResolvedBranch { name: target, oid: None });
        }
    }
    if let Some(resolved) = resolve_remote_default_branch_from_local(repo) {
        return Ok(resolved);
    }
    for r in &fetched.refs {
        if let Some(n) = normalize_remote_branch_reference(&r.name) {
            return Ok(ResolvedBranch { name: n, oid: Some(r.oid) });
        }
    }
    Err(GitErr::msg("resolve remote default: remote default branch not found"))
}

fn resolve_remote_default_branch_from_local(repo: &Repository) -> Option<ResolvedBranch> {
    let r = repo.find_reference("refs/remotes/origin/HEAD").ok()?;
    let target = normalize_remote_branch_reference(r.symbolic_target()?)?;
    Some(ResolvedBranch { name: target, oid: None })
}

/// `PlainClone` stand-in (libgit2 refuses a non-empty target directory, so this inits, fetches
/// and checks out by hand). The remote's default branch is used when `branch` is empty. An empty
/// remote yields `EMPTY_REMOTE`; any failure removes the `.git` it created.
pub(super) fn clone_into(dir: &Path, url: &str, branch: &str, auth: Option<&BasicAuth>) -> R<Repository> {
    let res = clone_inner(dir, url, branch, auth);
    if res.is_err() {
        let _ = fs::remove_dir_all(dir.join(".git"));
    }
    res
}

fn clone_inner(dir: &Path, url: &str, branch: &str, auth: Option<&BasicAuth>) -> R<Repository> {
    mkdir_all_private(dir)?;
    let repo = init_repo(dir)?;
    repo.remote("origin", url)?;
    let fetched = fetch_origin(&repo, auth)?;
    let oid_of = |name: &str| fetched.refs.iter().find(|r| r.name == name).map(|r| r.oid);
    let (branch_ref, oid) = if !branch.is_empty() {
        let name = format!("refs/heads/{branch}");
        let oid = oid_of(&name).ok_or_else(|| GitErr::kind(REF_NOT_FOUND))?;
        (name, oid)
    } else {
        default_branch_of(&fetched.refs).ok_or_else(|| GitErr::kind(REF_NOT_FOUND))?
    };
    repo.reference(&branch_ref, oid, true, "gitstore: clone")?;
    checkout_commit_force(&repo, oid)?;
    repo.set_head(&branch_ref)?;
    let short = branch_ref.strip_prefix("refs/heads/").unwrap_or(&branch_ref);
    set_branch_tracking(&repo, short, &branch_ref)?;
    Ok(repo)
}

/// The branch the advertised HEAD refers to, else the first advertised branch.
fn default_branch_of(refs: &[RemoteRef]) -> Option<(String, Oid)> {
    let head = refs.iter().find(|r| r.name == "HEAD");
    let find = |name: &str| refs.iter().find(|r| r.name == name).map(|r| (r.name.clone(), r.oid));
    if let Some(h) = head {
        if let Some(t) = h.symref.as_deref().and_then(normalize_remote_branch_reference)
            && let Some(found) = find(&t)
        {
            return Some(found);
        }
        if let Some(r) = refs.iter().find(|r| r.name.starts_with("refs/heads/") && r.oid == h.oid) {
            return Some((r.name.clone(), r.oid));
        }
    }
    refs.iter().find(|r| r.name.starts_with("refs/heads/")).map(|r| (r.name.clone(), r.oid))
}

/// `git.PlainInit(dir, false)`: unborn `master`, like go-git.
pub(super) fn init_repo(dir: &Path) -> R<Repository> {
    let mut opts = RepositoryInitOptions::new();
    opts.initial_head("master");
    Ok(Repository::init_opts(dir, &opts)?)
}

/// `verifyRepositoryHead`: every blob of the HEAD tree must be readable.
pub(super) fn verify_repository_head(repo: &Repository) -> R<()> {
    let Some(snap) = head_tree_snapshot(repo)? else { return Ok(()) };
    for (oid, mode) in snap.values() {
        if *mode != FILEMODE_COMMIT {
            repo.find_blob(*oid)?;
        }
    }
    Ok(())
}

/// `restoreMissingTrackedFiles`: rewrites HEAD-tracked files that vanished from the worktree.
pub(super) fn restore_missing_tracked_files(repo: &Repository, repo_dir: &Path) -> R<()> {
    let Some(snap) = head_tree_snapshot(repo)? else { return Ok(()) };
    for (path, (oid, mode)) in &snap {
        let dest = repo_dir.join(path);
        match fs::symlink_metadata(&dest) {
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if *mode == FILEMODE_COMMIT {
            continue;
        }
        let blob = repo.find_blob(*oid)?;
        if let Some(parent) = dest.parent() {
            mkdir_all_private(parent)?;
        }
        write_file_private(&dest, blob.content())?;
    }
    Ok(())
}

/// Pushes the checked-out branch to `origin`. With a stored tracking ref the push is forced but
/// only if the remote still has that exact commit when the push negotiates (force-with-lease via
/// libgit2's push negotiation callback). Without one (`allow_missing_remote`) the push is a
/// plain branch creation that fails if the branch exists. Updates the tracking ref on success.
pub(super) fn push_branch(
    repo: &Repository,
    auth: Option<&BasicAuth>,
    head: &HeadRef,
    allow_missing_remote: bool,
) -> R<()> {
    let remote_name = format!("refs/remotes/origin/{}", head.short());
    let lease = match repo.find_reference(&remote_name) {
        Ok(r) => r.target(),
        Err(e) => {
            let e = GitErr::from(e);
            if e.is(REF_NOT_FOUND) && allow_missing_remote {
                None
            } else if e.is(REF_NOT_FOUND) {
                return Err(GitErr::msg(format!("git token store: remote tracking branch {remote_name} not found")));
            } else {
                return Err(e.wrap(format!("git token store: inspect remote tracking branch {remote_name}")));
            }
        }
    };
    let push_err = |e: GitErr| e.wrap("git token store: push");
    let mut remote = repo.find_remote("origin").map_err(|e| push_err(e.into()))?;

    let rejection: RefCell<Option<String>> = RefCell::new(None);
    let mut cbs = remote_callbacks(auth);
    // Force-with-lease: the remote's ref as seen during this push's own negotiation must still be
    // the stored tracking commit, so no other writer can slip in between check and update.
    let stale: RefCell<bool> = RefCell::new(false);
    if let Some(expected) = lease {
        let stale = &stale;
        cbs.push_negotiation(move |updates| {
            let current = updates.iter().find(|u| u.dst_refname() == Some(head.name.as_str())).map(|u| u.src());
            if current != Some(expected) {
                *stale.borrow_mut() = true;
                return Err(git2::Error::from_str("force-with-lease: stale info"));
            }
            Ok(())
        });
    }
    cbs.push_update_reference(|name, status| {
        if let Some(s) = status {
            *rejection.borrow_mut() = Some(format!("{name}: {s}"));
        }
        Ok(())
    });
    let mut po = PushOptions::new();
    po.remote_callbacks(cbs);
    let force = if lease.is_some() { "+" } else { "" };
    let spec = format!("{force}{0}:{0}", head.name);
    if let Err(e) = remote.push(&[spec], Some(&mut po)) {
        if *stale.borrow() {
            return Err(GitErr::msg(format!(
                "git token store: push: force-with-lease: stale info for {} (expected {})",
                head.name,
                lease.map(|o| o.to_string()).unwrap_or_default()
            )));
        }
        return Err(push_err(e.into()));
    }
    if let Some(msg) = rejection.take() {
        return Err(GitErr::msg(format!("git token store: push: {msg}")));
    }
    repo.reference(&remote_name, head.oid, true, "gitstore: update remote tracking branch")
        .map_err(|e| GitErr::from(e).wrap(format!("git token store: update remote tracking branch {remote_name}")))?;
    Ok(())
}

//! Best-effort housekeeping after a push (Go: maybeRunGC). Squashing history orphans the
//! previous commit's objects on every push; this prunes unreachable loose objects once they are
//! older than the grace period, which keeps recently orphaned objects available for recovery.
//! Unlike go-git's `RepackObjects` it does not consolidate packfiles.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use git2::{Oid, Repository, Sort, TreeWalkMode, TreeWalkResult};

/// Recently orphaned objects stay available this long.
const PRUNE_GRACE_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

pub(super) fn run(repo_dir: &Path) -> Result<(), String> {
    let repo = Repository::open(repo_dir).map_err(|e| format!("open repository for GC: {e}"))?;
    let reachable = reachable_objects(&repo).map_err(|e| format!("walk reachable objects: {e}"))?;
    let cutoff = SystemTime::now().checked_sub(PRUNE_GRACE_PERIOD).unwrap_or(SystemTime::UNIX_EPOCH);
    prune_loose_objects(&repo_dir.join(".git").join("objects"), &reachable, cutoff);
    Ok(())
}

/// Every commit, tree and blob reachable from any ref or HEAD.
fn reachable_objects(repo: &Repository) -> Result<HashSet<Oid>, git2::Error> {
    let mut seen = HashSet::new();
    let mut walk = repo.revwalk()?;
    walk.set_sorting(Sort::NONE)?;
    walk.push_glob("refs/*")?;
    // An unborn or missing HEAD is fine; refs cover everything else.
    let _ = walk.push_head();
    for oid in walk {
        let oid = oid?;
        seen.insert(oid);
        let tree = repo.find_commit(oid)?.tree()?;
        if !seen.insert(tree.id()) {
            continue;
        }
        tree.walk(TreeWalkMode::PreOrder, |_, entry| {
            seen.insert(entry.id());
            TreeWalkResult::Ok
        })?;
    }
    Ok(seen)
}

/// Removes loose objects that are unreachable and last modified before `cutoff`.
fn prune_loose_objects(objects_dir: &Path, reachable: &HashSet<Oid>, cutoff: SystemTime) {
    let Ok(fanout) = fs::read_dir(objects_dir) else { return };
    for dir in fanout.flatten() {
        let prefix = dir.file_name().to_string_lossy().into_owned();
        if prefix.len() != 2 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(files) = fs::read_dir(dir.path()) else { continue };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let Ok(oid) = Oid::from_str(&format!("{prefix}{name}")) else { continue };
            if reachable.contains(&oid) {
                continue;
            }
            let old = file.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff);
            if old {
                let _ = fs::remove_file(file.path());
            }
        }
        // Succeeds only when the fan-out directory is now empty.
        let _ = fs::remove_dir(dir.path());
    }
}

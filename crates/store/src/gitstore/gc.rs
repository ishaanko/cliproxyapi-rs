//! Best-effort housekeeping after a push (Go: maybeRunGC). Squashing history orphans the
//! previous commit's objects on every push; this prunes unreachable loose objects once they are
//! older than the grace period, which keeps recently orphaned objects available for recovery,
//! then consolidates every reachable object into a single packfile (go-git `RepackObjects`).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use git2::{Oid, Repository, Sort, TreeWalkMode, TreeWalkResult};

/// Recently orphaned objects stay available this long.
const PRUNE_GRACE_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

pub(super) fn run(repo_dir: &Path) -> Result<(), String> {
    let repo = Repository::open(repo_dir).map_err(|e| format!("open repository for GC: {e}"))?;
    let reachable = reachable_objects(&repo).map_err(|e| format!("walk reachable objects: {e}"))?;
    let cutoff = SystemTime::now().checked_sub(PRUNE_GRACE_PERIOD).unwrap_or(SystemTime::UNIX_EPOCH);
    let objects_dir = repo_dir.join(".git").join("objects");
    prune_loose_objects(&objects_dir, &reachable, cutoff);
    repack(repo_dir, &repo, &objects_dir, &reachable)
}

/// Writes one pack with all reachable objects and removes the packs and loose objects it
/// supersedes. The old files are first moved aside and the repository is re-opened to prove
/// every reachable object is still readable; otherwise they are moved back.
fn repack(repo_dir: &Path, repo: &Repository, objects_dir: &Path, reachable: &HashSet<Oid>) -> Result<(), String> {
    if reachable.is_empty() {
        return Ok(());
    }
    let pack_dir = objects_dir.join("pack");
    fs::create_dir_all(&pack_dir).map_err(|e| format!("create pack directory: {e}"))?;
    let new_stem = {
        let mut pb = repo.packbuilder().map_err(|e| format!("create packbuilder: {e}"))?;
        let mut walk = repo.revwalk().map_err(|e| e.to_string())?;
        walk.push_glob("refs/*").map_err(|e| e.to_string())?;
        let _ = walk.push_head();
        pb.insert_walk(&mut walk).map_err(|e| format!("pack reachable objects: {e}"))?;
        pb.write(&pack_dir, 0o444).map_err(|e| format!("write pack: {e}"))?;
        pb.name().map(str::to_string).ok_or("packbuilder produced no pack name")?
    };

    // Superseded files: other packs, and loose objects that are now packed.
    let mut superseded: Vec<(PathBuf, String)> = Vec::new();
    for entry in fs::read_dir(&pack_dir).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("pack-") && !name.starts_with(&format!("pack-{new_stem}")) {
            superseded.push((entry.path(), format!("pack/{name}")));
        }
    }
    for dir in fs::read_dir(objects_dir).map_err(|e| e.to_string())?.flatten() {
        let prefix = dir.file_name().to_string_lossy().into_owned();
        if prefix.len() != 2 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        for file in fs::read_dir(dir.path()).map_err(|e| e.to_string())?.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if Oid::from_str(&format!("{prefix}{name}")).is_ok_and(|o| reachable.contains(&o)) {
                superseded.push((file.path(), format!("{prefix}/{name}")));
            }
        }
    }
    if superseded.is_empty() {
        return Ok(());
    }

    let trash = objects_dir.join(format!("gc-trash-{}", std::process::id()));
    fs::create_dir_all(&trash).map_err(|e| format!("create gc trash: {e}"))?;
    let restore = |moved: &[(PathBuf, PathBuf)]| {
        for (orig, aside) in moved.iter().rev() {
            let _ = fs::rename(aside, orig);
        }
    };
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(superseded.len());
    for (path, rel) in &superseded {
        let aside = trash.join(rel.replace('/', "-"));
        if let Err(e) = fs::rename(path, &aside) {
            restore(&moved);
            let _ = fs::remove_dir_all(&trash);
            return Err(format!("move aside {rel}: {e}"));
        }
        moved.push((path.clone(), aside));
    }
    let verified = Repository::open(repo_dir).and_then(|fresh| {
        let odb = fresh.odb()?;
        Ok(reachable.iter().all(|o| odb.exists(*o)))
    });
    if !matches!(verified, Ok(true)) {
        restore(&moved);
        let _ = fs::remove_dir_all(&trash);
        return Err("repack verification failed, original objects restored".into());
    }
    fs::remove_dir_all(&trash).map_err(|e| format!("remove gc trash: {e}"))
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

//! Plugin file naming, version rules and discovery (Go: `platform.go`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;

static PLUGIN_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").expect("static regex"));
static PLUGIN_VERSION: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9][0-9A-Za-z.+-]*$").expect("static regex"));

/// A discovered plugin binary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginFile {
    pub id: String,
    pub path: PathBuf,
    pub version: String,
}

pub fn valid_plugin_id(id: &str) -> bool {
    PLUGIN_ID.is_match(id)
}

pub fn valid_plugin_version(version: &str) -> bool {
    !version.is_empty() && !version.starts_with('v') && PLUGIN_VERSION.is_match(version)
}

/// Strips a leading `v`/`V` and validates (Go: `normalizePluginDesiredVersion`).
pub fn normalize_desired_version(version: &str) -> String {
    let mut v = version.trim();
    if v.len() > 1 && (v.starts_with('v') || v.starts_with('V')) {
        v = &v[1..];
    }
    if valid_plugin_version(v) { v.to_string() } else { String::new() }
}

/// Dynamic library extension for a Go-style OS name.
pub fn plugin_extension(goos: &str) -> &'static str {
    match goos {
        "darwin" => ".dylib",
        "windows" => ".dll",
        _ => ".so",
    }
}

pub fn current_goos() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

pub fn current_goarch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    }
}

/// Parses `<id>[-v<version>]<ext>` (Go: `pluginFileFromPath`). `required_extension` empty accepts
/// any known library extension.
pub fn plugin_file_from_path(path: &Path, required_extension: &str) -> Option<PluginFile> {
    let base = path.file_name()?.to_string_lossy().into_owned();
    let lower = base.to_lowercase();
    let extension = if !required_extension.trim().is_empty() {
        let ext = required_extension.trim();
        if !lower.ends_with(&ext.to_lowercase()) {
            return None;
        }
        ext.to_string()
    } else {
        [".so", ".dylib", ".dll"].iter().find(|e| lower.ends_with(**e))?.to_string()
    };
    let name = &base[..base.len() - extension.len()];
    let mut id = name.to_string();
    let mut version = String::new();
    if let Some(idx) = name.rfind("-v").filter(|i| *i > 0) {
        let (cand_id, cand_version) = (&name[..idx], &name[idx + 2..]);
        if valid_plugin_id(cand_id) && valid_plugin_version(cand_version) {
            id = cand_id.to_string();
            version = cand_version.to_string();
        }
    }
    if !valid_plugin_id(&id) {
        return None;
    }
    Some(PluginFile { id, path: path.to_path_buf(), version })
}

/// Plugin id of a path even when it is not a well-formed plugin file name.
pub fn plugin_id_from_path(path: &Path) -> String {
    if let Some(f) = plugin_file_from_path(path, "") {
        return f.id;
    }
    let base = path.file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = base.to_lowercase();
    for ext in [".so", ".dylib", ".dll"] {
        if lower.ends_with(ext) {
            return base[..base.len() - ext.len()].to_string();
        }
    }
    base
}

fn version_segment(segments: &[&str], index: usize) -> Option<i64> {
    match segments.get(index) {
        None => Some(0),
        Some(s) => s.parse::<i64>().ok().filter(|n| *n >= 0),
    }
}

/// Numeric dotted comparison; `None` when a segment is not a number (Go: `comparePluginVersions`).
pub fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let sa: Vec<&str> = a.split('.').collect();
    let sb: Vec<&str> = b.split('.').collect();
    for i in 0..sa.len().max(sb.len()) {
        let na = version_segment(&sa, i)?;
        let nb = version_segment(&sb, i)?;
        if na != nb {
            return Some(na.cmp(&nb));
        }
    }
    Some(std::cmp::Ordering::Equal)
}

fn file_preferred(candidate: &PluginFile, current: &PluginFile) -> bool {
    if candidate.version.is_empty() {
        return false;
    }
    if current.version.is_empty() {
        return true;
    }
    match compare_versions(&candidate.version, &current.version) {
        Some(ord) => ord == std::cmp::Ordering::Greater,
        None => candidate.version > current.version,
    }
}

fn file_preferred_for_desired(candidate: &PluginFile, current: &PluginFile, desired: &str) -> bool {
    let desired = normalize_desired_version(desired);
    if !desired.is_empty() {
        let cm = candidate.version == desired;
        let um = current.version == desired;
        if cm != um {
            return cm;
        }
    }
    file_preferred(candidate, current)
}

fn candidate_dirs(root: &Path) -> Vec<PathBuf> {
    vec![root.join(current_goos()).join(current_goarch()), root.to_path_buf()]
}

/// Selects the plugin files to load: one per id (newest version, or the desired one) from
/// `<root>/<goos>/<goarch>` then `<root>`. Also returns every candidate (Go:
/// `selectPluginFilesWithCandidates`).
pub fn select_plugin_files(
    root: &str,
    desired: &HashMap<String, String>,
) -> std::io::Result<(Vec<PluginFile>, Vec<PluginFile>)> {
    let root = if root.trim().is_empty() { "plugins" } else { root.trim() };
    let desired: HashMap<String, String> = desired
        .iter()
        .filter_map(|(id, v)| {
            let (id, v) = (id.trim(), normalize_desired_version(v));
            (!id.is_empty() && !v.is_empty()).then(|| (id.to_string(), v))
        })
        .collect();
    let extension = plugin_extension(current_goos());
    let mut selected: HashMap<String, PluginFile> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut all: Vec<PluginFile> = Vec::new();
    for dir in candidate_dirs(Path::new(root)) {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let is_regular = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
            if is_regular && entry.file_name().to_string_lossy().to_lowercase().ends_with(extension) {
                files.push(dir.join(entry.file_name()));
            }
        }
        files.sort();
        for path in files {
            let Some(file) = plugin_file_from_path(&path, extension) else { continue };
            all.push(file.clone());
            match selected.get(&file.id) {
                None => {
                    order.push(file.id.clone());
                    selected.insert(file.id.clone(), file);
                }
                Some(current) => {
                    let want = desired.get(&file.id).map(String::as_str).unwrap_or("");
                    if file_preferred_for_desired(&file, current, want) {
                        selected.insert(file.id.clone(), file);
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    for id in order {
        let file = selected.remove(&id).unwrap_or_default();
        if let Some(d) = desired.get(&id)
            && &file.version != d
        {
            continue;
        }
        out.push(file);
    }
    Ok((out, all))
}

/// Removes superseded plugin files of ids that loaded successfully (Go:
/// `cleanupUnselectedPluginFiles`).
pub fn cleanup_unselected_files(root: &str, loaded: &[PluginFile]) -> std::io::Result<()> {
    if loaded.is_empty() {
        return Ok(());
    }
    let (_, candidates) = select_plugin_files(root, &HashMap::new())?;
    let mut by_id: BTreeMap<&str, Vec<PathBuf>> = BTreeMap::new();
    for f in loaded {
        if f.id.trim().is_empty() || f.path.as_os_str().is_empty() {
            continue;
        }
        by_id.entry(f.id.as_str()).or_default().push(clean_path(&f.path));
    }
    let mut first_err = None;
    for cand in candidates {
        let Some(paths) = by_id.get(cand.id.as_str()) else { continue };
        if paths.contains(&clean_path(&cand.path)) {
            continue;
        }
        match std::fs::remove_file(&cand.path) {
            Ok(()) => tracing::info!(plugin_id = %cand.id, version = %cand.version, path = %cand.path.display(), "pluginhost: old plugin file removed"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("pluginhost: failed to remove old plugin file {}: {e}", cand.path.display());
                first_err.get_or_insert(e);
            }
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// Lexical path cleaning (Go: `filepath.Clean`).
pub fn clean_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from(".") } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn ext() -> &'static str {
        plugin_extension(current_goos())
    }

    fn platform_dir(root: &Path) -> PathBuf {
        let dir = root.join(current_goos()).join(current_goarch());
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        fs::write(path, b"x").unwrap();
    }

    fn pf(id: &str, path: PathBuf, version: &str) -> PluginFile {
        PluginFile { id: id.into(), path, version: version.into() }
    }

    #[test]
    fn candidate_dirs_prefer_the_platform_directory() {
        assert_eq!(
            candidate_dirs(Path::new("plugins")),
            vec![Path::new("plugins").join(current_goos()).join(current_goarch()), PathBuf::from("plugins")]
        );
    }

    #[test]
    fn extension_per_platform() {
        for (goos, want) in [("linux", ".so"), ("freebsd", ".so"), ("darwin", ".dylib"), ("windows", ".dll")] {
            assert_eq!(plugin_extension(goos), want);
        }
    }

    #[test]
    fn plugin_id_from_library_path() {
        for (path, want) in [
            ("plugins/example.so", "example"),
            ("plugins/example.dylib", "example"),
            ("plugins/example.dll", "example"),
            ("plugins/example.custom", "example.custom"),
        ] {
            assert_eq!(plugin_id_from_path(Path::new(path)), want, "{path}");
        }
    }

    #[test]
    fn selection_filters_invalid_ids_and_deduplicates_by_id() {
        let root = tempfile::tempdir().unwrap();
        let arch = platform_dir(root.path());
        let e = ext();
        for path in [
            root.path().join(format!("sample{e}")),
            arch.join(format!("sample{e}")),
            arch.join(format!("bad name{e}")),
            arch.join(format!("-bad{e}")),
            arch.join(format!("another{}", e.to_uppercase())),
            arch.join("ignored.txt"),
        ] {
            touch(&path);
        }
        fs::create_dir(arch.join(format!("dir{e}"))).unwrap();

        let (files, _) = select_plugin_files(&root.path().to_string_lossy(), &Default::default()).unwrap();
        assert_eq!(files, vec![pf("another", arch.join(format!("another{}", e.to_uppercase())), ""), pf("sample", arch.join(format!("sample{e}")), "")]);
    }

    #[test]
    fn platform_directory_wins_over_the_root_fallback() {
        let root = tempfile::tempdir().unwrap();
        let arch = platform_dir(root.path());
        let e = ext();
        touch(&root.path().join(format!("alpha{e}")));
        touch(&arch.join(format!("alpha{e}")));
        let (files, _) = select_plugin_files(&root.path().to_string_lossy(), &Default::default()).unwrap();
        assert_eq!(files, vec![pf("alpha", arch.join(format!("alpha{e}")), "")]);
    }

    fn versioned_pair() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let arch = platform_dir(root.path());
        let (older, newer) = (arch.join(format!("alpha-v1.0.3{}", ext())), arch.join(format!("alpha-v1.0.4{}", ext())));
        touch(&older);
        touch(&newer);
        (root, older, newer)
    }

    #[test]
    fn configured_version_beats_a_higher_version() {
        let (root, older, _) = versioned_pair();
        let desired = [("alpha".to_string(), "1.0.3".to_string())].into();
        let (files, _) = select_plugin_files(&root.path().to_string_lossy(), &desired).unwrap();
        assert_eq!(files, vec![pf("alpha", older, "1.0.3")]);
    }

    #[test]
    fn highest_version_wins_without_a_configured_version() {
        let (root, _, newer) = versioned_pair();
        let (files, _) = select_plugin_files(&root.path().to_string_lossy(), &Default::default()).unwrap();
        assert_eq!(files, vec![pf("alpha", newer, "1.0.4")]);
    }

    #[test]
    fn plugin_is_skipped_when_its_configured_version_is_missing() {
        let root = tempfile::tempdir().unwrap();
        touch(&platform_dir(root.path()).join(format!("alpha-v1.0.4{}", ext())));
        let desired = [("alpha".to_string(), "1.0.3".to_string())].into();
        let (files, _) = select_plugin_files(&root.path().to_string_lossy(), &desired).unwrap();
        assert!(files.is_empty());
    }
}

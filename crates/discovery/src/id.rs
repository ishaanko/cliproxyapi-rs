//! Persistent instance IDs and DNS-SD instance names (Go: internal/discovery/id.go).

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::LazyLock;

use parking_lot::Mutex;

use crate::service::{sanitize_instance_name, truncate_runes_to};

/// Prefix of default instance names.
pub const DEFAULT_INSTANCE_PREFIX: &str = "CPA-";
const INSTANCE_ID_FILENAME: &str = "instance_id";

static CACHED_IDS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn is_valid_hex4(s: &str) -> bool {
    s.len() == 4 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// Go `filepath.Clean` (unix semantics).
pub(crate) fn clean_path(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

/// Retrieves the persistent instance ID from `state_dir`, or generates a new 4-character hex ID
/// (e.g. "8F3B") and persists it atomically. Cached per directory for the process lifetime.
pub fn get_or_generate_instance_id(state_dir: &str) -> String {
    let clean_dir = if state_dir.is_empty() { String::new() } else { clean_path(state_dir) };

    let mut cache = CACHED_IDS.lock();
    if let Some(id) = cache.get(&clean_dir)
        && is_valid_hex4(id)
    {
        return id.clone();
    }

    if !clean_dir.is_empty() {
        let id_path = Path::new(&clean_dir).join(INSTANCE_ID_FILENAME);
        if let Ok(data) = std::fs::read(&id_path) {
            let text = String::from_utf8_lossy(&data);
            let id = text.trim();
            if is_valid_hex4(id) {
                let id = id.to_ascii_uppercase();
                cache.insert(clean_dir, id.clone());
                return id;
            }
        }
    }

    let id = format!("{:04X}", rand::random::<u16>());
    if !clean_dir.is_empty() {
        persist_id(&clean_dir, &id);
    }
    cache.insert(clean_dir, id.clone());
    id
}

/// Writes the ID through a temp file plus rename so concurrent readers never see a partial file.
fn persist_id(dir: &str, id: &str) {
    if make_state_dir(dir).is_err() {
        return;
    }
    let id_path = Path::new(dir).join(INSTANCE_ID_FILENAME);
    let tmp_path = Path::new(dir).join(format!("instance_id_{}.tmp", rand::random::<u64>()));
    let Ok(mut file) = create_private_file(&tmp_path) else { return };
    let _ = file.write_all(id.as_bytes());
    let _ = file.sync_all();
    drop(file);
    if std::fs::rename(&tmp_path, &id_path).is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
}

/// Go `os.MkdirAll(dir, 0700)`.
fn make_state_dir(dir: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Go `os.CreateTemp` + `Chmod(0600)`: a new file only the owner can read.
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Resets in-memory cached IDs (Go: `ResetCachedInstanceID`, for test isolation).
pub fn reset_cached_instance_id() {
    CACHED_IDS.lock().clear();
}

/// DNS-SD instance name that always includes the persistent short ID: empty custom name becomes
/// `CPA-<ShortID>`, a custom name becomes `<name>-<ShortID>`, capped at 63 bytes.
pub fn format_instance_name(custom_name: &str, instance_id: &str) -> String {
    let instance_id = if is_valid_hex4(instance_id) { instance_id } else { "0001" };
    let instance_id = instance_id.to_ascii_uppercase();
    let suffix = format!("-{instance_id}");
    let default_name = format!("{DEFAULT_INSTANCE_PREFIX}{instance_id}");
    let mut base = sanitize_instance_name(custom_name);
    if base.is_empty() || base.eq_ignore_ascii_case(&default_name) {
        return default_name;
    }
    if base.len() >= suffix.len() && base.as_bytes()[base.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes()) {
        // The suffix bytes are ASCII, so the cut lands on a char boundary.
        base = base[..base.len() - suffix.len()].trim().to_string();
    }
    let base = truncate_runes_to(&base, 63usize.saturating_sub(suffix.len())).trim().to_string();
    if base.is_empty() {
        return default_name;
    }
    base + &suffix
}

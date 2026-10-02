//! Path helpers: `~` expansion and lexical cleaning (ports of `util.ResolveAuthDir` and
//! `config.ResolvePluginsDir`).

use std::path::PathBuf;

use crate::error::{ConfigError, Result};
use crate::types::{Config, DEFAULT_AUTH_DIR, DEFAULT_PLUGINS_DIR};

/// The user's home directory, like Go's `os.UserHomeDir` ($HOME, or %USERPROFILE% on Windows).
fn user_home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Lexical path cleaning with Go `filepath.Clean` semantics (no filesystem access): collapses
/// separators, drops ".", resolves ".." against preceding elements.
pub fn clean_path(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let rooted = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|last| *last != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

/// Expands a leading `~` (a bare `~` or `~/rest`) to the home directory and cleans the result.
/// Anything else is only cleaned.
fn expand_dir(raw: &str, what: &str) -> Result<PathBuf> {
    if !raw.starts_with('~') {
        return Ok(PathBuf::from(clean_path(raw)));
    }
    let home = user_home_dir()
        .ok_or_else(|| ConfigError::invalid(format!("resolve {what}: $HOME is not defined")))?;
    let remainder = raw[1..].trim_start_matches(['/', '\\']);
    if remainder.is_empty() {
        return Ok(PathBuf::from(clean_path(&home.to_string_lossy())));
    }
    let normalized = remainder.replace('\\', "/");
    let joined = format!("{}/{normalized}", home.to_string_lossy());
    Ok(PathBuf::from(clean_path(&joined)))
}

/// Resolves the auth directory: empty means `~/.cli-proxy-api`, a leading `~` expands to the home
/// directory.
pub fn resolve_auth_dir(auth_dir: &str) -> Result<PathBuf> {
    let raw = if auth_dir.is_empty() { DEFAULT_AUTH_DIR } else { auth_dir };
    expand_dir(raw, "auth dir")
}

/// Resolves the plugin directory: empty means `plugins`, a leading `~` expands to the home
/// directory.
pub fn resolve_plugins_dir(plugins_dir: &str) -> Result<PathBuf> {
    let trimmed = plugins_dir.trim();
    let raw = if trimmed.is_empty() { DEFAULT_PLUGINS_DIR } else { trimmed };
    expand_dir(raw, "plugins directory")
}

impl Config {
    /// Resolves and stores the effective plugin directory.
    pub fn resolve_plugins_dir(&mut self) -> Result<()> {
        self.plugins.dir = resolve_plugins_dir(&self.plugins.dir)?.to_string_lossy().into_owned();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::clean_path;

    #[test]
    fn clean_matches_go() {
        for (input, want) in [
            ("a//b/./c/..", "a/b"),
            ("/../a", "/a"),
            ("../../a", "../../a"),
            ("", "."),
            ("./", "."),
            ("/", "/"),
        ] {
            assert_eq!(clean_path(input), want, "{input}");
        }
    }
}

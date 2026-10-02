//! `.env` loading (the `godotenv.Load` call in `cmd/server/main.go`).

use std::path::Path;

use crate::error::{ConfigError, Result};

/// Loads `<dir>/.env` into the process environment. Variables that are already set are never
/// overridden. Returns `Ok(false)` when there is no `.env` file.
pub fn load_dotenv(dir: impl AsRef<Path>) -> Result<bool> {
    let path = dir.as_ref().join(".env");
    match dotenvy::from_path(&path) {
        Ok(()) => Ok(true),
        Err(dotenvy::Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(dotenvy::Error::Io(err)) => {
            Err(ConfigError::io(format!("load {}", path.display()), err))
        }
        Err(err) => Err(ConfigError::invalid(format!(
            "load {}: {err}",
            path.display()
        ))),
    }
}

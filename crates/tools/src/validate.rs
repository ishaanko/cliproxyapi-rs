//! `validate_codex_models`: validates a Codex client model catalog file
//! (Go: cmd/validate_codex_models).

use crate::flags::{self, FlagDef, Kind};

const FLAGS: &[FlagDef] = &[FlagDef { name: "file", kind: Kind::Str, default: "", usage: "Codex client model catalog JSON file" }];

/// Program entry; returns the process exit code (2 for usage errors, 1 for an invalid catalog).
pub fn run(args: Vec<String>) -> i32 {
    let parsed = match flags::parse("validate_codex_models", FLAGS, &args) {
        Ok(p) => p,
        Err(flags::Exit(code)) => return code,
    };
    let input_path = parsed.string("file");
    if input_path.trim().is_empty() {
        eprintln!("error: --file is required");
        return 2;
    }
    match validate_file(&input_path) {
        Ok(()) => {
            println!("Validated Codex client model catalog: {input_path}");
            0
        }
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

/// Reads and validates the catalog; errors carry the Go `error: ...` messages.
fn validate_file(path: &str) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("error: read {path}: {}", io_message(&e)))?;
    cpa_core::registry::validate_codex_client_models_json(&data)
        .map_err(|e| format!("error: invalid Codex client model catalog {path}: {e}"))
}

/// Go-style OS error text (`no such file or directory`), lowercase and without the errno suffix.
fn io_message(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or(&text);
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_lowercase(), chars.as_str()),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn requires_a_file_flag() {
        assert_eq!(run(args(&[])), 2);
        assert_eq!(run(args(&["--file", "  "])), 2);
    }

    #[test]
    fn reports_unreadable_and_invalid_catalogs() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        let err = validate_file(missing.to_str().unwrap()).unwrap_err();
        assert!(err.starts_with("error: read ") && err.ends_with(": no such file or directory"), "{err}");

        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{").unwrap();
        let err = validate_file(bad.to_str().unwrap()).unwrap_err();
        assert!(err.starts_with("error: invalid Codex client model catalog "), "{err}");
        assert_eq!(run(args(&["-file", bad.to_str().unwrap()])), 1);
    }

    #[test]
    fn accepts_the_embedded_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        std::fs::write(&path, cpa_core::registry::get_codex_client_models_json()).unwrap();
        assert_eq!(run(args(&["--file", path.to_str().unwrap()])), 0);
    }
}

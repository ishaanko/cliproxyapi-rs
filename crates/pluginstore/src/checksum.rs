//! `checksums.txt` parsing and archive checksum verification (Go `checksum.go`).

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::errf;
use crate::registry::hex_error_text;

/// Parses `sha256  name` lines (a leading `*` on the name marks binary mode).
pub fn parse_checksums(data: &[u8]) -> Result<HashMap<String, String>> {
    let text = String::from_utf8_lossy(data);
    let mut out = HashMap::new();
    for (line_number, raw_line) in text.split('\n').enumerate() {
        let line_number = line_number + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 {
            return Err(errf!("line {line_number}: invalid checksum entry"));
        }
        let hash = fields[0].trim().to_lowercase();
        if hash.len() != 64 {
            return Err(errf!("line {line_number}: invalid sha256 length"));
        }
        if let Err(err) = hex::decode(&hash) {
            return Err(errf!("line {line_number}: invalid sha256: {}", hex_error_text(&err)));
        }
        let name = fields[1].trim();
        let name = name.strip_prefix('*').unwrap_or(name);
        out.insert(name.to_string(), hash);
    }
    Ok(out)
}

/// Verifies `data` against the checksum recorded for `name`.
pub fn verify_checksum(name: &str, data: &[u8], checksums: &HashMap<String, String>) -> Result<()> {
    let expected = checksums.get(name).map(|s| s.trim().to_lowercase()).unwrap_or_default();
    if expected.is_empty() {
        return Err(errf!("checksum for {name} not found"));
    }
    if hex::encode(Sha256::digest(data)) != expected {
        return Err(errf!("checksum mismatch for {name}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_verify() {
        let data = b"zip-data";
        let text = format!("{}  sample-provider_0.1.0_darwin_arm64.zip\n", hex::encode(Sha256::digest(data)));
        let checksums = parse_checksums(text.as_bytes()).expect("parse");
        verify_checksum("sample-provider_0.1.0_darwin_arm64.zip", data, &checksums).expect("verify");
    }

    #[test]
    fn rejects_missing_and_mismatch() {
        let mut checksums = HashMap::new();
        checksums.insert("sample-provider.zip".to_string(), hex::encode(Sha256::digest(b"zip-data")));
        assert!(verify_checksum("missing.zip", b"zip-data", &checksums).is_err());
        assert!(verify_checksum("sample-provider.zip", b"other", &checksums).is_err());
    }
}

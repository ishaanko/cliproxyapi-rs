//! Session identity helpers for executors (Go: helps/cpa_session.go and derived_session.go).

use cpa_runtime::conductor::session::{canonical_session_id, identity::derived_id};
use cpa_runtime::executor::{Metadata, Options, meta};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The internal session id used to expand `$CPA-SESSION-ID` in custom headers.
///
/// Go annotates the request context; here the result is passed to
/// `cpa_core::util::apply_custom_headers_from_attrs` as its `session_id`. Resolution order:
/// an explicit annotation (`existing`, where `Some("")` is an explicit clear and wins), then the
/// session id of the client request (`client_session_id`), then the canonical id derived from the
/// request headers, payload and metadata. `None` when no id can be determined.
pub fn ensure_session_id(
    existing: Option<&str>,
    client_session_id: &str,
    opts: &Options,
    payload: &[u8],
) -> Option<String> {
    if let Some(id) = existing {
        return Some(id.to_string());
    }
    if !client_session_id.is_empty() {
        return Some(client_session_id.to_string());
    }
    let eval_payload: &[u8] = if opts.original_request.is_empty() { payload } else { &opts.original_request };
    let canonical = canonical_session_id(&opts.headers, eval_payload, &opts.metadata);
    (!canonical.is_empty()).then_some(canonical)
}

/// The first context-derived session identity in metadata order.
pub fn derived_session_id(metadata_sets: &[&Metadata]) -> String {
    metadata_sets.iter().map(|m| derived_id(m)).find(|id| !id.is_empty()).unwrap_or_default()
}

/// Maps a derived session identity to a provider-scoped stable UUID (SHA-1 name UUID in the OID
/// namespace), "" when there is none.
pub fn derived_session_uuid(provider: &str, metadata_sets: &[&Metadata]) -> String {
    stable_provider_session_uuid(provider, "derived-session", &derived_session_id(metadata_sets))
}

/// Prefers a long-lived execution session and falls back to the derived identity.
pub fn provider_session_uuid(provider: &str, metadata_sets: &[&Metadata]) -> String {
    for metadata in metadata_sets {
        let execution_id = metadata_string(metadata, meta::EXECUTION_SESSION_ID);
        if !execution_id.is_empty() {
            return stable_provider_session_uuid(provider, "execution-session", &execution_id);
        }
    }
    derived_session_uuid(provider, metadata_sets)
}

fn stable_provider_session_uuid(provider: &str, kind: &str, identity_value: &str) -> String {
    let provider = provider.trim().to_lowercase();
    let identity_value = identity_value.trim();
    if provider.is_empty() || identity_value.is_empty() {
        return String::new();
    }
    let identity = ["cli-proxy-api", provider.as_str(), kind, identity_value].join("\0");
    Uuid::new_v5(&Uuid::NAMESPACE_OID, identity.as_bytes()).to_string()
}

/// Maps a derived session identity to Antigravity's negative decimal session id, "" when none.
pub fn derived_antigravity_session_id(metadata_sets: &[&Metadata]) -> String {
    let derived = derived_session_id(metadata_sets);
    if derived.is_empty() {
        return String::new();
    }
    let sum = Sha256::digest(format!("cli-proxy-api:antigravity:derived-session\0{derived}").as_bytes());
    let value = u64::from_be_bytes(sum[..8].try_into().unwrap_or([0; 8])) & 0x7FFF_FFFF_FFFF_FFFF;
    format!("-{value}")
}

fn metadata_string(metadata: &Metadata, key: &str) -> String {
    metadata.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn md(pairs: &[(&str, &str)]) -> Metadata {
        pairs.iter().map(|(k, v)| (k.to_string(), json!(v))).collect()
    }

    #[test]
    fn derived_ids_are_provider_scoped_and_stable() {
        let m = md(&[(meta::DERIVED_SESSION_ID, " ctx:v1:abc ")]);
        let a = derived_session_uuid("Claude", &[&m]);
        assert_eq!(a, derived_session_uuid("claude", &[&m]));
        assert_ne!(a, derived_session_uuid("codex", &[&m]));
        assert!(Uuid::parse_str(&a).is_ok());
        assert_eq!(derived_session_uuid("claude", &[&Metadata::new()]), "");
        assert_eq!(derived_session_uuid("", &[&m]), "");
        let ag = derived_antigravity_session_id(&[&m]);
        assert!(ag.starts_with('-') && ag[1..].parse::<i64>().is_ok());
        assert_eq!(ag, derived_antigravity_session_id(&[&Metadata::new(), &m]));
    }

    #[test]
    fn execution_session_wins_over_derived() {
        let m = md(&[(meta::EXECUTION_SESSION_ID, "exec-1"), (meta::DERIVED_SESSION_ID, "d")]);
        let exec = provider_session_uuid("codex", &[&m]);
        assert_ne!(exec, derived_session_uuid("codex", &[&m]));
        let only_derived = md(&[(meta::DERIVED_SESSION_ID, "d")]);
        assert_eq!(provider_session_uuid("codex", &[&only_derived]), derived_session_uuid("codex", &[&only_derived]));
    }

    #[test]
    fn session_resolution_order() {
        let opts = Options::new(cpa_translator::Format::OpenAI);
        assert_eq!(ensure_session_id(Some(""), "c", &opts, b"{}"), Some(String::new()));
        assert_eq!(ensure_session_id(Some("x"), "c", &opts, b"{}"), Some("x".into()));
        assert_eq!(ensure_session_id(None, "c", &opts, b"{}"), Some("c".into()));
    }
}

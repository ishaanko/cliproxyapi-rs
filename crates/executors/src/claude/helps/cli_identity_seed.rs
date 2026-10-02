//! Stable identity seeds for `fingerprint-profile: claude-code-cli` on non-OAuth credentials (Go:
//! helps/claude_cli_identity_seed.go). Real OAuth credentials keep their stored account and device
//! pool; this only fills gaps so `apply_claude_credential_metadata` stays the single identity path.

use std::borrow::Cow;

use cpa_auth::Auth;
use cpa_auth::claude::{DEVICE_IDS_METADATA_KEY, ensure_device_id_pool};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::credential_identity::{claude_credential_account_uuid, has_canonical_device_id_pool};

/// Go: `StableClaudeCLIDeviceID`: hex sha256 of `cpa-claude-code-cli-device|<seed>`.
pub fn stable_claude_cli_device_id(seed: &str) -> String {
    hex::encode(Sha256::digest(format!("cpa-claude-code-cli-device|{seed}").as_bytes()))
}

/// Go: `StableClaudeCLIAccountUUID`: v5 UUID (OID namespace) of `cpa-claude-code-cli-account|<seed>`.
pub fn stable_claude_cli_account_uuid(seed: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("cpa-claude-code-cli-account|{seed}").as_bytes()).to_string()
}

/// Go: `ClaudeCLIAuthIdentitySeed`: a credential identity that does not rotate with delegated
/// provider access tokens; "" when the auth has none.
pub fn claude_cli_auth_identity_seed(auth: &Auth) -> String {
    let id = auth.id.trim();
    if !id.is_empty() {
        return format!("auth-id|{id}");
    }
    let index = auth.index.trim();
    if !index.is_empty() {
        return format!("auth-index|{index}");
    }
    let file_name = auth.file_name.trim();
    if !file_name.is_empty() {
        return format!("auth-file|{file_name}");
    }
    String::new()
}

/// Go: `PrepareClaudeCLIFingerprintAuth`: the auth that should receive
/// `apply_claude_credential_metadata`. Synthesized identity goes into a clone so the shared
/// credential is never mutated on the request path.
pub fn prepare_claude_cli_fingerprint_auth<'a>(
    auth: &'a Auth,
    seed: &str,
    synthesize_missing: bool,
) -> Cow<'a, Auth> {
    if !synthesize_missing {
        return Cow::Borrowed(auth);
    }
    let mut local = auth.clone();
    ensure_claude_cli_fingerprint_identity(&mut local, seed, true);
    Cow::Owned(local)
}

/// Go: `EnsureClaudeCLIFingerprintIdentity`: with `synthesize_missing`, fills a missing
/// `account_uuid` and device pool from `seed` (blank seed means `anonymous`); otherwise a no-op so
/// missing OAuth identity surfaces as a credential error.
pub fn ensure_claude_cli_fingerprint_identity(auth: &mut Auth, seed: &str, synthesize_missing: bool) {
    if !synthesize_missing {
        return;
    }
    let seed = match seed.trim() {
        "" => "anonymous",
        trimmed => trimmed,
    };
    if claude_credential_account_uuid(auth).is_empty() {
        auth.metadata.insert("account_uuid".into(), Value::String(stable_claude_cli_account_uuid(seed)));
    }
    if !has_canonical_device_id_pool(auth.metadata.get(DEVICE_IDS_METADATA_KEY)) {
        auth.metadata.insert(
            DEVICE_IDS_METADATA_KEY.into(),
            Value::Array(vec![Value::String(stable_claude_cli_device_id(seed))]),
        );
    }
    ensure_device_id_pool(&mut auth.metadata);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::helps::credential_identity::apply_claude_credential_metadata;
    use crate::helps::id_cache::is_valid_user_id;
    use cpa_json::J;
    use serde_json::json;

    const SESSION: &str = "11111111-2222-4333-8444-555555555555";

    #[test]
    fn synthesizes_stable_sources_and_feeds_credential_metadata() {
        let mut auth = Auth::default();
        ensure_claude_cli_fingerprint_identity(&mut auth, "key-a", true);
        let account = claude_credential_account_uuid(&auth);
        assert!(!account.is_empty());
        let pool = ensure_device_id_pool(&mut auth.metadata).0;
        assert_eq!(pool, vec![stable_claude_cli_device_id("key-a")]);

        // A second call must not rotate identity.
        ensure_claude_cli_fingerprint_identity(&mut auth, "key-a", true);
        assert_eq!(claude_credential_account_uuid(&auth), account);

        let (updated, device) = apply_claude_credential_metadata(br#"{"messages":[]}"#, &mut auth, SESSION).expect("apply");
        assert_eq!(device, pool[0]);
        let root = cpa_json::parse(&updated);
        let user_id = root.g("metadata.user_id").str();
        assert!(is_valid_user_id(&user_id));
        assert_eq!(cpa_json::parse_str(&user_id).g("account_uuid").str(), account);
        assert_eq!(cpa_json::parse_str(&user_id).g("session_id").str(), SESSION);
    }

    #[test]
    fn identity_seed_prefers_stable_auth_identity() {
        let mut auth = Auth::default();
        assert_eq!(claude_cli_auth_identity_seed(&auth), "");
        auth.file_name = "kimi.json".into();
        assert_eq!(claude_cli_auth_identity_seed(&auth), "auth-file|kimi.json");
        auth.index = "kimi-index".into();
        assert_eq!(claude_cli_auth_identity_seed(&auth), "auth-index|kimi-index");
        auth.id = "kimi-auth".into();
        assert_eq!(claude_cli_auth_identity_seed(&auth), "auth-id|kimi-auth");
    }

    #[test]
    fn prepare_does_not_mutate_shared_metadata() {
        let mut shared = Auth::new("kimi-shared", "claude");
        shared.metadata.insert("access_token".into(), json!("token-1"));
        let seed = claude_cli_auth_identity_seed(&shared);
        let prepared = prepare_claude_cli_fingerprint_auth(&shared, &seed, true);
        assert!(matches!(prepared, Cow::Owned(_)));
        assert!(claude_credential_account_uuid(&shared).is_empty());
        assert!(!claude_credential_account_uuid(&prepared).is_empty());
        assert!(!shared.metadata.contains_key(DEVICE_IDS_METADATA_KEY));
        assert!(matches!(prepare_claude_cli_fingerprint_auth(&shared, &seed, false), Cow::Borrowed(_)));
    }

    #[test]
    fn noop_without_synthesize_and_preserves_existing_oauth_identity() {
        let mut auth = Auth::default();
        ensure_claude_cli_fingerprint_identity(&mut auth, "key-a", false);
        assert!(claude_credential_account_uuid(&auth).is_empty());

        let device = "b".repeat(64);
        auth.metadata.insert("account_uuid".into(), json!("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"));
        auth.metadata.insert(DEVICE_IDS_METADATA_KEY.into(), json!([device]));
        ensure_claude_cli_fingerprint_identity(&mut auth, "key-a", true);
        assert_eq!(claude_credential_account_uuid(&auth), "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        assert_eq!(ensure_device_id_pool(&mut auth.metadata).0, vec![device]);
    }
}

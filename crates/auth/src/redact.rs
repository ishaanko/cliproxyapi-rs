//! `Debug` for types that hold credentials: only identifying, non-secret fields are printed, so a
//! stray `{:?}` in a log line cannot leak tokens.

macro_rules! secret_debug {
    ($ty:ty { $($field:ident),* $(,)? }) => {
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($field), &self.$field))*
                    .finish_non_exhaustive()
            }
        }
    };
}

use crate::{antigravity, claude, codex, kimi, meta, storage, xai};

secret_debug!(storage::ClaudeTokenStorage { email });
secret_debug!(storage::CodexTokenStorage { email, plan_type });
secret_debug!(storage::XaiTokenStorage { email, base_url });
secret_debug!(storage::KimiTokenStorage { domain, base_url });
secret_debug!(storage::VertexCredentialStorage {
    project_id,
    email,
    location
});
secret_debug!(storage::MetaTokenStorage { email, base_url });
secret_debug!(claude::ClaudeTokenData { email });
secret_debug!(claude::ClaudeAuthBundle { last_refresh });
secret_debug!(codex::CodexTokenData { email, plan_type });
secret_debug!(codex::CodexAuthBundle { last_refresh });
secret_debug!(codex::DeviceTokenResponse {});
secret_debug!(xai::TokenData {
    email,
    subject,
    token_type
});
secret_debug!(xai::AuthBundle {
    base_url,
    token_endpoint
});
secret_debug!(kimi::KimiTokenData { token_type, scope });
secret_debug!(kimi::KimiAuthBundle {});
secret_debug!(antigravity::TokenResponse {
    token_type,
    expires_in
});
secret_debug!(meta::TokenData {
    token_type,
    expires_in,
    error
});
secret_debug!(meta::MintedKeyResponse {
    base_url,
    subs_tier_name
});
secret_debug!(meta::MetaAuthBundle { email });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_tokens() {
        let s = storage::ClaudeTokenStorage {
            access_token: "sk-ant-oat01-SECRET".into(),
            refresh_token: "sk-ant-ort01-SECRET".into(),
            email: "u@x.com".into(),
            ..Default::default()
        };
        let text = format!("{s:?}");
        assert!(
            text.contains("u@x.com") && !text.contains("SECRET"),
            "{text}"
        );

        let mut auth = crate::types::Auth {
            storage: Some(crate::storage::TokenStorage::Claude(s)),
            ..Default::default()
        };
        auth.metadata.insert("access_token".into(), "SECRET".into());
        assert!(!format!("{auth:?}").contains("SECRET"));

        let td = kimi::KimiTokenData {
            access_token: "SECRET".into(),
            ..Default::default()
        };
        assert!(!format!("{td:?}").contains("SECRET"));
        let dev = codex::DeviceTokenResponse {
            authorization_code: "SECRET".into(),
            ..Default::default()
        };
        assert!(!format!("{dev:?}").contains("SECRET"));
    }
}

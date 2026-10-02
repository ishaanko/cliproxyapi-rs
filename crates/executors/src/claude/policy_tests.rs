//! Fingerprint policy tests ported from claude_fingerprint_policy_test.go.

use cpa_auth::Auth;
use cpa_config::{ClaudeKey, Config};
use serde_json::Value;

use super::policy::resolve_claude_fingerprint_policy;
use super::signing::{ClaudeCchUpstreamKind, claude_cch_signing_enabled};

struct Case {
    name: &'static str,
    provider: &'static str,
    api_key: &'static str,
    attrs: &'static [(&'static str, &'static str)],
    metadata: &'static [(&'static str, &'static str)],
    cfg: Option<Config>,
    auth_oauth: bool,
    profile: bool,
    synthesize: bool,
    cancellation: bool,
}

fn base(name: &'static str, api_key: &'static str) -> Case {
    Case {
        name,
        provider: "",
        api_key,
        attrs: &[],
        metadata: &[],
        cfg: None,
        auth_oauth: false,
        profile: false,
        synthesize: false,
        cancellation: false,
    }
}

#[test]
fn resolves_fingerprint_policy_per_credential() {
    let gateway_cfg = Config {
        claude_key: vec![ClaudeKey {
            api_key: "key-config".into(),
            base_url: "https://gateway.example".into(),
            fingerprint_profile: "claude-code-cli".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let cases = vec![
        Case {
            attrs: &[("api_key", "sk-ant-oat-real")],
            auth_oauth: true,
            profile: true,
            cancellation: true,
            ..base("real oauth token", "sk-ant-oat-real")
        },
        Case { attrs: &[("api_key", "key-default")], ..base("api key default", "key-default") },
        Case {
            attrs: &[("api_key", "key-attr"), ("fingerprint_profile", "claude-code-cli")],
            profile: true,
            synthesize: true,
            ..base("claude-code-cli attribute", "key-attr")
        },
        Case {
            attrs: &[("api_key", "key-attr-legacy"), ("fingerprint_profile", "oauth-cli")],
            profile: true,
            synthesize: true,
            ..base("oauth-cli alias", "key-attr-legacy")
        },
        Case {
            attrs: &[
                ("api_key", "key-attr-gateway"),
                ("base_url", "https://gateway.example"),
                ("fingerprint_profile", "claude-code-cli"),
            ],
            profile: true,
            synthesize: true,
            ..base("gateway attribute", "key-attr-gateway")
        },
        Case {
            attrs: &[("api_key", "key-config"), ("base_url", "https://gateway.example")],
            cfg: Some(gateway_cfg),
            profile: true,
            synthesize: true,
            ..base("config entry", "key-config")
        },
        Case {
            attrs: &[("api_key", "key-metadata")],
            metadata: &[("fingerprint_profile", "claude-code-cli")],
            profile: true,
            synthesize: true,
            ..base("metadata profile", "key-metadata")
        },
        Case {
            provider: "kimi",
            metadata: &[("access_token", "kimi-access-token")],
            ..base("kimi default token has no fingerprint", "kimi-access-token")
        },
        Case {
            provider: "kimi",
            metadata: &[("access_token", "kimi-access-token"), ("fingerprint_profile", "claude-code-cli")],
            profile: true,
            synthesize: true,
            ..base("kimi opts in", "kimi-access-token")
        },
        Case {
            provider: "kimi",
            metadata: &[("access_token", "kimi-access-token"), ("fingerprint-profile", "claude-code-cli")],
            profile: true,
            synthesize: true,
            ..base("kimi hyphenated profile", "kimi-access-token")
        },
        Case {
            attrs: &[("api_key", "key-unknown"), ("fingerprint_profile", "not-a-profile")],
            ..base("unknown profile ignored", "key-unknown")
        },
    ];
    for case in cases {
        let mut auth = Auth::new("a", if case.provider.is_empty() { "claude" } else { case.provider });
        for (k, v) in case.attrs {
            auth.attributes.insert((*k).into(), (*v).into());
        }
        for (k, v) in case.metadata {
            auth.metadata.insert((*k).into(), Value::String((*v).into()));
        }
        let cfg = case.cfg.unwrap_or_default();
        let fp = resolve_claude_fingerprint_policy(&cfg, &auth, case.api_key);
        assert_eq!(fp.auth_is_oauth_token, case.auth_oauth, "{}", case.name);
        assert_eq!(fp.profile_claude_code_cli, case.profile, "{}", case.name);
        assert_eq!((fp.use_oauth_betas, fp.apply_cli_identity), (case.profile, case.profile), "{}", case.name);
        assert_eq!(fp.synthesize_identity, case.synthesize, "{}", case.name);
        assert_eq!((fp.mcp_alias, fp.inject_diagnostics), (case.profile, case.profile), "{}", case.name);
        assert_eq!(fp.oauth_cancellation, case.cancellation, "{}", case.name);
    }
}

/// The policy follows the credential; the upstream origin only decides CCH signing.
#[test]
fn policy_is_origin_independent_and_only_signing_follows_origin() {
    let cfg = Config {
        claude_key: vec![ClaudeKey {
            api_key: "key-origin-independent".into(),
            fingerprint_profile: "claude-code-cli".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut auth = Auth::new("a", "claude");
    auth.attributes.insert("api_key".into(), "key-origin-independent".into());
    let policy = resolve_claude_fingerprint_policy(&cfg, &auth, "key-origin-independent");
    assert!(policy.profile_claude_code_cli && !policy.auth_is_oauth_token);
    for origin in [
        "https://api.anthropic.com/v1/messages?beta=true",
        "https://gateway.example/v1/messages?beta=true",
        "https://api.kimi.com/v1/messages",
    ] {
        let want = origin == "https://api.anthropic.com/v1/messages?beta=true";
        assert_eq!(
            claude_cch_signing_enabled("key-origin-independent", ClaudeCchUpstreamKind::Anthropic, policy.profile_claude_code_cli, origin),
            want,
            "{origin}"
        );
    }
}

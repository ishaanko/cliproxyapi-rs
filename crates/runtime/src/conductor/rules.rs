//! Request-scoped error rules (Go: conductor_request_scoped_errors.go).
//!
//! Operators can classify specific upstream errors per credential/provider: a rule matches on
//! status plus a body substring or regex and picks one of four actions. Rules are evaluated before
//! the built-in classification.

use cpa_auth::types::{AUTH_KIND_OAUTH, Auth};
use cpa_config::{Config, RequestScopedErrorRule};
use regex::Regex;
use serde_json::Value;

use super::cooldown::ExecResult;
use super::errors::{CODE_FORCE_COOLDOWN, CODE_REQUEST_SCOPED, auth_error_base_message};
use super::models::resolve_openai_compat_config_for_auth;
use crate::executor::ExecError;

/// Compiled rule patterns, cached (invalid patterns are cached as `None` and never match).
static REGEX_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, Option<Regex>>>,
> = std::sync::LazyLock::new(Default::default);

fn regex_matches(pattern: &str, body: &str) -> bool {
    let re = {
        let mut cache = REGEX_CACHE.lock();
        if cache.len() > 256 && !cache.contains_key(pattern) {
            cache.clear();
        }
        cache
            .entry(pattern.to_string())
            .or_insert_with(|| Regex::new(pattern).ok())
            .clone()
    };
    re.is_some_and(|re| re.is_match(body))
}

pub const ACTION_STOP: &str = "stop";
pub const ACTION_STOP_AND_COOLDOWN: &str = "stop-and-cooldown";
pub const ACTION_CONTINUE: &str = "continue";
pub const ACTION_CONTINUE_AND_COOLDOWN: &str = "continue-and-cooldown";

/// Rule list for an auth: metadata first, then OAuth config by provider, else the config entry
/// that backs the API-key/compat credential.
pub fn extract_rules(auth: &Auth, cfg: &Config) -> Vec<RequestScopedErrorRule> {
    let raw = auth
        .metadata
        .get("request_scoped_errors")
        .or_else(|| auth.metadata.get("request-scoped-errors"));
    if let Some(Value::Array(items)) = raw
        && let Ok(rules) =
            serde_json::from_value::<Vec<RequestScopedErrorRule>>(Value::Array(items.clone()))
        && !rules.is_empty()
    {
        return rules;
    }
    let provider = auth.provider.trim().to_lowercase();
    if auth.auth_kind() == AUTH_KIND_OAUTH {
        return cfg
            .oauth_request_scoped_errors
            .get(&provider)
            .filter(|r| !r.is_empty())
            .cloned()
            .unwrap_or_default();
    }

    let index = auth.attr("config_index").parse::<usize>().ok();
    let provider_key = auth
        .attributes
        .get("provider_key")
        .cloned()
        .unwrap_or_default();
    let mut compat_name = auth
        .attributes
        .get("compat_name")
        .cloned()
        .unwrap_or_default();
    if compat_name.is_empty() {
        if let Some(rest) = provider.strip_prefix("openai-compatible-") {
            compat_name = rest.to_string();
        } else if let Some(rest) = provider.strip_prefix("openai-compatibility:") {
            compat_name = rest.to_string();
        }
    }
    if !compat_name.is_empty()
        || !provider_key.is_empty()
        || provider == "openai-compatibility"
        || provider.starts_with("openai-compatibility:")
        || provider.starts_with("openai-compatible")
    {
        if let Some(entry) =
            resolve_openai_compat_config_for_auth(cfg, auth, &provider_key, &compat_name)
        {
            return entry.request_scoped_errors.clone();
        }
    }
    let at = |len: usize| index.filter(|i| *i < len);
    match provider.as_str() {
        "claude" => {
            at(cfg.claude_key.len()).map(|i| cfg.claude_key[i].request_scoped_errors.clone())
        }
        "codex" => at(cfg.codex_key.len()).map(|i| cfg.codex_key[i].request_scoped_errors.clone()),
        "xai" => at(cfg.xai_key.len()).map(|i| cfg.xai_key[i].request_scoped_errors.clone()),
        "meta" => at(cfg.meta_key.len()).map(|i| cfg.meta_key[i].request_scoped_errors.clone()),
        "gemini" => {
            at(cfg.gemini_key.len()).map(|i| cfg.gemini_key[i].request_scoped_errors.clone())
        }
        "interactions" | "gemini-interactions" => at(cfg.interactions_key.len())
            .map(|i| cfg.interactions_key[i].request_scoped_errors.clone()),
        _ => None,
    }
    .unwrap_or_default()
}

/// Body text rules match against: upstream body, else the conductor message, else `Error()`.
fn extract_error_body(err: &ExecError) -> String {
    if let Some(body) = &err.body
        && !body.is_empty()
    {
        return String::from_utf8_lossy(body).into_owned();
    }
    if err.auth_code.is_some() {
        let msg = auth_error_base_message(err);
        if !msg.is_empty() {
            return msg;
        }
    }
    err.message.clone()
}

/// First matching rule's action for the error, if any.
pub fn match_action(auth: &Auth, err: &ExecError, cfg: &Config) -> Option<&'static str> {
    let rules = extract_rules(auth, cfg);
    if rules.is_empty() {
        return None;
    }
    let status = err.status as i64;
    let body = extract_error_body(err);
    for rule in &rules {
        if rule.status <= 0 || rule.status != status {
            continue;
        }
        if rule.r#match.is_empty() && rule.match_regexr.is_empty() {
            continue;
        }
        let mut matched = rule
            .r#match
            .iter()
            .any(|s| !s.is_empty() && body.contains(s.as_str()));
        if !matched {
            matched = rule
                .match_regexr
                .iter()
                .any(|p| !p.is_empty() && regex_matches(p, &body));
        }
        if !matched {
            continue;
        }
        match rule.action.trim().to_lowercase().as_str() {
            ACTION_STOP => return Some(ACTION_STOP),
            ACTION_STOP_AND_COOLDOWN => return Some(ACTION_STOP_AND_COOLDOWN),
            ACTION_CONTINUE => return Some(ACTION_CONTINUE),
            ACTION_CONTINUE_AND_COOLDOWN => return Some(ACTION_CONTINUE_AND_COOLDOWN),
            _ => continue,
        }
    }
    None
}

/// `stop`/`continue` mark the failure request-scoped (no cooldown); the `-and-cooldown` forms
/// force a cooldown.
pub fn apply_action_to_result(action: Option<&str>, result: &mut ExecResult) {
    let (Some(action), Some(err)) = (action, result.error.as_mut()) else {
        return;
    };
    match action {
        ACTION_STOP | ACTION_CONTINUE => err.code = CODE_REQUEST_SCOPED.into(),
        ACTION_STOP_AND_COOLDOWN | ACTION_CONTINUE_AND_COOLDOWN => {
            err.code = CODE_FORCE_COOLDOWN.into()
        }
        _ => {}
    }
}

pub fn is_stop(action: Option<&str>) -> bool {
    matches!(action, Some(ACTION_STOP) | Some(ACTION_STOP_AND_COOLDOWN))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(status: i64, m: &str, action: &str) -> RequestScopedErrorRule {
        RequestScopedErrorRule {
            status,
            r#match: vec![m.into()],
            match_regexr: vec![],
            action: action.into(),
        }
    }

    fn claude_key_auth() -> (Config, Auth) {
        let mut cfg = Config::default();
        cfg.claude_key.push(cpa_config::ClaudeKey {
            api_key: "sk".into(),
            request_scoped_errors: vec![
                rule(400, "content filter", "STOP"),
                RequestScopedErrorRule {
                    status: 500,
                    r#match: vec![],
                    match_regexr: vec!["overload(ed)?".into()],
                    action: "continue-and-cooldown".into(),
                },
                rule(429, "x", "bogus"),
            ],
            ..Default::default()
        });
        let mut auth = Auth::new("k", "claude");
        auth.attributes.insert("api_key".into(), "sk".into());
        auth.attributes.insert("config_index".into(), "0".into());
        auth.attributes
            .insert("source".into(), "config:claude[abc]".into());
        (cfg, auth)
    }

    #[test]
    fn rules_match_status_and_body_and_normalize_action() {
        let (cfg, auth) = claude_key_auth();
        let e = ExecError::new(400, "blocked by content filter");
        assert_eq!(match_action(&auth, &e, &cfg), Some(ACTION_STOP));
        assert_eq!(
            match_action(&auth, &ExecError::new(400, "other"), &cfg),
            None
        );
        assert_eq!(
            match_action(&auth, &ExecError::new(500, "server overloaded"), &cfg),
            Some(ACTION_CONTINUE_AND_COOLDOWN)
        );
        // Unknown actions never match; status must be equal.
        assert_eq!(match_action(&auth, &ExecError::new(429, "x"), &cfg), None);
        assert_eq!(
            match_action(&auth, &ExecError::new(401, "content filter"), &cfg),
            None
        );
    }

    #[test]
    fn metadata_rules_override_and_apply_to_result() {
        let (cfg, mut auth) = claude_key_auth();
        auth.metadata.insert(
            "request-scoped-errors".into(),
            serde_json::json!([{"status": 502, "match": ["bad"], "action": "continue"}]),
        );
        assert_eq!(
            match_action(&auth, &ExecError::new(502, "bad gateway"), &cfg),
            Some(ACTION_CONTINUE)
        );
        assert_eq!(
            match_action(&auth, &ExecError::new(400, "content filter"), &cfg),
            None
        );

        let mut result = ExecResult {
            auth_id: "k".into(),
            provider: "claude".into(),
            model: "m".into(),
            route_model: "m".into(),
            success: false,
            retry_after: None,
            credential_scope: false,
            error: Some(Default::default()),
            options: crate::executor::Options::new(cpa_translator::Format::OpenAI),
            skip_quota_observation: true,
            response_headers: Default::default(),
        };
        apply_action_to_result(Some(ACTION_STOP_AND_COOLDOWN), &mut result);
        assert_eq!(result.error.as_ref().unwrap().code, CODE_FORCE_COOLDOWN);
        assert!(is_stop(Some(ACTION_STOP_AND_COOLDOWN)) && !is_stop(Some(ACTION_CONTINUE)));
    }
}

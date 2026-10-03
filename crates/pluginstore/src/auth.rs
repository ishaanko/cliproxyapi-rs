//! Plugin store auth rules, secrets and request validation (Go `auth.go`).

use std::fmt;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroize;

use crate::error::Result;
use crate::errf;
use crate::goturl::{GoUrl, path_escape};
use crate::http::Headers;
use crate::registry::{
    INSTALL_TYPE_DIRECT, INSTALL_TYPE_GITHUB_RELEASE, Plugin, Source, github_repository_parts,
    has_sensitive_query_parameter, is_false, null_default, plugin_artifacts, plugin_install_type,
};

pub const REQUEST_KIND_REGISTRY: &str = "registry";
pub const REQUEST_KIND_METADATA: &str = "metadata";
pub const REQUEST_KIND_ARTIFACT: &str = "artifact";

pub const AUTH_TYPE_NONE: &str = "none";
pub const AUTH_TYPE_BEARER: &str = "bearer";
pub const AUTH_TYPE_BASIC: &str = "basic";
pub const AUTH_TYPE_HEADER: &str = "header";
pub const AUTH_TYPE_GITHUB_TOKEN: &str = "github-token";

/// Environment lookup override (tests and embedders); the default reads the process env.
pub type EnvFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Env reader handed to auth helpers: `getenv` semantics (unset reads as empty).
#[derive(Clone, Copy)]
pub(crate) struct Env<'a>(pub(crate) Option<&'a EnvFn>);

impl Env<'_> {
    pub(crate) fn get(&self, name: &str) -> String {
        match self.0 {
            Some(lookup) => lookup(name),
            None => std::env::var(name).unwrap_or_default(),
        }
    }
}

/// Auth rule from `plugins.store-auth`. JSON uses snake_case, YAML kebab-case.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    #[serde(rename = "match", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub match_url: String,
    #[serde(rename = "apply_to", alias = "apply-to", skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub apply_to: Vec<String>,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub kind: String,
    #[serde(rename = "token_env", alias = "token-env", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub token_env: String,
    #[serde(rename = "username_env", alias = "username-env", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub username_env: String,
    #[serde(rename = "password_env", alias = "password-env", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub password_env: String,
    #[serde(rename = "header_name", alias = "header-name", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub header_name: String,
    #[serde(rename = "header_value_env", alias = "header-value-env", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub header_value_env: String,
    #[serde(rename = "allow_insecure", alias = "allow-insecure", skip_serializing_if = "is_false", deserialize_with = "null_default")]
    pub allow_insecure: bool,
}

impl From<&cpa_config::PluginStoreAuth> for AuthConfig {
    fn from(item: &cpa_config::PluginStoreAuth) -> Self {
        Self {
            match_url: item.r#match.clone(),
            apply_to: item.apply_to.clone(),
            kind: item.kind.clone(),
            token_env: item.token_env.clone(),
            username_env: item.username_env.clone(),
            password_env: item.password_env.clone(),
            header_name: item.header_name.clone(),
            header_value_env: item.header_value_env.clone(),
            allow_insecure: item.allow_insecure,
        }
    }
}

/// Short-lived credential material that is overwritten on [`Secret::clear`] and on drop.
/// Serialized as base64 (Go `[]byte`), never as plain text.
#[derive(Clone, Default, PartialEq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Lossy UTF-8 view, for building header values.
    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }

    /// Overwrites the secret and releases its storage.
    pub fn clear(&mut self) {
        self.0.zeroize();
        self.0 = Vec::new();
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.as_bytes().to_vec())
    }
}

impl From<Vec<u8>> for Secret {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let Some(encoded) = Option::<String>::deserialize(deserializer)? else {
            return Ok(Secret::default());
        };
        STANDARD.decode(encoded).map(Secret).map_err(serde::de::Error::custom)
    }
}

/// Auth rule with credentials already resolved (supplied by CLIProxyAPIHome).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResolvedAuthConfig {
    #[serde(rename = "match", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub match_url: String,
    #[serde(rename = "apply_to", skip_serializing_if = "Vec::is_empty", deserialize_with = "null_default")]
    pub apply_to: Vec<String>,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub kind: String,
    #[serde(skip_serializing_if = "Secret::is_empty", deserialize_with = "null_default")]
    pub token: Secret,
    #[serde(skip_serializing_if = "Secret::is_empty", deserialize_with = "null_default")]
    pub username: Secret,
    #[serde(skip_serializing_if = "Secret::is_empty", deserialize_with = "null_default")]
    pub password: Secret,
    #[serde(rename = "header_name", skip_serializing_if = "String::is_empty", deserialize_with = "null_default")]
    pub header_name: String,
    #[serde(rename = "header_value", skip_serializing_if = "Secret::is_empty", deserialize_with = "null_default")]
    pub header_value: Secret,
}

impl ResolvedAuthConfig {
    /// Overwrites all secret material.
    pub fn clear(&mut self) {
        self.token.clear();
        self.username.clear();
        self.password.clear();
        self.header_value.clear();
        self.apply_to = Vec::new();
    }
}

pub fn clear_resolved_auth_configs(auth: &mut [ResolvedAuthConfig]) {
    for item in auth {
        item.clear();
    }
}

/// Copy of the first resolved rule matching the request.
pub fn resolved_auth_for_request(
    auth: &[ResolvedAuthConfig],
    request_url: &str,
    kind: &str,
) -> Option<ResolvedAuthConfig> {
    matching_resolved_auth_config(auth, request_url, kind).cloned()
}

pub fn validate_resolved_auth_config(item: &ResolvedAuthConfig) -> Result<()> {
    let parsed = match GoUrl::parse(item.match_url.trim()) {
        Ok(parsed) if !parsed.scheme.is_empty() && !parsed.host.is_empty() => parsed,
        _ => return Err(errf!("plugin store resolved auth match is invalid")),
    };
    if !parsed.scheme.eq_ignore_ascii_case("https") {
        return Err(errf!("plugin store resolved auth match must use https"));
    }
    if parsed.has_user || !parsed.raw_query.is_empty() || !parsed.fragment.is_empty() {
        return Err(errf!(
            "plugin store resolved auth match must not contain credentials, query, or fragment"
        ));
    }
    for kind in &item.apply_to {
        match kind.trim().to_lowercase().as_str() {
            REQUEST_KIND_REGISTRY | REQUEST_KIND_METADATA | REQUEST_KIND_ARTIFACT => {}
            _ => return Err(errf!("plugin store resolved auth has unsupported apply_to {kind:?}")),
        }
    }
    match item.kind.trim().to_lowercase().as_str() {
        "" | AUTH_TYPE_NONE => Ok(()),
        AUTH_TYPE_BEARER | AUTH_TYPE_GITHUB_TOKEN => {
            if item.token.is_empty() {
                return Err(errf!("plugin store resolved auth token is empty"));
            }
            Ok(())
        }
        AUTH_TYPE_BASIC => {
            if item.username.is_empty() || item.password.is_empty() {
                return Err(errf!("plugin store resolved basic auth is incomplete"));
            }
            Ok(())
        }
        AUTH_TYPE_HEADER => {
            if item.header_name.trim().is_empty() || item.header_name.contains(['\r', '\n', ':']) {
                return Err(errf!("plugin store resolved auth header name is invalid"));
            }
            if item.header_value.is_empty() || secret_contains_crlf(&item.header_value) {
                return Err(errf!("plugin store resolved auth header value is invalid"));
            }
            Ok(())
        }
        _ => Err(errf!("unsupported plugin store resolved auth type {:?}", item.kind)),
    }
}

/// Trims and lowercases rule fields, defaults the type to `none`, dedupes `apply_to`
/// and drops rules without a match URL.
pub fn normalize_auth_configs(auth: &[AuthConfig]) -> Vec<AuthConfig> {
    let mut out = Vec::with_capacity(auth.len());
    for item in auth {
        let mut item = item.clone();
        item.match_url = item.match_url.trim().to_string();
        item.kind = item.kind.trim().to_lowercase();
        item.token_env = item.token_env.trim().to_string();
        item.username_env = item.username_env.trim().to_string();
        item.password_env = item.password_env.trim().to_string();
        item.header_name = item.header_name.trim().to_string();
        item.header_value_env = item.header_value_env.trim().to_string();
        if item.kind.is_empty() {
            item.kind = AUTH_TYPE_NONE.to_string();
        }
        if item.match_url.is_empty() {
            continue;
        }
        if !item.apply_to.is_empty() {
            let mut apply_to: Vec<String> = Vec::with_capacity(item.apply_to.len());
            for value in &item.apply_to {
                let value = value.trim().to_lowercase();
                if value.is_empty() || apply_to.contains(&value) {
                    continue;
                }
                apply_to.push(value);
            }
            item.apply_to = apply_to;
        }
        out.push(item);
    }
    out
}

/// Whether a matching rule exists whose credentials are present in the environment.
pub fn auth_configured(auth: &[AuthConfig], request_url: &str, kind: &str) -> bool {
    auth_configured_env(Env(None), auth, request_url, kind)
}

pub(crate) fn auth_configured_env(env: Env<'_>, auth: &[AuthConfig], request_url: &str, kind: &str) -> bool {
    let Some(item) = matching_auth_config(auth, request_url, kind) else {
        return false;
    };
    let set = |name: &str| !env.get(name).trim().is_empty();
    match item.kind.trim().to_lowercase().as_str() {
        AUTH_TYPE_BEARER | AUTH_TYPE_GITHUB_TOKEN => set(&item.token_env),
        AUTH_TYPE_BASIC => set(&item.username_env) && set(&item.password_env),
        AUTH_TYPE_HEADER => !item.header_name.is_empty() && set(&item.header_value_env),
        _ => false,
    }
}

/// Whether installing the plugin from the source would use configured credentials.
pub fn plugin_auth_configured(source: &Source, plugin: &Plugin, auth: &[AuthConfig]) -> bool {
    plugin_auth_configured_env(Env(None), source, plugin, auth)
}

pub(crate) fn plugin_auth_configured_env(
    env: Env<'_>,
    source: &Source,
    plugin: &Plugin,
    auth: &[AuthConfig],
) -> bool {
    if auth_configured_env(env, auth, &source.url, REQUEST_KIND_REGISTRY) {
        return true;
    }
    match plugin_install_type(plugin).as_str() {
        INSTALL_TYPE_DIRECT => plugin_artifacts(plugin)
            .iter()
            .any(|artifact| auth_configured_env(env, auth, &artifact.url, REQUEST_KIND_ARTIFACT)),
        INSTALL_TYPE_GITHUB_RELEASE => plugin_github_release_auth_configured(env, plugin, auth),
        _ => false,
    }
}

fn plugin_github_release_auth_configured(env: Env<'_>, plugin: &Plugin, auth: &[AuthConfig]) -> bool {
    let Ok((owner, repo)) = github_repository_parts(&plugin.repository) else {
        return false;
    };
    let releases_url = format!(
        "https://api.github.com/repos/{}/{}/releases/",
        path_escape(&owner),
        path_escape(&repo)
    );
    auth_configured_env(env, auth, &format!("{releases_url}latest"), REQUEST_KIND_METADATA)
        || auth_configured_env(env, auth, &format!("{releases_url}tags/"), REQUEST_KIND_METADATA)
}

/// Applies the matching environment-backed rule to `headers` (no resolved rules).
#[cfg(test)]
pub(crate) fn apply_plugin_store_auth(
    env: Env<'_>,
    headers: &mut Headers,
    auth: &[AuthConfig],
    request_url: &str,
    kind: &str,
) -> Result<()> {
    apply_plugin_store_auth_for_client(env, headers, &[], auth, request_url, kind).map(|_| ())
}

/// Applies auth for the request: a matching resolved rule wins (even `none`, which blocks
/// environment fallback), otherwise the environment-backed rule. Returns whether a
/// credential header was set.
pub(crate) fn apply_plugin_store_auth_for_client(
    env: Env<'_>,
    headers: &mut Headers,
    resolved: &[ResolvedAuthConfig],
    auth: &[AuthConfig],
    request_url: &str,
    kind: &str,
) -> Result<bool> {
    if let Some(item) = matching_resolved_auth_config(resolved, request_url, kind) {
        return apply_resolved_plugin_store_auth(headers, item);
    }
    let Some(item) = matching_auth_config(auth, request_url, kind) else {
        return Ok(false);
    };
    match item.kind.trim().to_lowercase().as_str() {
        "" | AUTH_TYPE_NONE => return Ok(false),
        AUTH_TYPE_BEARER | AUTH_TYPE_GITHUB_TOKEN => {
            let token = env_value_required(env, &item.token_env, "token-env")?;
            headers.set("Authorization", format!("Bearer {token}"));
        }
        AUTH_TYPE_BASIC => {
            let username = env_value_required(env, &item.username_env, "username-env")?;
            let password = env_value_required(env, &item.password_env, "password-env")?;
            let encoded = STANDARD.encode(format!("{username}:{password}"));
            headers.set("Authorization", format!("Basic {encoded}"));
        }
        AUTH_TYPE_HEADER => {
            if item.header_name.trim().is_empty() {
                return Err(errf!("plugin store auth missing header-name"));
            }
            let value = env_value_required(env, &item.header_value_env, "header-value-env")?;
            headers.set(&item.header_name, value);
        }
        _ => return Err(errf!("unsupported plugin store auth type {:?}", item.kind)),
    }
    Ok(true)
}

fn apply_resolved_plugin_store_auth(headers: &mut Headers, item: &ResolvedAuthConfig) -> Result<bool> {
    match item.kind.trim().to_lowercase().as_str() {
        "" | AUTH_TYPE_NONE => return Ok(false),
        AUTH_TYPE_BEARER | AUTH_TYPE_GITHUB_TOKEN => {
            if item.token.is_empty() {
                return Err(errf!("plugin store resolved auth token is empty"));
            }
            headers.set("Authorization", format!("Bearer {}", item.token.to_string_lossy()));
        }
        AUTH_TYPE_BASIC => {
            if item.username.is_empty() || item.password.is_empty() {
                return Err(errf!("plugin store resolved basic auth is incomplete"));
            }
            let mut credential = Vec::with_capacity(item.username.len() + 1 + item.password.len());
            credential.extend_from_slice(item.username.as_bytes());
            credential.push(b':');
            credential.extend_from_slice(item.password.as_bytes());
            let encoded = STANDARD.encode(&credential);
            credential.zeroize();
            headers.set("Authorization", format!("Basic {encoded}"));
        }
        AUTH_TYPE_HEADER => {
            if item.header_name.trim().is_empty() {
                return Err(errf!("plugin store resolved auth missing header-name"));
            }
            if item.header_value.is_empty() {
                return Err(errf!("plugin store resolved auth header value is empty"));
            }
            headers.set(&item.header_name, item.header_value.to_string_lossy());
        }
        _ => return Err(errf!("unsupported plugin store resolved auth type {:?}", item.kind)),
    }
    Ok(true)
}

/// Rejects malformed URLs, embedded credentials, sensitive query parameters and plain
/// http without a matching `allow-insecure` rule.
pub(crate) fn validate_plugin_store_request_url(auth: &[AuthConfig], request_url: &str, kind: &str) -> Result<()> {
    let parsed = match GoUrl::parse(request_url.trim()) {
        Ok(parsed) if !parsed.scheme.is_empty() && !parsed.host.is_empty() => parsed,
        _ => return Err(errf!("invalid plugin store url")),
    };
    if parsed.has_user {
        return Err(errf!("plugin store url must not contain credentials"));
    }
    if has_sensitive_query_parameter(&parsed) {
        return Err(errf!("plugin store url contains sensitive query parameter"));
    }
    if parsed.scheme.eq_ignore_ascii_case("http") && !allow_insecure_plugin_store_url(auth, request_url, kind) {
        return Err(errf!("insecure plugin store url requires matching allow-insecure auth rule"));
    }
    Ok(())
}

fn allow_insecure_plugin_store_url(auth: &[AuthConfig], request_url: &str, kind: &str) -> bool {
    matching_auth_config(auth, request_url, kind).is_some_and(|item| item.allow_insecure)
}

pub(crate) fn validate_resolved_auth_expiry(
    auth: &[ResolvedAuthConfig],
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    request_url: &str,
    kind: &str,
) -> Result<()> {
    let Some(expires_at) = expires_at else {
        return Ok(());
    };
    if matching_resolved_auth_config(auth, request_url, kind).is_none() {
        return Ok(());
    }
    if now >= expires_at {
        return Err(errf!("plugin store resolved auth expired"));
    }
    Ok(())
}

pub(crate) fn matching_auth_config(auth: &[AuthConfig], request_url: &str, kind: &str) -> Option<AuthConfig> {
    let request_url = request_url.trim();
    let kind = kind.trim().to_lowercase();
    normalize_auth_configs(auth).into_iter().find(|item| {
        url_matches_auth_rule(request_url, &item.match_url) && applies_to(&item.apply_to, &kind)
    })
}

pub(crate) fn matching_resolved_auth_config<'a>(
    auth: &'a [ResolvedAuthConfig],
    request_url: &str,
    kind: &str,
) -> Option<&'a ResolvedAuthConfig> {
    let request_url = request_url.trim();
    let kind = kind.trim().to_lowercase();
    auth.iter().find(|item| {
        url_matches_auth_rule(request_url, item.match_url.trim()) && applies_to(&item.apply_to, &kind)
    })
}

fn applies_to(apply_to: &[String], kind: &str) -> bool {
    apply_to.is_empty() || apply_to.iter().any(|value| value.trim().eq_ignore_ascii_case(kind))
}

pub(crate) fn resolved_auth_configured(item: &ResolvedAuthConfig) -> bool {
    match item.kind.trim().to_lowercase().as_str() {
        AUTH_TYPE_BEARER | AUTH_TYPE_GITHUB_TOKEN => !item.token.is_empty(),
        AUTH_TYPE_BASIC => !item.username.is_empty() && !item.password.is_empty(),
        AUTH_TYPE_HEADER => !item.header_name.trim().is_empty() && !item.header_value.is_empty(),
        _ => false,
    }
}

fn secret_contains_crlf(secret: &Secret) -> bool {
    secret.as_bytes().iter().any(|&b| b == b'\r' || b == b'\n')
}

/// Scheme and host (with port) must match exactly (case-insensitively) and the request
/// path must equal the rule path or sit under it on a segment boundary.
fn url_matches_auth_rule(request_url: &str, match_url: &str) -> bool {
    let request = match GoUrl::parse(request_url.trim()) {
        Ok(request) if !request.scheme.is_empty() && !request.host.is_empty() => request,
        _ => return false,
    };
    let rule = match GoUrl::parse(match_url.trim()) {
        Ok(rule) if !rule.scheme.is_empty() && !rule.host.is_empty() => rule,
        _ => return false,
    };
    if !request.scheme.eq_ignore_ascii_case(&rule.scheme) || !request.host.eq_ignore_ascii_case(&rule.host) {
        return false;
    }
    path_matches_auth_rule(&request.path, &rule.path)
}

fn path_matches_auth_rule(request_path: &str, rule_path: &str) -> bool {
    if rule_path.is_empty() || rule_path == "/" {
        return true;
    }
    let request_path = if request_path.is_empty() { "/" } else { request_path };
    if request_path == rule_path {
        return true;
    }
    if rule_path.ends_with('/') {
        return request_path.starts_with(rule_path);
    }
    request_path.starts_with(&format!("{rule_path}/"))
}

fn env_value_required(env: Env<'_>, env_name: &str, field: &str) -> Result<String> {
    let env_name = env_name.trim();
    if env_name.is_empty() {
        return Err(errf!("plugin store auth missing {field}"));
    }
    let value = env.get(env_name).trim().to_string();
    if value.is_empty() {
        return Err(errf!("plugin store auth env {env_name} is empty"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::env_fn;

    fn bearer_rule(match_url: &str, apply_to: &[&str]) -> AuthConfig {
        AuthConfig {
            match_url: match_url.into(),
            apply_to: apply_to.iter().map(|s| s.to_string()).collect(),
            kind: AUTH_TYPE_BEARER.into(),
            token_env: "PLUGIN_STORE_TOKEN".into(),
            ..Default::default()
        }
    }

    #[test]
    fn auth_matches_url_host_and_path_boundaries() {
        let env = env_fn(&[("PLUGIN_STORE_TOKEN", "secret-token")]);
        let auth = vec![bearer_rule("https://downloads.example/private", &[REQUEST_KIND_ARTIFACT])];
        let cases = [
            ("exact path", "https://downloads.example/private", true),
            ("child path", "https://downloads.example/private/plugin.zip", true),
            ("sibling prefix", "https://downloads.example/private2/plugin.zip", false),
            ("similar host", "https://downloads.example.evil/private/plugin.zip", false),
            ("different scheme", "http://downloads.example/private/plugin.zip", false),
        ];
        for (name, url, want) in cases {
            let mut headers = Headers::new();
            apply_plugin_store_auth(Env(Some(&env)), &mut headers, &auth, url, REQUEST_KIND_ARTIFACT)
                .expect("apply auth");
            assert_eq!(!headers.get("Authorization").is_empty(), want, "{name}");
        }
    }

    #[test]
    fn github_token_uses_explicit_token_env() {
        let env = env_fn(&[("PLUGIN_STORE_TOKEN", "secret-token")]);
        let mut headers = Headers::new();
        let auth = vec![AuthConfig {
            match_url: "https://api.github.com/repos/author-name/sample-provider/releases/".into(),
            apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
            kind: AUTH_TYPE_GITHUB_TOKEN.into(),
            token_env: "PLUGIN_STORE_TOKEN".into(),
            ..Default::default()
        }];
        apply_plugin_store_auth(
            Env(Some(&env)),
            &mut headers,
            &auth,
            "https://api.github.com/repos/author-name/sample-provider/releases/assets/1",
            REQUEST_KIND_ARTIFACT,
        )
        .expect("apply auth");
        assert_eq!(headers.get("Authorization"), "Bearer secret-token");
    }

    #[test]
    fn plugin_auth_configured_covers_install_request_kinds() {
        let env = env_fn(&[("PLUGIN_STORE_TOKEN", "secret-token")]);
        let source = Source { url: "https://registry.example/registry.json".into(), ..Default::default() };
        let direct = Plugin {
            id: "sample-provider".into(),
            version: "1.0.0".into(),
            install: crate::registry::InstallPlan {
                kind: INSTALL_TYPE_DIRECT.into(),
                artifacts: vec![crate::registry::Artifact {
                    goos: "linux".into(),
                    goarch: "amd64".into(),
                    url: "https://downloads.example/private/sample-provider.zip".into(),
                    sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                    size: 0,
                }],
            },
            ..Default::default()
        };
        let github = Plugin {
            id: "sample-provider".into(),
            repository: "https://github.com/author-name/sample-provider".into(),
            ..Default::default()
        };
        let cases = [
            ("registry", &github, bearer_rule("https://registry.example/", &[REQUEST_KIND_REGISTRY])),
            ("direct artifact", &direct, bearer_rule("https://downloads.example/private/", &[REQUEST_KIND_ARTIFACT])),
            (
                "github metadata",
                &github,
                bearer_rule(
                    "https://api.github.com/repos/author-name/sample-provider/releases/",
                    &[REQUEST_KIND_METADATA],
                ),
            ),
        ];
        for (name, plugin, rule) in cases {
            assert!(plugin_auth_configured_env(Env(Some(&env)), &source, plugin, &[rule]), "{name}");
        }
    }

    #[test]
    fn resolved_auth_takes_priority_over_environment_auth() {
        let env = env_fn(&[("PLUGIN_STORE_TOKEN", "environment-token")]);
        let resolved = vec![ResolvedAuthConfig {
            match_url: "https://downloads.example/private/".into(),
            apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
            kind: AUTH_TYPE_BEARER.into(),
            token: Secret::from("resolved-token"),
            ..Default::default()
        }];
        let auth = vec![bearer_rule("https://downloads.example/private/", &[REQUEST_KIND_ARTIFACT])];
        let mut headers = Headers::new();
        let applied = apply_plugin_store_auth_for_client(
            Env(Some(&env)),
            &mut headers,
            &resolved,
            &auth,
            "https://downloads.example/private/plugin.zip",
            REQUEST_KIND_ARTIFACT,
        )
        .expect("apply");
        assert!(applied);
        assert_eq!(headers.get("Authorization"), "Bearer resolved-token");
    }

    #[test]
    fn resolved_none_rule_blocks_environment_fallback() {
        let env = env_fn(&[("PLUGIN_STORE_TOKEN", "environment-token")]);
        let resolved = vec![ResolvedAuthConfig {
            match_url: "https://downloads.example/private/".into(),
            apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
            kind: AUTH_TYPE_NONE.into(),
            ..Default::default()
        }];
        let auth = vec![bearer_rule("https://downloads.example/private/", &[REQUEST_KIND_ARTIFACT])];
        let mut headers = Headers::new();
        let applied = apply_plugin_store_auth_for_client(
            Env(Some(&env)),
            &mut headers,
            &resolved,
            &auth,
            "https://downloads.example/private/plugin.zip",
            REQUEST_KIND_ARTIFACT,
        )
        .expect("apply");
        assert!(!applied && headers.get("Authorization").is_empty());
    }

    #[test]
    fn resolved_auth_clear_overwrites_secrets() {
        let mut auth = ResolvedAuthConfig {
            token: Secret::from("temporary-token"),
            username: Secret::from("user"),
            password: Secret::from("pass"),
            header_value: Secret::from("header"),
            ..Default::default()
        };
        auth.clear();
        assert!(auth.token.is_empty() && auth.username.is_empty() && auth.password.is_empty() && auth.header_value.is_empty());
    }

    #[test]
    fn request_url_rejects_credentials_without_leaking_them() {
        let err = validate_plugin_store_request_url(&[], "https://user:password@downloads.example/plugin.zip", REQUEST_KIND_ARTIFACT)
            .expect_err("credentials rejected");
        assert!(!err.to_string().contains("password"), "{err}");
    }
}

//! Store client: registry fetch, GitHub release resolution and the shared HTTP GET with
//! manual redirects, per-hop credentials and rate-limit handling (Go `github.go`).

use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::{
    AuthConfig, EnvFn, REQUEST_KIND_ARTIFACT, REQUEST_KIND_METADATA, REQUEST_KIND_REGISTRY, ResolvedAuthConfig,
    apply_plugin_store_auth_for_client, auth_configured_env, matching_resolved_auth_config,
    resolved_auth_configured, validate_plugin_store_request_url, validate_resolved_auth_expiry,
};
use crate::error::{Context, Error, Result};
use crate::errf;
use crate::goturl::{GoUrl, path_escape};
use crate::http::{BodyReader, Headers, HttpDoer, HttpRequest, HttpResponse, default_doer};
use crate::ratelimit::{DEFAULT_GITHUB_RATE_LIMITER, GitHubRateLimiter, github_rate_limit_key};
use crate::registry::{
    DEFAULT_REGISTRY_URL, Plugin, Registry, github_repository_parts, normalize_version, null_default, parse_registry,
    valid_plugin_version,
};
use crate::request_identity::PreparedAuth;

const USER_AGENT: &str = "CLIProxyAPI";
const MAX_PLUGIN_STORE_REDIRECTS: usize = 10;

/// Plugin store HTTP client. Fields mirror Go's exported `Client` struct; the zero value
/// (`Client::default()`) fetches the official registry over a default reqwest client.
#[derive(Clone, Default)]
pub struct Client {
    /// Transport; `None` uses a process-wide default.
    pub http_client: Option<Arc<dyn HttpDoer>>,
    /// Distinguishes request state for different proxy/egress configurations.
    pub network_scope: String,
    /// Cooldown state; `None` uses the process-wide default limiter.
    pub rate_limiter: Option<Arc<GitHubRateLimiter>>,
    pub registry_url: String,
    pub user_agent: String,
    pub auth: Vec<AuthConfig>,
    pub resolved_auth: Vec<ResolvedAuthConfig>,
    pub resolved_auth_expires_at: Option<DateTime<Utc>>,
    pub(crate) prepared_auth: Option<Arc<PreparedAuth>>,
    pub(crate) env: Option<EnvFn>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("network_scope", &self.network_scope)
            .field("registry_url", &self.registry_url)
            .field("auth_rules", &self.auth.len())
            .field("resolved_auth_rules", &self.resolved_auth.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Release {
    #[serde(deserialize_with = "null_default")]
    pub tag_name: String,
    #[serde(deserialize_with = "null_default")]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReleaseAsset {
    #[serde(rename = "url", deserialize_with = "null_default")]
    pub api_url: String,
    #[serde(deserialize_with = "null_default")]
    pub name: String,
    #[serde(deserialize_with = "null_default")]
    pub browser_download_url: String,
}

impl Client {
    pub async fn fetch_registry(&self, ctx: &Context) -> Result<Registry> {
        let registry_url = self.registry_url.trim();
        let registry_url = if registry_url.is_empty() { DEFAULT_REGISTRY_URL } else { registry_url };
        let data = self.get(ctx, registry_url, "application/json", REQUEST_KIND_REGISTRY, 0).await?;
        parse_registry(&data)
    }

    /// Latest published release of the plugin's GitHub repository, mirroring the WebUI
    /// panel update check.
    pub async fn fetch_latest_release(&self, ctx: &Context, plugin: &Plugin) -> Result<Release> {
        let (owner, repo) = github_repository_parts(&plugin.repository)?;
        let release_url = format!(
            "https://api.github.com/repos/{}/{}/releases/latest",
            path_escape(&owner),
            path_escape(&repo)
        );
        let data = self.get(ctx, &release_url, "application/vnd.github+json", REQUEST_KIND_METADATA, 0).await?;
        decode_release(&data)
    }

    /// Published release by its exact GitHub tag.
    pub async fn fetch_release_by_tag(&self, ctx: &Context, plugin: &Plugin, tag: &str) -> Result<Release> {
        let (owner, repo) = github_repository_parts(&plugin.repository)?;
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(errf!("release tag is required"));
        }
        let release_url = format!(
            "https://api.github.com/repos/{}/{}/releases/tags/{}",
            path_escape(&owner),
            path_escape(&repo),
            path_escape(tag)
        );
        let data = self.get(ctx, &release_url, "application/vnd.github+json", REQUEST_KIND_METADATA, 0).await?;
        decode_release(&data)
    }

    /// Downloads a release asset, preferring the API URL when credentials are configured
    /// for it (private repositories) and the browser URL otherwise.
    pub async fn download_asset(&self, ctx: &Context, asset: &ReleaseAsset) -> Result<Vec<u8>> {
        let mut download_url = asset.browser_download_url.trim().to_string();
        let api_url = asset.api_url.trim();
        if (download_url.is_empty() || self.release_asset_api_authenticated(api_url)) && !api_url.is_empty() {
            download_url = api_url.to_string();
        }
        if download_url.is_empty() {
            return Err(errf!("asset {:?} missing download url", asset.name));
        }
        self.get(ctx, &download_url, "application/octet-stream", REQUEST_KIND_ARTIFACT, 0).await
    }

    fn release_asset_api_authenticated(&self, api_url: &str) -> bool {
        let api_url = api_url.trim();
        if api_url.is_empty() {
            return false;
        }
        if let Some(item) = matching_resolved_auth_config(&self.resolved_auth, api_url, REQUEST_KIND_ARTIFACT) {
            return resolved_auth_configured(item);
        }
        auth_configured_env(self.env(), &self.auth, api_url, REQUEST_KIND_ARTIFACT)
    }

    /// Headers for the request plus whether credentials were applied. Reuses the
    /// prepared snapshot when the request matches it.
    pub(crate) fn auth_headers(&self, request_url: &str, kind: &str) -> Result<(Headers, bool)> {
        validate_plugin_store_request_url(&self.auth, request_url, kind)?;
        validate_resolved_auth_expiry(&self.resolved_auth, self.resolved_auth_expires_at, Utc::now(), request_url, kind)?;
        if let Some(prepared) = &self.prepared_auth
            && prepared.request_url == request_url && prepared.kind == kind {
                return Ok((prepared.headers.clone(), prepared.authenticated));
            }
        let mut headers = Headers::new();
        let authenticated = apply_plugin_store_auth_for_client(
            self.env(),
            &mut headers,
            &self.resolved_auth,
            &self.auth,
            request_url,
            kind,
        )?;
        Ok((headers, authenticated))
    }

    /// GET with manual redirect following (credentials are re-evaluated for each hop),
    /// GitHub cooldown checks and an optional body size cap (`max_size <= 0` is unlimited).
    pub(crate) async fn get(
        &self,
        ctx: &Context,
        request_url: &str,
        accept: &str,
        kind: &str,
        max_size: i64,
    ) -> Result<Vec<u8>> {
        let mut current_url = request_url.trim().to_string();
        let limiter = self.github_rate_limiter();
        let doer = self.doer();
        let mut redirects = 0;
        loop {
            if let Some(err) = ctx.err() {
                return Err(err);
            }
            let (mut headers, authenticated) = self.auth_headers(&current_url, kind)?;
            let rate_key = github_rate_limit_key(&current_url, &self.network_scope, &headers, authenticated);
            limiter.check(&rate_key)?;
            if headers.get("Accept").is_empty() {
                headers.set("Accept", accept);
            }
            if headers.get("User-Agent").is_empty() {
                headers.set("User-Agent", self.user_agent());
            }
            let response = get_no_redirect(ctx, doer.as_ref(), &current_url, headers).await?;
            if redirect_status(response.status) {
                let _ = limiter.observe(&rate_key, response.status, &response.headers, &[]);
                let next = redirect_url(&response, &current_url);
                drop(response);
                let next = next?;
                if redirects >= MAX_PLUGIN_STORE_REDIRECTS {
                    return Err(errf!("stopped after {MAX_PLUGIN_STORE_REDIRECTS} redirects"));
                }
                redirects += 1;
                current_url = next;
                continue;
            }
            return read_response(response, max_size, authenticated, &limiter, &rate_key).await;
        }
    }

    pub(crate) fn github_rate_limiter(&self) -> Arc<GitHubRateLimiter> {
        match &self.rate_limiter {
            Some(limiter) => limiter.clone(),
            None => DEFAULT_GITHUB_RATE_LIMITER.clone(),
        }
    }

    fn doer(&self) -> Arc<dyn HttpDoer> {
        match &self.http_client {
            Some(doer) => doer.clone(),
            None => default_doer(),
        }
    }

    fn user_agent(&self) -> String {
        let agent = self.user_agent.trim();
        if agent.is_empty() { USER_AGENT.to_string() } else { agent.to_string() }
    }
}

fn decode_release(data: &[u8]) -> Result<Release> {
    serde_json::from_slice::<Release>(data).map_err(|err| errf!("decode release: {err}"))
}

/// Derives the plugin version from the release tag, stripping a leading "v"/"V" and
/// validating the result.
pub fn release_version(release: &Release) -> Result<String> {
    let version = normalize_version(&release.tag_name);
    if !valid_plugin_version(&version) {
        return Err(errf!("invalid release tag {:?}", release.tag_name));
    }
    Ok(version)
}

async fn get_no_redirect(
    ctx: &Context,
    doer: &dyn HttpDoer,
    request_url: &str,
    headers: Headers,
) -> Result<HttpResponse> {
    let request = HttpRequest { url: request_url.to_string(), headers };
    tokio::select! {
        biased;
        _ = ctx.cancelled() => Err(Error::Canceled),
        result = doer.get(request) => result.map_err(|err| plugin_store_request_error(request_url, &err.to_string())),
    }
}

fn redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn redirect_url(response: &HttpResponse, request_url: &str) -> Result<String> {
    let location = response.headers.get("Location").trim();
    if location.is_empty() {
        return Err(errf!("redirect missing Location header"));
    }
    let base = reqwest::Url::parse(request_url).map_err(|err| errf!("parse redirect base: {err}"))?;
    let next = base.join(location).map_err(|err| errf!("parse redirect location: {err}"))?;
    if next.host_str().is_none_or(str::is_empty) {
        return Err(errf!("redirect location is not absolute"));
    }
    Ok(next.to_string())
}

/// Reads up to `limit` bytes (`limit < 0` for all), ignoring read errors after partial data
/// like `io.ReadAll(io.LimitReader(...))` whose error is discarded.
async fn read_body(body: &mut dyn BodyReader, limit: Option<usize>) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    while let Some(chunk) = body.chunk().await? {
        data.extend_from_slice(&chunk);
        if let Some(limit) = limit
            && data.len() >= limit {
                data.truncate(limit);
                break;
            }
    }
    Ok(data)
}

async fn read_response(
    mut response: HttpResponse,
    max_size: i64,
    authenticated: bool,
    limiter: &GitHubRateLimiter,
    rate_key: &str,
) -> Result<Vec<u8>> {
    let status = response.status;
    if !(200..300).contains(&status) {
        // Apply header-based limits before reading a potentially slow body.
        limiter.observe(rate_key, status, &response.headers, &[])?;
        if authenticated && (rate_key.is_empty() || status != 403) {
            return Err(errf!("unexpected status {status}"));
        }
        let body = read_body(response.body.as_mut(), Some(4096)).await.unwrap_or_default();
        limiter.observe(rate_key, status, &response.headers, &body)?;
        if authenticated {
            return Err(errf!("unexpected status {status}"));
        }
        return Err(errf!("unexpected status {status}: {}", String::from_utf8_lossy(&body).trim()));
    }
    let _ = limiter.observe(rate_key, status, &response.headers, &[]);
    let limit = (max_size > 0).then(|| usize::try_from(max_size).unwrap_or(usize::MAX).saturating_add(1));
    let data = read_body(response.body.as_mut(), limit)
        .await
        .map_err(|err| errf!("read response: {err}"))?;
    if max_size > 0 && data.len() as u64 > max_size as u64 {
        return Err(errf!("response exceeds maximum allowed size of {max_size} bytes"));
    }
    Ok(data)
}

/// `request <safe-url> failed: <cause>`, with credentials, query and fragment removed
/// from the URL.
pub(crate) fn plugin_store_request_error(request_url: &str, cause: &str) -> Error {
    let mut safe_url = "plugin store url".to_string();
    if let Ok(mut parsed) = GoUrl::parse(request_url.trim())
        && !parsed.scheme.is_empty() && !parsed.host.is_empty() {
            parsed.has_user = false;
            parsed.raw_query.clear();
            parsed.force_query = false;
            parsed.fragment.clear();
            safe_url = parsed.render();
        }
    errf!("request {safe_url} failed: {cause}")
}

/// Picks the platform archive and `checksums.txt` assets from a release.
pub fn select_release_assets(
    release: &Release,
    id: &str,
    version: &str,
    goos: &str,
    goarch: &str,
) -> Result<(ReleaseAsset, ReleaseAsset)> {
    let archive_name = archive_name(id, version, goos, goarch);
    let mut archive_asset = ReleaseAsset::default();
    let mut checksum_asset = ReleaseAsset::default();
    for asset in &release.assets {
        let name = asset.name.trim();
        if name == archive_name {
            archive_asset = asset.clone();
        } else if name == "checksums.txt" {
            checksum_asset = asset.clone();
        }
    }
    if archive_asset.name.trim().is_empty() {
        return Err(errf!("release asset {archive_name} not found"));
    }
    if checksum_asset.name.trim().is_empty() {
        return Err(errf!("release asset checksums.txt not found"));
    }
    Ok((archive_asset, checksum_asset))
}

/// `{id}_{version}_{goos}_{goarch}.zip`
pub fn archive_name(id: &str, version: &str, goos: &str, goarch: &str) -> String {
    format!("{}_{}_{}_{}.zip", id.trim(), version.trim(), goos.trim(), goarch.trim())
}

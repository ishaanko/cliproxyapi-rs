//! Claude Code device profile: baseline fingerprint values, version compare/upgrade rules and the
//! 7 day per-credential cache (Go: helps/claude_device_profile.go). In Home mode the profile is
//! kept in Home KV instead (`cpa:claude:device-profile:*`, with a short lock key per scope).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use cpa_auth::Auth;
use cpa_config::Config;
use cpa_home::kv::hash_key_part;
use cpa_home::{Client, HomeError, KvSetOptions};
use serde::{Deserialize, Serialize};
use http::header::{HeaderName, HeaderValue};
use http::HeaderMap;
use parking_lot::{Mutex, RwLock};
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::helps::home_kv;

use super::client_detection::{
    header_value, native_claude_entrypoint, parse_claude_code_user_agent_details,
    plausible_claude_code_user_agent,
};

pub const DEFAULT_CLAUDE_FINGERPRINT_USER_AGENT: &str = "claude-cli/2.1.280 (external, cli)";
pub const DEFAULT_CLAUDE_FINGERPRINT_PACKAGE_VERSION: &str = "0.112.1";
pub const DEFAULT_CLAUDE_FINGERPRINT_RUNTIME_VERSION: &str = "v26.3.0";
pub const DEFAULT_CLAUDE_FINGERPRINT_OS: &str = "MacOS";
pub const DEFAULT_CLAUDE_FINGERPRINT_ARCH: &str = "arm64";
/// Go: `claudeDeviceProfileTTL`.
pub const CLAUDE_DEVICE_PROFILE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Go: `claudeDeviceProfileLockTTL` (Home KV upgrade lock).
const CLAUDE_DEVICE_PROFILE_LOCK_TTL: Duration = Duration::from_secs(5);
/// Go: `claudeDeviceProfileCleanupPeriod`.
const CLAUDE_DEVICE_PROFILE_CLEANUP_PERIOD: Duration = Duration::from_secs(3600);

static CLAUDE_CLI_VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^claude-cli/([0-9]+)\.([0-9]+)\.([0-9]+)").expect("static regex"));
static CLAUDE_PACKAGE_VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]+\.[0-9]+\.[0-9]+$").expect("static regex"));
static CLAUDE_RUNTIME_VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v[0-9]+\.[0-9]+\.[0-9]+$").expect("static regex"));

/// Go: `claudeCLIVersion` (major.minor.patch parsed from a `claude-cli/` user agent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ClaudeCliVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

/// Go: `ClaudeDeviceProfile`. `version` is the parsed `user_agent` (Go's `version`/`hasVersion`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeDeviceProfile {
    pub user_agent: String,
    pub package_version: String,
    pub runtime_version: String,
    pub os: String,
    pub arch: String,
    pub(crate) version: Option<ClaudeCliVersion>,
}

struct CacheEntry {
    profile: ClaudeDeviceProfile,
    expire: Instant,
}

struct ProfileCache {
    entries: HashMap<String, CacheEntry>,
    last_cleanup: Instant,
}

static DEVICE_PROFILE_CACHE: LazyLock<RwLock<ProfileCache>> = LazyLock::new(|| {
    RwLock::new(ProfileCache { entries: HashMap::new(), last_cleanup: Instant::now() })
});

type CandidateHook = Arc<dyn Fn(&ClaudeDeviceProfile) + Send + Sync>;
static BEFORE_CANDIDATE_STORE: Mutex<Option<CandidateHook>> = Mutex::new(None);

/// Go: `ClaudeDeviceProfileBeforeCandidateStore` (test hook run just before a candidate is stored).
pub fn set_claude_device_profile_before_candidate_store(hook: Option<CandidateHook>) {
    *BEFORE_CANDIDATE_STORE.lock() = hook;
}

/// Go: `ClaudeDeviceProfileStabilizationEnabled`.
pub fn claude_device_profile_stabilization_enabled(cfg: Option<&Config>) -> bool {
    cfg.and_then(|c| c.claude_header_defaults.stabilize_device_profile).unwrap_or(false)
}

/// Go: `ResetClaudeDeviceProfileCache`.
pub fn reset_claude_device_profile_cache() {
    let mut cache = DEVICE_PROFILE_CACHE.write();
    cache.entries.clear();
    cache.last_cleanup = Instant::now();
}

/// Go: `MapStainlessOS` (runtime OS to Stainless SDK name).
pub fn map_stainless_os() -> String {
    match std::env::consts::OS {
        "macos" => "MacOS".to_string(),
        "windows" => "Windows".to_string(),
        "linux" => "Linux".to_string(),
        "freebsd" => "FreeBSD".to_string(),
        other => format!("Other::{other}"),
    }
}

/// Go: `MapStainlessArch` (runtime arch to Stainless SDK name).
pub fn map_stainless_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x64".to_string(),
        "aarch64" => "arm64".to_string(),
        "x86" => "x86".to_string(),
        other => format!("other::{other}"),
    }
}

/// Go: `defaultClaudeDeviceProfile` (configured baseline with built-in fallbacks).
pub fn default_claude_device_profile(cfg: Option<&Config>) -> ClaudeDeviceProfile {
    fn hdr_default(cfg_val: &str, fallback: &str) -> String {
        let trimmed = cfg_val.trim();
        if trimmed.is_empty() { fallback.to_string() } else { trimmed.to_string() }
    }
    let empty = Default::default();
    let hd = cfg.map(|c| &c.claude_header_defaults).unwrap_or(&empty);
    let user_agent = hdr_default(&hd.user_agent, DEFAULT_CLAUDE_FINGERPRINT_USER_AGENT);
    ClaudeDeviceProfile {
        package_version: hdr_default(&hd.package_version, DEFAULT_CLAUDE_FINGERPRINT_PACKAGE_VERSION),
        runtime_version: hdr_default(&hd.runtime_version, DEFAULT_CLAUDE_FINGERPRINT_RUNTIME_VERSION),
        os: hdr_default(&hd.os, DEFAULT_CLAUDE_FINGERPRINT_OS),
        arch: hdr_default(&hd.arch, DEFAULT_CLAUDE_FINGERPRINT_ARCH),
        version: parse_claude_cli_version(&user_agent),
        user_agent,
    }
}

/// Go: `parseClaudeCLIVersion`.
pub fn parse_claude_cli_version(user_agent: &str) -> Option<ClaudeCliVersion> {
    let caps = CLAUDE_CLI_VERSION_RE.captures(user_agent.trim())?;
    Some(ClaudeCliVersion {
        major: caps[1].parse().ok()?,
        minor: caps[2].parse().ok()?,
        patch: caps[3].parse().ok()?,
    })
}

/// Go: `shouldUpgradeClaudeDeviceProfile`: only a strictly newer versioned candidate upgrades.
fn should_upgrade_claude_device_profile(candidate: &ClaudeDeviceProfile, current: &ClaudeDeviceProfile) -> bool {
    let Some(candidate_version) = candidate.version else { return false };
    if candidate.user_agent.is_empty() {
        return false;
    }
    match current.version {
        Some(current_version) if !current.user_agent.is_empty() => candidate_version > current_version,
        _ => true,
    }
}

/// Go: `plausibleClaudeCLIVersion`; the baseline is a floor for patch releases of one release line.
pub fn plausible_claude_cli_version(candidate: ClaudeCliVersion, baseline: ClaudeCliVersion) -> bool {
    candidate.major == baseline.major && candidate.minor == baseline.minor && candidate.patch >= baseline.patch
}

/// Go: `meetsClaudeDeviceProfileBaseline`: exact version, package and runtime match.
pub fn meets_claude_device_profile_baseline(candidate: &ClaudeDeviceProfile, baseline: &ClaudeDeviceProfile) -> bool {
    let (Some(cv), Some(bv)) = (candidate.version, baseline.version) else { return false };
    if candidate.user_agent.is_empty() || baseline.user_agent.is_empty() {
        return false;
    }
    cv == bv
        && candidate.package_version == baseline.package_version
        && candidate.runtime_version == baseline.runtime_version
}

/// Go: `pinClaudeDeviceProfilePlatform`.
fn pin_claude_device_profile_platform(mut profile: ClaudeDeviceProfile, baseline: &ClaudeDeviceProfile) -> ClaudeDeviceProfile {
    profile.os = baseline.os.clone();
    profile.arch = baseline.arch.clone();
    profile
}

/// Go: `normalizeClaudeDeviceProfile`: pins the platform and replaces any software tuple that
/// does not exactly match the measured baseline.
fn normalize_claude_device_profile(profile: ClaudeDeviceProfile, baseline: &ClaudeDeviceProfile) -> ClaudeDeviceProfile {
    let mut profile = pin_claude_device_profile_platform(profile, baseline);
    if !meets_claude_device_profile_baseline(&profile, baseline) {
        profile.user_agent = baseline.user_agent.clone();
        profile.package_version = baseline.package_version.clone();
        profile.runtime_version = baseline.runtime_version.clone();
        profile.version = baseline.version;
    }
    profile
}

/// Go: `firstNonEmptyHeader` (`Header.Get` trimmed, fallback when empty).
fn first_non_empty_header(headers: &HeaderMap, name: &str, fallback: &str) -> String {
    let value = header_value(headers, name);
    let trimmed = value.trim();
    if trimmed.is_empty() { fallback.to_string() } else { trimmed.to_string() }
}

/// Go: `extractClaudeDeviceProfile`: the profile a native Claude Code client advertises.
fn extract_claude_device_profile(headers: &HeaderMap, cfg: Option<&Config>) -> Option<ClaudeDeviceProfile> {
    let user_agent_raw = header_value(headers, "User-Agent");
    let user_agent = user_agent_raw.trim();
    let version = parse_claude_cli_version(user_agent)?;
    if !super::client_detection::claude_code_native_user_agent_matches(user_agent) {
        return None;
    }
    let baseline = default_claude_device_profile(cfg);
    let mut package_version = first_non_empty_header(headers, "X-Stainless-Package-Version", &baseline.package_version);
    if !CLAUDE_PACKAGE_VERSION_RE.is_match(&package_version) {
        package_version = baseline.package_version.clone();
    }
    let mut runtime_version = first_non_empty_header(headers, "X-Stainless-Runtime-Version", &baseline.runtime_version);
    if !CLAUDE_RUNTIME_VERSION_RE.is_match(&runtime_version) {
        runtime_version = baseline.runtime_version.clone();
    }
    Some(ClaudeDeviceProfile {
        user_agent: user_agent.to_string(),
        package_version,
        runtime_version,
        os: first_non_empty_header(headers, "X-Stainless-Os", &baseline.os),
        arch: first_non_empty_header(headers, "X-Stainless-Arch", &baseline.arch),
        version: Some(version),
    })
}

/// Go: `claudeDeviceProfileScopeKey`.
fn claude_device_profile_scope_key(auth: Option<&Auth>, api_key: &str) -> String {
    if let Some(a) = auth
        && !a.id.trim().is_empty()
    {
        return format!("auth:{}", a.id.trim());
    }
    if !api_key.trim().is_empty() {
        return format!("api_key:{}", api_key.trim());
    }
    "global".to_string()
}

/// Go: `claudeDeviceProfileSubclientScope`. The CLI keeps the legacy base scope.
fn claude_device_profile_subclient_scope(profile: &ClaudeDeviceProfile) -> String {
    let (entrypoint, _) = parse_claude_code_user_agent_details(&profile.user_agent);
    if entrypoint.is_empty() || entrypoint == "cli" {
        return String::new();
    }
    if native_claude_entrypoint(&entrypoint) {
        return entrypoint;
    }
    "other".to_string()
}

/// Go: `claudeDeviceProfileScopedKey`: the credential scope plus the client subclient.
fn claude_device_profile_scoped_key(auth: Option<&Auth>, api_key: &str, profile: &ClaudeDeviceProfile) -> String {
    let mut key = claude_device_profile_scope_key(auth, api_key);
    let subclient = claude_device_profile_subclient_scope(profile);
    if !subclient.is_empty() {
        key.push_str("|subclient:");
        key.push_str(&subclient);
    }
    key
}

/// Go: `claudeDeviceProfileCacheKey` (sha256 hex of the scoped key).
fn claude_device_profile_cache_key(auth: Option<&Auth>, api_key: &str, profile: &ClaudeDeviceProfile) -> String {
    hex::encode(Sha256::digest(claude_device_profile_scoped_key(auth, api_key, profile).as_bytes()))
}

/// Go: `claudeDeviceProfileKVKey`: Home KV key of the stored profile.
fn claude_device_profile_kv_key(auth: Option<&Auth>, api_key: &str, profile: &ClaudeDeviceProfile) -> String {
    format!("cpa:claude:device-profile:{}", hash_key_part(&claude_device_profile_scoped_key(auth, api_key, profile)))
}

/// Go: `claudeDeviceProfileLockKVKey`: Home KV key of the short lock around profile upgrades.
fn claude_device_profile_lock_kv_key(auth: Option<&Auth>, api_key: &str, profile: &ClaudeDeviceProfile) -> String {
    format!(
        "cpa:claude:device-profile-lock:{}",
        hash_key_part(&claude_device_profile_scoped_key(auth, api_key, profile))
    )
}

/// Drops expired entries at most once per cleanup period (Go runs a goroutine ticker).
fn purge_expired_if_due(cache: &mut ProfileCache, now: Instant) {
    if now.duration_since(cache.last_cleanup) < CLAUDE_DEVICE_PROFILE_CLEANUP_PERIOD {
        return;
    }
    cache.entries.retain(|_, entry| entry.expire > now);
    cache.last_cleanup = now;
}

/// Go: `ResolveClaudeDeviceProfile`: the baseline profile when Home KV fails.
pub fn resolve_claude_device_profile(
    auth: Option<&Auth>,
    api_key: &str,
    headers: &HeaderMap,
    cfg: Option<&Config>,
) -> ClaudeDeviceProfile {
    resolve_claude_device_profile_required_blocking(auth, api_key, headers, cfg)
        .unwrap_or_else(|_| default_claude_device_profile(cfg))
}

/// Go: `ResolveClaudeDeviceProfileRequired`: a stable Claude Code device profile for request-time
/// paths. In Home mode the profile is shared through Home KV and any Home failure is an error.
pub async fn resolve_claude_device_profile_required(
    auth: Option<&Auth>,
    api_key: &str,
    headers: &HeaderMap,
    cfg: Option<&Config>,
) -> Result<ClaudeDeviceProfile, HomeError> {
    match home_kv::client()? {
        Some(client) => resolve_claude_device_profile_home(&client, auth, api_key, headers, cfg).await,
        None => Ok(resolve_claude_device_profile_local(auth, api_key, headers, cfg)),
    }
}

/// [`resolve_claude_device_profile_required`] for synchronous callers; the local path needs no
/// async runtime.
pub fn resolve_claude_device_profile_required_blocking(
    auth: Option<&Auth>,
    api_key: &str,
    headers: &HeaderMap,
    cfg: Option<&Config>,
) -> Result<ClaudeDeviceProfile, HomeError> {
    match home_kv::client()? {
        Some(client) => home_kv::call(resolve_claude_device_profile_home(&client, auth, api_key, headers, cfg)),
        None => Ok(resolve_claude_device_profile_local(auth, api_key, headers, cfg)),
    }
}

/// Go: `claudeDeviceProfileKVValue`: the stored profile (software tuple and platform).
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct ProfileKvValue {
    user_agent: String,
    package_version: String,
    runtime_version: String,
    os: String,
    arch: String,
}

impl ProfileKvValue {
    fn from_profile(profile: &ClaudeDeviceProfile) -> Self {
        Self {
            user_agent: profile.user_agent.clone(),
            package_version: profile.package_version.clone(),
            runtime_version: profile.runtime_version.clone(),
            os: profile.os.clone(),
            arch: profile.arch.clone(),
        }
    }

    /// Go: `ToProfile` (trims every field and parses the version).
    fn into_profile(self) -> ClaudeDeviceProfile {
        let user_agent = self.user_agent.trim().to_string();
        ClaudeDeviceProfile {
            version: parse_claude_cli_version(&user_agent),
            user_agent,
            package_version: self.package_version.trim().to_string(),
            runtime_version: self.runtime_version.trim().to_string(),
            os: self.os.trim().to_string(),
            arch: self.arch.trim().to_string(),
        }
    }
}

/// Go: `resolveClaudeDeviceProfileHome`: the profile lives in Home KV for 7 days. A native client's
/// candidate takes a 5 second lock (`SET NX`) so concurrent nodes agree; only a strictly newer
/// candidate than the stored profile replaces it, and a node that lost the lock serves the stored
/// profile.
async fn resolve_claude_device_profile_home(
    client: &Client,
    auth: Option<&Auth>,
    api_key: &str,
    headers: &HeaderMap,
    cfg: Option<&Config>,
) -> Result<ClaudeDeviceProfile, HomeError> {
    let baseline = default_claude_device_profile(cfg);
    let candidate = extract_claude_device_profile(headers, cfg)
        .map(|c| pin_claude_device_profile_platform(c, &baseline))
        .filter(|c| meets_claude_device_profile_baseline(c, &baseline));

    let scope_profile = candidate.clone().unwrap_or_default();
    let value_key = claude_device_profile_kv_key(auth, api_key, &scope_profile);
    let Some(candidate) = candidate else {
        return match read_claude_device_profile_value_from_home(client, &value_key, &baseline).await? {
            None => Ok(baseline),
            Some(profile) => {
                client.kv_expire(&value_key, CLAUDE_DEVICE_PROFILE_TTL).await?;
                Ok(profile)
            }
        };
    };

    let lock_key = claude_device_profile_lock_kv_key(auth, api_key, &scope_profile);
    let got_lock = client.kv_set_nx(&lock_key, b"1", CLAUDE_DEVICE_PROFILE_LOCK_TTL).await?;
    let hook = BEFORE_CANDIDATE_STORE.lock().clone();
    if let Some(hook) = hook {
        hook(&candidate);
    }

    let cached = read_claude_device_profile_value_from_home(client, &value_key, &baseline).await?;
    if let Some(cached) = &cached
        && !should_upgrade_claude_device_profile(&candidate, cached)
    {
        client.kv_expire(&value_key, CLAUDE_DEVICE_PROFILE_TTL).await?;
        return Ok(cached.clone());
    }
    if !got_lock {
        return cached
            .ok_or_else(|| HomeError::other("home kv device profile lock not acquired and profile missing"));
    }

    let raw = serde_json::to_vec(&ProfileKvValue::from_profile(&candidate)).map_err(HomeError::other)?;
    let opts = KvSetOptions { ex: CLAUDE_DEVICE_PROFILE_TTL, ..Default::default() };
    if !client.kv_set(&value_key, &raw, opts).await? {
        return Err(HomeError::other("home kv device profile write skipped"));
    }
    Ok(candidate)
}

/// Go: `readClaudeDeviceProfileValueFromHome`: the stored profile normalized against `baseline`,
/// `None` when absent or without a user agent.
async fn read_claude_device_profile_value_from_home(
    client: &Client,
    key: &str,
    baseline: &ClaudeDeviceProfile,
) -> Result<Option<ClaudeDeviceProfile>, HomeError> {
    let Some(raw) = client.kv_get(key).await? else { return Ok(None) };
    let value: ProfileKvValue = serde_json::from_slice(&raw).map_err(HomeError::other)?;
    let profile = value.into_profile();
    if profile.user_agent.is_empty() {
        return Ok(None);
    }
    Ok(Some(normalize_claude_device_profile(profile, baseline)))
}

/// Go: `resolveClaudeDeviceProfileLocal`: learns a native client's profile per credential,
/// upgrades only to strictly newer versions and never drops below the baseline.
fn resolve_claude_device_profile_local(
    auth: Option<&Auth>,
    api_key: &str,
    headers: &HeaderMap,
    cfg: Option<&Config>,
) -> ClaudeDeviceProfile {
    let now = Instant::now();
    let baseline = default_claude_device_profile(cfg);
    let candidate = extract_claude_device_profile(headers, cfg)
        .map(|c| pin_claude_device_profile_platform(c, &baseline))
        .filter(|c| meets_claude_device_profile_baseline(c, &baseline));
    let cache_key = match &candidate {
        Some(c) => claude_device_profile_cache_key(auth, api_key, c),
        None => claude_device_profile_cache_key(auth, api_key, &ClaudeDeviceProfile::default()),
    };
    let ttl_expire = now + CLAUDE_DEVICE_PROFILE_TTL;

    if let Some(candidate) = candidate {
        let hook = BEFORE_CANDIDATE_STORE.lock().clone();
        if let Some(hook) = hook {
            hook(&candidate);
        }
        let mut cache = DEVICE_PROFILE_CACHE.write();
        purge_expired_if_due(&mut cache, now);
        if let Some(entry) = cache.entries.get_mut(&cache_key)
            && entry.expire > now
            && !entry.profile.user_agent.is_empty()
        {
            entry.profile = normalize_claude_device_profile(std::mem::take(&mut entry.profile), &baseline);
            if !should_upgrade_claude_device_profile(&candidate, &entry.profile) {
                entry.expire = ttl_expire;
                return entry.profile.clone();
            }
        }
        cache.entries.insert(cache_key, CacheEntry { profile: candidate.clone(), expire: ttl_expire });
        return candidate;
    }

    let mut cache = DEVICE_PROFILE_CACHE.write();
    purge_expired_if_due(&mut cache, now);
    if let Some(entry) = cache.entries.get_mut(&cache_key)
        && entry.expire > now
        && !entry.profile.user_agent.is_empty()
    {
        entry.profile = normalize_claude_device_profile(std::mem::take(&mut entry.profile), &baseline);
        entry.expire = ttl_expire;
        return entry.profile.clone();
    }
    baseline
}

/// Sets `name` to `value` unless it is not a valid header name/value (Go would send it as is).
pub(crate) fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
        headers.insert(n, v);
    }
}

/// Go: `ApplyClaudeDeviceProfileHeaders`.
pub fn apply_claude_device_profile_headers(headers: &mut HeaderMap, profile: &ClaudeDeviceProfile) {
    for name in [
        "User-Agent",
        "X-Stainless-Package-Version",
        "X-Stainless-Runtime-Version",
        "X-Stainless-Os",
        "X-Stainless-Arch",
    ] {
        headers.remove(name);
    }
    set_header(headers, "User-Agent", &profile.user_agent);
    set_header(headers, "X-Stainless-Package-Version", &profile.package_version);
    set_header(headers, "X-Stainless-Runtime-Version", &profile.runtime_version);
    set_header(headers, "X-Stainless-Os", &profile.os);
    set_header(headers, "X-Stainless-Arch", &profile.arch);
}

/// Go: `DefaultClaudeVersion` (e.g. "2.1.280" from the baseline user agent).
pub fn default_claude_version(cfg: Option<&Config>) -> String {
    match parse_claude_cli_version(&default_claude_device_profile(cfg).user_agent) {
        Some(v) => format!("{}.{}.{}", v.major, v.minor, v.patch),
        None => "2.1.280".to_string(),
    }
}

/// Go: `ApplyClaudeDefaultDeviceProfileHeaders`.
pub fn apply_claude_default_device_profile_headers(headers: &mut HeaderMap, cfg: Option<&Config>) {
    apply_claude_device_profile_headers(headers, &default_claude_device_profile(cfg));
}

/// Go: `ApplyClaudeLegacyDeviceHeaders`. `incoming` is Go's `ginHeaders` (client request headers).
pub fn apply_claude_legacy_device_headers(
    headers: &mut HeaderMap,
    incoming: &HeaderMap,
    cfg: Option<&Config>,
    confirmed_claude_code: bool,
) {
    let profile = default_claude_device_profile(cfg);
    // Go's `miscEnsure`: keep a valid current value, else a valid incoming one, else the fallback.
    let ensure = |headers: &mut HeaderMap, name: &str, fallback: &str, valid: Option<&dyn Fn(&str) -> bool>| {
        let ok = |v: &str| valid.is_none_or(|f| f(v));
        let current = header_value(headers, name);
        if !current.trim().is_empty() && ok(current.trim()) {
            return;
        }
        let incoming_value = header_value(incoming, name);
        if !incoming_value.trim().is_empty() && ok(incoming_value.trim()) {
            set_header(headers, name, incoming_value.trim());
            return;
        }
        set_header(headers, name, fallback);
    };

    if confirmed_claude_code {
        let runtime_ok = |v: &str| v == profile.runtime_version;
        let package_ok = |v: &str| v == profile.package_version;
        ensure(headers, "X-Stainless-Runtime-Version", &profile.runtime_version, Some(&runtime_ok));
        ensure(headers, "X-Stainless-Package-Version", &profile.package_version, Some(&package_ok));
        ensure(headers, "X-Stainless-Os", &map_stainless_os(), None);
        ensure(headers, "X-Stainless-Arch", &map_stainless_arch(), None);
        let client_ua = header_value(incoming, "User-Agent");
        let client_ua = client_ua.trim();
        if plausible_claude_code_user_agent(client_ua, cfg) {
            set_header(headers, "User-Agent", client_ua);
            return;
        }
    }

    // Unconfirmed clients must not leak a copied or third-party software profile.
    set_header(headers, "X-Stainless-Runtime-Version", &profile.runtime_version);
    set_header(headers, "X-Stainless-Package-Version", &profile.package_version);
    set_header(headers, "X-Stainless-Os", &profile.os);
    set_header(headers, "X-Stainless-Arch", &profile.arch);
    set_header(headers, "User-Agent", &profile.user_agent);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    fn device_headers(user_agent: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        set_header(&mut h, "User-Agent", user_agent);
        set_header(&mut h, "X-Stainless-Package-Version", DEFAULT_CLAUDE_FINGERPRINT_PACKAGE_VERSION);
        set_header(&mut h, "X-Stainless-Runtime-Version", DEFAULT_CLAUDE_FINGERPRINT_RUNTIME_VERSION);
        set_header(&mut h, "X-Stainless-Os", "Windows");
        set_header(&mut h, "X-Stainless-Arch", "x64");
        h
    }

    fn auth(id: &str) -> Auth {
        Auth::new(id, "claude")
    }

    fn header(h: &HeaderMap, name: &str) -> String {
        header_value(h, name)
    }

    fn software(p: &ClaudeDeviceProfile) -> (&str, &str, &str) {
        (&p.user_agent, &p.package_version, &p.runtime_version)
    }

    #[test]
    fn local_resolution_falls_back_to_baseline_for_unmeasured_signals() {
        let baseline = default_claude_device_profile(None);
        let mut invalid = device_headers("claude-cli/999.0.0 (external, cli)");
        set_header(&mut invalid, "X-Stainless-Package-Version", "999.0.0");
        set_header(&mut invalid, "X-Stainless-Runtime-Version", "v999.0.0");
        let got = resolve_claude_device_profile(Some(&auth("dp-invalid")), "api-key", &invalid, None);
        assert_eq!(software(&got), software(&baseline));

        // A newer patch release is not an exact measured software tuple either.
        let newer = device_headers("claude-cli/2.1.281 (external, cli)");
        let got = resolve_claude_device_profile(Some(&auth("dp-newer-patch")), "api-key", &newer, None);
        assert_eq!(software(&got), software(&baseline));
    }

    #[test]
    fn confirmed_baseline_client_is_preserved_and_subclients_are_isolated() {
        let a = auth("dp-subclient-isolation");
        let cli_ua = "claude-cli/2.1.280 (external, cli)";
        let vscode_ua = "claude-cli/2.1.280 (external, claude-vscode, agent-sdk/0.3.220)";
        let cli = resolve_claude_device_profile(Some(&a), "api-key", &device_headers(cli_ua), None);
        assert_eq!(cli.user_agent, cli_ua);
        assert_eq!((cli.package_version.as_str(), cli.runtime_version.as_str()), ("0.112.1", "v26.3.0"));
        assert_eq!((cli.os.as_str(), cli.arch.as_str()), ("MacOS", "arm64"), "platform is pinned to the baseline");

        let vscode = resolve_claude_device_profile(Some(&a), "api-key", &device_headers(vscode_ua), None);
        assert_eq!(vscode.user_agent, vscode_ua);
        // The CLI scope is untouched by the VS Code client; no headers resolves the CLI scope.
        let again = resolve_claude_device_profile(Some(&a), "api-key", &HeaderMap::new(), None);
        assert_eq!(again.user_agent, cli_ua);
        assert_ne!(
            claude_device_profile_cache_key(Some(&a), "api-key", &cli),
            claude_device_profile_cache_key(Some(&a), "api-key", &vscode)
        );
    }

    #[test]
    fn upgrade_recheck_keeps_the_cached_profile_when_a_racing_candidate_stores_first() {
        const UA: &str = "claude-cli/2.1.60 (external, cli)";
        let mut cfg = Config::default();
        cfg.claude_header_defaults.user_agent = UA.into();
        cfg.claude_header_defaults.package_version = "0.70.0".into();
        cfg.claude_header_defaults.runtime_version = "v22.0.0".into();
        cfg.claude_header_defaults.os = "MacOS".into();
        cfg.claude_header_defaults.arch = "arm64".into();
        let cfg = Arc::new(cfg);
        let a = Arc::new(auth("dp-racy-upgrade"));

        let mk = |os: &str, arch: &str| {
            let mut h = device_headers(UA);
            set_header(&mut h, "X-Stainless-Package-Version", "0.70.0");
            set_header(&mut h, "X-Stainless-Runtime-Version", "v22.0.0");
            set_header(&mut h, "X-Stainless-Os", os);
            set_header(&mut h, "X-Stainless-Arch", arch);
            h
        };

        let (paused_tx, paused_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let paused_once = AtomicBool::new(false);
        set_claude_device_profile_before_candidate_store(Some(Arc::new(move |candidate: &ClaudeDeviceProfile| {
            if candidate.user_agent != UA || paused_once.swap(true, Ordering::SeqCst) {
                return;
            }
            let _ = paused_tx.send(());
            let _ = release_rx.lock().recv();
        })));

        let (low_cfg, low_auth, low_headers) = (cfg.clone(), a.clone(), mk("Linux", "x64"));
        let low = std::thread::spawn(move || {
            resolve_claude_device_profile(Some(&low_auth), "key-racy-upgrade", &low_headers, Some(&low_cfg))
        });
        paused_rx.recv_timeout(Duration::from_secs(5)).expect("low candidate paused before storing");
        let high = resolve_claude_device_profile(Some(&a), "key-racy-upgrade", &mk("MacOS", "arm64"), Some(&cfg));
        let _ = release_tx.send(());
        let low = low.join().expect("low thread");
        set_claude_device_profile_before_candidate_store(None);

        for p in [&low, &high] {
            assert_eq!((p.user_agent.as_str(), p.package_version.as_str()), (UA, "0.70.0"));
            assert_eq!((p.os.as_str(), p.arch.as_str()), ("MacOS", "arm64"));
        }
    }

    #[test]
    fn legacy_headers_replace_invalid_native_software_signals() {
        let baseline = default_claude_device_profile(None);
        let mut incoming = device_headers("claude-cli/999.0.0 (external, cli)");
        set_header(&mut incoming, "X-Stainless-Package-Version", "999.0.0");
        set_header(&mut incoming, "X-Stainless-Runtime-Version", "v999.0.0");
        let mut out = HeaderMap::new();
        apply_claude_legacy_device_headers(&mut out, &incoming, None, true);
        assert_eq!(header(&out, "User-Agent"), baseline.user_agent);
        assert_eq!(header(&out, "X-Stainless-Package-Version"), baseline.package_version);
        assert_eq!(header(&out, "X-Stainless-Runtime-Version"), baseline.runtime_version);
    }

    #[test]
    fn legacy_headers_accept_configured_measured_baseline() {
        let mut cfg = Config::default();
        cfg.claude_header_defaults.user_agent = "claude-cli/2.2.0 (external, cli)".into();
        cfg.claude_header_defaults.package_version = "0.95.0".into();
        cfg.claude_header_defaults.runtime_version = "v26.4.0".into();
        cfg.claude_header_defaults.os = "MacOS".into();
        cfg.claude_header_defaults.arch = "arm64".into();
        let mut incoming = device_headers("claude-cli/2.2.0 (external, cli)");
        set_header(&mut incoming, "X-Stainless-Package-Version", "0.95.0");
        set_header(&mut incoming, "X-Stainless-Runtime-Version", "v26.4.0");
        let mut out = HeaderMap::new();
        apply_claude_legacy_device_headers(&mut out, &incoming, Some(&cfg), true);
        assert_eq!(header(&out, "User-Agent"), "claude-cli/2.2.0 (external, cli)");
        assert_eq!(header(&out, "X-Stainless-Package-Version"), "0.95.0");
        assert_eq!(header(&out, "X-Stainless-Runtime-Version"), "v26.4.0");
        // Confirmed clients keep their own platform headers.
        assert_eq!(header(&out, "X-Stainless-Os"), "Windows");
        assert_eq!(header(&out, "X-Stainless-Arch"), "x64");

        let mut unconfirmed = HeaderMap::new();
        apply_claude_legacy_device_headers(&mut unconfirmed, &incoming, Some(&cfg), false);
        assert_eq!(header(&unconfirmed, "X-Stainless-Os"), "MacOS");
    }

    #[test]
    fn default_version_and_profile_headers() {
        assert_eq!(default_claude_version(None), "2.1.280");
        let mut h = HeaderMap::new();
        set_header(&mut h, "x-stainless-os", "stale");
        apply_claude_default_device_profile_headers(&mut h, None);
        assert_eq!(header(&h, "X-Stainless-Os"), "MacOS");
        assert_eq!(h.get_all("x-stainless-os").iter().count(), 1);
    }
}

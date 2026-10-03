//! Server config variations used by scenarios (applied on top of `ConfigSpec::baseline`).

use crate::config::{ConfigSpec, KeyEntry, ModelCfg, ScopedRule};

fn all_keys(s: &mut ConfigSpec) -> impl Iterator<Item = &mut KeyEntry> {
    s.claude.iter_mut().chain(s.codex.iter_mut()).chain(s.gemini.iter_mut())
}

pub fn no_retry(s: &mut ConfigSpec) {
    s.request_retry = 0;
}

/// Retry rounds without transient cooldown, so every round is immediate.
pub fn rounds_no_cooldown(s: &mut ConfigSpec) {
    s.request_retry = 3;
    s.transient_cooldown_seconds = Some(-1);
}

pub fn max_one_credential(s: &mut ConfigSpec) {
    s.max_retry_credentials = 1;
}

pub fn disable_cooling(s: &mut ConfigSpec) {
    s.disable_cooling = true;
}

pub fn fill_first(s: &mut ConfigSpec) {
    s.strategy = Some("fill-first".into());
}

/// Weighted round robin, 3:1 in favor of the first key of each family.
pub fn weighted(s: &mut ConfigSpec) {
    s.strategy = Some("weighted-round-robin".into());
    for list in [&mut s.claude, &mut s.codex, &mut s.gemini] {
        for (i, k) in list.iter_mut().enumerate() {
            k.weight = Some(if i == 0 { 3 } else { 1 });
        }
    }
}

/// Second key of every family outranks the first.
pub fn priority(s: &mut ConfigSpec) {
    for list in [&mut s.claude, &mut s.codex, &mut s.gemini] {
        if let Some(k) = list.get_mut(1) {
            k.priority = Some(10);
        }
    }
}

/// First claude key serves models under the `team/` prefix.
pub fn prefix(s: &mut ConfigSpec) {
    s.claude[0].prefix = Some("team".into());
}

pub fn prefix_forced(s: &mut ConfigSpec) {
    prefix(s);
    s.force_model_prefix = true;
}

/// Claude keys expose one model under an alias instead of the catalog.
pub fn claude_alias(s: &mut ConfigSpec) {
    for k in &mut s.claude {
        k.models = vec![ModelCfg { name: "claude-sonnet-4-5-20250929".into(), alias: "sonnet-alias".into(), ..Default::default() }];
    }
}

pub fn claude_excluded(s: &mut ConfigSpec) {
    for k in &mut s.claude {
        k.excluded_models = vec!["claude-sonnet-*".into(), "claude-3-*".into()];
    }
}

/// One round, no cooldown: a stream whose bootstrap fails on every key fails as a whole.
pub fn bootstrap_off(s: &mut ConfigSpec) {
    no_retry(s);
    disable_cooling(s);
}

/// Same, but the handler may call the executor again before the first byte.
pub fn bootstrap_one(s: &mut ConfigSpec) {
    bootstrap_off(s);
    s.bootstrap_retries = 1;
}

pub fn stream_keepalive(s: &mut ConfigSpec) {
    s.keepalive_seconds = 1;
}

pub fn nonstream_keepalive(s: &mut ConfigSpec) {
    s.nonstream_keepalive = 1;
}

pub fn passthrough_headers(s: &mut ConfigSpec) {
    s.passthrough_headers = true;
}

pub fn codex_websockets(s: &mut ConfigSpec) {
    for k in &mut s.codex {
        k.websockets = Some(true);
    }
}

/// Custom upstream headers on every key; `$X-Client-Tag` copies the client's header.
pub fn custom_headers(s: &mut ConfigSpec) {
    for k in all_keys(s) {
        k.headers = vec![("X-E2E-Static".into(), "static-value".into()), ("X-E2E-Copied".into(), "$X-Client-Tag".into())];
    }
    s.compat[0].headers = vec![("X-E2E-Static".into(), "static-value".into()), ("X-E2E-Copied".into(), "$X-Client-Tag".into())];
}

/// Claude 429s whose body mentions `rate_limit` stop the request (no failover, no cooldown).
pub fn scoped_stop(s: &mut ConfigSpec) {
    for k in &mut s.claude {
        k.scoped_errors = vec![ScopedRule { status: 429, matches: "rate_limit", action: "stop" }];
    }
}

/// Claude 400s are rotated to the next key and cool the credential.
pub fn scoped_continue_cooldown(s: &mut ConfigSpec) {
    for k in &mut s.claude {
        k.scoped_errors = vec![ScopedRule { status: 400, matches: "mock upstream", action: "continue-and-cooldown" }];
    }
}

/// A template client key switches the server into safe mode (proxy endpoints answer 403).
pub fn example_keys(s: &mut ConfigSpec) {
    s.client_keys.push("your-api-key-1".into());
}

pub fn usage_stats(s: &mut ConfigSpec) {
    s.usage_statistics = true;
}

pub fn no_management(s: &mut ConfigSpec) {
    s.management_secret = None;
}

/// No client API keys configured: the proxy API is open.
pub fn open_access(s: &mut ConfigSpec) {
    s.client_keys.clear();
}

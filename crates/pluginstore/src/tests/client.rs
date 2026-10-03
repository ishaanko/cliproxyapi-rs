//! Ports of Go `github_test.go`, `request_identity_test.go`, `github_rate_limit_test.go` and
//! the client-level parts of `auth_test.go`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{Duration, Utc};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::auth::*;
use crate::error::Context;
use crate::github::{Client, Release, ReleaseAsset, plugin_store_request_error, release_version, select_release_assets};
use crate::http::{BodyReader, DoError, Headers, HttpResponse, ReqwestDoer};
use crate::ratelimit::{GitHubRateLimiter, github_rate_limit_key};
use crate::registry::{Artifact, DEFAULT_REGISTRY_URL, Plugin};
use crate::testutil::*;

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn plugin(repository: &str) -> Plugin {
    Plugin { repository: repository.into(), ..Default::default() }
}

fn artifact(url: &str, sha256: &str) -> Artifact {
    Artifact { goos: "linux".into(), goarch: "amd64".into(), url: url.into(), sha256: sha256.into(), size: 0 }
}

fn github_token_resolved(token: &str) -> Vec<ResolvedAuthConfig> {
    vec![ResolvedAuthConfig {
        match_url: "https://api.github.com/".into(),
        kind: AUTH_TYPE_GITHUB_TOKEN.into(),
        token: Secret::from(token),
        ..Default::default()
    }]
}

fn release_ok() -> Result<HttpResponse, DoError> {
    Ok(HttpResponse::from_bytes(200, Headers::new(), r#"{"tag_name":"v1.0.0"}"#))
}

fn limited(retry_after: &str) -> Result<HttpResponse, DoError> {
    Ok(HttpResponse::from_bytes(429, headers(&[("Retry-After", retry_after)]), "limited"))
}

// --- github_test.go ---

#[test]
fn select_release_assets_picks_archive_and_checksums() {
    let release = Release {
        tag_name: String::new(),
        assets: vec![
            ReleaseAsset {
                name: "sample-provider_0.1.0_darwin_arm64.zip".into(),
                browser_download_url: "https://example.com/sample-provider.zip".into(),
                ..Default::default()
            },
            ReleaseAsset {
                name: "checksums.txt".into(),
                browser_download_url: "https://example.com/checksums.txt".into(),
                ..Default::default()
            },
        ],
    };
    let (archive, checksums) =
        select_release_assets(&release, "sample-provider", "0.1.0", "darwin", "arm64").expect("select");
    assert_eq!(archive.browser_download_url, "https://example.com/sample-provider.zip");
    assert_eq!(checksums.browser_download_url, "https://example.com/checksums.txt");
}

#[test]
fn select_release_assets_rejects_missing_assets() {
    let zip = ReleaseAsset {
        name: "sample-provider_0.1.0_darwin_arm64.zip".into(),
        browser_download_url: "https://example.com/sample-provider.zip".into(),
        ..Default::default()
    };
    let checksums = ReleaseAsset {
        name: "checksums.txt".into(),
        browser_download_url: "https://example.com/checksums.txt".into(),
        ..Default::default()
    };
    let cases = [
        ("missing zip", vec![checksums.clone()], "sample-provider_0.1.0_darwin_arm64.zip"),
        ("missing checksum", vec![zip], "checksums.txt"),
    ];
    for (name, assets, want) in cases {
        let release = Release { tag_name: String::new(), assets };
        let err = select_release_assets(&release, "sample-provider", "0.1.0", "darwin", "arm64").expect_err(name);
        assert!(err.to_string().contains(want), "{name}: {err}");
    }
}

#[test]
fn release_version_cases() {
    let cases = [
        ("v prefix", "v1.2.3", Some("1.2.3")),
        ("no prefix", "0.1.0", Some("0.1.0")),
        ("whitespace", " v2.0.0 ", Some("2.0.0")),
        ("empty", "", None),
        ("non numeric", "latest", None),
    ];
    for (name, tag, want) in cases {
        let got = release_version(&Release { tag_name: tag.into(), assets: Vec::new() });
        match want {
            Some(want) => assert_eq!(got.expect(name), want, "{name}"),
            None => assert!(got.is_err(), "{name}"),
        }
    }
}

// --- request_identity_test.go ---

#[test]
fn latest_release_cache_key_normalizes_repository() {
    let client = Client::default();
    let key = client.latest_release_cache_key(&plugin("https://github.com/Owner/Repo/")).expect("key");
    let other = client.latest_release_cache_key(&plugin("https://github.com/owner/repo")).expect("key");
    assert_eq!(key, other);
}

#[test]
fn latest_release_cache_key_tracks_environment_credentials() {
    let token = Arc::new(Mutex::new("secret-token-one".to_string()));
    let env_token = token.clone();
    let mut client = Client::default();
    client.auth = vec![AuthConfig {
        match_url: "https://api.github.com/".into(),
        kind: AUTH_TYPE_GITHUB_TOKEN.into(),
        token_env: "PLUGIN_STORE_CACHE_TEST_TOKEN".into(),
        ..Default::default()
    }];
    client.env = Some(Arc::new(move |_| env_token.lock().clone()));
    let plugin = plugin("https://github.com/owner/repo");
    let first = client.latest_release_cache_key(&plugin).expect("key");
    *token.lock() = "secret-token-two".to_string();
    let second = client.latest_release_cache_key(&plugin).expect("key");
    assert_ne!(first, second, "credential rotation did not change key");
    assert!(!format!("{first}{second}").contains("secret-token"), "cache keys retain raw credentials");
}

#[tokio::test]
async fn prepare_latest_release_snapshots_environment_credentials() {
    let token = Arc::new(Mutex::new("original-token".to_string()));
    let env_token = token.clone();
    let mut client = Client::default();
    client.auth = vec![AuthConfig {
        match_url: "https://api.github.com/".into(),
        kind: AUTH_TYPE_GITHUB_TOKEN.into(),
        token_env: "PLUGIN_STORE_SNAPSHOT_TEST_TOKEN".into(),
        ..Default::default()
    }];
    client.env = Some(Arc::new(move |_| env_token.lock().clone()));
    client.http_client = Some(fn_doer(|request| {
        assert_eq!(request.headers.get("Authorization"), "Bearer original-token", "request did not use the snapshot");
        release_ok()
    }));
    client.rate_limiter = Some(Arc::new(GitHubRateLimiter::new()));
    let plugin = plugin("https://github.com/owner/repo");
    let (prepared, original_key) = client.prepare_latest_release(&plugin).expect("prepare");
    *token.lock() = "rotated-token".to_string();
    let rotated_key = client.latest_release_cache_key(&plugin).expect("key");
    assert_ne!(original_key, rotated_key, "rotation did not change the unprepared identity");
    prepared.fetch_latest_release(&Context::background(), &plugin).await.expect("release");
}

#[test]
fn latest_release_cache_key_rejects_expired_credentials() {
    let mut client = Client::default();
    client.resolved_auth = github_token_resolved("secret");
    client.resolved_auth_expires_at = Some(Utc::now() - Duration::hours(1));
    assert!(
        client.latest_release_cache_key(&plugin("https://github.com/owner/repo")).is_err(),
        "expired credentials must not access a cached private release"
    );
}

// --- github_rate_limit_test.go (client level) ---

#[tokio::test]
async fn cooldown_shared_across_clients_repositories_and_assets() {
    let now = Arc::new(Mutex::new(Utc::now()));
    let clock = now.clone();
    let limiter = Arc::new(GitHubRateLimiter::with_clock(move || *clock.lock()));
    let calls = Arc::new(AtomicU32::new(0));
    let counter = calls.clone();
    let doer = fn_doer(move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 { limited("60") } else { release_ok() }
    });
    let make = || Client { http_client: Some(doer.clone()), rate_limiter: Some(limiter.clone()), ..Default::default() };
    let (first, second) = (make(), make());
    let ctx = Context::background();
    let err_first = first.fetch_latest_release(&ctx, &plugin("https://github.com/owner/first")).await;
    let err_second =
        second.fetch_release_by_tag(&ctx, &plugin("https://github.com/other/second"), "v1.0.0").await;
    let err_asset = second
        .download_asset(
            &ctx,
            &ReleaseAsset {
                api_url: "https://api.github.com:443/repos/other/second/releases/assets/1".into(),
                ..Default::default()
            },
        )
        .await;
    for err in [err_first.err(), err_second.err(), err_asset.err()] {
        assert!(err.expect("request failed").rate_limit().is_some());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    second
        .get(&ctx, DEFAULT_REGISTRY_URL, "application/json", REQUEST_KIND_REGISTRY, 0)
        .await
        .expect("API cooldown must not block the raw registry");
    *now.lock() += Duration::minutes(1);
    second
        .fetch_latest_release(&ctx, &plugin("https://github.com/other/second"))
        .await
        .expect("recovery");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn cooldown_separates_credentials_and_network_scope() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = calls.clone();
    let mut client = Client {
        rate_limiter: Some(Arc::new(GitHubRateLimiter::new())),
        http_client: Some(fn_doer(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            limited("3600")
        })),
        ..Default::default()
    };
    let plugin = plugin("https://github.com/owner/repo");
    let ctx = Context::background();
    for token in ["", "token-a", "token-b", "token-a", ""] {
        client.resolved_auth = if token.is_empty() { Vec::new() } else { github_token_resolved(token) };
        let _ = client.fetch_latest_release(&ctx, &plugin).await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3, "credential scopes");
    client.network_scope = "other-egress".into();
    let _ = client.fetch_latest_release(&ctx, &plugin).await;
    assert_eq!(calls.load(Ordering::SeqCst), 4, "egress scopes");
}

#[tokio::test]
async fn authenticated_rate_limit_body_is_not_exposed() {
    let client = Client {
        rate_limiter: Some(Arc::new(GitHubRateLimiter::new())),
        resolved_auth: github_token_resolved("secret-token"),
        http_client: Some(fn_doer(|_| {
            Ok(HttpResponse::from_bytes(
                403,
                Headers::new(),
                r#"{"message":"secondary rate limit: secret-token"}"#,
            ))
        })),
        ..Default::default()
    };
    let err = client
        .fetch_latest_release(&Context::background(), &plugin("https://github.com/owner/repo"))
        .await
        .expect_err("rate limited");
    assert!(err.rate_limit().is_some() && !err.to_string().contains("secret-token"), "{err}");
}

#[tokio::test]
async fn default_cooldown_blocks_concurrent_new_clients() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = calls.clone();
    let doer = fn_doer(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        limited("3600")
    });
    let scope = "default_cooldown_blocks_concurrent_new_clients";
    let key = github_rate_limit_key("https://api.github.com/", scope, &Headers::new(), false);
    let first = Client { network_scope: scope.into(), http_client: Some(doer.clone()), ..Default::default() };
    let _ = first
        .fetch_latest_release(&Context::background(), &plugin("https://github.com/owner/first"))
        .await;
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let client = Client { network_scope: scope.into(), http_client: Some(doer.clone()), ..Default::default() };
        tasks.push(tokio::spawn(async move {
            client
                .fetch_latest_release(&Context::background(), &plugin("https://github.com/other/second"))
                .await
        }));
    }
    for task in tasks {
        let err = task.await.expect("join").expect_err("blocked");
        assert!(err.rate_limit().is_some(), "request error = {err}");
    }
    crate::ratelimit::DEFAULT_GITHUB_RATE_LIMITER.remove_entry(&key);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Body that fails when read and records whether it was dropped (closed).
struct HeaderOnlyBody {
    reads: Arc<AtomicU32>,
    closed: Arc<AtomicBool>,
}

#[async_trait]
impl BodyReader for HeaderOnlyBody {
    async fn chunk(&mut self) -> std::io::Result<Option<Bytes>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::other("body must not be read"))
    }
}

impl Drop for HeaderOnlyBody {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn header_cooldown_closes_body_without_reading() {
    let reads = Arc::new(AtomicU32::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let (body_reads, body_closed) = (reads.clone(), closed.clone());
    let client = Client {
        rate_limiter: Some(Arc::new(GitHubRateLimiter::new())),
        http_client: Some(fn_doer(move |_| {
            Ok(HttpResponse {
                status: 429,
                headers: headers(&[("Retry-After", "60")]),
                body: Box::new(HeaderOnlyBody { reads: body_reads.clone(), closed: body_closed.clone() }),
            })
        })),
        ..Default::default()
    };
    let err = client
        .fetch_latest_release(&Context::background(), &plugin("https://github.com/owner/repo"))
        .await
        .expect_err("rate limited");
    assert!(err.rate_limit().is_some());
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert!(closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn successful_final_request_returns_release() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = calls.clone();
    let reset = (Utc::now() + Duration::hours(1)).timestamp().to_string();
    let client = Client {
        rate_limiter: Some(Arc::new(GitHubRateLimiter::new())),
        http_client: Some(fn_doer(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(HttpResponse::from_bytes(
                200,
                headers(&[("X-RateLimit-Remaining", "0"), ("X-RateLimit-Reset", reset.as_str())]),
                r#"{"tag_name":"v1.0.0"}"#,
            ))
        })),
        ..Default::default()
    };
    let ctx = Context::background();
    let release = client.fetch_latest_release(&ctx, &plugin("https://github.com/owner/repo")).await.expect("release");
    assert_eq!(release.tag_name, "v1.0.0");
    let err = client
        .fetch_latest_release(&ctx, &plugin("https://github.com/owner/other"))
        .await
        .expect_err("cooldown after final request");
    assert!(err.rate_limit().is_some() && calls.load(Ordering::SeqCst) == 1, "{err}");
}

#[tokio::test]
async fn non_github_rate_limit_does_not_cool_github() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = calls.clone();
    let client = Client {
        rate_limiter: Some(Arc::new(GitHubRateLimiter::new())),
        http_client: Some(fn_doer(move |request| {
            counter.fetch_add(1, Ordering::SeqCst);
            if !request.url.starts_with("https://api.github.com/") { limited("3600") } else { release_ok() }
        })),
        ..Default::default()
    };
    let ctx = Context::background();
    let err = client.fetch_registry(&ctx).await.expect_err("non-API error");
    assert!(err.rate_limit().is_none(), "{err}");
    client
        .fetch_latest_release(&ctx, &plugin("https://github.com/owner/repo"))
        .await
        .expect("release");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// --- auth_test.go (client level) ---

fn test_transport() -> Arc<dyn crate::http::HttpDoer> {
    let client = ReqwestDoer::client_builder().no_proxy().build().expect("client");
    Arc::new(ReqwestDoer::new(client))
}

fn header_rule(match_url: &str, kind: &str, allow_insecure: bool) -> AuthConfig {
    AuthConfig {
        match_url: match_url.into(),
        apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
        kind: kind.into(),
        header_name: "X-Plugin-Token".into(),
        header_value_env: "PLUGIN_STORE_HEADER".into(),
        allow_insecure,
        ..Default::default()
    }
}

#[tokio::test]
async fn auth_header_is_reevaluated_across_redirect() {
    let artifact_data = b"artifact-data".to_vec();
    let redirected = Arc::new(Mutex::new(None::<String>));
    let seen = redirected.clone();
    let body = artifact_data.clone();
    let target = spawn_server(move |_, request_headers| {
        *seen.lock() = Some(request_headers.get("X-Plugin-Token").to_string());
        (200, vec![], body.clone())
    })
    .await;
    let initial = Arc::new(Mutex::new(None::<String>));
    let seen_initial = initial.clone();
    let location = format!("{target}/artifact.zip");
    let source = spawn_server(move |_, request_headers| {
        *seen_initial.lock() = Some(request_headers.get("X-Plugin-Token").to_string());
        (302, vec![("Location".to_string(), location.clone())], Vec::new())
    })
    .await;

    let client = Client {
        http_client: Some(test_transport()),
        env: Some(env_fn(&[("PLUGIN_STORE_HEADER", "secret-token")])),
        auth: vec![
            header_rule(&format!("{source}/private/"), AUTH_TYPE_HEADER, true),
            AuthConfig {
                match_url: format!("{target}/"),
                apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
                kind: AUTH_TYPE_NONE.into(),
                allow_insecure: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let data = client
        .download_artifact(&Context::background(), &artifact(&format!("{source}/private/artifact.zip"), SHA))
        .await
        .expect("download");
    assert_eq!(data, artifact_data);
    assert_eq!(initial.lock().as_deref(), Some("secret-token"));
    assert_eq!(redirected.lock().as_deref(), Some(""), "redirect target must not receive the header");
}

#[tokio::test]
async fn auth_header_is_applied_to_matching_redirect() {
    let redirected = Arc::new(Mutex::new(None::<String>));
    let seen = redirected.clone();
    let server = spawn_server(move |path, request_headers| {
        if path == "/private/start.zip" {
            return (302, vec![("Location".to_string(), "/private/artifact.zip".to_string())], Vec::new());
        }
        *seen.lock() = Some(request_headers.get("X-Plugin-Token").to_string());
        (200, vec![], b"artifact-data".to_vec())
    })
    .await;
    let client = Client {
        http_client: Some(test_transport()),
        env: Some(env_fn(&[("PLUGIN_STORE_HEADER", "secret-token")])),
        auth: vec![header_rule(&format!("{server}/private/"), AUTH_TYPE_HEADER, true)],
        ..Default::default()
    };
    client
        .download_artifact(&Context::background(), &artifact(&format!("{server}/private/start.zip"), SHA))
        .await
        .expect("download");
    assert_eq!(redirected.lock().as_deref(), Some("secret-token"));
}

#[tokio::test]
async fn resolved_auth_is_not_forwarded_across_origin_redirect() {
    let redirected_auth = Arc::new(Mutex::new(None::<String>));
    let seen = redirected_auth.clone();
    let client = Client {
        http_client: Some(fn_doer(move |request| {
            if request.url.starts_with("https://source.example/") {
                return Ok(HttpResponse::from_bytes(
                    302,
                    headers(&[("Location", "https://target.example/artifact.zip")]),
                    "",
                ));
            }
            *seen.lock() = Some(request.headers.get("Authorization").to_string());
            Ok(HttpResponse::from_bytes(200, Headers::new(), "artifact"))
        })),
        resolved_auth: vec![ResolvedAuthConfig {
            match_url: "https://source.example/private/".into(),
            apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
            kind: AUTH_TYPE_BEARER.into(),
            token: Secret::from("temporary-token"),
            ..Default::default()
        }],
        ..Default::default()
    };
    client
        .download_artifact(&Context::background(), &artifact("https://source.example/private/artifact.zip", SHA))
        .await
        .expect("download");
    assert_eq!(redirected_auth.lock().as_deref(), Some(""));
}

#[tokio::test]
async fn authenticated_failure_does_not_expose_response_body() {
    let client = Client {
        http_client: Some(fn_doer(|_| {
            Ok(HttpResponse::from_bytes(401, Headers::new(), "secret diagnostic body"))
        })),
        resolved_auth: vec![ResolvedAuthConfig {
            match_url: "https://downloads.example/".into(),
            apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
            kind: AUTH_TYPE_BEARER.into(),
            token: Secret::from("temporary-token"),
            ..Default::default()
        }],
        ..Default::default()
    };
    let err = client
        .download_artifact(&Context::background(), &artifact("https://downloads.example/artifact.zip", SHA))
        .await
        .expect_err("unauthorized");
    assert!(!err.to_string().contains("secret diagnostic body"), "{err}");
    assert_eq!(err.to_string(), "unexpected status 401");
}

#[test]
fn request_error_redacts_query_and_fragment() {
    let url = "https://user:password@downloads.example/plugin.zip?trace=private-value#section";
    let err = plugin_store_request_error(url, "context canceled").to_string();
    for leaked in ["private-value", "section", "trace=", "password", "user@"] {
        assert!(!err.contains(leaked), "leaked {leaked}: {err}");
    }
    assert_eq!(err, "request https://downloads.example/plugin.zip failed: context canceled");
}

#[tokio::test]
async fn resolved_auth_expiry_rejects_authenticated_request() {
    let client = Client {
        http_client: Some(failing_doer()),
        resolved_auth: vec![ResolvedAuthConfig {
            match_url: "https://downloads.example/private/".into(),
            apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
            kind: AUTH_TYPE_BEARER.into(),
            token: Secret::from("temporary-token"),
            ..Default::default()
        }],
        resolved_auth_expires_at: Some(Utc::now() - Duration::seconds(1)),
        ..Default::default()
    };
    let err = client
        .download_artifact(&Context::background(), &artifact("https://downloads.example/private/plugin.zip", SHA))
        .await
        .expect_err("expired");
    assert!(err.to_string().contains("resolved auth expired"), "{err}");
}

#[tokio::test]
async fn download_artifact_enforces_declared_size_during_read() {
    let data = b"0123456789".to_vec();
    let offset = Arc::new(Mutex::new(0usize));
    let (body_data, body_offset) = (data.clone(), offset.clone());
    let client = Client {
        http_client: Some(fn_doer(move |_| {
            Ok(HttpResponse {
                status: 200,
                headers: Headers::new(),
                body: Box::new(TrackingBody { data: body_data.clone(), offset: body_offset.clone(), chunk_size: 1 }),
            })
        })),
        ..Default::default()
    };
    let mut item = artifact("https://downloads.example/sample-provider.zip", &hex::encode(Sha256::digest(&data)));
    item.size = 4;
    let err = client.download_artifact(&Context::background(), &item).await.expect_err("size limit");
    assert!(err.to_string().contains("maximum allowed size"), "{err}");
    assert!(*offset.lock() <= 5, "download read {} bytes, want at most size+1", *offset.lock());
}

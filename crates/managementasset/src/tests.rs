use super::*;
use axum::Router;
use axum::http::HeaderMap;
use axum::routing::get;
use std::sync::Mutex as StdMutex;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn release_json(download_url: &str, digest: &str) -> String {
    format!(r#"{{"assets":[{{"name":"management.html","browser_download_url":"{download_url}","digest":"{digest}"}}]}}"#)
}

#[tokio::test]
async fn fetch_latest_asset_sets_github_authorization() {
    let seen: Arc<StdMutex<Option<String>>> = Arc::default();
    let seen_in = seen.clone();
    let app = Router::new().route(
        "/",
        get(move |headers: HeaderMap| {
            let seen = seen_in.clone();
            async move {
                *seen.lock().unwrap() = headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from);
                release_json("https://example.com/management.html", "sha256:abc123")
            }
        }),
    );
    let url = serve(app).await;
    let client = reqwest::Client::new();

    let (asset, hash) = fetch_latest_asset(&client, &url, "asset-token").await.unwrap();
    assert_eq!(seen.lock().unwrap().as_deref(), Some("Bearer asset-token"));
    assert_eq!(asset.name, MANAGEMENT_ASSET_NAME);
    assert_eq!(hash, "abc123");

    let (asset, hash) = fetch_latest_asset(&client, &url, "").await.unwrap();
    assert_eq!(*seen.lock().unwrap(), None);
    assert_eq!((asset.name.as_str(), hash.as_str()), (MANAGEMENT_ASSET_NAME, "abc123"));
}

#[test]
fn auto_update_skip_reason_cases() {
    let mut cluster = Config::default();
    cluster.home.enabled = true;
    let mut panel_disabled = Config::default();
    panel_disabled.remote_management.disable_control_panel = true;
    let mut update_disabled = Config::default();
    update_disabled.remote_management.disable_auto_update_panel = true;

    assert_eq!(auto_update_skip_reason(None), Some("config not yet available"));
    assert_eq!(auto_update_skip_reason(Some(&cluster)), Some("cluster mode enabled"));
    assert_eq!(auto_update_skip_reason(Some(&panel_disabled)), Some("control panel disabled"));
    assert_eq!(auto_update_skip_reason(Some(&update_disabled)), Some("disable-auto-update-panel is enabled"));
    assert_eq!(auto_update_skip_reason(Some(&Config::default())), None);
}

#[test]
fn release_url_resolution() {
    let default = DEFAULT_MANAGEMENT_RELEASE_URL;
    for (repo, want) in [
        ("", default),
        ("   ", default),
        ("not a url", default),
        ("example.com/owner/repo", default),
        ("https://example.com/owner/repo", default),
        ("https://github.com/owner", default),
        ("https://github.com/owner/repo", "https://api.github.com/repos/owner/repo/releases/latest"),
        ("https://github.com/owner/repo.git/", "https://api.github.com/repos/owner/repo/releases/latest"),
        ("https://api.github.com/repos/owner/repo", "https://api.github.com/repos/owner/repo/releases/latest"),
        ("https://api.github.com/repos/owner/repo/releases/latest/", "https://api.github.com/repos/owner/repo/releases/latest"),
    ] {
        assert_eq!(resolve_release_url(repo), want, "{repo}");
    }
}

#[test]
fn static_dir_resolution_order() {
    // MANAGEMENT_STATIC_PATH wins; a path naming management.html resolves to its directory.
    assert_eq!(static_dir_from("/srv/panel/", "/w", "/etc/c.yaml"), "/srv/panel");
    assert_eq!(static_dir_from("/srv/panel/Management.HTML", "/w", "/etc/c.yaml"), "/srv/panel");
    // Then WRITABLE_PATH/static, then static next to the config file.
    assert_eq!(static_dir_from("", "/w", "/etc/c.yaml"), "/w/static");
    assert_eq!(static_dir_from(" ", "", "/etc/c.yaml"), "/etc/static");
    assert_eq!(static_dir_from("", "", "config.yaml"), "static");
    assert_eq!(static_dir_from("", "", "  "), "");
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().to_str().unwrap();
    assert_eq!(static_dir_from("", "", config_dir), format!("{config_dir}/static"));
}

#[test]
fn digest_parsing() {
    assert_eq!(parse_digest(" SHA256:ABC123 "), "abc123");
    assert_eq!(parse_digest("abc123"), "abc123");
    assert_eq!(parse_digest(""), "");
}

fn sources(release_url: &str, fallback_url: &str) -> Sources {
    Sources { release_url: Some(release_url.to_string()), fallback_url: fallback_url.to_string() }
}

#[tokio::test]
async fn sync_downloads_verifies_and_replaces() {
    let body = "<html>panel</html>";
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(body)));
    let base: Arc<StdMutex<String>> = Arc::default();
    let base_in = base.clone();
    let app = Router::new()
        .route("/release", get(move || {
            let base = base_in.clone();
            let digest = digest.clone();
            async move { release_json(&format!("{}/asset", base.lock().unwrap()), &digest) }
        }))
        .route("/asset", get(move || async move { body }));
    let url = serve(app).await;
    *base.lock().unwrap() = url.clone();

    let dir = tempfile::tempdir().unwrap();
    let static_dir = dir.path().join("static");
    let static_dir = static_dir.to_str().unwrap();
    let ok = ensure_latest(static_dir, "direct", "", sources(&format!("{url}/release"), "http://127.0.0.1:1/"), false).await;
    assert!(ok);
    let path = format!("{static_dir}/management.html");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
    // No temp files left behind.
    assert_eq!(std::fs::read_dir(static_dir).unwrap().count(), 1);
}

#[tokio::test]
async fn sync_aborts_on_digest_mismatch_and_keeps_existing_file() {
    let base: Arc<StdMutex<String>> = Arc::default();
    let base_in = base.clone();
    let app = Router::new()
        .route("/release", get(move || {
            let base = base_in.clone();
            async move { release_json(&format!("{}/asset", base.lock().unwrap()), &format!("sha256:{}", "0".repeat(64))) }
        }))
        .route("/asset", get(|| async { "tampered" }));
    let url = serve(app).await;
    *base.lock().unwrap() = url.clone();

    let dir = tempfile::tempdir().unwrap();
    let static_dir = dir.path().to_str().unwrap();
    let path = format!("{static_dir}/management.html");

    // No local file: nothing is written, and the fallback is not consulted for a mismatch.
    let ok = ensure_latest(static_dir, "direct", "", sources(&format!("{url}/release"), &format!("{url}/asset")), false).await;
    assert!(!ok);
    assert!(!Path::new(&path).exists());

    std::fs::write(&path, "old").unwrap();
    let ok = ensure_latest(static_dir, "direct", "", sources(&format!("{url}/release"), &format!("{url}/asset")), false).await;
    assert!(ok);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
}

#[tokio::test]
async fn sync_skips_download_when_hash_matches_and_uses_fallback_when_missing() {
    let hits: Arc<StdMutex<Vec<&'static str>>> = Arc::default();
    let hits_release = hits.clone();
    let hits_asset = hits.clone();
    let hits_fallback = hits.clone();
    let digest = format!("sha256:{}", hex::encode(Sha256::digest("same")));
    let app = Router::new()
        .route("/release", get(move || {
            hits_release.lock().unwrap().push("release");
            let digest = digest.clone();
            async move { release_json("http://127.0.0.1:1/never", &digest) }
        }))
        .route("/asset", get(move || {
            hits_asset.lock().unwrap().push("asset");
            async { "x" }
        }))
        .route("/fallback", get(move || {
            hits_fallback.lock().unwrap().push("fallback");
            async { "fallback page" }
        }))
        .route("/broken", get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom") }));
    let url = serve(app).await;

    // Up to date: only the release metadata is fetched.
    let dir = tempfile::tempdir().unwrap();
    let static_dir = dir.path().to_str().unwrap();
    std::fs::write(format!("{static_dir}/management.html"), "same").unwrap();
    assert!(ensure_latest(static_dir, "direct", "", sources(&format!("{url}/release"), &format!("{url}/fallback")), false).await);
    assert_eq!(*hits.lock().unwrap(), vec!["release"]);

    // Release lookup fails and the file is missing: the fallback page is installed.
    let dir = tempfile::tempdir().unwrap();
    let static_dir = dir.path().to_str().unwrap();
    assert!(ensure_latest(static_dir, "direct", "", sources(&format!("{url}/broken"), &format!("{url}/fallback")), false).await);
    assert_eq!(std::fs::read_to_string(format!("{static_dir}/management.html")).unwrap(), "fallback page");
    assert_eq!(*hits.lock().unwrap(), vec!["release", "fallback"]);
}

//! Embedded management UI (`ui/dist`), served at `/`, `/management.html` and its assets.
//! The folder is optional at build time: when it is missing nothing is embedded and the routes
//! fall back to the Go behavior (JSON root, 404 panel).

use axum::http::{HeaderValue, header};
use rust_embed::RustEmbed;

use cpa_config::Config;

use crate::reply::Reply;

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct UiAssets;

fn serve(path: &str) -> Option<Reply> {
    let file = UiAssets::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut reply = Reply::new(200).with_body(file.data.into_owned());
    let content_type = if mime.type_() == mime_guess::mime::TEXT || mime.subtype() == "javascript" {
        format!("{}; charset=utf-8", mime.essence_str())
    } else {
        mime.essence_str().to_string()
    };
    if let Ok(v) = HeaderValue::from_str(&content_type) {
        reply.headers.insert(header::CONTENT_TYPE, v);
    }
    Some(reply)
}

/// `index.html` of the embedded UI.
pub fn index() -> Option<Reply> {
    serve("index.html")
}

/// An embedded static asset for a request path (`/assets/x.js` -> `assets/x.js`).
pub fn asset(request_path: &str) -> Option<Reply> {
    let path = request_path.trim_start_matches('/');
    if path.is_empty() || path.contains("..") {
        return None;
    }
    serve(path)
}

/// `serveManagementControlPanel` for the downloaded panel: serves `management.html` from the
/// static directory, fetching it synchronously on the first request when it is missing. 404 when
/// the panel is disabled (or Home mode is on), the path cannot be resolved or the download fails;
/// 500 when the file cannot be inspected. `if_modified_since` is the request header value.
pub async fn downloaded_panel(config_file_path: &str, cfg: &Config, if_modified_since: Option<&str>) -> Reply {
    if cfg.home.enabled || cfg.remote_management.disable_control_panel {
        return Reply::new(404);
    }
    let file_path = cpa_managementasset::file_path(config_file_path);
    if file_path.trim().is_empty() {
        return Reply::new(404);
    }

    if let Err(e) = tokio::fs::metadata(&file_path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::error!("failed to stat management control panel asset: {e}");
            return Reply::new(500);
        }
        // The sync runs detached from this request (the shared flight is its own task), so a
        // client disconnect does not cancel the bootstrap.
        let static_dir = cpa_managementasset::static_dir(config_file_path);
        let available = cpa_managementasset::ensure_latest_management_html(
            &static_dir,
            &cfg.proxy_url,
            &cfg.remote_management.panel_github_repository,
        )
        .await;
        if !available {
            return Reply::new(404);
        }
    }
    serve_file(&file_path, if_modified_since).await
}

/// `c.File` (`http.ServeFile`) for a regular file: content type from the extension, `Last-Modified`
/// and `If-Modified-Since` handling. Byte ranges are not supported.
async fn serve_file(path: &str, if_modified_since: Option<&str>) -> Reply {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Reply::new(404),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Reply::new(404),
        Err(_) => return Reply::new(500),
    };
    let modified = metadata.modified().ok().map(|t| {
        let secs = t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        (secs, chrono::DateTime::from_timestamp(secs as i64, 0).map(|d| d.format("%a, %d %b %Y %H:%M:%S GMT").to_string()))
    });
    if let (Some((secs, Some(_))), Some(since)) = (&modified, if_modified_since)
        && let Ok(since) = chrono::DateTime::parse_from_rfc2822(since)
        && *secs as i64 <= since.timestamp()
    {
        return Reply::new(304);
    }
    let data = match tokio::fs::read(path).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Reply::new(404),
        Err(_) => return Reply::new(500),
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let content_type = if mime.type_() == mime_guess::mime::TEXT {
        format!("{}; charset=utf-8", mime.essence_str())
    } else {
        mime.essence_str().to_string()
    };
    let mut reply = Reply::new(200).content_type(&content_type).with_body(data);
    if let Some((_, Some(last_modified))) = modified {
        reply = reply.with_header(header::LAST_MODIFIED, &last_modified);
    }
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serves_downloaded_panel_from_the_static_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("static")).unwrap();
        std::fs::write(dir.path().join("static/management.html"), "<html>panel</html>").unwrap();
        let config_path = dir.path().join("config.yaml");
        let config_path = config_path.to_str().unwrap();

        let cfg = Config::default();
        let reply = downloaded_panel(config_path, &cfg, None).await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body.as_ref(), b"<html>panel</html>");
        assert_eq!(reply.headers[header::CONTENT_TYPE], "text/html; charset=utf-8");

        let last_modified = reply.headers[header::LAST_MODIFIED].to_str().unwrap().to_string();
        assert_eq!(downloaded_panel(config_path, &cfg, Some(&last_modified)).await.status, 304);

        let mut disabled = Config::default();
        disabled.remote_management.disable_control_panel = true;
        assert_eq!(downloaded_panel(config_path, &disabled, None).await.status, 404);
        let mut home = Config::default();
        home.home.enabled = true;
        assert_eq!(downloaded_panel(config_path, &home, None).await.status, 404);
    }
}

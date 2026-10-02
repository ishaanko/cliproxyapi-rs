//! Embedded management UI (`ui/dist`), served at `/`, `/management.html` and its assets.
//! The folder is optional at build time: when it is missing nothing is embedded and the routes
//! fall back to the Go behavior (JSON root, 404 panel).

use axum::http::{HeaderValue, header};
use rust_embed::RustEmbed;

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

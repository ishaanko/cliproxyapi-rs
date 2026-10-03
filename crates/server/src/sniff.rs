//! `http.DetectContentType`: net/http sniffs the first write's bytes when a handler sets
//! no `Content-Type`, which is how raw (`alt=json`) Gemini streams end up as `text/plain`.

use axum::http::{HeaderMap, HeaderValue, header};

/// Content type of `data` (Go: `http.DetectContentType`).
pub fn detect_content_type(data: &[u8]) -> &'static str {
    cpa_executors::helps::content_type::detect_content_type(data)
}

/// Sets the sniffed `Content-Type` for a first write when the handler set none.
pub fn ensure_content_type(headers: &mut HeaderMap, first_write: &[u8]) {
    if first_write.is_empty() || headers.contains_key(header::CONTENT_TYPE) {
        return;
    }
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(detect_content_type(first_write)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_text_is_plain() {
        assert_eq!(detect_content_type(br#"{"a":1}"#), "text/plain; charset=utf-8");
        assert_eq!(detect_content_type(b"  <html>x"), "text/html; charset=utf-8");
        assert_eq!(detect_content_type(&[0, 1, 2]), "application/octet-stream");
    }
}

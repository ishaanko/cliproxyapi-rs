//! `http.DetectContentType` subset: net/http sniffs the first write when a handler sets no
//! `Content-Type`, which applies to plugin-served management and resource responses.

const HTML_SIGS: [&str; 17] = [
    "<!DOCTYPE HTML", "<HTML", "<HEAD", "<SCRIPT", "<IFRAME", "<H1", "<DIV", "<FONT", "<TABLE", "<A", "<STYLE", "<TITLE", "<B", "<BODY", "<BR", "<P",
    "<!--",
];

/// Content type of `data` (first 512 bytes considered), following Go's signature order for the
/// text, markup, document, common image and archive types; anything else is text or binary.
pub fn detect_content_type(data: &[u8]) -> &'static str {
    let data = &data[..data.len().min(512)];
    let first = data.iter().position(|b| !matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' ')).unwrap_or(data.len());
    let trimmed = &data[first..];
    for sig in HTML_SIGS {
        let n = sig.len();
        if trimmed.len() > n && trimmed[..n].eq_ignore_ascii_case(sig.as_bytes()) && matches!(trimmed[n], b' ' | b'>') {
            return "text/html; charset=utf-8";
        }
    }
    if trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case(b"<?xml") {
        return "text/xml; charset=utf-8";
    }
    if data.starts_with(b"%PDF-") {
        return "application/pdf";
    }
    if data.starts_with(b"%!PS-Adobe-") {
        return "application/postscript";
    }
    if data.starts_with(&[0xFE, 0xFF]) {
        return "text/plain; charset=utf-16be";
    }
    if data.starts_with(&[0xFF, 0xFE]) {
        return "text/plain; charset=utf-16le";
    }
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return "text/plain; charset=utf-8";
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png";
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg";
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return "image/gif";
    }
    if data.starts_with(&[0x1F, 0x8B, 0x08]) {
        return "application/x-gzip";
    }
    if data.starts_with(b"PK\x03\x04") {
        return "application/zip";
    }
    let binary = data.iter().any(|&b| matches!(b, 0x00..=0x08 | 0x0B | 0x0E..=0x1A | 0x1C..=0x1F));
    if binary { "application/octet-stream" } else { "text/plain; charset=utf-8" }
}

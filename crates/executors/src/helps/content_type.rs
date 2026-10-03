//! `http.DetectContentType`: the WHATWG MIME sniffing algorithm over the first 512 bytes
//! (Go: net/http/internal/sniff.go). Used where Go sniffs uploaded files and unset response types.

const SNIFF_LEN: usize = 512;

const HTML_SIGS: [&str; 17] = [
    "<!DOCTYPE HTML", "<HTML", "<HEAD", "<SCRIPT", "<IFRAME", "<H1", "<DIV", "<FONT", "<TABLE", "<A", "<STYLE", "<TITLE", "<B", "<BODY", "<BR", "<P",
    "<!--",
];

/// Prefix signatures: (bytes, content type).
const EXACT: &[(&[u8], &str)] = &[
    (b"%PDF-", "application/pdf"),
    (b"%!PS-Adobe-", "application/postscript"),
];

/// Masked signatures: (mask, pattern, content type); a `0x00` mask byte ignores that byte.
type Masked = (&'static [u8], &'static [u8], &'static str);

const BOMS: &[Masked] = &[
    (b"\xFF\xFF\x00\x00", b"\xFE\xFF\x00\x00", "text/plain; charset=utf-16be"),
    (b"\xFF\xFF\x00\x00", b"\xFF\xFE\x00\x00", "text/plain; charset=utf-16le"),
    (b"\xFF\xFF\xFF\x00", b"\xEF\xBB\xBF\x00", "text/plain; charset=utf-8"),
];

const IMAGES: &[(&[u8], &str)] = &[
    (b"\x00\x00\x01\x00", "image/x-icon"),
    (b"\x00\x00\x02\x00", "image/x-icon"),
    (b"BM", "image/bmp"),
    (b"GIF87a", "image/gif"),
    (b"GIF89a", "image/gif"),
];

const WEBP: Masked = (b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF\xFF\xFF", b"RIFF\x00\x00\x00\x00WEBPVP", "image/webp");

const IMAGES_TAIL: &[(&[u8], &str)] = &[(b"\x89PNG\x0D\x0A\x1A\x0A", "image/png"), (b"\xFF\xD8\xFF", "image/jpeg")];

const AV_BEFORE_MP4: &[Masked] = &[
    (b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF", b"FORM\x00\x00\x00\x00AIFF", "audio/aiff"),
    (b"\xFF\xFF\xFF", b"ID3", "audio/mpeg"),
    (b"\xFF\xFF\xFF\xFF\xFF", b"OggS\x00", "application/ogg"),
    (b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF", b"MThd\x00\x00\x00\x06", "audio/midi"),
    (b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF", b"RIFF\x00\x00\x00\x00AVI ", "video/avi"),
    (b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF", b"RIFF\x00\x00\x00\x00WAVE", "audio/wave"),
];

const FONTS_AND_ARCHIVES: &[(&[u8], &str)] = &[
    (b"\x00\x01\x00\x00", "font/ttf"),
    (b"OTTO", "font/otf"),
    (b"ttcf", "font/collection"),
    (b"wOFF", "font/woff"),
    (b"wOF2", "font/woff2"),
    (b"\x1F\x8B\x08", "application/x-gzip"),
    (b"PK\x03\x04", "application/zip"),
    (b"Rar!\x1A\x07\x00", "application/x-rar-compressed"),
    (b"Rar!\x1A\x07\x01\x00", "application/x-rar-compressed"),
    (b"\x00\x61\x73\x6D", "application/wasm"),
];

fn is_ws(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn masked(sig: &Masked, data: &[u8]) -> Option<&'static str> {
    let (mask, pat, ct) = *sig;
    if data.len() < pat.len() {
        return None;
    }
    pat.iter().enumerate().all(|(i, p)| data[i] & mask[i] == *p).then_some(ct)
}

fn exact(table: &[(&[u8], &'static str)], data: &[u8]) -> Option<&'static str> {
    table.iter().find(|(sig, _)| data.starts_with(sig)).map(|(_, ct)| *ct)
}

fn html(data: &[u8]) -> Option<&'static str> {
    HTML_SIGS.iter().find_map(|sig| {
        let sig = sig.as_bytes();
        if data.len() < sig.len() + 1 {
            return None;
        }
        let matches = sig.iter().zip(data).all(|(b, d)| if b.is_ascii_uppercase() { *b == d & 0xDF } else { b == d });
        (matches && matches!(data[sig.len()], b' ' | b'>')).then_some("text/html; charset=utf-8")
    })
}

/// `mp4Sig`: an `ftyp` box naming an `mp4` brand.
fn mp4(data: &[u8]) -> Option<&'static str> {
    if data.len() < 12 {
        return None;
    }
    let box_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < box_size || !box_size.is_multiple_of(4) || &data[4..8] != b"ftyp" {
        return None;
    }
    let mut st = 8;
    while st < box_size {
        if st != 12 && &data[st..st + 3] == b"mp4" {
            return Some("video/mp4");
        }
        st += 4;
    }
    None
}

/// Content type of `data`, `application/octet-stream` when nothing matches.
pub fn detect_content_type(data: &[u8]) -> &'static str {
    let data = &data[..data.len().min(SNIFF_LEN)];
    let first = data.iter().position(|b| !is_ws(*b)).unwrap_or(data.len());
    let trimmed = &data[first..];
    if let Some(ct) = html(trimmed) {
        return ct;
    }
    if let Some(ct) = masked(&(b"\xFF\xFF\xFF\xFF\xFF", b"<?xml", "text/xml; charset=utf-8"), trimmed) {
        return ct;
    }
    if let Some(ct) = exact(EXACT, data) {
        return ct;
    }
    if let Some(ct) = BOMS.iter().find_map(|s| masked(s, data)) {
        return ct;
    }
    if let Some(ct) = exact(IMAGES, data) {
        return ct;
    }
    if let Some(ct) = masked(&WEBP, data) {
        return ct;
    }
    if let Some(ct) = exact(IMAGES_TAIL, data) {
        return ct;
    }
    if let Some(ct) = AV_BEFORE_MP4.iter().find_map(|s| masked(s, data)) {
        return ct;
    }
    if let Some(ct) = mp4(data) {
        return ct;
    }
    if data.starts_with(b"\x1A\x45\xDF\xA3") {
        return "video/webm";
    }
    // 34 NULL bytes followed by "LP".
    if data.len() >= 36 && data[..34].iter().all(|b| *b == 0) && &data[34..36] == b"LP" {
        return "application/vnd.ms-fontobject";
    }
    if let Some(ct) = exact(FONTS_AND_ARCHIVES, data) {
        return ct;
    }
    let binary = trimmed.iter().any(|&b| matches!(b, 0x00..=0x08 | 0x0B | 0x0E..=0x1A | 0x1C..=0x1F));
    if binary { "application/octet-stream" } else { "text/plain; charset=utf-8" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_types() {
        assert_eq!(detect_content_type(b"  <html>x"), "text/html; charset=utf-8");
        assert_eq!(detect_content_type(b"\x89PNG\r\n\x1a\nxx"), "image/png");
        assert_eq!(detect_content_type(b"RIFF\x01\x02\x03\x04WEBPVP8 "), "image/webp");
        assert_eq!(detect_content_type(b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom"), "video/mp4");
        assert_eq!(detect_content_type(br#"{"a":1}"#), "text/plain; charset=utf-8");
        assert_eq!(detect_content_type(&[0, 1, 2]), "application/octet-stream");
        assert_eq!(detect_content_type(b""), "text/plain; charset=utf-8");
    }
}

//! Byte/string helpers shared by the helps modules.

/// `bytes.TrimSpace`: trims Unicode white space (UTF-8 aware), ASCII-only for invalid UTF-8.
pub fn trim_space(b: &[u8]) -> &[u8] {
    // Hot on every stream line: an ASCII non-space first and last byte means nothing to trim
    // (Unicode spaces start with a non-ASCII byte), so skip the UTF-8 validation.
    if let (Some(&first), Some(&last)) = (b.first(), b.last())
        && first > b' '
        && first < 0x80
        && last > b' '
        && last < 0x80
    {
        return b;
    }
    match std::str::from_utf8(b) {
        Ok(s) => s.trim().as_bytes(),
        Err(_) => b.trim_ascii(),
    }
}

/// `strings.TrimSpace`.
pub fn trim_space_str(s: &str) -> &str {
    s.trim()
}

/// Value of an SSE `data:` line: the trimmed JSON object payload of a line, `None` for blank
/// lines, `[DONE]`, `event:` lines and anything that is not a JSON object (Go: jsonPayload).
pub fn json_payload(line: &[u8]) -> Option<&[u8]> {
    let mut trimmed = trim_space(line);
    if trimmed.is_empty() || trimmed == b"[DONE]" || trimmed.starts_with(b"event:") {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = trim_space(rest);
    }
    (trimmed.first() == Some(&b'{')).then_some(trimmed)
}

/// Like [`json_payload`] but any non-empty, non-`[DONE]` payload is returned, not only objects
/// (Go: ExtractStreamJSONPayload).
pub fn extract_stream_json_payload(line: &[u8]) -> Option<&[u8]> {
    let mut trimmed = trim_space(line);
    if trimmed.is_empty() || trimmed == b"[DONE]" || trimmed.starts_with(b"event:") {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = trim_space(rest);
    }
    (!trimmed.is_empty() && trimmed != b"[DONE]").then_some(trimmed)
}

/// Splits `payload` on `\n` and calls `f` with each non-empty trimmed line (Go: IterateStreamLines).
pub fn iterate_stream_lines(payload: &[u8], mut f: impl FnMut(&[u8])) {
    for line in payload.split(|b| *b == b'\n') {
        let trimmed = trim_space(line);
        if !trimmed.is_empty() {
            f(trimmed);
        }
    }
}

/// `bytes.Contains`.
pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || memchr::memmem::find(hay, needle).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_extraction() {
        assert_eq!(json_payload(b"  data: {\"a\":1}  "), Some(&b"{\"a\":1}"[..]));
        assert_eq!(json_payload(b"data: [DONE]"), None);
        assert_eq!(json_payload(b"event: message_start"), None);
        assert_eq!(json_payload(b"data: 12"), None);
        assert_eq!(extract_stream_json_payload(b"data: 12"), Some(&b"12"[..]));
        assert_eq!(extract_stream_json_payload(b"[DONE]"), None);
        assert_eq!(trim_space("\u{a0} x \u{b}".as_bytes()), b"x");
    }
}

//! SSE JSON validator for Responses streams (Go: `sseJSONValidationState` in handlers_stream.go).
//! Reassembles frames split across chunks and rejects `data:` payloads that are not valid JSON
//! with a 502 `invalid SSE data JSON (len=N): "<first 512 bytes>"`.

/// `strconv.Quote` for a byte string (the Go `%q` verb on `[]byte`).
pub fn go_quote(bytes: &[u8]) -> String {
    let mut out = String::from("\"");
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                quote_str(text, &mut out);
                break;
            }
            Err(e) => {
                let (valid, bad) = rest.split_at(e.valid_up_to());
                quote_str(std::str::from_utf8(valid).unwrap_or(""), &mut out);
                let bad_len = e.error_len().unwrap_or(bad.len()).max(1);
                for b in &bad[..bad_len.min(bad.len())] {
                    out.push_str(&format!("\\x{b:02x}"));
                }
                rest = &bad[bad_len.min(bad.len())..];
            }
        }
    }
    out.push('"');
    out
}

fn quote_str(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_control() => {
                let n = c as u32;
                if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
}

fn trim_ascii_space(b: &[u8]) -> &[u8] {
    // bytes.TrimSpace also trims Unicode spaces; SSE payloads only use ASCII whitespace.
    b.trim_ascii()
}

fn replace_all(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Joined `data:` payload of a frame (`sseJSONValidationDataPayload`).
fn data_payload(frame: &[u8]) -> (Vec<u8>, bool) {
    let mut payload = Vec::new();
    let mut found = false;
    for line in frame.split(|b| *b == b'\n') {
        let line = trim_ascii_space(line);
        let Some(rest) = line.strip_prefix(b"data:") else {
            continue;
        };
        if found {
            payload.push(b'\n');
        }
        payload.extend_from_slice(trim_ascii_space(rest));
        found = true;
    }
    (payload, found)
}

fn data_ok(payload: &[u8], found: bool) -> bool {
    let payload = trim_ascii_space(payload);
    !found || payload.is_empty() || payload == b"[DONE]" || cpa_json::valid(payload)
}

fn validate_frame(frame: &[u8]) -> Result<(), String> {
    let (payload, found) = data_payload(frame);
    if data_ok(&payload, found) {
        return Ok(());
    }
    let payload = trim_ascii_space(&payload);
    const MAX: usize = 512;
    let preview = &payload[..payload.len().min(MAX)];
    Err(format!("invalid SSE data JSON (len={}): {}", payload.len(), go_quote(preview)))
}

#[derive(Default)]
pub struct SseJsonValidator {
    pending: Vec<u8>,
    pending_err: Option<String>,
    prev_ends_with_cr: bool,
}

impl SseJsonValidator {
    /// Adds a chunk and returns the complete, validated frames ready to forward.
    pub fn add_chunk(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        if let Some(err) = self.pending_err.take() {
            return Err(err);
        }
        if chunk.is_empty() {
            return Ok(Vec::new());
        }
        let mut chunk = chunk;
        if self.prev_ends_with_cr {
            if chunk[0] == b'\n' {
                chunk = &chunk[1..];
            }
            self.prev_ends_with_cr = false;
        }
        if chunk.is_empty() {
            return Ok(Vec::new());
        }
        let ends_with_cr = chunk[chunk.len() - 1] == b'\r';
        let chunk = replace_all(&replace_all(chunk, b"\r\n", b"\n"), b"\r", b"\n");
        self.prev_ends_with_cr = ends_with_cr;

        if !self.pending.is_empty() && !self.pending.ends_with(b"\n") && !chunk.starts_with(b"\n") {
            let first_line = chunk.split(|b| *b == b'\n').next().unwrap_or(&[]);
            let first = trim_ascii_space(first_line);
            if first.starts_with(b"data:") || first.starts_with(b"event:") {
                self.pending.push(b'\n');
            }
        }
        self.pending.extend_from_slice(&chunk);

        let mut output = Vec::new();
        while let Some(idx) = find(&self.pending, b"\n\n") {
            let frame_end = idx + 2;
            if let Err(err) = validate_frame(&self.pending[..frame_end]) {
                if !output.is_empty() {
                    self.pending.clear();
                    self.pending_err = Some(err);
                    return Ok(output);
                }
                return Err(err);
            }
            output.extend_from_slice(&self.pending[..frame_end]);
            self.pending.drain(..frame_end);
        }

        if trim_ascii_space(&self.pending).is_empty() {
            self.pending.clear();
            return Ok(output);
        }
        let (payload, found) = data_payload(&self.pending);
        if data_ok(&payload, found) {
            output.append(&mut self.pending);
        }
        Ok(output)
    }

    /// Called when the upstream closes: a dangling partial frame must still be valid.
    pub fn finish(&mut self) -> Result<(), String> {
        self.prev_ends_with_cr = false;
        if let Some(err) = self.pending_err.take() {
            self.pending.clear();
            return Err(err);
        }
        if trim_ascii_space(&self.pending).is_empty() {
            self.pending.clear();
            return Ok(());
        }
        let result = validate_frame(&self.pending);
        self.pending.clear();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_frames_pass_through_and_partial_frames_wait() {
        let mut v = SseJsonValidator::default();
        let out = v.add_chunk(b"event: a\ndata: {\"x\":").unwrap();
        assert!(out.is_empty());
        let out = v.add_chunk(b"1}\n\n").unwrap();
        assert_eq!(out, b"event: a\ndata: {\"x\":1}\n\n");
        assert!(v.finish().is_ok());
    }

    #[test]
    fn crlf_is_normalized_even_when_split() {
        let mut v = SseJsonValidator::default();
        // A complete-looking JSON line is released immediately (as in Go); the blank remainder of
        // the split CRLF is dropped.
        let a = v.add_chunk(b"data: {\"a\":1}\r").unwrap();
        assert_eq!(a, b"data: {\"a\":1}\n");
        let b = v.add_chunk(b"\n\r\n").unwrap();
        assert!(b.is_empty());
    }

    #[test]
    fn invalid_data_json_is_rejected_with_preview() {
        let mut v = SseJsonValidator::default();
        let err = v.add_chunk(b"data: {oops\n\n").unwrap_err();
        assert_eq!(err, "invalid SSE data JSON (len=5): \"{oops\"");
    }

    #[test]
    fn done_marker_and_empty_data_are_allowed() {
        let mut v = SseJsonValidator::default();
        assert_eq!(v.add_chunk(b"data: [DONE]\n\n").unwrap(), b"data: [DONE]\n\n");
        assert_eq!(v.add_chunk(b"data:\n\n").unwrap(), b"data:\n\n");
    }

    #[test]
    fn valid_frame_before_invalid_is_flushed_then_error_surfaces() {
        let mut v = SseJsonValidator::default();
        let out = v.add_chunk(b"data: {\"a\":1}\n\ndata: nope\n\n").unwrap();
        assert_eq!(out, b"data: {\"a\":1}\n\n");
        assert!(v.add_chunk(b"data: {\"b\":2}\n\n").is_err());
    }

    #[test]
    fn dangling_invalid_tail_fails_on_finish() {
        let mut v = SseJsonValidator::default();
        assert!(v.add_chunk(b"data: {\"a\"").unwrap().is_empty());
        assert!(v.finish().is_err());
    }

    #[test]
    fn go_quote_matches_strconv() {
        assert_eq!(go_quote(b"a\"b\n"), "\"a\\\"b\\n\"");
        assert_eq!(go_quote(&[0x01, 0xff]), "\"\\x01\\xff\"");
        assert_eq!(go_quote("é".as_bytes()), "\"é\"");
    }
}

//! In-memory `multipart/form-data` reader with the line semantics of Go's `mime/multipart`
//! (preamble skipping, LF-only bodies, final boundary without newline, error texts).

use crate::util::parse_media_type;

/// One form part.
pub struct Part {
    /// `Part.FormName()`: the `name` of a `form-data` disposition, else empty.
    pub form_name: String,
    pub body: Vec<u8>,
}

/// Where a reader failure happened (Go wraps them differently).
pub enum PartError {
    /// `NextPart` failed.
    Next(String),
    /// Reading the part body failed.
    Read(String),
}

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    dash_boundary: Vec<u8>,
    nl: &'static [u8],
    parts_read: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8], boundary: &str) -> Self {
        Reader { data, pos: 0, dash_boundary: format!("--{boundary}").into_bytes(), nl: b"\r\n", parts_read: 0 }
    }

    /// The next line including its `\n`; the flag is true when the data ended without one.
    fn read_line(&mut self) -> Option<(&'a [u8], bool)> {
        if self.pos >= self.data.len() {
            return None;
        }
        let rest = &self.data[self.pos..];
        match rest.iter().position(|b| *b == b'\n') {
            Some(i) => {
                self.pos += i + 1;
                Some((&rest[..=i], false))
            }
            None => {
                self.pos = self.data.len();
                Some((rest, true))
            }
        }
    }

    fn skip_lwsp(rest: &[u8]) -> &[u8] {
        let n = rest.iter().take_while(|b| **b == b' ' || **b == b'\t').count();
        &rest[n..]
    }

    fn is_final_boundary(&self, line: &[u8]) -> bool {
        let mut dash_dash = self.dash_boundary.clone();
        dash_dash.extend_from_slice(b"--");
        if !line.starts_with(&dash_dash) {
            return false;
        }
        let rest = Self::skip_lwsp(&line[dash_dash.len()..]);
        rest.is_empty() || rest == self.nl
    }

    fn is_boundary_delimiter_line(&mut self, line: &[u8]) -> bool {
        if !line.starts_with(&self.dash_boundary) {
            return false;
        }
        let rest = Self::skip_lwsp(&line[self.dash_boundary.len()..]);
        if self.parts_read == 0 && rest == b"\n" {
            self.nl = b"\n";
        }
        rest == self.nl
    }

    /// `NextPart`: `Ok(None)` is the end of the form.
    pub fn next_part(&mut self) -> Result<Option<Part>, PartError> {
        if self.dash_boundary == b"--" {
            return Err(PartError::Next("multipart: boundary is empty".into()));
        }
        let mut expect_new_part = false;
        loop {
            let Some((line, no_newline)) = self.read_line() else {
                // End of data before any boundary: wrapped io.EOF, which callers treat as the end.
                return Ok(None);
            };
            // A last line without newline is the end: a final boundary is the clean EOF, anything
            // else is Go's wrapped `NextPart: EOF`, which the caller also treats as the end.
            if no_newline {
                return Ok(None);
            }
            if self.is_boundary_delimiter_line(line) {
                self.parts_read += 1;
                return self.read_part().map(Some);
            }
            if self.is_final_boundary(line) {
                return Ok(None);
            }
            if expect_new_part {
                return Err(PartError::Next(format!("multipart: expecting a new Part; got line {:?}", String::from_utf8_lossy(line))));
            }
            if self.parts_read == 0 {
                continue;
            }
            if line == self.nl {
                expect_new_part = true;
                continue;
            }
            return Err(PartError::Next(format!("multipart: unexpected line in Next(): {:?}", String::from_utf8_lossy(line))));
        }
    }

    fn read_part(&mut self) -> Result<Part, PartError> {
        // MIME headers up to the blank line.
        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let Some((line, no_newline)) = self.read_line() else {
                return Err(PartError::Next("unexpected EOF".into()));
            };
            let text = String::from_utf8_lossy(line);
            let trimmed = text.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if no_newline {
                return Err(PartError::Next("unexpected EOF".into()));
            }
            if trimmed.starts_with([' ', '\t']) {
                match headers.last_mut() {
                    Some((_, v)) => {
                        v.push(' ');
                        v.push_str(trimmed.trim());
                    }
                    None => return Err(PartError::Next(format!("malformed MIME header initial line: {trimmed}"))),
                }
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                return Err(PartError::Next(format!("malformed MIME header line: {trimmed}")));
            };
            if key.is_empty() || key.contains([' ', '\t']) {
                return Err(PartError::Next(format!("malformed MIME header line: {trimmed}")));
            }
            headers.push((key.to_ascii_lowercase(), value.trim().to_string()));
        }
        let form_name = headers
            .iter()
            .find(|(k, _)| k == "content-disposition")
            .and_then(|(_, v)| parse_media_type(v))
            .filter(|(disposition, _)| disposition == "form-data")
            .and_then(|(_, params)| params.into_iter().find(|(k, _)| k == "name").map(|(_, v)| v))
            .unwrap_or_default();
        let body = self.read_body()?;
        Ok(Part { form_name, body })
    }

    /// Body bytes up to the next boundary line (which is left for `next_part`).
    fn read_body(&mut self) -> Result<Vec<u8>, PartError> {
        let rest = &self.data[self.pos..];
        let boundary_after = |idx: usize, prefix_len: usize| -> bool {
            match rest.get(idx + prefix_len) {
                Some(b) => matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'-'),
                None => false,
            }
        };
        // An empty body: the boundary follows the header terminator directly.
        if rest.starts_with(&self.dash_boundary) && boundary_after(0, self.dash_boundary.len()) {
            return Ok(Vec::new());
        }
        let mut needle = self.nl.to_vec();
        needle.extend_from_slice(&self.dash_boundary);
        let mut from = 0;
        let finder = memchr::memmem::Finder::new(&needle);
        while let Some(i) = finder.find(&rest[from..]) {
            let idx = from + i;
            if boundary_after(idx, needle.len()) {
                let body = rest[..idx].to_vec();
                // Leave the position at the separator so `next_part` consumes it as Go does.
                self.pos += idx;
                return Ok(body);
            }
            from = idx + 1;
        }
        Err(PartError::Read("unexpected EOF".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(boundary: &str, parts: &[(&str, &str)]) -> Vec<u8> {
        let mut out = String::new();
        for (name, value) in parts {
            out.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"));
        }
        out.push_str(&format!("--{boundary}--\r\n"));
        out.into_bytes()
    }

    fn all(data: &[u8], boundary: &str) -> Vec<(String, String)> {
        let mut r = Reader::new(data, boundary);
        let mut out = vec![];
        while let Ok(Some(p)) = r.next_part() {
            out.push((p.form_name, String::from_utf8_lossy(&p.body).into_owned()));
        }
        out
    }

    #[test]
    fn parses_parts() {
        let data = body("b", &[("sdp", "v=0\r\n"), ("session", "{\"a\":1}")]);
        assert_eq!(all(&data, "b"), vec![("sdp".into(), "v=0\r\n".into()), ("session".into(), "{\"a\":1}".into())]);
    }

    #[test]
    fn empty_and_preamble() {
        assert!(all(b"", "b").is_empty());
        let mut data = b"preamble\r\n".to_vec();
        data.extend(body("b", &[("x", "")]));
        assert_eq!(all(&data, "b"), vec![("x".into(), String::new())]);
    }

    #[test]
    fn truncated_body_errors() {
        let mut r = Reader::new(b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nxx", "b");
        assert!(matches!(r.next_part(), Err(PartError::Read(_))));
    }
}

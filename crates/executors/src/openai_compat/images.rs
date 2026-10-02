//! Image endpoint payload preparation (Go: openai_compat_executor.go `prepareOpenAICompatImagesPayload`
//! and `rewriteOpenAICompatImagesMultipartPayload`).
//!
//! JSON bodies get the upstream `model` (and `stream`) set; multipart edit bodies are re-encoded
//! with `model`/`stream` written first, then the remaining fields and files, keeping each file
//! part's own headers.

use std::collections::BTreeMap;

use cpa_runtime::executor::ExecError;

use crate::helps::payload::{set_bool_if_different, set_string_if_different};

/// Returns the body and `Content-Type` for the upstream image request.
pub fn prepare_images_payload(
    payload: &[u8],
    model: &str,
    content_type: &str,
    stream: bool,
) -> Result<(Vec<u8>, String), ExecError> {
    let model = model.trim();
    let content_type = content_type.trim();
    if cpa_json::valid(payload) {
        let mut v = cpa_json::parse(payload);
        if v.is_object() {
            if !model.is_empty() {
                set_string_if_different(&mut v, "model", model);
            }
            if stream {
                set_bool_if_different(&mut v, "stream", true);
            } else {
                cpa_json::delete(&mut v, "stream");
            }
            return Ok((cpa_json::to_vec(&v), "application/json".into()));
        }
        return Ok((payload.to_vec(), "application/json".into()));
    }
    let Some((media_type, params)) = parse_media_type(content_type) else {
        return Ok((payload.to_vec(), content_type.to_string()));
    };
    if !media_type.trim().to_lowercase().starts_with("multipart/") {
        return Ok((payload.to_vec(), content_type.to_string()));
    }
    let boundary = params.get("boundary").map(|b| b.trim()).unwrap_or_default();
    if boundary.is_empty() {
        return Err(ExecError::new(0, "multipart boundary is missing"));
    }
    rewrite_multipart(payload, model, boundary, stream)
}

/// Go: `mime.ParseMediaType` (lower-cased type and parameter names, quoted values unescaped).
pub fn parse_media_type(value: &str) -> Option<(String, BTreeMap<String, String>)> {
    let mut parts = split_params(value);
    let media = parts.remove(0).trim().to_lowercase();
    if media.is_empty() || !media.chars().all(|c| c.is_ascii_graphic() && !"()<>@,;:\\\"[]?=".contains(c)) {
        return None;
    }
    let mut params = BTreeMap::new();
    for part in parts {
        let (k, v) = part.split_once('=')?;
        let key = k.trim().to_lowercase();
        if key.is_empty() {
            return None;
        }
        let v = v.trim();
        let val = if let Some(inner) = v.strip_prefix('"') {
            let inner = inner.strip_suffix('"')?;
            unescape_quoted(inner)
        } else {
            v.to_string()
        };
        params.entry(key).or_insert(val);
    }
    Some((media, params))
}

/// Splits on `;` outside quoted strings.
fn split_params(value: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut escaped) = (Vec::new(), String::new(), false, false);
    for c in value.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
        } else if quoted && c == '\\' {
            cur.push(c);
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
            cur.push(c);
        } else if c == ';' && !quoted {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

fn unescape_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    out
}

struct FilePart {
    key: String,
    filename: String,
    headers: BTreeMap<String, Vec<String>>,
    data: Vec<u8>,
}

#[derive(Default)]
struct Form {
    values: Vec<(String, Vec<String>)>,
    files: Vec<FilePart>,
}

fn canonical_header_key(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = true;
    for c in name.trim().chars() {
        out.push(if upper { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() });
        upper = c == '-';
    }
    out
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// Splits a multipart body into raw parts (headers block + content), accepting CRLF or LF line
/// ends like Go's reader. `None` when the body has no closing boundary or no parts.
fn split_parts<'a>(body: &'a [u8], boundary: &str) -> Option<Vec<&'a [u8]>> {
    let dash = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    // Position just after the opening delimiter line.
    let mut pos = {
        let mut p = find(body, &dash, 0)?;
        while p > 0 && body[p - 1] != b'\n' {
            p = find(body, &dash, p + 1)?;
        }
        p + dash.len()
    };
    loop {
        if body[pos..].starts_with(b"--") {
            return Some(parts);
        }
        // Skip transport padding and the line terminator.
        let nl = find(body, b"\n", pos)?;
        let start = nl + 1;
        let mut search = start;
        let end = loop {
            let at = find(body, &dash, search)?;
            if at == 0 || body[at - 1] == b'\n' {
                break at;
            }
            search = at + 1;
        };
        let mut content_end = end - 1;
        if content_end > start && body[content_end - 1] == b'\r' {
            content_end -= 1;
        }
        parts.push(&body[start..content_end.max(start)]);
        pos = end + dash.len();
    }
}

fn parse_form(body: &[u8], boundary: &str) -> Result<Form, String> {
    let parts = split_parts(body, boundary).ok_or("multipart: NextPart: EOF")?;
    let mut form = Form::default();
    for raw in parts {
        let (head, data) = match find(raw, b"\r\n\r\n", 0) {
            Some(i) => (&raw[..i], &raw[i + 4..]),
            None => match find(raw, b"\n\n", 0) {
                Some(i) => (&raw[..i], &raw[i + 2..]),
                None if raw.is_empty() => (raw, raw),
                None => (raw, &raw[raw.len()..]),
            },
        };
        let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for line in String::from_utf8_lossy(head).lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            headers.entry(canonical_header_key(k)).or_default().push(v.trim().to_string());
        }
        let disposition = headers.get("Content-Disposition").and_then(|v| v.first()).cloned().unwrap_or_default();
        let Some((kind, params)) = parse_media_type(&disposition) else { continue };
        if kind != "form-data" {
            continue;
        }
        let Some(name) = params.get("name").filter(|n| !n.is_empty()) else { continue };
        match params.get("filename").filter(|f| !f.is_empty()) {
            Some(filename) => {
                let base = filename.rsplit('/').next().unwrap_or(filename).to_string();
                form.files.push(FilePart { key: name.clone(), filename: base, headers, data: data.to_vec() });
            }
            None => {
                let value = String::from_utf8_lossy(data).into_owned();
                match form.values.iter_mut().find(|(k, _)| k == name) {
                    Some((_, vs)) => vs.push(value),
                    None => form.values.push((name.clone(), vec![value])),
                }
            }
        }
    }
    Ok(form)
}

fn escape_quotes(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

struct Writer {
    boundary: String,
    buf: Vec<u8>,
    first: bool,
}

impl Writer {
    fn new() -> Self {
        let raw: [u8; 30] = rand::random();
        Self { boundary: hex::encode(raw), buf: Vec::new(), first: true }
    }

    /// Go: `multipart.Writer.CreatePart` (headers sorted by key) plus the part body.
    fn part(&mut self, headers: &BTreeMap<String, Vec<String>>, data: &[u8]) {
        if self.first {
            self.first = false;
        } else {
            self.buf.extend_from_slice(b"\r\n");
        }
        self.buf.extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        for (k, vs) in headers {
            for v in vs {
                self.buf.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
            }
        }
        self.buf.extend_from_slice(b"\r\n");
        self.buf.extend_from_slice(data);
    }

    fn field(&mut self, name: &str, value: &str) {
        let headers = BTreeMap::from([(
            "Content-Disposition".to_string(),
            vec![format!("form-data; name=\"{}\"", escape_quotes(name))],
        )]);
        self.part(&headers, value.as_bytes());
    }

    fn finish(mut self) -> (Vec<u8>, String) {
        if !self.first {
            self.buf.extend_from_slice(b"\r\n");
        }
        self.buf.extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        let content_type = format!("multipart/form-data; boundary={}", self.boundary);
        (self.buf, content_type)
    }
}

fn rewrite_multipart(
    payload: &[u8],
    model: &str,
    boundary: &str,
    stream: bool,
) -> Result<(Vec<u8>, String), ExecError> {
    let form = parse_form(payload, boundary)
        .map_err(|e| ExecError::new(0, format!("read multipart form failed: {e}")))?;
    let mut writer = Writer::new();
    if !model.is_empty() {
        writer.field("model", model);
    }
    if stream {
        writer.field("stream", "true");
    }
    for (key, values) in &form.values {
        if key == "model" || key == "stream" {
            continue;
        }
        for value in values {
            writer.field(key, value);
        }
    }
    for file in &form.files {
        let mut headers = file.headers.clone();
        headers.insert(
            "Content-Disposition".into(),
            vec![format!(
                "form-data; name=\"{}\"; filename=\"{}\"",
                escape_quotes(&file.key),
                escape_quotes(&file.filename)
            )],
        );
        if headers.get("Content-Type").and_then(|v| v.first()).is_none_or(|v| v.is_empty()) {
            headers.insert("Content-Type".into(), vec!["application/octet-stream".into()]);
        }
        writer.part(&headers, &file.data);
    }
    Ok(writer.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_payload_sets_model_and_stream() {
        let (out, ct) = prepare_images_payload(br#"{"model":"alias","prompt":"x","stream":true}"#, "up", "", false).unwrap();
        assert_eq!(ct, "application/json");
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"model":"up","prompt":"x"}"#);
        let (out, _) = prepare_images_payload(br#"{"prompt":"x"}"#, "up", "", true).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), r#"{"prompt":"x","model":"up","stream":true}"#);
    }

    #[test]
    fn multipart_edit_is_reencoded_with_model_first() {
        let body = b"--b0\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nalias\r\n--b0\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nhello\r\n--b0\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNGDATA\r\n--b0--\r\n";
        let (out, ct) = prepare_images_payload(body, "up", "multipart/form-data; boundary=b0", true).unwrap();
        let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap();
        let text = String::from_utf8(out).unwrap();
        let expect = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nup\r\n--{b}\r\nContent-Disposition: form-data; name=\"stream\"\r\n\r\ntrue\r\n--{b}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nhello\r\n--{b}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNGDATA\r\n--{b}--\r\n",
            b = boundary
        );
        assert_eq!(text, expect);
    }

    #[test]
    fn multipart_without_boundary_fails() {
        let err = prepare_images_payload(b"--x", "m", "multipart/form-data", false).unwrap_err();
        assert_eq!(err.message, "multipart boundary is missing");
    }
}

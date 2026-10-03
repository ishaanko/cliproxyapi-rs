//! Multipart and urlencoded forms the way Go's `mime/multipart` and gin's `PostForm` expose
//! them: `ReadForm` splitting (parts with a filename are files, the rest are values), canonical
//! MIME header names, and the `multipart.Writer` output used to rebuild upstream requests.

use bytes::Bytes;
use futures_util::stream;

/// `ErrNotMultipart` message of `net/http`.
pub const ERR_NOT_MULTIPART: &str = "request Content-Type isn't multipart/form-data";
/// `ErrMissingBoundary` message of `net/http`.
pub const ERR_MISSING_BOUNDARY: &str = "no multipart boundary param in Content-Type";

/// One uploaded file part (`multipart.FileHeader` plus its content).
#[derive(Debug, Clone)]
pub struct FilePart {
    pub filename: String,
    /// Part headers with canonical names (Go: `FileHeader.Header`), in arrival order.
    pub headers: Vec<(String, String)>,
    pub data: Bytes,
}

impl FilePart {
    /// `FileHeader.Header.Get(name)`.
    pub fn header(&self, name: &str) -> &str {
        let name = canonical_header_name(name);
        self.headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str()).unwrap_or("")
    }
}

/// `multipart.Form`: values and files per field name, in first-appearance order.
#[derive(Debug, Clone, Default)]
pub struct Form {
    pub values: Vec<(String, Vec<String>)>,
    pub files: Vec<(String, Vec<FilePart>)>,
}

impl Form {
    /// `c.PostForm(key)`: first value, empty when absent.
    pub fn value(&self, key: &str) -> &str {
        self.values.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.first()).map(String::as_str).unwrap_or("")
    }

    /// `form.File[key]`.
    pub fn file(&self, key: &str) -> &[FilePart] {
        self.files.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_slice()).unwrap_or(&[])
    }

    fn push_value(&mut self, key: &str, value: String) {
        match self.values.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => v.push(value),
            None => self.values.push((key.to_string(), vec![value])),
        }
    }

    fn push_file(&mut self, key: &str, file: FilePart) {
        match self.files.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => v.push(file),
            None => self.files.push((key.to_string(), vec![file])),
        }
    }
}

/// `textproto.CanonicalMIMEHeaderKey`.
pub fn canonical_header_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = true;
    for c in name.chars() {
        out.push(if upper { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() });
        upper = c == '-';
    }
    out
}

/// Boundary of a `multipart/form-data` Content-Type (Go: `multipartReader`).
fn boundary(content_type: &str) -> Result<String, &'static str> {
    if content_type.trim().is_empty() {
        return Err(ERR_NOT_MULTIPART);
    }
    let mut parts = content_type.split(';');
    let media = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    if media != "multipart/form-data" {
        return Err(ERR_NOT_MULTIPART);
    }
    for param in parts {
        let Some((key, value)) = param.split_once('=') else { continue };
        if key.trim().eq_ignore_ascii_case("boundary") {
            let value = value.trim();
            let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    Err(ERR_MISSING_BOUNDARY)
}

/// `Request.ParseMultipartForm`: the error text is what gin reports in `Invalid request: ...`.
pub async fn parse_multipart(content_type: &str, body: Bytes) -> Result<Form, String> {
    let boundary = boundary(content_type).map_err(str::to_string)?;
    let mut mp = multer::Multipart::new(stream::once(async move { Ok::<_, std::io::Error>(body) }), boundary);
    let mut form = Form::default();
    loop {
        let field = match mp.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => return Err("multipart: NextPart: EOF".to_string()),
        };
        let name = field.name().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let filename = field.file_name().unwrap_or("").to_string();
        let headers: Vec<(String, String)> = field
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((canonical_header_name(k.as_str()), v.to_str().ok()?.to_string())))
            .collect();
        let data = field.bytes().await.map_err(|_| "multipart: NextPart: EOF".to_string())?;
        if filename.is_empty() {
            form.push_value(&name, String::from_utf8_lossy(&data).into_owned());
        } else {
            form.push_file(&name, FilePart { filename, headers, data });
        }
    }
    Ok(form)
}

/// `application/x-www-form-urlencoded` body as form values (Go: `ParseForm`).
pub fn parse_urlencoded(body: &[u8]) -> Form {
    let mut form = Form::default();
    for (k, v) in url::form_urlencoded::parse(body) {
        form.push_value(&k, v.into_owned());
    }
    form
}

/// `multipart.Writer` replacement: boundary, field and file parts, close.
pub struct Writer {
    boundary: String,
    buf: Vec<u8>,
    started: bool,
}

impl Writer {
    pub fn new() -> Self {
        // Go: 30 random bytes as hex.
        let boundary: String = (0..30).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
        Writer { boundary, buf: Vec::new(), started: false }
    }

    /// `FormDataContentType`.
    pub fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    fn part(&mut self, headers: &[(String, String)], data: &[u8]) {
        if self.started {
            self.buf.extend_from_slice(format!("\r\n--{}\r\n", self.boundary).as_bytes());
        } else {
            self.buf.extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        }
        self.started = true;
        // CreatePart writes headers sorted by key.
        let mut sorted: Vec<&(String, String)> = headers.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in sorted {
            self.buf.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        self.buf.extend_from_slice(b"\r\n");
        self.buf.extend_from_slice(data);
    }

    /// `WriteField`.
    pub fn write_field(&mut self, name: &str, value: &str) {
        let disposition = format!("form-data; name=\"{}\"", escape_quotes(name));
        self.part(&[("Content-Disposition".into(), disposition)], value.as_bytes());
    }

    /// A file part: the original headers with `Content-Disposition` rebuilt and a default
    /// `Content-Type` (Go: `FileContentDisposition` + `application/octet-stream`).
    pub fn write_file(&mut self, name: &str, file: &FilePart) {
        let mut headers: Vec<(String, String)> = file.headers.iter().filter(|(k, _)| k != "Content-Disposition").cloned().collect();
        let disposition = format!("form-data; name=\"{}\"; filename=\"{}\"", escape_quotes(name), escape_quotes(&file.filename));
        headers.push(("Content-Disposition".into(), disposition));
        if !headers.iter().any(|(k, v)| k == "Content-Type" && !v.is_empty()) {
            headers.retain(|(k, _)| k != "Content-Type");
            headers.push(("Content-Type".into(), "application/octet-stream".into()));
        }
        self.part(&headers, &file.data);
    }

    /// `Close`: returns the body.
    pub fn finish(mut self) -> Vec<u8> {
        self.buf.extend_from_slice(format!("\r\n--{}--\r\n", self.boundary).as_bytes());
        self.buf
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

/// `escapeQuotes`: backslash-escapes `\` and `"`.
fn escape_quotes(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn splits_values_and_files_like_readform() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nhello\r\n--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNGDATA\r\n--b--\r\n";
        let form = parse_multipart("multipart/form-data; boundary=b", Bytes::from_static(body)).await.unwrap();
        assert_eq!(form.value("prompt"), "hello");
        let file = &form.file("image")[0];
        assert_eq!(file.filename, "a.png");
        assert_eq!(file.header("content-type"), "image/png");
        assert_eq!(&file.data[..], b"PNGDATA");
    }

    #[tokio::test]
    async fn rejects_missing_boundary_and_wrong_type() {
        assert_eq!(parse_multipart("", Bytes::new()).await.unwrap_err(), ERR_NOT_MULTIPART);
        assert_eq!(parse_multipart("application/json", Bytes::new()).await.unwrap_err(), ERR_NOT_MULTIPART);
        assert_eq!(parse_multipart("multipart/form-data", Bytes::new()).await.unwrap_err(), ERR_MISSING_BOUNDARY);
    }

    #[test]
    fn writer_sorts_headers_and_defaults_file_type() {
        let mut w = Writer::new();
        w.write_field("model", "m");
        w.write_file("image", &FilePart { filename: "a\"b.png".into(), headers: vec![], data: Bytes::from_static(b"x") });
        let ct = w.content_type();
        let boundary = ct.rsplit('=').next().unwrap().to_string();
        let body = String::from_utf8(w.finish()).unwrap();
        assert_eq!(
            body,
            format!("--{b}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n--{b}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a\\\"b.png\"\r\nContent-Type: application/octet-stream\r\n\r\nx\r\n--{b}--\r\n", b = boundary)
        );
    }
}

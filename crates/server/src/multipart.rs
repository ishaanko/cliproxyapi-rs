//! Multipart and urlencoded forms the way Go's `mime/multipart` and gin's `PostForm` expose
//! them: `ReadForm` splitting (parts with a filename are files, the rest are values), canonical
//! MIME header names, and the `multipart.Writer` output used to rebuild upstream requests.

use std::collections::HashMap;

use bytes::Bytes;
use futures_util::stream;

/// `ErrNotMultipart` message of `net/http`.
pub const ERR_NOT_MULTIPART: &str = "request Content-Type isn't multipart/form-data";
/// `ErrMissingBoundary` message of `net/http`.
pub const ERR_MISSING_BOUNDARY: &str = "no multipart boundary param in Content-Type";

/// `ErrMessageTooLarge` message of `mime/multipart`.
pub const ERR_MESSAGE_TOO_LARGE: &str = "multipart: message too large";
/// Parts `ReadForm` accepts before giving up (Go: `multipartmaxparts` default).
const MAX_PARTS: usize = 1000;
/// Memory budget of `ReadForm` for non-file data: gin's `MaxMultipartMemory` (32 MiB) plus the
/// 10 MiB `ReadForm` reserves.
const MAX_FORM_MEMORY: i64 = (32 << 20) + (10 << 20);
/// `net/http` `parsePostForm` read cap for urlencoded bodies.
const MAX_URLENCODED_BYTES: usize = 10 << 20;
const MAP_ENTRY_OVERHEAD: i64 = 200;
const FILE_HEADER_SIZE: i64 = 100;

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

/// `multipart.Form`: values and files per field name. Iteration follows first appearance (Go's
/// maps have no order); lookups go through a key index.
#[derive(Debug, Clone, Default)]
pub struct Form {
    values: Vec<(String, Vec<String>)>,
    value_index: HashMap<String, usize>,
    files: Vec<(String, Vec<FilePart>)>,
    file_index: HashMap<String, usize>,
}

/// Appends to the entry of `key`, creating it on first use.
fn push_keyed<T>(entries: &mut Vec<(String, Vec<T>)>, index: &mut HashMap<String, usize>, key: &str, item: T) {
    match index.get(key) {
        Some(&i) => entries[i].1.push(item),
        None => {
            index.insert(key.to_string(), entries.len());
            entries.push((key.to_string(), vec![item]));
        }
    }
}

impl Form {
    /// `c.PostForm(key)`: first value, empty when absent.
    pub fn value(&self, key: &str) -> &str {
        let found = self.value_index.get(key).and_then(|&i| self.values[i].1.first());
        found.map(String::as_str).unwrap_or("")
    }

    /// `form.File[key]`.
    pub fn file(&self, key: &str) -> &[FilePart] {
        self.file_index.get(key).map(|&i| self.files[i].1.as_slice()).unwrap_or(&[])
    }

    /// `form.Value` entries in first-appearance order.
    pub fn values(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.values.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// `form.File` entries in first-appearance order.
    pub fn files(&self) -> impl Iterator<Item = (&str, &[FilePart])> {
        self.files.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    pub fn push_value(&mut self, key: &str, value: String) {
        push_keyed(&mut self.values, &mut self.value_index, key, value);
    }

    pub fn push_file(&mut self, key: &str, file: FilePart) {
        push_keyed(&mut self.files, &mut self.file_index, key, file);
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
/// Like `ReadForm`, more than 1000 parts or more non-file data than the memory budget allows
/// fails with `multipart: message too large`.
pub async fn parse_multipart(content_type: &str, body: Bytes) -> Result<Form, String> {
    let boundary = boundary(content_type).map_err(str::to_string)?;
    let mut mp = multer::Multipart::new(stream::once(async move { Ok::<_, std::io::Error>(body) }), boundary);
    let mut form = Form::default();
    let mut parts_left = MAX_PARTS;
    let mut budget = MAX_FORM_MEMORY;
    loop {
        let mut field = match mp.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => return Err("multipart: NextPart: EOF".to_string()),
        };
        if parts_left == 0 {
            return Err(ERR_MESSAGE_TOO_LARGE.to_string());
        }
        parts_left -= 1;
        let name = field.name().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let filename = go_base(field.file_name().unwrap_or(""));
        budget -= name.len() as i64 + MAP_ENTRY_OVERHEAD;
        let headers: Vec<(String, String)> = field
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((canonical_header_name(k.as_str()), v.to_str().ok()?.to_string())))
            .collect();
        if !filename.is_empty() {
            budget -= mime_header_size(&headers) + MAP_ENTRY_OVERHEAD + FILE_HEADER_SIZE;
        }
        if budget < 0 {
            return Err(ERR_MESSAGE_TOO_LARGE.to_string());
        }
        let mut data = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(|_| "multipart: NextPart: EOF".to_string())? {
            data.extend_from_slice(&chunk);
            // Only non-file values count against the in-memory budget (files spill to disk in Go).
            if filename.is_empty() {
                budget -= chunk.len() as i64;
                if budget < 0 {
                    return Err(ERR_MESSAGE_TOO_LARGE.to_string());
                }
            }
        }
        if filename.is_empty() {
            form.push_value(&name, String::from_utf8_lossy(&data).into_owned());
        } else {
            form.push_file(&name, FilePart { filename, headers, data: Bytes::from(data) });
        }
    }
    Ok(form)
}

/// `mimeHeaderSize` of `mime/multipart`.
fn mime_header_size(headers: &[(String, String)]) -> i64 {
    400 + headers.iter().map(|(k, v)| (k.len() + v.len()) as i64 + MAP_ENTRY_OVERHEAD).sum::<i64>()
}

/// `filepath.Base` as `Part.FileName` applies it (an empty name stays empty): last path element,
/// `/` for a name of only slashes.
fn go_base(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// `application/x-www-form-urlencoded` body as form values (Go: `ParseForm`). A body over 10 MiB
/// fails with `http: POST too large`, which gin ignores, so the values read as empty.
pub fn parse_urlencoded(body: &[u8]) -> Form {
    let mut form = Form::default();
    if body.len() > MAX_URLENCODED_BYTES {
        return form;
    }
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

    fn part(name: &str, value: &str) -> String {
        format!("--b\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
    }

    #[tokio::test]
    async fn caps_parts_at_one_thousand_like_readform() {
        let ok: String = (0..1000).map(|i| part(&format!("f{i}"), "v")).collect::<String>() + "--b--\r\n";
        let form = parse_multipart("multipart/form-data; boundary=b", Bytes::from(ok)).await.unwrap();
        assert_eq!(form.value("f999"), "v");
        let too_many: String = (0..1001).map(|i| part(&format!("f{i}"), "v")).collect::<String>() + "--b--\r\n";
        assert_eq!(parse_multipart("multipart/form-data; boundary=b", Bytes::from(too_many)).await.unwrap_err(), ERR_MESSAGE_TOO_LARGE);
    }

    #[tokio::test]
    async fn repeated_keys_keep_order_and_filenames_are_base_names() {
        let body = part("k", "1") + &part("k", "2") + "--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"../dir/a.png\"\r\n\r\nX\r\n--b--\r\n";
        let form = parse_multipart("multipart/form-data; boundary=b", Bytes::from(body)).await.unwrap();
        assert_eq!(form.values().collect::<Vec<_>>(), [("k", &["1".to_string(), "2".to_string()][..])]);
        assert_eq!(form.value("k"), "1");
        assert_eq!(form.file("image")[0].filename, "a.png");
    }

    #[test]
    fn urlencoded_over_ten_mib_reads_as_empty() {
        assert_eq!(parse_urlencoded(b"a=1&a=2&b=%20x").value("b"), " x");
        let mut big = b"a=".to_vec();
        big.resize(MAX_URLENCODED_BYTES + 1, b'x');
        assert_eq!(parse_urlencoded(&big).value("a"), "");
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

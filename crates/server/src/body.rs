//! Request body handling (Go: sdk/api/handlers/request_body.go).

use axum::http::HeaderMap;
use bytes::Bytes;

/// `ReadRequestBody` after the raw bytes were read: decodes the `Content-Encoding` list
/// (right to left, only `zstd` is supported). An undecodable body that is already valid JSON is
/// used as is; otherwise the error text becomes the 400 message.
pub fn decode_request_body(headers: &HeaderMap, raw: Bytes) -> Result<Bytes, String> {
    let encoding = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or("");
    if encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
        return Ok(raw);
    }
    match decode_encodings(&raw, encoding) {
        Ok(decoded) => Ok(decoded),
        Err(err) => {
            if cpa_json::valid(&raw) {
                Ok(raw)
            } else {
                Err(err)
            }
        }
    }
}

fn decode_encodings(raw: &Bytes, encoding: &str) -> Result<Bytes, String> {
    let mut body = raw.clone();
    for part in encoding.split(',').rev() {
        match part.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => {}
            "zstd" => {
                let decoded = zstd::stream::decode_all(&body[..])
                    .map_err(|e| format!("failed to decode zstd request body: {e}"))?;
                body = Bytes::from(decoded);
            }
            other => return Err(format!("unsupported request content encoding: {other}")),
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(encoding: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("content-encoding", HeaderValue::from_str(encoding).unwrap());
        h
    }

    #[test]
    fn zstd_body_is_decoded() {
        let payload = br#"{"model":"m","stream":true}"#;
        let compressed = zstd::stream::encode_all(&payload[..], 1).unwrap();
        let out = decode_request_body(&headers("zstd"), Bytes::from(compressed)).unwrap();
        assert_eq!(&out[..], &payload[..]);
    }

    #[test]
    fn identity_and_json_fallback() {
        let raw = Bytes::from_static(br#"{"a":1}"#);
        assert_eq!(decode_request_body(&headers("identity"), raw.clone()).unwrap(), raw);
        // unsupported encoding but the bytes already are JSON
        assert_eq!(decode_request_body(&headers("gzip"), raw.clone()).unwrap(), raw);
        // unsupported encoding and not JSON
        let err = decode_request_body(&headers("gzip"), Bytes::from_static(b"\x1f\x8b")).unwrap_err();
        assert_eq!(err, "unsupported request content encoding: gzip");
    }

    #[test]
    fn encodings_apply_right_to_left() {
        let payload = Bytes::from_static(br#"{"x":1}"#);
        let compressed = zstd::stream::encode_all(&payload[..], 1).unwrap();
        // "identity, zstd": zstd decoded first, identity skipped
        let out = decode_request_body(&headers("identity, zstd"), Bytes::from(compressed)).unwrap();
        assert_eq!(out, payload);
    }
}

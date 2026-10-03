//! Bounded GET helper (Go: internal/httpfetch).

use futures_util::StreamExt;

/// Failure modes of [`get_bytes`]; the messages match the Go errors.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("create request: {0}")]
    CreateRequest(String),
    #[error("request failed: {0}")]
    Request(reqwest::Error),
    #[error("unexpected status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("read response: {0}")]
    Read(reqwest::Error),
    #[error("response exceeds maximum allowed size of {0} bytes")]
    TooLarge(u64),
}

/// Performs a GET with the supplied headers (empty values are skipped), requires a 2xx status and
/// returns the body. When `max_size` is positive the body is rejected once it exceeds it.
/// Cancel by dropping the future.
pub async fn get_bytes(
    client: &reqwest::Client,
    request_url: &str,
    headers: &[(&str, &str)],
    max_size: u64,
) -> Result<Vec<u8>, FetchError> {
    let mut request = client
        .get(request_url)
        .build()
        .map_err(|e| FetchError::CreateRequest(e.to_string()))?;
    for (key, value) in headers {
        if value.is_empty() {
            continue;
        }
        let name = http::HeaderName::from_bytes(key.as_bytes())
            .map_err(|e| FetchError::CreateRequest(e.to_string()))?;
        let value = http::HeaderValue::from_str(value)
            .map_err(|e| FetchError::CreateRequest(e.to_string()))?;
        request.headers_mut().insert(name, value);
    }

    let response = client.execute(request).await.map_err(FetchError::Request)?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = read_limited(response, 4096).await.unwrap_or_default();
        return Err(FetchError::Status {
            status,
            body: String::from_utf8_lossy(&body).trim().to_string(),
        });
    }

    let limit = if max_size > 0 { max_size.saturating_add(1) } else { u64::MAX };
    let data = read_limited(response, limit).await.map_err(FetchError::Read)?;
    if max_size > 0 && data.len() as u64 > max_size {
        return Err(FetchError::TooLarge(max_size));
    }
    Ok(data)
}

/// Reads at most `limit` bytes of the body (`io.LimitReader` + `io.ReadAll`).
async fn read_limited(response: reqwest::Response, limit: u64) -> Result<Vec<u8>, reqwest::Error> {
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let room = usize::try_from(limit.saturating_sub(out.len() as u64)).unwrap_or(usize::MAX);
        if chunk.len() >= room {
            out.extend_from_slice(&chunk[..room]);
            break;
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn returns_body_and_sends_headers() {
        let app = Router::new().route(
            "/",
            get(|headers: HeaderMap| async move {
                let ua = headers.get("user-agent").and_then(|v| v.to_str().ok());
                let accept = headers.get("accept").and_then(|v| v.to_str().ok());
                if ua != Some("agent") || accept != Some("application/json") {
                    return (StatusCode::BAD_REQUEST, "missing headers");
                }
                (StatusCode::OK, "payload")
            }),
        );
        let url = serve(app).await;
        let data = get_bytes(
            &reqwest::Client::new(),
            &url,
            &[("User-Agent", "agent"), ("Accept", "application/json")],
            0,
        )
        .await
        .unwrap();
        assert_eq!(data, b"payload");
    }

    #[tokio::test]
    async fn rejects_error_status() {
        let app = Router::new().route("/", get(|| async { (StatusCode::NOT_FOUND, "missing\n") }));
        let url = serve(app).await;
        let err = get_bytes(&reqwest::Client::new(), &url, &[], 0).await.unwrap_err();
        assert!(err.to_string().contains("unexpected status 404"), "{err}");
        assert!(err.to_string().ends_with("missing"), "{err}");
    }

    #[tokio::test]
    async fn enforces_max_size() {
        let app = Router::new().route("/", get(|| async { "0123456789" }));
        let url = serve(app).await;
        let err = get_bytes(&reqwest::Client::new(), &url, &[], 4).await.unwrap_err();
        assert!(err.to_string().contains("maximum allowed size"), "{err}");
        assert_eq!(get_bytes(&reqwest::Client::new(), &url, &[], 10).await.unwrap().len(), 10);
    }
}

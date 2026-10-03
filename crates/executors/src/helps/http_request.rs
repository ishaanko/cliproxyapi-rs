//! Shared pieces of the executors' `PrepareRequest` / `HttpRequest` (Go: the credential
//! injection methods on every ProviderExecutor, used for arbitrary upstream calls).

use cpa_auth::Auth;
use cpa_runtime::executor::ExecError;
use http::{HeaderName, HeaderValue, header};
use std::collections::HashMap;

use super::status::transport_error;

/// `req.Header.Set(name, value)`; an unrepresentable name or value leaves the header untouched.
pub fn set_header(req: &mut reqwest::Request, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
        req.headers_mut().insert(n, v);
    }
}

/// `req.Header.Del(name)`.
pub fn del_header(req: &mut reqwest::Request, name: &str) {
    if let Ok(n) = HeaderName::from_bytes(name.as_bytes()) {
        req.headers_mut().remove(n);
    }
}

/// Bearer `token` into `Authorization`, or no `Authorization` when the token is blank (the
/// `if TrimSpace(token) != "" { Set } else { Del }` shape most executors share).
pub fn set_bearer_or_clear(req: &mut reqwest::Request, token: &str) {
    if token.trim().is_empty() {
        req.headers_mut().remove(header::AUTHORIZATION);
    } else {
        set_header(req, "Authorization", &format!("Bearer {token}"));
    }
}

/// `util.ApplyCustomHeadersFromAttrs(req, auth.Attributes)`. No inbound request is available
/// here, so `$HEADER` variables resolve to nothing.
pub fn apply_attr_headers(req: &mut reqwest::Request, auth: &Auth) {
    let attrs: HashMap<String, String> = auth.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    cpa_core::util::apply_custom_headers_from_attrs(req.headers_mut(), &attrs, None, None);
}

/// `httpClient.Do(req)` with transport failures mapped to status-less errors.
pub async fn execute(client: &reqwest::Client, req: reqwest::Request) -> Result<reqwest::Response, ExecError> {
    client.execute(req).await.map_err(|e| transport_error(&e))
}

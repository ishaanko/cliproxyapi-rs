//! xAI image and video endpoints (Go: xai_executor_media.go). Bodies pass through (image refs
//! are normalized) and responses are returned as the upstream sent them.

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_json::J;
use cpa_runtime::executor::{ExecError, Options, Request, Response};
use http::Method;

use super::XaiExecutor;
use super::execute::{content_type, read_body};
use super::request::{
    IDEMPOTENCY_KEY_META_KEY, IMAGES_GENERATIONS_PATH, VIDEOS_EDITS_PATH, VIDEOS_EXTENSIONS_PATH,
    VIDEOS_GENERATIONS_PATH, VIDEOS_PATH, apply_headers, chat_base_url, creds, log_resolved_base_url,
    metadata_string, normalize_image_refs, video_endpoint_path,
};
use super::response::status_err_for_body;
use crate::helps::session::ensure_session_id;

/// Go: `url.PathEscape`.
fn path_escape(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b'=' | b':' | b'@');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The model reported for usage: the payload `model`, else the request model.
fn media_model(req: &Request) -> String {
    let model = cpa_json::parse(&req.payload).g("model").str().trim().to_string();
    if model.is_empty() { req.model.trim().to_string() } else { model }
}

impl XaiExecutor {
    /// Go: executeImages.
    pub(super) async fn execute_images(
        &self,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        endpoint_path: &str,
    ) -> Result<Response, ExecError> {
        let reporter = self.new_reporter(&media_model(req), auth, opts);
        let result = async {
            let cfg = self.config();
            let session = ensure_session_id(None, "", opts, &req.payload);
            let (token, _) = creds(Some(auth));
            let base_url = chat_base_url(Some(auth));
            log_resolved_base_url(&base_url);
            let endpoint_path = if endpoint_path.is_empty() { IMAGES_GENERATIONS_PATH } else { endpoint_path };
            let payload = normalize_image_refs(&req.payload);
            let url = format!("{}{endpoint_path}", base_url.trim_end_matches('/'));
            let headers = apply_headers(Some(auth), &token, false, "", opts, session.as_deref())?;
            self.record_request(&cfg, auth, opts, &url, &headers, &payload);
            let resp = self.send(&cfg, auth, opts, &reporter, &url, headers, payload).await?;
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            let data = read_body(&cfg, opts, &reporter, resp).await?;
            if !(200..300).contains(&status) {
                tracing::debug!(
                    "request error, error status: {status}, error message: {}",
                    crate::helps::logging::summarize_error_body(&content_type(&resp_headers), &data)
                );
                return Err(status_err_for_body(status, &data));
            }
            reporter.observe_response_model(&data);
            reporter.ensure_published();
            Ok(Response { payload: data, headers: resp_headers, ..Default::default() })
        }
        .await;
        reporter.track_failure(&result);
        result
    }

    /// Go: executeVideos. Creation endpoints POST; a payload carrying only `request_id` polls
    /// `GET /videos/{id}`.
    pub(super) async fn execute_videos(&self, auth: &Auth, req: &Request, opts: &Options) -> Result<Response, ExecError> {
        let reporter = self.new_reporter(&media_model(req), auth, opts);
        let result = async {
            let cfg = self.config();
            let session = ensure_session_id(None, "", opts, &req.payload);
            let (token, _) = creds(Some(auth));
            let base_url = chat_base_url(Some(auth));
            log_resolved_base_url(&base_url);

            let payload = normalize_image_refs(&req.payload);
            let mut method = Method::POST;
            let mut endpoint_path = VIDEOS_GENERATIONS_PATH.to_string();
            let mut body = Some(payload.clone());
            match video_endpoint_path(opts) {
                path @ (VIDEOS_GENERATIONS_PATH | VIDEOS_EDITS_PATH | VIDEOS_EXTENSIONS_PATH) => {
                    endpoint_path = path.to_string();
                }
                _ => {
                    let request_id = cpa_json::parse(&payload).g("request_id").str().trim().to_string();
                    if !request_id.is_empty() {
                        method = Method::GET;
                        endpoint_path = format!("{VIDEOS_PATH}/{}", path_escape(&request_id));
                        body = None;
                    }
                }
            }
            let url = format!("{}{endpoint_path}", base_url.trim_end_matches('/'));
            let mut headers = apply_headers(Some(auth), &token, false, "", opts, session.as_deref())?;
            if method == Method::POST {
                let mut key = metadata_string(&opts.metadata, IDEMPOTENCY_KEY_META_KEY);
                if key.is_empty() {
                    key = opts.headers.get("x-idempotency-key").and_then(|v| v.to_str().ok()).unwrap_or_default().trim().to_string();
                }
                if !key.is_empty()
                    && let Ok(value) = http::HeaderValue::from_str(&key)
                {
                    headers.insert("x-idempotency-key", value);
                }
            }
            self.record_request(&cfg, auth, opts, &url, &headers, &payload);
            let resp = self.send_method(&cfg, auth, opts, &reporter, method, &url, headers, body).await?;
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            let data: Bytes = read_body(&cfg, opts, &reporter, resp).await?;
            if !(200..300).contains(&status) {
                tracing::debug!(
                    "request error, error status: {status}, error message: {}",
                    crate::helps::logging::summarize_error_body(&content_type(&resp_headers), &data)
                );
                return Err(status_err_for_body(status, &data));
            }
            reporter.observe_response_model(&data);
            reporter.ensure_published();
            Ok(Response { payload: data, headers: resp_headers, ..Default::default() })
        }
        .await;
        reporter.track_failure(&result);
        result
    }
}

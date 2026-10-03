//! Upstream request logging glue shared by the executors in this crate (Go: the
//! `helps.RecordAPIRequest(ctx, cfg, helps.UpstreamRequestLog{...})` blocks).

use cpa_auth::Auth;
use cpa_config::Config;
use cpa_runtime::apilog::{ApiLogHandle, UpstreamRequestLog};
use http::HeaderMap;

/// Records an outbound upstream request; the request details (including the body copy) are only
/// built when the call is part of an inbound request.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_request(
    log: &ApiLogHandle,
    cfg: &Config,
    provider: &str,
    auth: Option<&Auth>,
    method: &str,
    url: &str,
    headers: &HeaderMap,
    body: &[u8],
) {
    if log.get().is_none() {
        return;
    }
    log.record_api_request(cfg, UpstreamRequestLog::from_auth(provider, auth, method, url, headers, body));
}

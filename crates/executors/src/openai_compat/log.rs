//! Upstream request logging glue shared by the executors in this crate (Go: the
//! `helps.RecordAPIRequest(ctx, cfg, helps.UpstreamRequestLog{...})` blocks).

use cpa_auth::Auth;
use cpa_config::Config;
use cpa_runtime::apilog::{ApiLogHandle, UpstreamRequestLog};
use http::HeaderMap;

/// Go's `auth.ID`, `auth.Label` and `auth.AccountInfo()` fields of an [`UpstreamRequestLog`].
pub(crate) fn auth_log_fields(provider: &str, auth: Option<&Auth>) -> UpstreamRequestLog {
    let mut info = UpstreamRequestLog { provider: provider.to_string(), ..Default::default() };
    if let Some(auth) = auth {
        info.auth_id = auth.id.clone();
        info.auth_label = auth.label.clone();
        let (kind, value) = auth.account_info();
        info.auth_type = kind.to_string();
        info.auth_value = value;
    }
    info
}

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
    log.record_api_request(
        cfg,
        UpstreamRequestLog {
            url: url.to_string(),
            method: method.to_string(),
            headers: headers.clone(),
            body: body.to_vec(),
            ..auth_log_fields(provider, auth)
        },
    );
}

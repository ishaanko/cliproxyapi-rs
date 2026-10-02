//! Shared test helpers.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

/// Unsigned JWT with the given claims.
pub(crate) fn make_jwt(claims: &Value) -> String {
    let h = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
    let p = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap_or_default());
    format!("{h}.{p}.sig")
}

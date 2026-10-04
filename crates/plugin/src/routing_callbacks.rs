//! `host.routing.*` callbacks (Go: `routing_callbacks.go`).

use std::sync::Arc;

use cpa_pluginapi::api::{HostRoutingResetCooldownRequest, HostRoutingResetCooldownResponse};

use crate::callbacks::{CbIdentity, decode, marshal_result};
use crate::error::HostError;
use crate::host::Host;

impl Host {
    /// Clears quota and cooldown routing state for one credential, the same reset
    /// `POST /v8/management/routing/cooldown/reset` performs. The manager's in-memory reset never
    /// touches token files (Go passes `WithSkipPersist` for the same effect).
    pub(crate) fn cb_routing_reset_cooldown(self: &Arc<Self>, id: &CbIdentity, raw: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: HostRoutingResetCooldownRequest = decode(raw, "host routing reset cooldown request")?;
        let auth_index = req.auth_index.trim().to_string();
        let auth = self.auth_by_index(&auth_index)?;
        let Some(manager) = self.auth_manager() else {
            return Err(HostError::msg("core auth manager unavailable"));
        };
        let reset = manager.reset_quota(&auth.id).map_err(|e| HostError::msg(format!("reset cooldown: {e}")))?;
        let Some((mut updated, models)) = reset else {
            return Err(HostError::msg(format!("auth not found for auth_index {auth_index}")));
        };
        let index = updated.ensure_index();
        tracing::info!(plugin_id = %id.plugin_id, auth_index = index.as_str(), models = models.len(), "pluginhost: plugin reset credential cooldown");
        marshal_result(&HostRoutingResetCooldownResponse { auth_index: index, models })
    }
}

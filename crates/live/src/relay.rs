//! WebRTC media relay (stub, replaced below).
use std::sync::Arc;

use async_trait::async_trait;
use cpa_config::CodexLiveMediaRelayConfig;

use crate::media::{MediaError, MediaLimiter, MediaRelayFactory, MediaRelaySession, MediaRoute};

pub struct PionMediaRelay;

impl PionMediaRelay {
    pub fn new(_cfg: &CodexLiveMediaRelayConfig, _limiter: Arc<MediaLimiter>) -> Result<Self, String> {
        Ok(PionMediaRelay)
    }
}

#[async_trait]
impl MediaRelayFactory for PionMediaRelay {
    async fn new_session(&self, _o: &str, _r: MediaRoute) -> Result<(Arc<dyn MediaRelaySession>, String), MediaError> {
        Err(MediaError::new("unimplemented"))
    }
}

//! Config-driven side services owned by the [`Service`](super::Service): the mDNS advertiser
//! (Go: `applyDiscoveryConfigContext`) and the pprof server (Go: `applyPprofConfigContext`).
//! `Service` calls [`Extras::apply`] on start and after every accepted config reload, and
//! [`Extras::shutdown`] when it stops.

use std::sync::Arc;

use cpa_config::{Config, DEFAULT_PORT};
use cpa_discovery::Ctx;

use super::discovery::DiscoveryManager;
use super::pprof::PprofServer;

#[derive(Default)]
pub(super) struct Extras {
    discovery: DiscoveryManager,
    pprof: PprofServer,
}

impl Extras {
    /// pprof first, then discovery, as in Go. The advertised endpoint is the configured listen
    /// port (the HTTP server falls back to 8317 when it is unset).
    pub(super) async fn apply(&self, cfg: &Arc<Config>) {
        let ctx = Ctx::background();
        if !self.pprof.apply(&ctx, cfg).await {
            return;
        }
        let port = if cfg.port > 0 { cfg.port } else { DEFAULT_PORT };
        self.discovery.apply(&ctx, cfg, port, cfg.tls.enable).await;
    }

    pub(super) async fn shutdown(&self) {
        if let Err(err) = self.pprof.shutdown(&Ctx::background()).await {
            tracing::error!("failed to stop pprof server: {err}");
        }
        if let Err(err) = self.discovery.shutdown().await {
            tracing::error!("failed to stop discovery advertiser: {err}");
        }
    }
}

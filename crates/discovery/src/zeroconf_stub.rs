//! Non-Unix stand-ins: mDNS needs raw multicast sockets that are only wired up for Unix.

use async_trait::async_trait;

use crate::ctx::Ctx;
use crate::interfaces::Interface;
use crate::types::{Advertiser, Browser, DiscoveredService, Error, ServiceSpec};

fn unsupported() -> Error {
    Error::new("discovery: mDNS is not supported on this platform")
}

#[derive(Default)]
pub struct ZeroconfAdvertiser;

impl ZeroconfAdvertiser {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Advertiser for ZeroconfAdvertiser {
    async fn start(&self, _ctx: &Ctx, _spec: ServiceSpec) -> Result<(), Error> {
        Err(unsupported())
    }

    async fn stop(&self) -> Result<(), Error> {
        Ok(())
    }
}

#[derive(Default)]
pub struct ZeroconfBrowser;

impl ZeroconfBrowser {
    pub fn new(_ifaces: Vec<Interface>) -> Self {
        Self
    }
}

#[async_trait]
impl Browser for ZeroconfBrowser {
    async fn browse(&self, _ctx: &Ctx, _service_type: &str, _domain: &str) -> Result<Vec<DiscoveredService>, Error> {
        Err(unsupported())
    }

    async fn browse_with_fallback(&self, _ctx: &Ctx) -> Result<Vec<DiscoveredService>, Error> {
        Err(unsupported())
    }

    async fn browse_with_fallback_service_type(&self, _ctx: &Ctx, _service_type: &str) -> Result<Vec<DiscoveredService>, Error> {
        Err(unsupported())
    }
}

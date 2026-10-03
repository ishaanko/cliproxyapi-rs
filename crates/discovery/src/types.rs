//! Shared discovery types and the advertiser/browser contracts (Go: internal/discovery/types.go).

use std::collections::BTreeMap;
use std::net::IpAddr;

use async_trait::async_trait;
use serde::Serialize;

use crate::ctx::{Ctx, CtxError};
use crate::interfaces::Interface;

/// Proposed common service type for AI gateways.
pub const DEFAULT_SERVICE_TYPE: &str = "_ai-gateway._tcp";
/// Link-local domain used by mDNS.
pub const DEFAULT_DOMAIN: &str = "local.";
pub const SUBTYPE_CHAT_COMPLETIONS: &str = "_chat-completions";
pub const SUBTYPE_RESPONSES: &str = "_responses";
pub const SUBTYPE_MESSAGES: &str = "_messages";
pub const SUBTYPE_GENERATE_CONTENT: &str = "_generate-content";
pub const SUBTYPE_INTERACTIONS: &str = "_interactions";
/// Product identifier for CLIProxyAPI.
pub const PRODUCT_CPA: &str = "cliproxyapi";

/// Error type for discovery operations; messages match the Go `fmt.Errorf` strings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Error(pub String);

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl From<CtxError> for Error {
    fn from(err: CtxError) -> Self {
        Self(err.to_string())
    }
}

/// A service to be advertised on the local network.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceSpec {
    pub instance_name: String,
    pub service_type: String,
    pub domain: String,
    pub port: i64,
    pub subtypes: Vec<String>,
    pub text_records: Vec<String>,
    pub interfaces: Vec<Interface>,
    pub advertised_ips: Vec<String>,
}

/// A discovered AI gateway on the LAN. Field order and names are the `-discover-json` schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DiscoveredService {
    pub instance_name: String,
    pub service_type: String,
    pub domain: String,
    pub host: String,
    pub port: i64,
    pub ipv4: Vec<IpAddr>,
    pub ipv6: Vec<IpAddr>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub protocols: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub product: String,
    pub auth_required: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub auth_methods: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub endpoints: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub node_role: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub raw_txt: BTreeMap<String, String>,
}

/// Server-side mDNS advertisement lifecycle.
#[async_trait]
pub trait Advertiser: Send + Sync {
    async fn start(&self, ctx: &Ctx, spec: ServiceSpec) -> Result<(), Error>;
    async fn stop(&self) -> Result<(), Error>;
}

/// Client-side mDNS browsing.
#[async_trait]
pub trait Browser: Send + Sync {
    async fn browse(&self, ctx: &Ctx, service_type: &str, domain: &str) -> Result<Vec<DiscoveredService>, Error>;
    async fn browse_with_fallback(&self, ctx: &Ctx) -> Result<Vec<DiscoveredService>, Error>;
    async fn browse_with_fallback_service_type(&self, ctx: &Ctx, service_type: &str) -> Result<Vec<DiscoveredService>, Error>;
}

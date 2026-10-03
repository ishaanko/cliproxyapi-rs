//! Advertiser and browser built on the mDNS engine (Go: internal/discovery/zeroconf.go).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::ctx::Ctx;
use crate::interfaces::{Interface, extract_interface_ips, ip_is_loopback, ip_is_unspecified};
use crate::id::DEFAULT_INSTANCE_PREFIX;
use crate::mdns::{self, Server, ServiceEntry};
use crate::service::{sanitize_instance_name, sanitize_subtype};
use crate::txt::{MAX_TXT_RECORD_BYTES, parse_txt_records};
use crate::types::{
    Advertiser, Browser, DEFAULT_DOMAIN, DEFAULT_SERVICE_TYPE, DiscoveredService, Error, PRODUCT_CPA, ServiceSpec,
};

pub const MAX_DISCOVERED_SERVICES: usize = 256;
pub const MAX_BROWSE_ENTRIES: usize = 256;
pub const MAX_BROWSE_TXT_RECORDS: usize = 64;
pub const MAX_BROWSE_TXT_BYTES: usize = 16 * 1024;
pub const MAX_DISCOVERED_ADDRESSES: usize = 32;
pub const MAX_DISCOVERED_METADATA_ITEMS: usize = 32;
pub const MAX_METADATA_ITEM_BYTES: usize = 64;

/// Bare hostname without a `.local` suffix, so the responder does not double it.
fn extract_clean_host() -> String {
    let host = hostname().unwrap_or_default();
    if host.is_empty() {
        return "localhost".into();
    }
    let h = host.trim();
    let h = h.strip_suffix('.').unwrap_or(h);
    let h = h.strip_suffix(".local").unwrap_or(h);
    let h = h.strip_suffix('.').unwrap_or(h);
    if h.is_empty() { "localhost".into() } else { h.to_string() }
}

fn hostname() -> Option<String> {
    nix::unistd::gethostname().ok().map(|h| h.to_string_lossy().into_owned())
}

#[derive(Default)]
struct AdvertiserState {
    server: Option<Arc<Server>>,
    in_flight: bool,
    closed: bool,
}

/// Registers one service with the mDNS responder (Go: `ZeroconfAdvertiser`).
#[derive(Default)]
pub struct ZeroconfAdvertiser {
    state: Mutex<AdvertiserState>,
}

impl ZeroconfAdvertiser {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Shuts a responder down on a blocking thread (it sends goodbyes and joins its workers).
async fn shutdown_server(server: Arc<Server>) {
    let _ = tokio::task::spawn_blocking(move || server.shutdown()).await;
}

#[async_trait]
impl Advertiser for ZeroconfAdvertiser {
    /// Registers and starts the advertisement for the primary service type and its subtypes.
    async fn start(&self, ctx: &Ctx, mut spec: ServiceSpec) -> Result<(), Error> {
        if let Some(err) = ctx.err() {
            return Err(err.into());
        }
        if spec.interfaces.is_empty() {
            return Err(Error::new("discovery: cannot start advertiser with empty interface list (refusing fallback to all interfaces)"));
        }
        if !(1..=65535).contains(&spec.port) {
            return Err(Error::new(format!("discovery: invalid service port {} (must be between 1 and 65535)", spec.port)));
        }
        {
            let mut state = self.state.lock();
            if state.server.is_some() || state.in_flight {
                return Err(Error::new("discovery: advertiser already started"));
            }
            state.in_flight = true;
            state.closed = false;
        }

        let result = self.register(ctx, &mut spec).await;
        // Decide under the lock, shut servers down after releasing it.
        let (to_shutdown, outcome) = {
            let mut state = self.state.lock();
            state.in_flight = false;
            match result {
                // Defensive: a failed start never leaves a half-registered server behind.
                Err(err) => (state.server.take(), Err(err)),
                Ok(server) if state.closed => {
                    (Some(server), Err(Error::new("discovery: advertiser stopped before start completed")))
                }
                Ok(server) => {
                    state.server = Some(server);
                    (None, Ok(()))
                }
            }
        };
        if let Some(server) = to_shutdown {
            shutdown_server(server).await;
        }
        outcome
    }

    /// Shuts down the responder, sending goodbye packets. Idempotent.
    async fn stop(&self) -> Result<(), Error> {
        let server = {
            let mut state = self.state.lock();
            state.closed = true;
            state.server.take()
        };
        if let Some(server) = server {
            shutdown_server(server).await;
        }
        Ok(())
    }
}

impl ZeroconfAdvertiser {
    async fn register(&self, ctx: &Ctx, spec: &mut ServiceSpec) -> Result<Arc<Server>, Error> {
        let domain = if spec.domain.is_empty() { DEFAULT_DOMAIN.to_string() } else { spec.domain.clone() };
        let service_type = if spec.service_type.is_empty() { DEFAULT_SERVICE_TYPE.to_string() } else { spec.service_type.clone() };

        let clean_host = extract_clean_host();
        let mut ips = spec.advertised_ips.clone();
        if ips.is_empty() {
            ips = extract_interface_ips(&spec.interfaces);
        }
        if ips.is_empty() {
            return Err(Error::new("discovery: no usable IP addresses found on specified interfaces"));
        }

        spec.instance_name = sanitize_instance_name(&spec.instance_name);
        if spec.instance_name.is_empty() {
            spec.instance_name = format!("{DEFAULT_INSTANCE_PREFIX}0001");
        }
        if let Some(err) = ctx.err() {
            return Err(err.into());
        }

        let mut primary_service = service_type;
        for sub in &spec.subtypes {
            let clean = sanitize_subtype(sub);
            if !clean.is_empty() {
                primary_service.push(',');
                primary_service.push_str(&clean);
            }
        }

        let server = mdns::register_proxy(
            &spec.instance_name,
            &primary_service,
            &domain,
            spec.port,
            &clean_host,
            &ips,
            &spec.text_records,
            &spec.interfaces,
        )
        .map_err(|e| Error::new(format!("discovery: failed to register primary service {primary_service}: {e}")))?;
        let server = Arc::new(server);
        if let Some(err) = ctx.err() {
            shutdown_server(server).await;
            return Err(err.into());
        }
        Ok(server)
    }
}

/// DNS-SD browsing with CPA-first ordering (Go: `ZeroconfBrowser`).
#[derive(Default)]
pub struct ZeroconfBrowser {
    ifaces: Vec<Interface>,
}

impl ZeroconfBrowser {
    /// Browses on `ifaces`, or on every multicast interface when empty.
    pub fn new(ifaces: Vec<Interface>) -> Self {
        Self { ifaces }
    }
}

/// Go `browseEntryWithinLimits`.
pub fn browse_entry_within_limits(entry: &ServiceEntry) -> bool {
    if entry.text.len() > MAX_BROWSE_TXT_RECORDS {
        return false;
    }
    let mut total = 0usize;
    for record in &entry.text {
        if record.len() > MAX_TXT_RECORD_BYTES {
            return false;
        }
        total += record.len() + 1;
        if total > MAX_BROWSE_TXT_BYTES {
            return false;
        }
    }
    true
}

#[async_trait]
impl Browser for ZeroconfBrowser {
    /// Standard mDNS browse for `service_type` until `ctx` ends or enough entries were seen.
    async fn browse(&self, ctx: &Ctx, service_type: &str, domain: &str) -> Result<Vec<DiscoveredService>, Error> {
        let domain = if domain.is_empty() { DEFAULT_DOMAIN } else { domain };
        let service_type = if service_type.is_empty() { DEFAULT_SERVICE_TYPE } else { service_type };

        let (tx, mut rx) = mpsc::channel::<ServiceEntry>(32);
        let browse_ctx = ctx.with_cancel();

        // Stops the underlying client after a bounded number of entries, separately from the
        // caller's context, so a LAN flood cannot grow its sent-entries cache for the full browse.
        let consume = async {
            let mut discovered: Vec<DiscoveredService> = Vec::new();
            let mut seen: BTreeMap<String, usize> = BTreeMap::new();
            let mut entries_seen = 0usize;
            while let Some(entry) = rx.recv().await {
                if entries_seen >= MAX_BROWSE_ENTRIES {
                    continue;
                }
                entries_seen += 1;
                if browse_entry_within_limits(&entry) {
                    let svc = entry_to_discovered(&entry);
                    if svc.port != 0 && (!svc.ipv4.is_empty() || !svc.ipv6.is_empty()) {
                        let key = discovered_service_key(&svc);
                        if let Some(&index) = seen.get(&key) {
                            merge_discovered_service(&mut discovered[index], &svc);
                        } else if discovered.len() < MAX_DISCOVERED_SERVICES {
                            seen.insert(key, discovered.len());
                            discovered.push(svc);
                        }
                    }
                }
                if entries_seen == MAX_BROWSE_ENTRIES {
                    browse_ctx.cancel();
                }
            }
            discovered
        };
        let (result, discovered) = tokio::join!(mdns::browse(&browse_ctx, service_type, domain, tx, &self.ifaces), consume);
        match result {
            Ok(()) => Ok(discovered),
            Err(err) => Err(Error::new(format!("discovery: browse query failed: {err}"))),
        }
    }

    /// Discovers all AI gateways on the LAN (`_ai-gateway._tcp`), CPA instances first.
    async fn browse_with_fallback(&self, ctx: &Ctx) -> Result<Vec<DiscoveredService>, Error> {
        self.browse_with_fallback_service_type(ctx, DEFAULT_SERVICE_TYPE).await
    }

    async fn browse_with_fallback_service_type(&self, ctx: &Ctx, service_type: &str) -> Result<Vec<DiscoveredService>, Error> {
        let service_type = if service_type.trim().is_empty() { DEFAULT_SERVICE_TYPE } else { service_type };
        let all = self.browse(ctx, service_type, DEFAULT_DOMAIN).await?;
        let (mut cpa, other): (Vec<_>, Vec<_>) = all.into_iter().partition(|gw| gw.product == PRODUCT_CPA);
        cpa.extend(other);
        Ok(cpa)
    }
}

fn discovered_service_key(svc: &DiscoveredService) -> String {
    [svc.instance_name.as_str(), svc.service_type.as_str(), svc.domain.as_str()].join("\0")
}

/// Go `mergeDiscoveredService`: folds a later sighting of the same instance into `dst`, within
/// the address, metadata and TXT size limits.
pub fn merge_discovered_service(dst: &mut DiscoveredService, src: &DiscoveredService) {
    if !src.host.is_empty() {
        dst.host = src.host.clone();
    }
    if src.port != 0 {
        dst.port = src.port;
    }
    append_unique_ips(&mut dst.ipv4, &src.ipv4);
    append_unique_ips(&mut dst.ipv6, &src.ipv6);
    if !src.product.is_empty() {
        dst.product = src.product.clone();
    }
    if !src.version.is_empty() {
        dst.version = src.version.clone();
    }
    if !src.node_role.is_empty() {
        dst.node_role = src.node_role.clone();
    }
    merge_raw_txt_records(&mut dst.raw_txt, &src.raw_txt);
    if let Some(value) = dst.raw_txt.get("auth_required") {
        dst.auth_required = value.trim().eq_ignore_ascii_case("true");
    }
    append_unique_strings(&mut dst.auth_methods, &src.auth_methods);
    append_unique_strings(&mut dst.protocols, &src.protocols);
    append_unique_strings(&mut dst.features, &src.features);
    for (key, value) in &src.endpoints {
        dst.endpoints.insert(key.clone(), value.clone());
    }
}

pub fn merge_raw_txt_records(dst: &mut BTreeMap<String, String>, src: &BTreeMap<String, String>) {
    let mut total_bytes = raw_txt_map_bytes(dst);
    for (key, value) in src {
        if key.is_empty() {
            continue;
        }
        let mut replaced_bytes = 0;
        if let Some(existing) = dst.get(key) {
            replaced_bytes = raw_txt_record_bytes(key, existing);
        } else if dst.len() >= MAX_BROWSE_TXT_RECORDS {
            continue;
        }
        let next_bytes = total_bytes - replaced_bytes + raw_txt_record_bytes(key, value);
        if next_bytes > MAX_BROWSE_TXT_BYTES {
            continue;
        }
        dst.insert(key.clone(), value.clone());
        total_bytes = next_bytes;
    }
}

pub fn raw_txt_map_bytes(records: &BTreeMap<String, String>) -> usize {
    records.iter().map(|(k, v)| raw_txt_record_bytes(k, v)).sum()
}

fn raw_txt_record_bytes(key: &str, value: &str) -> usize {
    key.len() + value.len() + 1
}

fn append_unique_ips(dst: &mut Vec<IpAddr>, src: &[IpAddr]) {
    for ip in src {
        if dst.len() >= MAX_DISCOVERED_ADDRESSES {
            continue;
        }
        if !dst.iter().any(|existing| existing.to_canonical() == ip.to_canonical()) {
            dst.push(*ip);
        }
    }
}

fn append_unique_strings(dst: &mut Vec<String>, src: &[String]) {
    for value in src {
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_METADATA_ITEM_BYTES || dst.len() >= MAX_DISCOVERED_METADATA_ITEMS {
            continue;
        }
        if !dst.iter().any(|existing| existing == value) {
            dst.push(value.to_string());
        }
    }
}

/// Splits a comma separated TXT list into at most 32 unique, bounded items.
fn parse_txt_list(mut value: &str) -> Vec<String> {
    let mut result = Vec::new();
    while !value.is_empty() && result.len() < MAX_DISCOVERED_METADATA_ITEMS {
        let item = match value.split_once(',') {
            Some((before, after)) => {
                value = after;
                before
            }
            None => {
                let item = value;
                value = "";
                item
            }
        };
        let item = item.trim();
        if item.is_empty() || item.len() > MAX_METADATA_ITEM_BYTES {
            continue;
        }
        append_unique_strings(&mut result, &[item.to_string()]);
    }
    result
}

/// Accepts only safe relative API paths: leading `/`, no scheme, protocol-relative prefix,
/// backslash, traversal or control characters.
pub fn sanitize_endpoint_path(p: &str) -> String {
    let p = p.trim();
    if !p.starts_with('/') || p.starts_with("//") || p.contains('\\') || p.contains("://") || p.contains("..") {
        return String::new();
    }
    if p.chars().any(|c| (c as u32) < 32 || c as u32 == 127) {
        return String::new();
    }
    p.to_string()
}

/// Go `entryToDiscovered`.
pub fn entry_to_discovered(e: &ServiceEntry) -> DiscoveredService {
    let parsed = parse_txt_records(&e.text);
    let port = if (1..=65535).contains(&e.port) { e.port } else { 0 };
    let v4: Vec<IpAddr> = e.addr_ipv4.iter().map(|ip| IpAddr::V4(*ip)).collect();
    let v6: Vec<IpAddr> = e.addr_ipv6.iter().map(|ip| IpAddr::V6(*ip)).collect();
    let get = |key: &str| parsed.get(key).cloned().unwrap_or_default();

    let mut svc = DiscoveredService {
        instance_name: e.record.instance.clone(),
        service_type: e.record.service.clone(),
        domain: e.record.domain.clone(),
        host: e.host_name.clone(),
        port,
        ipv4: filter_usable_ips(&v4),
        ipv6: filter_usable_ips(&v6),
        product: get("product"),
        version: get("version"),
        node_role: get("node_role"),
        raw_txt: parsed.clone(),
        ..DiscoveredService::default()
    };
    if get("auth_required") == "true" {
        svc.auth_required = true;
    }
    let methods = get("auth_methods");
    if !methods.is_empty() {
        svc.auth_methods = parse_txt_list(&methods);
    }
    let protos = get("protocols");
    if !protos.is_empty() {
        svc.protocols = parse_txt_list(&protos);
    }
    let feats = get("features");
    if !feats.is_empty() {
        svc.features = parse_txt_list(&feats);
    }
    for (txt_key, name) in [("api_openai", "openai"), ("api_anthropic", "anthropic"), ("api_gemini", "gemini")] {
        if let Some(v) = parsed.get(txt_key) {
            let clean = sanitize_endpoint_path(v);
            if !clean.is_empty() {
                svc.endpoints.insert(name.to_string(), clean);
            }
        }
    }
    svc
}

/// Drops loopback and unspecified addresses and caps the list at 32.
pub fn filter_usable_ips(ips: &[IpAddr]) -> Vec<IpAddr> {
    ips.iter()
        .filter(|ip| !ip_is_loopback(**ip) && !ip_is_unspecified(**ip))
        .take(MAX_DISCOVERED_ADDRESSES)
        .copied()
        .collect()
}

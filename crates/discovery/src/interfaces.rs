//! Network interface enumeration and LAN filtering (Go: internal/discovery/interfaces.go).

use std::io;
use std::net::IpAddr;

/// Interface prefixes excluded by default (virtual/container/VPN/P2P).
pub const IGNORED_INTERFACE_PREFIXES: &[&str] = &[
    "docker", "veth", "utun", "tailscale", "wg", "tun", "tap", "br-", "cni", "flannel", "virbr", "vmnet", "vboxnet", "awdl", "llw",
];

const PHYSICAL_LAN_PREFIXES: &[&str] = &["en", "eth", "em", "igb", "ix", "re", "wl", "wlan", "wifi", "wi-fi", "ethernet", "bond"];

/// Go `net.Interface` plus its addresses (Go: `iface.Addrs()`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interface {
    pub index: u32,
    pub name: String,
    pub up: bool,
    pub loopback: bool,
    pub point_to_point: bool,
    pub multicast: bool,
    pub addrs: Vec<IpAddr>,
}

/// Go `net.IP.IsLoopback` (IPv4-mapped addresses count as IPv4).
pub(crate) fn ip_is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// Go `net.IP.IsUnspecified`.
pub(crate) fn ip_is_unspecified(ip: IpAddr) -> bool {
    ip.to_canonical().is_unspecified()
}

/// Go `net.Interfaces()`, ordered by index.
#[cfg(unix)]
pub fn list_interfaces() -> io::Result<Vec<Interface>> {
    use nix::ifaddrs::getifaddrs;
    use nix::net::if_::{InterfaceFlags, if_nametoindex};

    let mut out: Vec<Interface> = Vec::new();
    for ifa in getifaddrs().map_err(io::Error::from)? {
        let pos = match out.iter().position(|i| i.name == ifa.interface_name) {
            Some(pos) => pos,
            None => {
                let flags = ifa.flags;
                out.push(Interface {
                    index: if_nametoindex(ifa.interface_name.as_str()).unwrap_or(0),
                    name: ifa.interface_name.clone(),
                    up: flags.contains(InterfaceFlags::IFF_UP),
                    loopback: flags.contains(InterfaceFlags::IFF_LOOPBACK),
                    point_to_point: flags.contains(InterfaceFlags::IFF_POINTOPOINT),
                    multicast: flags.contains(InterfaceFlags::IFF_MULTICAST),
                    addrs: Vec::new(),
                });
                out.len() - 1
            }
        };
        let Some(address) = ifa.address else { continue };
        if let Some(v4) = address.as_sockaddr_in() {
            out[pos].addrs.push(IpAddr::V4(v4.ip()));
        } else if let Some(v6) = address.as_sockaddr_in6() {
            out[pos].addrs.push(IpAddr::V6(v6.ip()));
        }
    }
    out.retain(|i| i.index != 0);
    out.sort_by_key(|i| i.index);
    Ok(out)
}

#[cfg(not(unix))]
pub fn list_interfaces() -> io::Result<Vec<Interface>> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "network interface enumeration is not supported on this platform"))
}

/// Selects qualified physical multicast-capable LAN interfaces. A non-empty `include` accepts only
/// matching names; a non-empty `exclude` drops matching names.
pub fn filter_interfaces(include: &[String], exclude: &[String]) -> io::Result<Vec<Interface>> {
    Ok(filter_interface_list(list_interfaces()?, include, exclude))
}

/// The filtering rules of [`filter_interfaces`] over an explicit interface list.
pub fn filter_interface_list(all: Vec<Interface>, include: &[String], exclude: &[String]) -> Vec<Interface> {
    let mut valid = Vec::new();
    for iface in all {
        // Must be UP, not loopback, not point-to-point, and multicast capable.
        if !iface.up || iface.loopback || iface.point_to_point || !iface.multicast {
            continue;
        }
        let name = iface.name.to_lowercase();
        if !exclude.is_empty() && matches_any(&name, exclude) {
            continue;
        }
        // Default allow-list: common physical LAN adapters unless the user gave an include list.
        if include.is_empty() && (is_virtual_or_tunnel(&name) || !is_likely_physical_lan(&name)) {
            continue;
        }
        if !include.is_empty() && !matches_any(&name, include) {
            continue;
        }
        // Needs at least one usable non-loopback address.
        let has_valid_ip = iface.addrs.iter().any(|ip| !ip_is_loopback(*ip) && !ip_is_unspecified(*ip));
        if has_valid_ip {
            valid.push(iface);
        }
    }
    valid
}

pub(crate) fn is_virtual_or_tunnel(name: &str) -> bool {
    IGNORED_INTERFACE_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

pub(crate) fn is_likely_physical_lan(name: &str) -> bool {
    PHYSICAL_LAN_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Case-insensitive name match; a trailing `*` makes the pattern a prefix.
pub(crate) fn matches_any(name: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        let pattern = pattern.trim().to_lowercase();
        if pattern.is_empty() {
            continue;
        }
        if let Some(prefix) = pattern.strip_suffix('*') {
            if name.starts_with(prefix) {
                return true;
            }
        } else if name == pattern {
            return true;
        }
    }
    false
}

/// Non-loopback addresses of the selected interfaces as strings (Go: `extractInterfaceIPs`).
pub fn extract_interface_ips(ifaces: &[Interface]) -> Vec<String> {
    ifaces
        .iter()
        .flat_map(|iface| iface.addrs.iter())
        .filter(|ip| !ip_is_loopback(**ip) && !ip_is_unspecified(**ip))
        .map(|ip| ip.to_string())
        .collect()
}

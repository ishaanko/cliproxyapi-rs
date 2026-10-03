//! Local network mDNS / DNS-SD advertising and discovery (Go: internal/discovery, plus the
//! `-discover` scan in internal/cmd/discover.go).
//!
//! - [`service`] builds a [`ServiceSpec`] from the config (instance ID, name, subtypes, TXT).
//! - [`ZeroconfAdvertiser`] publishes it; [`ZeroconfBrowser`] scans for `_ai-gateway._tcp`.
//! - [`scan`] is the `-discover` / `-discover-json` command.
//!
//! The mDNS engine (`dns`, `net`, `mdns`) is a port of libp2p/zeroconf and miekg/dns rather than a
//! wrapper over another mDNS crate, because the Go build's observable quirks (presentation-format
//! name escaping, probe and announce timers, subtype handling) are part of the contract. It is
//! Unix only; other platforms get stubs that report an error.

pub mod ctx;
pub mod dns;
pub mod id;
pub mod interfaces;
pub mod scan;
pub mod service;
pub mod txt;
pub mod types;

#[cfg(unix)]
pub mod mdns;
#[cfg(unix)]
mod net;
#[cfg(unix)]
pub mod zeroconf;
#[cfg(not(unix))]
#[path = "zeroconf_stub.rs"]
pub mod zeroconf;

pub use ctx::{Ctx, CtxError};
pub use id::{DEFAULT_INSTANCE_PREFIX, format_instance_name, get_or_generate_instance_id, reset_cached_instance_id};
pub use interfaces::{IGNORED_INTERFACE_PREFIXES, Interface, filter_interfaces};
pub use service::{build_service_spec, resolve_discovery_state_dir};
pub use txt::{TxtOptions, build_txt_records, parse_txt_records};
pub use types::*;
pub use zeroconf::{ZeroconfAdvertiser, ZeroconfBrowser};

#[cfg(all(test, unix))]
mod tests;

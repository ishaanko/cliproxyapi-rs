//! The `-discover` / `-discover-json` LAN scan command (Go: internal/cmd/discover.go).

use std::collections::HashSet;
use std::io::Write;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::ctx::Ctx;
use crate::interfaces::{filter_interfaces, ip_is_loopback, ip_is_unspecified};
use crate::types::{Browser, DEFAULT_SERVICE_TYPE, DiscoveredService, Error};

/// Options of a one-shot LAN discovery scan.
#[derive(Debug, Clone, Default)]
pub struct DiscoverOptions {
    pub timeout: Duration,
    pub json_output: bool,
    pub service_type: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// Builds the real browser over the filtered LAN interfaces (Go: `newLANBrowser`).
fn new_lan_browser(include: &[String], exclude: &[String]) -> Result<Box<dyn Browser>, String> {
    let ifaces = filter_interfaces(include, exclude).map_err(|e| e.to_string())?;
    if ifaces.is_empty() {
        return Err("no qualified physical interfaces found for LAN discovery".to_string());
    }
    Ok(Box::new(crate::zeroconf::ZeroconfBrowser::new(ifaces)))
}

/// Splits comma-separated interface names, dropping blanks and duplicates (Go: `ParseInterfaceList`).
pub fn parse_interface_list<S: AsRef<str>>(values: &[S]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in values {
        for part in raw.as_ref().split(',') {
            let part = part.trim();
            if part.is_empty() || !seen.insert(part.to_string()) {
                continue;
            }
            out.push(part.to_string());
        }
    }
    out
}

/// Explicit CLI filters win over config filters.
pub fn resolve_discovery_interface_filters(
    cli_include: &[String],
    cli_exclude: &[String],
    cfg_include: &[String],
    cfg_exclude: &[String],
) -> (Vec<String>, Vec<String>) {
    if !cli_include.is_empty() || !cli_exclude.is_empty() {
        return (cli_include.to_vec(), cli_exclude.to_vec());
    }
    (cfg_include.to_vec(), cfg_exclude.to_vec())
}

#[derive(Deserialize, Default)]
struct ScanConfig {
    #[serde(default)]
    discovery: ScanDiscovery,
}

#[derive(Deserialize, Default)]
struct ScanDiscovery {
    #[serde(default)]
    interfaces: ScanInterfaces,
}

#[derive(Deserialize, Default)]
struct ScanInterfaces {
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

/// Reads `discovery.interfaces` from a config file. Missing or invalid files yield empty filters
/// so the default physical LAN allow-list applies; unrelated config problems are ignored.
pub fn load_discovery_scan_filters(config_path: &str) -> (Vec<String>, Vec<String>) {
    let config_path = config_path.trim();
    let path = if config_path.is_empty() {
        match std::env::current_dir() {
            Ok(wd) => wd.join("config.yaml"),
            Err(_) => PathBuf::from("config.yaml"),
        }
    } else {
        PathBuf::from(config_path)
    };
    let Ok(raw) = std::fs::read_to_string(&path) else { return (Vec::new(), Vec::new()) };
    let Ok(parsed) = serde_yaml_ng::from_str::<Option<ScanConfig>>(&raw) else { return (Vec::new(), Vec::new()) };
    let interfaces = parsed.unwrap_or_default().discovery.interfaces;
    (parse_interface_list(&interfaces.include), parse_interface_list(&interfaces.exclude))
}

/// Runs the scan against the real LAN, printing to stdout/stderr. Returns the exit code.
pub async fn do_discover_with_options(opts: DiscoverOptions) -> i32 {
    let (include, exclude) = resolve_discovery_interface_filters(&opts.include, &opts.exclude, &[], &[]);
    run_discover_with_options(&opts, &mut std::io::stdout(), &mut std::io::stderr(), || new_lan_browser(&include, &exclude)).await
}

/// Go `time.Duration.String` for the whole-second timeouts used here.
fn go_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// Go `json.MarshalIndent(v, "", "  ")`: two-space indent, with `<`, `>`, `&` and the Unicode
/// line separators escaped the way `encoding/json` does by default.
fn go_json_indent(value: &impl serde::Serialize) -> Result<String, serde_json::Error> {
    let out = serde_json::to_string_pretty(value)?;
    Ok(out
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

fn print_json_error(stdout: &mut dyn Write, message: &str) {
    let payload = serde_json::json!({"error": message, "gateways": []});
    if let Ok(out) = go_json_indent(&payload) {
        let _ = writeln!(stdout, "{out}");
    }
}

/// Go `runDiscoverWithOptions`: scans with the browser from `new_browser` and prints the result.
/// Returns 0 on success and 1 on error.
pub async fn run_discover_with_options<F>(opts: &DiscoverOptions, stdout: &mut dyn Write, stderr: &mut dyn Write, new_browser: F) -> i32
where
    F: FnOnce() -> Result<Box<dyn Browser>, String>,
{
    let mut timeout = opts.timeout;
    if timeout.is_zero() {
        timeout = Duration::from_secs(3);
    } else if timeout > Duration::from_secs(60) {
        timeout = Duration::from_secs(60);
    }
    let service_type = opts.service_type.trim();
    let service_type = if service_type.is_empty() { DEFAULT_SERVICE_TYPE } else { service_type };
    let json_output = opts.json_output;

    if !json_output {
        let _ = writeln!(stdout, "Scanning LAN for AI Gateways ({service_type})... (timeout {})", go_duration(timeout));
    }

    let ctx = Ctx::background().with_timeout(timeout);

    let browser = match new_browser() {
        Ok(b) => b,
        Err(err) => {
            report_error(json_output, stdout, stderr, &err);
            return 1;
        }
    };
    let gateways = match browser.browse_with_fallback_service_type(&ctx, service_type).await {
        Ok(g) => g,
        Err(Error(err)) => {
            report_error(json_output, stdout, stderr, &err);
            return 1;
        }
    };

    if json_output {
        return match go_json_indent(&gateways) {
            Ok(out) => {
                let _ = writeln!(stdout, "{out}");
                0
            }
            Err(err) => {
                let _ = writeln!(stderr, "failed to marshal results: {err}");
                1
            }
        };
    }

    if gateways.is_empty() {
        let _ = writeln!(stdout, "\nNo AI gateways found on local network.");
        let _ = writeln!(stdout, "Tips:");
        let _ = writeln!(stdout, "  1. Ensure the target CPA instance has 'discovery.enabled: true' in its config.yaml.");
        let _ = writeln!(stdout, "  2. Ensure your device is on the same local Wi-Fi / Ethernet subnet (mDNS does not traverse WAN).");
        let _ = writeln!(stdout, "  3. Check that your local firewall allows UDP port 5353 multicast traffic.");
        return 0;
    }

    print_gateways(stdout, &gateways);
    0
}

fn report_error(json_output: bool, stdout: &mut dyn Write, stderr: &mut dyn Write, message: &str) {
    if json_output {
        print_json_error(stdout, message);
    } else {
        let _ = writeln!(stderr, "Error scanning LAN: {message}");
    }
}

fn join_host_port(host: &str, port: i64) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

fn print_gateways(stdout: &mut dyn Write, gateways: &[DiscoveredService]) {
    let _ = write!(stdout, "\nFound {} AI Gateway(s) on local network:\n\n", gateways.len());
    for (i, gw) in gateways.iter().enumerate() {
        let mut product_label = sanitize_terminal(&gw.product);
        if product_label.is_empty() {
            product_label = "generic".into();
        }
        let mut version_label = sanitize_terminal(&gw.version);
        if version_label.is_empty() {
            version_label = "unknown".into();
        }
        let instance_label = sanitize_terminal(&gw.instance_name);
        let _ = writeln!(stdout, "[{}] {instance_label} (Product: {product_label}, Version: {version_label})", i + 1);

        let (primary_ip, all_ips) = preferred_display_addresses(gw);
        let host_port = join_host_port(&primary_ip, gw.port);
        let host_label = sanitize_terminal(&gw.host);
        let _ = writeln!(stdout, "    Host:      {host_label} ({host_port})");
        if all_ips.len() > 1 {
            let _ = writeln!(stdout, "    Addresses: {}", all_ips.join(", "));
        }

        let joined = |items: &[String]| items.iter().map(|s| sanitize_terminal(s)).collect::<Vec<_>>().join(", ");
        if !gw.protocols.is_empty() {
            let _ = writeln!(stdout, "    Protocols: {}", joined(&gw.protocols));
        }
        if !gw.features.is_empty() {
            let _ = writeln!(stdout, "    Features:  {}", joined(&gw.features));
        }

        let mut auth_status = "No".to_string();
        if gw.auth_required {
            auth_status = "Required".into();
            if !gw.auth_methods.is_empty() {
                auth_status = format!("Required ({})", joined(&gw.auth_methods));
            }
        }
        let _ = writeln!(stdout, "    Auth:      {auth_status}");

        let scheme = if gw.raw_txt.get("tls").is_some_and(|v| v == "1") { "https" } else { "http" };
        let _ = writeln!(stdout, "    Base URLs:");
        match gw.endpoints.get("openai") {
            Some(path) => {
                let _ = writeln!(stdout, "      - OpenAI:    {scheme}://{host_port}{}", sanitize_terminal(path));
            }
            None => {
                let _ = writeln!(stdout, "      - OpenAI:    {scheme}://{host_port}/v1");
            }
        }
        if let Some(path) = gw.endpoints.get("anthropic") {
            let _ = writeln!(stdout, "      - Anthropic: {scheme}://{host_port}{}", sanitize_terminal(path));
        }
        if let Some(path) = gw.endpoints.get("gemini") {
            let _ = writeln!(stdout, "      - Gemini:    {scheme}://{host_port}{}", sanitize_terminal(path));
        }
        let _ = writeln!(stdout);
    }
}

fn is_link_local_unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Picks the address to print as the connect target and the list of all usable addresses:
/// IPv4 first, then routable IPv6, then a sanitized hostname, then link-local IPv6.
pub fn preferred_display_addresses(gw: &DiscoveredService) -> (String, Vec<String>) {
    let all: Vec<String> =
        gw.ipv4.iter().filter(|ip| !ip_is_loopback(**ip) && !ip_is_unspecified(**ip)).map(|ip| ip.to_string()).collect();
    if let Some(first) = all.first() {
        return (first.clone(), all);
    }

    let mut routable = Vec::new();
    let mut link_local = Vec::new();
    for ip in &gw.ipv6 {
        if ip_is_loopback(*ip) || ip_is_unspecified(*ip) {
            continue;
        }
        if is_link_local_unicast(*ip) {
            link_local.push(ip.to_string());
        } else {
            routable.push(ip.to_string());
        }
    }
    if let Some(first) = routable.first() {
        let primary = first.clone();
        routable.extend(link_local);
        return (primary, routable);
    }

    let host = sanitize_display_host(&gw.host);
    if !host.is_empty() {
        let mut all = vec![host.clone()];
        all.extend(link_local);
        return (host, all);
    }
    if let Some(first) = link_local.first() {
        return (first.clone(), link_local);
    }
    ("127.0.0.1".to_string(), Vec::new())
}

/// A hostname safe to print as a connect target, or "" when it is empty, `localhost` or malformed.
fn sanitize_display_host(host: &str) -> String {
    let host = host.strip_suffix('.').unwrap_or(host);
    let host = sanitize_terminal(host.trim());
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return String::new();
    }
    if !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
        return String::new();
    }
    if host.contains("..") || host.starts_with('-') || host.starts_with('.') || host.ends_with('-') || host.ends_with('.') {
        return String::new();
    }
    host
}

/// Characters Go's `unicode.IsPrint` rejects that this port cannot derive from `char` methods:
/// format controls (Cf), the private-use area and line/paragraph separators.
fn is_non_printable_format(c: char) -> bool {
    let cp = c as u32;
    matches!(cp,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x08E2 | 0x180E | 0x200B..=0x200F | 0x2028..=0x202E
        | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F
        | 0xE000..=0xF8FF | 0xF0000..=0x10FFFF)
}

/// Strips control characters, ANSI escapes, Bidi overrides and invisible format characters to
/// prevent terminal injection and spoofing. Tabs become spaces.
pub fn sanitize_terminal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\t' {
            out.push(' ');
            continue;
        }
        // unicode.IsPrint allows only the ASCII space among space characters.
        if c.is_control() || (c.is_whitespace() && c != ' ') || is_non_printable_format(c) {
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct FakeBrowser {
        result: Vec<DiscoveredService>,
        service_type: Mutex<String>,
        deadline: Mutex<Option<Duration>>,
    }

    #[async_trait]
    impl Browser for std::sync::Arc<FakeBrowser> {
        async fn browse(&self, _: &Ctx, _: &str, _: &str) -> Result<Vec<DiscoveredService>, Error> {
            Ok(self.result.clone())
        }
        async fn browse_with_fallback(&self, _: &Ctx) -> Result<Vec<DiscoveredService>, Error> {
            Ok(self.result.clone())
        }
        async fn browse_with_fallback_service_type(&self, ctx: &Ctx, service_type: &str) -> Result<Vec<DiscoveredService>, Error> {
            *self.service_type.lock() = service_type.to_string();
            *self.deadline.lock() = ctx.time_remaining();
            Ok(self.result.clone())
        }
    }

    async fn run(opts: DiscoverOptions, browser: Result<std::sync::Arc<FakeBrowser>, String>) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_discover_with_options(&opts, &mut out, &mut err, || browser.map(|b| Box::new(b) as Box<dyn Browser>)).await;
        (code, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned())
    }

    fn json_opts(timeout: Duration) -> DiscoverOptions {
        DiscoverOptions { timeout, json_output: true, ..Default::default() }
    }

    #[tokio::test]
    async fn json_empty_prints_empty_array() {
        let (code, out, err) = run(json_opts(Duration::ZERO), Ok(Default::default())).await;
        assert_eq!(code, 0);
        assert!(err.is_empty());
        let got: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn custom_service_type_reaches_browser() {
        let browser = std::sync::Arc::new(FakeBrowser::default());
        let opts = DiscoverOptions { service_type: "_custom._tcp".into(), ..json_opts(Duration::from_secs(2)) };
        let (code, _, _) = run(opts, Ok(browser.clone())).await;
        assert_eq!(code, 0);
        assert_eq!(*browser.service_type.lock(), "_custom._tcp");
    }

    #[tokio::test]
    async fn uses_exact_timeout() {
        let browser = std::sync::Arc::new(FakeBrowser::default());
        let (code, _, _) = run(json_opts(Duration::from_secs(2)), Ok(browser.clone())).await;
        assert_eq!(code, 0);
        let remaining = browser.deadline.lock().unwrap_or_default();
        assert!(remaining > Duration::from_millis(1500) && remaining <= Duration::from_millis(2500), "{remaining:?}");
    }

    #[test]
    fn cli_filters_win_over_config() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (include, exclude) = resolve_discovery_interface_filters(&s(&["docker0"]), &[], &s(&["en0"]), &s(&["awdl0"]));
        assert_eq!((include, exclude), (s(&["docker0"]), vec![]));
        let (include, exclude) = resolve_discovery_interface_filters(&[], &[], &s(&["docker0"]), &s(&["veth0"]));
        assert_eq!((include, exclude), (s(&["docker0"]), s(&["veth0"])));
    }

    #[test]
    fn scan_filters_ignore_unrelated_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "redis-usage-queue-retention-seconds: 99999\ndiscovery:\n  interfaces:\n    include:\n      - docker0\n").unwrap();
        let (include, exclude) = load_discovery_scan_filters(path.to_str().unwrap());
        assert_eq!(include, vec!["docker0".to_string()]);
        assert!(exclude.is_empty());
    }

    #[test]
    fn scan_filters_empty_path_uses_working_config() {
        // Reads ./config.yaml of the process; only checks that a missing/foreign file never panics.
        let (include, exclude) = load_discovery_scan_filters("/nonexistent/config.yaml");
        assert!(include.is_empty() && exclude.is_empty());
    }

    #[test]
    fn display_addresses_skip_bare_link_local() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let gw = DiscoveredService { host: "gateway.local.".into(), ipv6: vec![ip("fe80::1")], ..Default::default() };
        let (primary, all) = preferred_display_addresses(&gw);
        assert_eq!(primary, "gateway.local");
        assert_eq!(all, vec!["gateway.local".to_string(), "fe80::1".to_string()]);

        let gw = DiscoveredService { host: "\x1b[31mevil.local.".into(), ipv6: vec![ip("fe80::1")], ..Default::default() };
        let (primary, all) = preferred_display_addresses(&gw);
        assert_eq!(primary, "fe80::1");
        assert!(!primary.contains('\x1b') && !all.join(",").contains("evil"));

        let gw = DiscoveredService { ipv6: vec![ip("fe80::1"), ip("2001:db8::10")], ..Default::default() };
        assert_eq!(preferred_display_addresses(&gw).0, "2001:db8::10");
    }

    #[tokio::test]
    async fn json_error_has_error_field() {
        let (code, out, _) = run(json_opts(Duration::from_secs(2)), Err("no qualified physical interfaces found for LAN discovery".into())).await;
        assert_eq!(code, 1);
        let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(payload.get("error").is_some());
    }

    #[tokio::test]
    async fn clamps_timeout_and_prints_text() {
        let browser = std::sync::Arc::new(FakeBrowser {
            result: vec![DiscoveredService {
                instance_name: "office-gateway".into(),
                product: "cliproxyapi".into(),
                version: "1".into(),
                port: 8317,
                ipv4: vec!["192.0.2.10".parse().unwrap()],
                endpoints: [("openai".to_string(), "/v1".to_string())].into(),
                raw_txt: [("tls".to_string(), "0".to_string())].into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let opts = DiscoverOptions { timeout: Duration::from_secs(120), ..Default::default() };
        let (code, out, _) = run(opts, Ok(browser)).await;
        assert_eq!(code, 0);
        assert!(out.contains("timeout 1m0s"), "{out}");
        assert!(out.contains("office-gateway"), "{out}");
        assert!(out.contains("http://192.0.2.10:8317/v1"), "{out}");
    }

    #[test]
    fn json_escapes_html_like_go() {
        let gw = DiscoveredService { instance_name: "a<b>&c".into(), ..Default::default() };
        let out = go_json_indent(&vec![gw]).unwrap();
        assert!(out.contains("a\\u003cb\\u003e\\u0026c"), "{out}");
    }
}

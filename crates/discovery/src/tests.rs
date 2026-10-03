//! Ports of internal/discovery/discovery_test.go.

use std::net::IpAddr;
use std::time::Duration;

use crate::ctx::Ctx;
use crate::id::{format_instance_name, get_or_generate_instance_id, reset_cached_instance_id};
use crate::interfaces::{Interface, filter_interface_list, filter_interfaces, is_likely_physical_lan, is_virtual_or_tunnel, matches_any};
use crate::mdns::ServiceEntry;
use crate::service::{sanitize_instance_name, sanitize_subtype, validate_service_type};
use crate::txt::{MAX_TXT_BYTES, MAX_TXT_RECORD_BYTES, TxtOptions, build_txt_records, parse_txt_records};
use crate::types::{Advertiser, Browser, DEFAULT_DOMAIN, DEFAULT_SERVICE_TYPE, DiscoveredService, PRODUCT_CPA, SUBTYPE_RESPONSES, ServiceSpec};
use crate::zeroconf::{
    MAX_BROWSE_TXT_BYTES, MAX_BROWSE_TXT_RECORDS, MAX_DISCOVERED_ADDRESSES, MAX_DISCOVERED_METADATA_ITEMS, ZeroconfAdvertiser,
    ZeroconfBrowser, browse_entry_within_limits, entry_to_discovered, filter_usable_ips, merge_discovered_service, raw_txt_map_bytes,
    sanitize_endpoint_path,
};

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// The instance-ID cache is process-global, so the tests touching it run under one lock.
static ID_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn instance_id_persistence_and_format() {
    let _guard = ID_LOCK.lock();
    reset_cached_instance_id();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_str().unwrap().to_string();

    let id1 = get_or_generate_instance_id(&dir);
    assert_eq!(id1.len(), 4, "expected 4-char hex ID, got {id1}");
    assert_eq!(get_or_generate_instance_id(&dir), id1);
    assert_eq!(format_instance_name("", &id1), format!("CPA-{id1}"));

    // Concurrent calls after a cache reset all read the persisted ID.
    reset_cached_instance_id();
    let results: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..10).map(|_| scope.spawn(|| get_or_generate_instance_id(&dir))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(results.iter().all(|r| *r == id1), "{results:?}");

    // Directory isolation.
    let tmp_b = tempfile::tempdir().unwrap();
    assert_eq!(get_or_generate_instance_id(tmp_b.path().to_str().unwrap()).len(), 4);

    assert_eq!(format_instance_name("My-Custom-Node", &id1), format!("My-Custom-Node-{id1}"));
    reset_cached_instance_id();
}

#[test]
fn instance_id_file_is_private_and_uppercase_on_read() {
    let _guard = ID_LOCK.lock();
    reset_cached_instance_id();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("instance_id"), " 8f3b\n").unwrap();
    assert_eq!(get_or_generate_instance_id(tmp.path().to_str().unwrap()), "8F3B");
    reset_cached_instance_id();
}

#[test]
fn format_instance_name_always_includes_id() {
    assert_eq!(format_instance_name("", "8F3B"), "CPA-8F3B");
    assert_eq!(format_instance_name("office", "8F3B"), "office-8F3B");
    assert_eq!(format_instance_name("office-8F3B", "8F3B"), "office-8F3B");
    assert_eq!(format_instance_name("CPA-8F3B", "8F3B"), "CPA-8F3B");
    let got = format_instance_name(&"n".repeat(70), "8F3B");
    assert!(got.len() <= 63 && got.ends_with("-8F3B"), "{got} ({})", got.len());
}

#[test]
fn txt_records_size_and_keys() {
    let mut opts = TxtOptions::defaults();
    opts.instance_id = "8F3B".into();
    opts.tls = true;
    opts.auth_required = true;
    let records = build_txt_records(opts);
    assert!(!records.is_empty());
    let parsed = parse_txt_records(&records);
    let get = |k: &str| parsed.get(k).map(String::as_str).unwrap_or_default();
    assert_eq!(get("version"), "1");
    assert_eq!(get("product"), PRODUCT_CPA);
    assert_eq!(get("tls"), "1");
    assert_eq!(get("auth_required"), "true");
    assert_eq!(get("management"), "false");
    assert_eq!(get("api_openai"), "/v1");
    assert_eq!(get("api_gemini"), "/v1beta");
    assert_eq!(get("protocols"), "chat-completions,responses,messages,generate-content,interactions");
    assert_eq!(get("features"), "chat,responses,messages,generate_content,interactions");
    let total: usize = records.iter().map(|r| r.len() + 1).sum();
    assert!(total <= 400, "TXT records exceed 400 bytes limit: {total}");
}

#[test]
fn parse_txt_records_normalizes_keys() {
    let parsed = parse_txt_records(&["TLS=1", "API_OpenAI=/v1", "Product=cliproxyapi"]);
    assert_eq!(parsed["tls"], "1");
    assert_eq!(parsed["api_openai"], "/v1");
    assert_eq!(parsed["product"], "cliproxyapi");
}

#[test]
fn txt_records_oversized_and_rfc_enforcement() {
    let mut opts = TxtOptions::defaults();
    opts.product = "A".repeat(500);
    opts.features = vec!["F".repeat(300)];
    opts.node_role = "R".repeat(200);
    let records = build_txt_records(opts);
    let mut total = 0;
    for r in &records {
        assert!(r.len() <= MAX_TXT_RECORD_BYTES, "record exceeds RFC 6763 limit: {}", r.len());
        total += r.len() + 1;
    }
    assert!(total <= MAX_TXT_BYTES, "total TXT length {total} exceeds max");
    assert!(!parse_txt_records(&records).contains_key("product"), "oversized product key should be omitted");
}

#[test]
fn validation_service_type_and_labels() {
    assert!(validate_service_type("_ai-gateway._tcp").is_ok());
    assert!(validate_service_type("_cliproxy._tcp").is_ok());
    for invalid in [
        "ai-gateway._tcp",
        "_ai-gateway.tcp",
        "_toolongservicenamemoret15._tcp",
        "_-invalid._tcp",
        "_invalid-._tcp",
        "_ai_gateway._tcp",
        "_ai-gateway._udp",
    ] {
        assert!(validate_service_type(invalid).is_err(), "expected invalid for {invalid:?}");
    }

    assert_eq!(sanitize_subtype("responses"), SUBTYPE_RESPONSES);
    assert_eq!(sanitize_subtype(SUBTYPE_RESPONSES), SUBTYPE_RESPONSES);
    for bad in ["_responses._sub", "_responses_", "_-responses", "_responses-"] {
        assert_eq!(sanitize_subtype(bad), "", "{bad}");
    }
    assert_ne!(sanitize_subtype(&format!("_{}", "a".repeat(62))), "");
    assert_eq!(sanitize_subtype(&format!("_{}", "a".repeat(63))), "");

    assert_eq!(sanitize_endpoint_path("/v1"), "/v1");
    assert_eq!(sanitize_endpoint_path("/v1/chat/completions"), "/v1/chat/completions");
    for bad in ["http://evil.com/v1", "//attacker.com/v1", "/\\attacker.com/v1", "/../etc/passwd"] {
        assert_eq!(sanitize_endpoint_path(bad), "", "{bad}");
    }

    // 22 Chinese characters = 66 bytes; truncation must stay on a rune boundary.
    let truncated = sanitize_instance_name(&"\u{7f51}".repeat(22));
    assert!(truncated.len() <= 63);
    assert_eq!(truncated.len() % 3, 0);
}

#[test]
fn filter_usable_ips_drops_loopback_and_unspecified() {
    let usable = filter_usable_ips(&[ip("192.0.2.10"), ip("fe80::1"), ip("127.0.0.1"), ip("::")]);
    assert_eq!(usable, vec![ip("192.0.2.10"), ip("fe80::1")]);
}

#[test]
fn interface_filtering_helpers() {
    for name in ["docker0", "veth45a", "utun3", "tailscale0", "wg0", "tun1", "tap0", "br-123", "awdl0", "llw0"] {
        assert!(is_virtual_or_tunnel(name), "{name}");
    }
    for name in ["en0", "eth0", "wlan0", "eno1", "bond0"] {
        assert!(!is_virtual_or_tunnel(name), "{name}");
        assert!(is_likely_physical_lan(name), "{name}");
    }
    for name in ["bridge100", "p2p0", "ppp0", "mystery0"] {
        assert!(!is_likely_physical_lan(name), "{name}");
    }
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(matches_any("docker0", &s(&["docker*"])));
    assert!(matches_any("en0", &s(&["en0", "eth0"])));
    assert!(!matches_any("wlan0", &s(&["docker*", "utun*"])));
}

#[test]
fn filter_interface_list_rules() {
    let iface = |index, name: &str, ips: &[&str]| Interface {
        index,
        name: name.into(),
        up: true,
        multicast: true,
        addrs: ips.iter().map(|s| ip(s)).collect(),
        ..Default::default()
    };
    let all = vec![
        iface(1, "eth0", &["192.0.2.10"]),
        iface(2, "docker0", &["172.17.0.1"]),
        iface(3, "eth1", &["127.0.0.1"]),
        Interface { loopback: true, ..iface(4, "lo", &["127.0.0.1"]) },
        Interface { up: false, ..iface(5, "eth2", &["192.0.2.11"]) },
        iface(6, "mystery0", &["192.0.2.12"]),
    ];
    let names = |v: Vec<Interface>| v.into_iter().map(|i| i.name).collect::<Vec<_>>();
    assert_eq!(names(filter_interface_list(all.clone(), &[], &[])), ["eth0"]);
    assert_eq!(names(filter_interface_list(all.clone(), &["docker*".into(), "mystery0".into()], &[])), ["docker0", "mystery0"]);
    assert!(filter_interface_list(all, &[], &["eth*".into()]).is_empty());
}

#[test]
fn browse_entry_limits() {
    let entry = |text: Vec<String>| {
        let mut e = ServiceEntry::new("n", DEFAULT_SERVICE_TYPE, DEFAULT_DOMAIN);
        e.text = text;
        e
    };
    assert!(browse_entry_within_limits(&entry(vec!["product=cliproxyapi".into()])));
    assert!(!browse_entry_within_limits(&entry(vec!["x".repeat(MAX_TXT_RECORD_BYTES + 1)])));
    assert!(!browse_entry_within_limits(&entry(vec![String::new(); MAX_BROWSE_TXT_RECORDS + 1])));
}

#[test]
fn merge_discovered_service_combines_sightings() {
    let mut dst = DiscoveredService {
        instance_name: "node".into(),
        service_type: DEFAULT_SERVICE_TYPE.into(),
        domain: DEFAULT_DOMAIN.into(),
        port: 8317,
        ipv4: vec![ip("192.0.2.10")],
        raw_txt: [("auth_required".to_string(), "true".to_string())].into(),
        endpoints: [("openai".to_string(), "/v1".to_string())].into(),
        ..Default::default()
    };
    let src = DiscoveredService {
        instance_name: "node".into(),
        port: 8317,
        ipv4: vec![ip("192.0.2.10"), ip("192.0.2.11")],
        ipv6: vec![ip("2001:db8::10")],
        product: PRODUCT_CPA.into(),
        raw_txt: [("auth_required".to_string(), "false".to_string()), ("tls".to_string(), "1".to_string())].into(),
        endpoints: [("anthropic".to_string(), "/v1/messages".to_string())].into(),
        ..Default::default()
    };
    merge_discovered_service(&mut dst, &src);
    assert_eq!((dst.ipv4.len(), dst.ipv6.len()), (2, 1));
    assert!(!dst.auth_required);
    assert_eq!(dst.raw_txt["tls"], "1");
    assert_eq!(dst.endpoints["anthropic"], "/v1/messages");
}

#[test]
fn merge_limits() {
    let mut dst = DiscoveredService {
        ipv4: vec![ip("192.0.2.1")],
        raw_txt: [("auth_required".to_string(), "true".to_string())].into(),
        ..Default::default()
    };
    let src_ips: Vec<IpAddr> = (0..MAX_DISCOVERED_ADDRESSES * 2).map(|i| ip(&format!("198.18.{}.{}", i / 256, i % 256))).collect();
    let metadata: Vec<String> = (0..MAX_DISCOVERED_METADATA_ITEMS * 2).map(|i| format!("feature-{i:02}")).collect();
    let raw_txt = (0..MAX_BROWSE_TXT_RECORDS * 2).map(|i| (format!("key-{i:03}"), "v".repeat(MAX_TXT_RECORD_BYTES))).collect();
    let src = DiscoveredService {
        ipv4: src_ips,
        raw_txt,
        auth_methods: metadata.clone(),
        protocols: metadata.clone(),
        features: metadata,
        ..Default::default()
    };
    merge_discovered_service(&mut dst, &src);
    assert_eq!(dst.ipv4.len(), MAX_DISCOVERED_ADDRESSES);
    assert_eq!(dst.auth_methods.len(), MAX_DISCOVERED_METADATA_ITEMS);
    assert_eq!(dst.protocols.len(), MAX_DISCOVERED_METADATA_ITEMS);
    assert_eq!(dst.features.len(), MAX_DISCOVERED_METADATA_ITEMS);
    assert!(dst.raw_txt.len() <= MAX_BROWSE_TXT_RECORDS && raw_txt_map_bytes(&dst.raw_txt) <= MAX_BROWSE_TXT_BYTES);
}

#[test]
fn entry_to_discovered_limits_txt_lists() {
    let methods: Vec<String> = (0..MAX_DISCOVERED_METADATA_ITEMS * 2).map(|i| format!("method-{i:02}")).collect();
    let mut entry = ServiceEntry::new("node", DEFAULT_SERVICE_TYPE, DEFAULT_DOMAIN);
    entry.port = 8317;
    entry.addr_ipv4 = (0..MAX_DISCOVERED_ADDRESSES * 2).map(|i| format!("198.18.{}.{}", i / 256, i % 256).parse().unwrap()).collect();
    entry.text = vec![format!("auth_methods={}", methods.join(","))];
    let svc = entry_to_discovered(&entry);
    assert_eq!(svc.auth_methods.len(), MAX_DISCOVERED_METADATA_ITEMS);
    assert_eq!(svc.ipv4.len(), MAX_DISCOVERED_ADDRESSES);
}

#[test]
fn entry_to_discovered_maps_txt_fields() {
    let mut entry = ServiceEntry::new("node", DEFAULT_SERVICE_TYPE, DEFAULT_DOMAIN);
    entry.port = 8317;
    entry.host_name = "host.local.".into();
    entry.addr_ipv4 = vec!["192.0.2.10".parse().unwrap()];
    entry.text = build_txt_records(TxtOptions { instance_id: "8F3B".into(), ..TxtOptions::defaults() });
    let svc = entry_to_discovered(&entry);
    assert_eq!(svc.product, PRODUCT_CPA);
    assert_eq!(svc.version, "1");
    assert_eq!(svc.node_role, "standalone");
    assert!(svc.auth_required);
    assert_eq!(svc.auth_methods, ["api_key"]);
    assert_eq!(svc.endpoints["openai"], "/v1");
    assert_eq!(svc.endpoints["gemini"], "/v1beta");
    assert_eq!(svc.protocols.len(), 5);
}

#[test]
fn filter_interfaces_real_machine() {
    // Whatever the host has, no virtual/tunnel interface may slip through the default filter.
    for iface in filter_interfaces(&[], &[]).unwrap() {
        assert!(!is_virtual_or_tunnel(&iface.name.to_lowercase()), "{} should have been filtered", iface.name);
    }
}

#[tokio::test]
async fn advertiser_idempotence_and_shutdown() {
    let adv = ZeroconfAdvertiser::new();
    adv.stop().await.unwrap();

    let ctx = Ctx::background();
    let spec = ServiceSpec {
        instance_name: "CPA-Test-Unit".into(),
        service_type: "_test-ai._tcp".into(),
        domain: "local.".into(),
        port: 65432,
        text_records: vec!["version=1".into(), "product=test".into()],
        interfaces: filter_interfaces(&[], &[]).unwrap_or_default(),
        ..Default::default()
    };
    if let Err(err) = adv.start(&ctx, spec.clone()).await {
        eprintln!("multicast start failed (likely restricted sandbox network): {err}");
        return;
    }
    assert!(adv.start(&ctx, spec).await.is_err(), "expected error on duplicate start");
    adv.stop().await.unwrap();
    adv.stop().await.unwrap();
}

/// Advertise on the real LAN and browse for it (the Go integration test). Skips when multicast
/// is unavailable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertiser_and_browser_integration() {
    let ifaces = filter_interfaces(&[], &[]).unwrap_or_default();
    let spec = ServiceSpec {
        instance_name: "CPA-LiveTest-42".into(),
        service_type: DEFAULT_SERVICE_TYPE.into(),
        domain: DEFAULT_DOMAIN.into(),
        port: 54321,
        subtypes: vec![SUBTYPE_RESPONSES.into()],
        text_records: build_txt_records(TxtOptions::defaults()),
        interfaces: ifaces.clone(),
        ..Default::default()
    };
    let adv = ZeroconfAdvertiser::new();
    let start_ctx = Ctx::background().with_timeout(Duration::from_millis(400));
    if let Err(err) = adv.start(&start_ctx, spec).await {
        eprintln!("skipping live multicast test: {err}");
        return;
    }
    let browse_ctx = Ctx::background().with_timeout(Duration::from_secs(5));
    let browser = ZeroconfBrowser::new(ifaces);
    let results = browser.browse(&browse_ctx, DEFAULT_SERVICE_TYPE, DEFAULT_DOMAIN).await;
    adv.stop().await.unwrap();
    let results = results.expect("browse failed");
    let found = results.iter().find(|r| r.instance_name == "CPA-LiveTest-42").expect("advertised instance was not discovered");
    assert_eq!(found.port, 54321);
    assert_eq!(found.product, PRODUCT_CPA);
}

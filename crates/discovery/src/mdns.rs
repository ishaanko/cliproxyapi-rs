//! mDNS responder and browser (Go: libp2p/zeroconf v2.2.0 server.go, client.go, service.go).
//!
//! A deliberate port, quirks included: names are compared as exact presentation strings (so
//! instance names containing spaces never match a lookup), subtype queries are never answered
//! (zeroconf formats the subtype suffix twice), probes and announcements follow the same timers,
//! and browse results are delivered once per instance with at least one resolved address.
//! Unix only; the portable surface lives in `zeroconf.rs`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::ctx::Ctx;
use crate::dns::{CLASS_INET, CLASS_TOP_BIT, Msg, RData, Rr, TYPE_PTR};
use crate::interfaces::{Interface, list_interfaces};
use crate::net::{Conn, MDNS_GROUP_V4, MDNS_GROUP_V6, MDNS_PORT};

const DEFAULT_TTL: u32 = 3200;
const MULTICAST_REPETITIONS: usize = 2;
const INITIAL_QUERY_INTERVAL: Duration = Duration::from_secs(4);
const MAX_QUERY_INTERVAL: Duration = Duration::from_secs(60);
const CLEANUP_FREQ: Duration = Duration::from_secs(10);

fn trim_dot(s: &str) -> &str {
    s.trim_matches('.')
}

/// Names derived from a service type, instance and domain (Go: `ServiceRecord`).
#[derive(Debug, Clone, Default)]
pub struct ServiceRecord {
    pub instance: String,
    /// Primary service type, e.g. `_ai-gateway._tcp`.
    pub service: String,
    /// Full subtype names, e.g. `_responses._sub._ai-gateway._tcp.local.`.
    pub subtypes: Vec<String>,
    pub domain: String,
    service_name: String,
    service_instance_name: String,
    service_type_name: String,
}

impl ServiceRecord {
    /// Go `newServiceRecord`; `service` may carry subtypes as `type,sub1,sub2`.
    pub fn new(instance: &str, service: &str, domain: &str) -> Self {
        let mut parts = service.split(',');
        let service = parts.next().unwrap_or("").to_string();
        let service_name = format!("{}.{}.", trim_dot(&service), trim_dot(domain));
        let subtypes = parts.map(|sub| format!("{}._sub.{}", trim_dot(sub), service_name)).collect();
        let service_instance_name = if instance.is_empty() { String::new() } else { format!("{}.{}", trim_dot(instance), service_name) };
        let type_domain = if domain.is_empty() { "local" } else { trim_dot(domain) };
        Self {
            instance: instance.to_string(),
            service,
            subtypes,
            domain: domain.to_string(),
            service_name,
            service_instance_name,
            service_type_name: format!("_services._dns-sd._udp.{type_domain}."),
        }
    }

    /// `_foobar._tcp.local.`
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    /// `MyDemo Service._foobar._tcp.local.`
    pub fn service_instance_name(&self) -> &str {
        &self.service_instance_name
    }

    /// `_services._dns-sd._udp.local.`
    pub fn service_type_name(&self) -> &str {
        &self.service_type_name
    }
}

/// A registered service (server side) or a browse result (client side).
#[derive(Debug, Clone)]
pub struct ServiceEntry {
    pub record: ServiceRecord,
    pub host_name: String,
    pub port: i64,
    pub text: Vec<String>,
    pub expiry: Option<Instant>,
    pub addr_ipv4: Vec<Ipv4Addr>,
    pub addr_ipv6: Vec<Ipv6Addr>,
}

impl ServiceEntry {
    pub fn new(instance: &str, service: &str, domain: &str) -> Self {
        Self {
            record: ServiceRecord::new(instance, service, domain),
            host_name: String::new(),
            port: 0,
            text: Vec::new(),
            expiry: None,
            addr_ipv4: Vec::new(),
            addr_ipv6: Vec::new(),
        }
    }
}

/// Interfaces that are up and multicast capable (Go: `listMulticastInterfaces`).
fn list_multicast_interfaces() -> Vec<Interface> {
    list_interfaces().map(|all| all.into_iter().filter(|i| i.up && i.multicast).collect()).unwrap_or_default()
}

fn group_v4() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(MDNS_GROUP_V4), MDNS_PORT)
}

fn group_v6() -> SocketAddr {
    SocketAddr::new(IpAddr::V6(MDNS_GROUP_V6), MDNS_PORT)
}

// ---------------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------------

struct StopSignal {
    stopped: Mutex<bool>,
    cond: Condvar,
}

impl StopSignal {
    fn stop(&self) {
        *self.stopped.lock() = true;
        self.cond.notify_all();
    }

    fn is_stopped(&self) -> bool {
        *self.stopped.lock()
    }

    /// Sleeps up to `dur`; returns true when stopped (before or during the wait).
    fn sleep(&self, dur: Duration) -> bool {
        let deadline = std::time::Instant::now() + dur;
        let mut stopped = self.stopped.lock();
        while !*stopped {
            if self.cond.wait_until(&mut stopped, deadline).timed_out() {
                break;
            }
        }
        *stopped
    }
}

struct ServerShared {
    entry: ServiceEntry,
    v4: Option<Conn>,
    v6: Option<Conn>,
    ifaces: Vec<Interface>,
    ttl: u32,
    stop: StopSignal,
}

/// A running responder for one service (Go: `zeroconf.Server`).
pub struct Server {
    shared: Arc<ServerShared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    shutdown_done: Mutex<bool>,
}

/// Go `zeroconf.RegisterProxy`: advertises `instance` of `service` (`type[,subtype...]`) pointing
/// at `host` and `ips`, without looking up the local hostname or addresses.
#[allow(clippy::too_many_arguments)]
pub fn register_proxy(
    instance: &str,
    service: &str,
    domain: &str,
    port: i64,
    host: &str,
    ips: &[String],
    text: &[String],
    ifaces: &[Interface],
) -> Result<Server, String> {
    let mut entry = ServiceEntry::new(instance, service, domain);
    entry.port = port;
    entry.text = text.to_vec();
    entry.host_name = host.to_string();

    if entry.record.instance.is_empty() {
        return Err("missing service instance name".into());
    }
    if entry.record.service.is_empty() {
        return Err("missing service name".into());
    }
    if entry.host_name.is_empty() {
        return Err("missing host name".into());
    }
    if entry.record.domain.is_empty() {
        entry.record.domain = "local".into();
    }
    if entry.port == 0 {
        return Err("missing port".into());
    }

    if !trim_dot(&entry.host_name).ends_with(entry.record.domain.as_str()) {
        entry.host_name = format!("{}.{}.", trim_dot(&entry.host_name), trim_dot(&entry.record.domain));
    }

    for ip in ips {
        match ip.parse::<IpAddr>().map(|ip| ip.to_canonical()) {
            Ok(IpAddr::V4(v4)) => entry.addr_ipv4.push(v4),
            Ok(IpAddr::V6(v6)) => entry.addr_ipv6.push(v6),
            Err(_) => return Err(format!("failed to parse given IP: {ip}")),
        }
    }

    let ifaces: Vec<Interface> = if ifaces.is_empty() { list_multicast_interfaces() } else { ifaces.to_vec() };

    let v4 = Conn::join_v4(&ifaces);
    if let Err(err) = &v4 {
        tracing::info!("[zeroconf] no suitable IPv4 interface: {err}");
    }
    let v6 = Conn::join_v6(&ifaces);
    if let Err(err) = &v6 {
        tracing::info!("[zeroconf] no suitable IPv6 interface: {err}");
    }
    if v4.is_err() && v6.is_err() {
        return Err("no supported interface".into());
    }

    let shared = Arc::new(ServerShared {
        entry,
        v4: v4.ok(),
        v6: v6.ok(),
        ifaces,
        ttl: DEFAULT_TTL,
        stop: StopSignal { stopped: Mutex::new(false), cond: Condvar::new() },
    });
    let server = Server { shared, threads: Mutex::new(Vec::new()), shutdown_done: Mutex::new(false) };
    server.start();
    Ok(server)
}

impl Server {
    fn start(&self) {
        let mut threads = self.threads.lock();
        for is_v6 in [false, true] {
            let has_conn = if is_v6 { self.shared.v6.is_some() } else { self.shared.v4.is_some() };
            if has_conn {
                let shared = self.shared.clone();
                threads.push(std::thread::spawn(move || shared.recv_loop(is_v6)));
            }
        }
        let shared = self.shared.clone();
        threads.push(std::thread::spawn(move || shared.probe()));
    }

    /// Sends goodbye records, stops the receive and announce threads and waits for them. Blocking.
    pub fn shutdown(&self) {
        let mut done = self.shutdown_done.lock();
        if *done {
            return;
        }
        if let Err(err) = self.shared.unregister() {
            tracing::warn!("failed to unregister: {err}");
        }
        self.shared.stop.stop();
        for handle in self.threads.lock().drain(..) {
            let _ = handle.join();
        }
        *done = true;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.stop();
    }
}

impl ServerShared {
    fn recv_loop(&self, v6: bool) {
        let Some(conn) = (if v6 { self.v6.as_ref() } else { self.v4.as_ref() }) else { return };
        let mut buf = vec![0u8; 65536];
        while !self.stop.is_stopped() {
            match conn.recv(&mut buf) {
                Ok(Some(dgram)) => {
                    if let Ok(msg) = Msg::unpack(&buf[..dgram.len]) {
                        let _ = self.handle_query(&msg, dgram.ifindex, dgram.from);
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    if self.stop.sleep(Duration::from_millis(50)) {
                        return;
                    }
                }
            }
        }
    }

    /// Go `handleQuery`.
    fn handle_query(&self, query: &Msg, ifindex: u32, from: SocketAddr) -> Result<(), String> {
        // Ignore questions with an authority section.
        if !query.ns.is_empty() {
            return Ok(());
        }
        let mut result = Ok(());
        for q in &query.question {
            let mut resp = Msg::reply_to(query);
            resp.compress = true;
            resp.recursion_desired = false;
            resp.authoritative = true;
            self.handle_question(&q.name, &mut resp, query, ifindex);
            if resp.answer.is_empty() {
                continue;
            }
            let sent = if q.qclass & CLASS_TOP_BIT != 0 {
                self.unicast_response(&resp, ifindex, from)
            } else {
                self.multicast_response(&resp, ifindex)
            };
            if let Err(err) = sent {
                result = Err(err);
            }
        }
        result
    }

    fn handle_question(&self, name: &str, resp: &mut Msg, query: &Msg, ifindex: u32) {
        let rec = &self.entry.record;
        if name == rec.service_type_name() {
            self.service_type_name(resp, self.ttl);
            if is_known_answer(resp, query) {
                resp.answer.clear();
            }
        } else if name == rec.service_name() {
            self.compose_browsing_answers(resp, ifindex);
            if is_known_answer(resp, query) {
                resp.answer.clear();
            }
        } else if name == rec.service_instance_name() {
            self.compose_lookup_answers(resp, self.ttl, false);
        } else {
            // zeroconf appends "._sub.<service name>" to subtypes that already carry it, so this
            // never matches a real query. Kept for parity: subtype browses go unanswered.
            for subtype in &rec.subtypes {
                if name == format!("{subtype}._sub.{}", rec.service_name()) {
                    self.compose_browsing_answers(resp, ifindex);
                    if is_known_answer(resp, query) {
                        resp.answer.clear();
                    }
                    break;
                }
            }
        }
    }

    fn rr(&self, name: &str, class: u16, ttl: u32, data: RData) -> Rr {
        Rr { name: name.to_string(), class, ttl, data }
    }

    fn srv_txt(&self, srv_class: u16, ttl: u32) -> (Rr, Rr) {
        let rec = &self.entry.record;
        let srv = self.rr(
            rec.service_instance_name(),
            srv_class,
            ttl,
            RData::Srv { priority: 0, weight: 0, port: self.entry.port as u16, target: self.entry.host_name.clone() },
        );
        let txt = self.rr(rec.service_instance_name(), srv_class, ttl, RData::Txt(self.entry.text.clone()));
        (srv, txt)
    }

    fn compose_browsing_answers(&self, resp: &mut Msg, _ifindex: u32) {
        let rec = &self.entry.record;
        resp.answer.push(self.rr(rec.service_name(), CLASS_INET, self.ttl, RData::Ptr(rec.service_instance_name().to_string())));
        let (srv, txt) = self.srv_txt(CLASS_INET, self.ttl);
        resp.extra.push(srv);
        resp.extra.push(txt);
        self.append_addrs(&mut resp.extra, self.ttl, false);
    }

    fn compose_lookup_answers(&self, resp: &mut Msg, ttl: u32, flush_cache: bool) {
        let rec = &self.entry.record;
        // SRV and TXT always carry the mDNS cache-flush bit here; PTRs never do.
        let (srv, txt) = self.srv_txt(CLASS_INET | CLASS_TOP_BIT, ttl);
        let ptr = self.rr(rec.service_name(), CLASS_INET, ttl, RData::Ptr(rec.service_instance_name().to_string()));
        let dnssd = self.rr(rec.service_type_name(), CLASS_INET, ttl, RData::Ptr(rec.service_name().to_string()));
        resp.answer.extend([srv, txt, ptr, dnssd]);
        for subtype in &rec.subtypes {
            resp.answer.push(self.rr(subtype, CLASS_INET, ttl, RData::Ptr(rec.service_instance_name().to_string())));
        }
        self.append_addrs(&mut resp.answer, ttl, flush_cache);
    }

    fn service_type_name(&self, resp: &mut Msg, ttl: u32) {
        let rec = &self.entry.record;
        resp.answer.push(self.rr(rec.service_type_name(), CLASS_INET, ttl, RData::Ptr(rec.service_name().to_string())));
    }

    /// Go `appendAddrs`. The "no configured addresses" fallback to the receiving interface's
    /// addresses is unreachable here (the advertiser rejects an empty address list).
    fn append_addrs(&self, list: &mut Vec<Rr>, ttl: u32, flush_cache: bool) {
        // RFC 6762 section 10: A/AAAA records SHOULD use a TTL of 120s.
        let ttl = if ttl > 0 { 120 } else { ttl };
        let class = if flush_cache { CLASS_INET | CLASS_TOP_BIT } else { CLASS_INET };
        for ip in &self.entry.addr_ipv4 {
            list.push(self.rr(&self.entry.host_name, class, ttl, RData::A(*ip)));
        }
        for ip in &self.entry.addr_ipv6 {
            list.push(self.rr(&self.entry.host_name, class, ttl, RData::Aaaa(*ip)));
        }
    }

    /// Probing and announcement: three probes 250 ms apart, then two unsolicited announcements
    /// per interface (1 s, then 2 s apart). No conflict resolution, as in zeroconf.
    fn probe(&self) {
        let rec = &self.entry.record;
        let mut q = Msg::default();
        q.set_question(rec.service_instance_name(), TYPE_PTR);
        q.recursion_desired = false;
        let (srv, txt) = self.srv_txt(CLASS_INET, self.ttl);
        q.ns = vec![srv, txt];

        if self.stop.sleep(Duration::from_millis(rand::random_range(0..250))) {
            return;
        }
        for _ in 0..3 {
            if let Err(err) = self.multicast_response(&q, 0) {
                tracing::warn!("[ERR] zeroconf: failed to send probe: {err}");
            }
            if self.stop.sleep(Duration::from_millis(250)) {
                return;
            }
        }

        let mut timeout = Duration::from_secs(1);
        for _ in 0..MULTICAST_REPETITIONS {
            for intf in &self.ifaces {
                let mut resp = Msg { response: true, compress: true, ..Msg::default() };
                self.compose_lookup_answers(&mut resp, self.ttl, true);
                if let Err(err) = self.multicast_response(&resp, intf.index) {
                    tracing::warn!("[ERR] zeroconf: failed to send announcement: {err}");
                }
            }
            if self.stop.sleep(timeout) {
                return;
            }
            timeout *= 2;
        }
    }

    /// Goodbye packet: every record with TTL 0.
    fn unregister(&self) -> Result<(), String> {
        let mut resp = Msg { response: true, ..Msg::default() };
        self.compose_lookup_answers(&mut resp, 0, true);
        self.multicast_response(&resp, 0)
    }

    fn unicast_response(&self, resp: &Msg, ifindex: u32, from: SocketAddr) -> Result<(), String> {
        let buf = resp.pack().map_err(|e| e.to_string())?;
        let conn = if from.is_ipv4() { self.v4.as_ref() } else { self.v6.as_ref() };
        match conn {
            Some(conn) => conn.send_to(&buf, from, ifindex).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// Sends to the mDNS groups through `ifindex`, or through every interface when it is 0.
    fn multicast_response(&self, msg: &Msg, ifindex: u32) -> Result<(), String> {
        let buf = msg.pack().map_err(|e| format!("failed to pack msg {msg:?}: {e}"))?;
        for (conn, dest) in [(self.v4.as_ref(), group_v4()), (self.v6.as_ref(), group_v6())] {
            let Some(conn) = conn else { continue };
            if ifindex != 0 {
                let _ = conn.send_to(&buf, dest, ifindex);
            } else {
                for intf in &self.ifaces {
                    let _ = conn.send_to(&buf, dest, intf.index);
                }
            }
        }
        Ok(())
    }
}

/// RFC 6762 section 7.1 known-answer suppression.
fn is_known_answer(resp: &Msg, query: &Msg) -> bool {
    let (Some(first), false) = (resp.answer.first(), query.answer.is_empty()) else { return false };
    let RData::Ptr(answer_ptr) = &first.data else { return false };
    query.answer.iter().any(|known| match &known.data {
        RData::Ptr(ptr) => ptr == answer_ptr && known.ttl >= first.ttl / 2,
        _ => false,
    })
}

// ---------------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------------

/// Go `zeroconf.Browse`: queries for `service` in `domain` on `ifaces` (all multicast interfaces
/// when empty) and sends each resolved entry once on `entries`. Returns `Ok` when `ctx` ends and
/// `Err` for socket setup or query encoding failures.
pub async fn browse(ctx: &Ctx, service: &str, domain: &str, entries: mpsc::Sender<ServiceEntry>, ifaces: &[Interface]) -> Result<(), String> {
    let ifaces: Vec<Interface> = if ifaces.is_empty() { list_multicast_interfaces() } else { ifaces.to_vec() };
    let v4 = Arc::new(Conn::join_v4(&ifaces)?);
    let v6 = Arc::new(Conn::join_v6(&ifaces)?);

    // defaultParams: the cached names always use the "local" domain; only `Domain` is overridden.
    let mut params = ServiceRecord::new("", service, "local");
    if !domain.is_empty() {
        params.domain = domain.to_string();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let (msg_tx, mut msg_rx) = mpsc::channel::<Msg>(32);
    for conn in [v4.clone(), v6.clone()] {
        let stop = stop.clone();
        let tx = msg_tx.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            while !stop.load(Ordering::Relaxed) {
                match conn.recv(&mut buf) {
                    Ok(Some(dgram)) => {
                        if let Ok(msg) = Msg::unpack(&buf[..dgram.len])
                            && tx.blocking_send(msg).is_err()
                        {
                            return;
                        }
                    }
                    Ok(None) => {}
                    // Go stops reading after a fatal read error.
                    Err(_) => return,
                }
            }
        });
    }
    drop(msg_tx);
    let _guard = StopOnDrop(stop);

    let mut sent: Vec<(String, Instant)> = Vec::new();
    let mut interval = INITIAL_QUERY_INTERVAL;
    let mut next_query = Instant::now();
    let mut cleanup = tokio::time::interval_at(Instant::now() + CLEANUP_FREQ, CLEANUP_FREQ);
    loop {
        tokio::select! {
            _ = ctx.done() => return Ok(()),
            _ = tokio::time::sleep_until(next_query) => {
                send_query(&params, &ifaces, &v4, &v6)?;
                if interval != MAX_QUERY_INTERVAL {
                    let nanos = interval.as_nanos() as u64;
                    interval += Duration::from_nanos(rand::random_range(0..nanos)) + interval / 2;
                    interval = interval.min(MAX_QUERY_INTERVAL);
                }
                next_query = Instant::now() + interval;
            }
            _ = cleanup.tick() => {
                let now = Instant::now();
                sent.retain(|(_, expiry)| now <= *expiry);
            }
            msg = msg_rx.recv() => {
                let Some(msg) = msg else { return Ok(()) };
                let now = Instant::now();
                for (key, mut entry) in collect_entries(&params, &msg, now) {
                    if entry.expiry.is_none_or(|e| e <= now) {
                        sent.retain(|(k, _)| *k != key);
                        continue;
                    }
                    if sent.iter().any(|(k, _)| *k == key) {
                        continue;
                    }
                    // Plain browses need at least one resolved address; service-type enumeration
                    // keeps bare PTR entries.
                    if params.service_type_name() != params.service_name() && entry.addr_ipv4.is_empty() && entry.addr_ipv6.is_empty() {
                        continue;
                    }
                    let expiry = entry.expiry.unwrap_or(now);
                    entry.expiry = Some(expiry);
                    tokio::select! {
                        _ = ctx.done() => return Ok(()),
                        res = entries.send(entry) => {
                            if res.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    sent.push((key, expiry));
                }
            }
        }
    }
}

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Builds the browse/lookup question and writes it to every interface on both sockets.
fn send_query(params: &ServiceRecord, ifaces: &[Interface], v4: &Conn, v6: &Conn) -> Result<(), String> {
    let service_name = format!("{}.{}.", trim_dot(&params.service), trim_dot(&params.domain));
    let mut m = Msg::default();
    // Browses never carry an instance; a subtype browse asks for the first subtype.
    match params.subtypes.first() {
        Some(subtype) => m.set_question(subtype, TYPE_PTR),
        None => m.set_question(&service_name, TYPE_PTR),
    }
    m.recursion_desired = false;
    let buf = m.pack().map_err(|e| e.to_string())?;
    for iface in ifaces {
        let _ = v4.send_to(&buf, group_v4(), iface.index);
    }
    for iface in ifaces {
        let _ = v6.send_to(&buf, group_v6(), iface.index);
    }
    Ok(())
}

fn entry_slot<'a>(entries: &'a mut Vec<(String, ServiceEntry)>, key: &str, make: impl FnOnce() -> ServiceEntry) -> &'a mut ServiceEntry {
    let pos = match entries.iter().position(|(k, _)| k == key) {
        Some(pos) => pos,
        None => {
            entries.push((key.to_string(), make()));
            entries.len() - 1
        }
    };
    &mut entries[pos].1
}

/// Go client `mainloop` per-message body: turns one DNS message into candidate entries keyed by
/// instance name, then attaches A/AAAA records whose owner matches an entry's host name.
fn collect_entries(params: &ServiceRecord, msg: &Msg, now: Instant) -> Vec<(String, ServiceEntry)> {
    let mut entries: Vec<(String, ServiceEntry)> = Vec::new();
    let sections: Vec<&Rr> = msg.answer.iter().chain(&msg.ns).chain(&msg.extra).collect();
    let expiry = |ttl: u32| Some(now + Duration::from_secs(u64::from(ttl)));
    let new_entry = |instance: &str| ServiceEntry::new(instance, &params.service, &params.domain);

    for rr in &sections {
        match &rr.data {
            RData::Ptr(ptr) => {
                if params.service_name() != rr.name {
                    continue;
                }
                if !params.service_instance_name().is_empty() && params.service_instance_name() != ptr {
                    continue;
                }
                let entry = entry_slot(&mut entries, ptr, || new_entry(trim_dot(&ptr.replace(&rr.name, ""))));
                entry.expiry = expiry(rr.ttl);
            }
            RData::Srv { port, target, .. } => {
                if !params.service_instance_name().is_empty() && params.service_instance_name() != rr.name {
                    continue;
                }
                if !rr.name.ends_with(params.service_name()) {
                    continue;
                }
                let entry = entry_slot(&mut entries, &rr.name, || new_entry(trim_dot(&rr.name.replacen(params.service_name(), "", 1))));
                entry.host_name = target.clone();
                entry.port = i64::from(*port);
                entry.expiry = expiry(rr.ttl);
            }
            RData::Txt(text) => {
                if !params.service_instance_name().is_empty() && params.service_instance_name() != rr.name {
                    continue;
                }
                if !rr.name.ends_with(params.service_name()) {
                    continue;
                }
                let entry = entry_slot(&mut entries, &rr.name, || new_entry(trim_dot(&rr.name.replacen(params.service_name(), "", 1))));
                entry.text = text.clone();
                entry.expiry = expiry(rr.ttl);
            }
            _ => {}
        }
    }
    // Addresses are associated in a second round, once host names are known.
    for rr in &sections {
        match &rr.data {
            RData::A(ip) => {
                for (_, e) in entries.iter_mut().filter(|(_, e)| e.host_name == rr.name) {
                    e.addr_ipv4.push(*ip);
                }
            }
            RData::Aaaa(ip) => {
                for (_, e) in entries.iter_mut().filter(|(_, e)| e.host_name == rr.name) {
                    e.addr_ipv6.push(*ip);
                }
            }
            _ => {}
        }
    }
    entries
}

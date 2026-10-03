//! The Home control plane client (Go: `internal/home/client.go`).
//!
//! Home speaks a Redis-compatible protocol with overloaded keys: `GET config` returns the
//! config, `RPOP <json>` dispatches an auth, `LPUSH usage` reports usage, and a `SUBSCRIBE
//! config ...` connection doubles as the membership heartbeat. This client keeps the Go
//! structure: a command pool and a subscription pool that follow cluster failover, a fence
//! that kills all in-flight traffic after an ambiguous dispatch, and a dedicated release pool.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use cpa_config::{CredentialConcurrencyConfig, HomeConfig};
use parking_lot::{Mutex, RwLock};
use serde::Deserialize;

use crate::conn::{Conn, ConnOpts, Kill, Tracker, arg, new_tls_params};
use crate::error::HomeError;
use crate::requests::{
    AuthDispatchRequest, ConcurrencyReleaseFrame, ModelsRequest, PluginTask, RefreshRequest,
};
use crate::resp::Value;

const KEY_CONFIG: &str = "config";
const CHANNEL_CONFIG: &str = "config";
const KEY_USAGE: &str = "usage";
const KEY_IN_FLIGHT_SNAPSHOT: &str = "in-flight-snapshot";
const KEY_CONCURRENCY_RELEASE: &str = "concurrency-release";
const KEY_REQUEST_LOG: &str = "request-log";
const KEY_APP_LOG: &str = "app-log";
const KEY_PLUGIN_STATUS: &str = "plugin-status";
const KEY_PLUGIN_TASKS: &str = "plugin-tasks";
const KEY_PLUGIN_SYNC: &str = "plugin-sync";
const CHANNEL_CLUSTER: &str = "cluster";

const RECONNECT_FAILOVER_THRESHOLD: u32 = 3;
const REDIS_OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
const REFRESH_OPERATION_TIMEOUT: Duration = Duration::from_secs(35);
const PLUGIN_SYNC_OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
const PLUGIN_SYNC_UNSUPPORTED_ERROR_TYPE: &str = "plugin_sync_unsupported";

/// Cancellation of a subscriber lifetime (Go: its `context.Context`).
pub type Cancel = Arc<Kill>;

/// Membership recovery progress (Go: `recoveryState`).
pub const RECOVERY_STABLE: u32 = 0;
pub const RECOVERY_TAKEOVER_ELIGIBLE: u32 = 1;
pub const RECOVERY_SWITCHING: u32 = 2;
pub const RECOVERY_SWITCHING_TAKEOVER: u32 = 3;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClusterNode {
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub port: i64,
    #[serde(default)]
    pub client_count: i64,
    #[serde(default)]
    pub is_master: bool,
    #[serde(default)]
    pub last_seen_at: Option<String>,
}

#[derive(Deserialize)]
struct ClusterNodesEnvelope {
    #[serde(default)]
    nodes: Vec<ClusterNode>,
}

/// `SET` modifiers (Go: `KVSetOptions`).
#[derive(Debug, Clone, Copy, Default)]
pub struct KvSetOptions {
    pub ex: Duration,
    pub px: Duration,
    pub nx: bool,
    pub xx: bool,
}

/// Connections to one address, reused between commands (Go: go-redis's pool).
pub struct Pool {
    opts: Arc<ConnOpts>,
    idle: Mutex<Vec<Conn>>,
    closed: AtomicBool,
    tracker: Arc<Tracker>,
}

impl Pool {
    fn new(opts: Arc<ConnOpts>, tracker: Arc<Tracker>) -> Arc<Pool> {
        Arc::new(Pool { opts, idle: Mutex::new(Vec::new()), closed: AtomicBool::new(false), tracker })
    }

    async fn get(&self) -> Result<Conn, HomeError> {
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return Err(HomeError::Io("redis: client is closed".into()));
            }
            let reused = self.idle.lock().pop();
            match reused {
                Some(conn) if !conn.broken && !conn.kill_switch().is_dead() => return Ok(conn),
                Some(_) => continue,
                None => return Conn::dial(self.opts.clone(), Some(self.tracker.clone())).await,
            }
        }
    }

    fn put(&self, conn: Conn) {
        if conn.broken || self.closed.load(Ordering::SeqCst) {
            return;
        }
        self.idle.lock().push(conn);
    }

    /// Closes the pool and every connection it ever handed out that is still open.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.idle.lock().clear();
        self.tracker.kill_all();
    }
}

struct State {
    home_cfg: HomeConfig,
    seed_host: String,
    seed_port: i64,
    cmd: Option<Arc<Pool>>,
    cmd_opts: Option<Arc<ConnOpts>>,
    sub: Option<Arc<Pool>>,
    release: Option<Arc<Pool>>,
    lifecycle: CredentialConcurrencyConfig,
    managed: bool,
    instance_id: String,
    legacy_membership: bool,
    cluster_nodes: Vec<ClusterNode>,
    reconnect_failures: u32,
    test_operation_timeout: Option<Duration>,
}

pub struct Client {
    state: Mutex<State>,
    limiter: RwLock<Option<Arc<CredentialConcurrencyConfig>>>,
    heartbeat_ok: AtomicBool,
    dispatch_fenced: Arc<AtomicBool>,
    ambiguous_dispatch: AtomicBool,
    /// Latches when Home does not implement `CAS`. Deliberately not carried across
    /// [`Client::new_lifetime`]: support is a property of the Home deployment, so a new
    /// lifetime re-probes once and a Home upgrade takes effect on the next reconnect.
    cas_unsupported: AtomicBool,
    recovery_state: AtomicU32,
}

/// Parameters of one auth dispatch.
#[derive(Debug, Clone, Default)]
pub struct DispatchParams<'a> {
    pub model: &'a str,
    pub session_id: &'a str,
    pub parent_session_id: &'a str,
    pub headers: http::HeaderMap,
    pub count: i64,
    pub credential_policy: &'a str,
    pub retry_round: Option<i64>,
    /// `Some` (even empty) sends `excluded_auth_ids` and pins `count` to 1.
    pub excluded_auth_ids: Option<Vec<String>>,
    pub pinned_auth_id: &'a str,
}

fn trim(s: &str) -> &str {
    s.trim()
}

/// Go: `headersToLowerMap`.
pub fn headers_to_lower_map(headers: &http::HeaderMap) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        let k = name.as_str().trim().to_lowercase();
        if k.is_empty() {
            continue;
        }
        out.entry(k)
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).trim().to_string());
    }
    out.into_iter().map(|(k, v)| (k, v.join(", "))).collect()
}

/// Go: `queryToLowerMap`; `query` holds the decoded pairs in request order.
pub fn query_to_lower_map(query: &[(String, String)]) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, value) in query {
        let k = key.trim().to_lowercase();
        if k.is_empty() {
            continue;
        }
        out.entry(k).or_default().push(value.trim().to_string());
    }
    out.into_iter().map(|(k, v)| (k, v.join(", "))).collect()
}

/// Go: `newAuthDispatchRequest` (+ `...WithRetryRound`).
pub fn new_auth_dispatch_request(p: &DispatchParams<'_>) -> AuthDispatchRequest {
    let mut count = if p.count <= 0 { 1 } else { p.count };
    let excluded = p.excluded_auth_ids.as_ref().map(|ids| {
        // Keep count at one so older Home servers that ignore excluded_auth_ids do not apply
        // their legacy count-based retry cap before CPA can rotate credentials.
        count = 1;
        ids.clone()
    });
    let node_kind = p
        .headers
        .get("x-node-kind")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    AuthDispatchRequest {
        kind: "auth".into(),
        model: p.model.to_string(),
        count,
        concurrency_protocol: 1,
        session_id: trim(p.session_id).to_string(),
        parent_session_id: trim(p.parent_session_id).to_string(),
        node_kind,
        headers: headers_to_lower_map(&p.headers),
        credential_policy: trim(p.credential_policy).to_string(),
        retry_round: p.retry_round.map(|r| r.max(0)),
        excluded_auth_ids: excluded,
        pinned_auth_id: trim(p.pinned_auth_id).to_string(),
    }
}

fn duration_ceil(value: Duration, unit: Duration) -> i64 {
    if value.is_zero() || unit.is_zero() {
        return 0;
    }
    value.as_nanos().div_ceil(unit.as_nanos()) as i64
}

/// Go: `buildKVSetArgs`.
pub fn build_kv_set_args(key: &str, value: &[u8], opts: KvSetOptions) -> Result<Vec<Vec<u8>>, HomeError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(HomeError::other("home kv: key is empty"));
    }
    if !opts.ex.is_zero() && !opts.px.is_zero() {
        return Err(HomeError::other("home kv: EX and PX are mutually exclusive"));
    }
    if opts.nx && opts.xx {
        return Err(HomeError::other("home kv: NX and XX are mutually exclusive"));
    }
    let mut args = vec![arg(key), value.to_vec()];
    if !opts.ex.is_zero() {
        args.push(arg("EX"));
        args.push(arg(duration_ceil(opts.ex, Duration::from_secs(1)).to_string()));
    }
    if !opts.px.is_zero() {
        args.push(arg("PX"));
        args.push(arg(duration_ceil(opts.px, Duration::from_millis(1)).to_string()));
    }
    if opts.nx {
        args.push(arg("NX"));
    }
    if opts.xx {
        args.push(arg("XX"));
    }
    Ok(args)
}

fn bulk_opt(value: Value) -> Result<Option<Vec<u8>>, HomeError> {
    match value {
        Value::Nil => Ok(None),
        Value::Bulk(b) => Ok(Some(b.to_vec())),
        Value::Simple(s) => Ok(Some(s.into_bytes())),
        other => Err(HomeError::Other(format!("redis: unexpected reply type {other:?}"))),
    }
}

fn int_reply(value: Value) -> Result<i64, HomeError> {
    match value {
        Value::Int(n) => Ok(n),
        other => Err(HomeError::Other(format!("redis: unexpected reply type {other:?}"))),
    }
}

fn parse_cluster_nodes_payload(raw: &[u8]) -> Result<Vec<ClusterNode>, HomeError> {
    let envelope: ClusterNodesEnvelope = serde_json::from_slice(raw).map_err(HomeError::other)?;
    Ok(normalize_cluster_nodes(envelope.nodes))
}

fn normalize_cluster_nodes(nodes: Vec<ClusterNode>) -> Vec<ClusterNode> {
    let mut out: Vec<ClusterNode> = nodes
        .into_iter()
        .filter_map(|mut node| {
            node.ip = node.ip.trim().to_string();
            if node.ip.is_empty() || node.port <= 0 {
                return None;
            }
            node.client_count = node.client_count.max(0);
            Some(node)
        })
        .collect();
    out.sort_by_key(|n| n.client_count);
    out
}

/// Message kinds of a subscribed connection.
enum PubSubEvent {
    Subscription { kind: String, channel: String, count: i64 },
    Message { channel: String, payload: String },
    Pong,
    Other,
}

fn parse_pubsub(value: Value) -> Result<PubSubEvent, HomeError> {
    let Value::Array(items) = value else {
        return Err(HomeError::Other(format!("redis: unsupported pubsub message: {value:?}")));
    };
    let text = |i: usize| items.get(i).and_then(Value::text).unwrap_or_default();
    match text(0).as_str() {
        kind @ ("subscribe" | "unsubscribe") => Ok(PubSubEvent::Subscription {
            kind: kind.to_string(),
            channel: text(1),
            count: match items.get(2) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            },
        }),
        "message" => Ok(PubSubEvent::Message { channel: text(1), payload: text(2) }),
        "pong" => Ok(PubSubEvent::Pong),
        "pmessage" | "psubscribe" | "punsubscribe" => Ok(PubSubEvent::Other),
        other => Err(HomeError::Other(format!("redis: unsupported pubsub message: {other}"))),
    }
}

impl Client {
    pub fn new(cfg: HomeConfig) -> Client {
        let seed_host = cfg.host.trim().to_string();
        let seed_port = cfg.port;
        Client {
            state: Mutex::new(State {
                home_cfg: cfg,
                seed_host,
                seed_port,
                cmd: None,
                cmd_opts: None,
                sub: None,
                release: None,
                lifecycle: CredentialConcurrencyConfig::default(),
                managed: false,
                instance_id: uuid::Uuid::new_v4().to_string(),
                legacy_membership: false,
                cluster_nodes: Vec::new(),
                reconnect_failures: 0,
                test_operation_timeout: None,
            }),
            limiter: RwLock::new(None),
            heartbeat_ok: AtomicBool::new(false),
            dispatch_fenced: Arc::new(AtomicBool::new(false)),
            ambiguous_dispatch: AtomicBool::new(false),
            cas_unsupported: AtomicBool::new(false),
            recovery_state: AtomicU32::new(RECOVERY_STABLE),
        }
    }

    /// A fresh client preserving cluster failover and membership state (Go: `NewLifetime`).
    pub fn new_lifetime(&self) -> Client {
        let s = self.state.lock();
        let next = Client::new(s.home_cfg.clone());
        {
            let mut n = next.state.lock();
            n.seed_host.clone_from(&s.seed_host);
            n.seed_port = s.seed_port;
            n.cluster_nodes = s.cluster_nodes.clone();
            n.reconnect_failures = s.reconnect_failures;
            n.test_operation_timeout = s.test_operation_timeout;
            n.instance_id.clone_from(&s.instance_id);
            n.legacy_membership = s.legacy_membership;
        }
        next.recovery_state.store(self.recovery_state.load(Ordering::SeqCst), Ordering::SeqCst);
        next
    }

    /// Test hook: shortens dial/read/write timeouts.
    pub fn set_test_operation_timeout(&self, timeout: Duration) {
        self.state.lock().test_operation_timeout = Some(timeout);
    }

    pub fn membership_instance_id(&self) -> String {
        self.state.lock().instance_id.clone()
    }

    pub fn legacy_membership(&self) -> bool {
        self.state.lock().legacy_membership
    }

    /// Permanently downgrades this subscriber lifetime chain to the legacy protocol.
    pub fn enable_legacy_membership(&self) {
        self.state.lock().legacy_membership = true;
        self.suppress_takeover();
    }

    pub fn enabled(&self) -> bool {
        self.state.lock().home_cfg.enabled
    }

    pub fn heartbeat_ok(&self) -> bool {
        self.enabled() && self.heartbeat_ok.load(Ordering::SeqCst)
    }

    /// Forces the heartbeat flag for tests that need a "healthy" client without a subscriber.
    #[doc(hidden)]
    pub fn set_heartbeat_ok_for_tests(&self, ok: bool) {
        self.heartbeat_ok.store(ok, Ordering::SeqCst);
    }

    pub fn recovery_state(&self) -> u32 {
        self.recovery_state.load(Ordering::SeqCst)
    }

    pub fn set_recovery_state(&self, state: u32) {
        self.recovery_state.store(state, Ordering::SeqCst);
    }

    /// Node id the JWT assigned to this CPA instance.
    pub fn node_id(&self) -> String {
        self.state.lock().home_cfg.node_id.clone()
    }

    pub fn home_config(&self) -> HomeConfig {
        self.state.lock().home_cfg.clone()
    }

    /// Permanently ends this client's dispatch lifetime.
    pub fn close(&self) {
        self.dispatch_fenced.store(true, Ordering::SeqCst);
        self.heartbeat_ok.store(false, Ordering::SeqCst);
        let release = {
            let mut s = self.state.lock();
            Self::detach_clients_locked(&mut s);
            s.release.take()
        };
        if let Some(release) = release {
            release.close();
        }
    }

    /// Replaces the private bootstrap pools without ending the client lifetime.
    fn close_bootstrap_pools(&self) {
        self.heartbeat_ok.store(false, Ordering::SeqCst);
        let mut s = self.state.lock();
        Self::detach_clients_locked(&mut s);
    }

    /// Fences this client after an auth dispatch response is ambiguous.
    pub fn abort_ambiguous_dispatch(&self) {
        self.ambiguous_dispatch.store(true, Ordering::SeqCst);
        self.dispatch_fenced.store(true, Ordering::SeqCst);
        self.heartbeat_ok.store(false, Ordering::SeqCst);
        let release = {
            let mut s = self.state.lock();
            Self::detach_clients_locked(&mut s);
            s.release.take()
        };
        if let Some(release) = release {
            release.close();
        }
    }

    /// Whether this lifetime issued a dispatch whose delivery result is unknown.
    pub fn ambiguous_dispatch(&self) -> bool {
        self.ambiguous_dispatch.load(Ordering::SeqCst)
    }

    /// Forces the next subscriber lifetime through normal membership recovery.
    pub fn suppress_takeover(&self) {
        if self
            .recovery_state
            .compare_exchange(RECOVERY_TAKEOVER_ELIGIBLE, RECOVERY_STABLE, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            let _ = self.recovery_state.compare_exchange(
                RECOVERY_SWITCHING_TAKEOVER,
                RECOVERY_SWITCHING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }

    pub fn is_dispatch_fenced(&self) -> bool {
        self.dispatch_fenced.load(Ordering::SeqCst)
    }

    /// Defers client shutdown to the service lifetime owner.
    pub fn set_managed_lifetime(&self, managed: bool) {
        self.state.lock().managed = managed;
    }

    fn managed_lifetime(&self) -> bool {
        self.state.lock().managed
    }

    fn detach_clients_locked(s: &mut State) {
        s.cmd_opts = None;
        if let Some(cmd) = s.cmd.take() {
            cmd.close();
        }
        if let Some(sub) = s.sub.take() {
            sub.close();
        }
    }

    fn addr_locked(s: &State) -> Option<String> {
        let host = s.home_cfg.host.trim();
        if host.is_empty() || s.home_cfg.port <= 0 {
            return None;
        }
        Some(if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{}", s.home_cfg.port)
        } else {
            format!("{host}:{}", s.home_cfg.port)
        })
    }

    /// `host:port` currently targeted, if configured.
    pub fn addr(&self) -> Option<String> {
        Self::addr_locked(&self.state.lock())
    }

    fn pool_tracker(&self) -> Arc<Tracker> {
        Arc::new(Tracker::new(self.dispatch_fenced.clone()))
    }

    fn conn_opts_locked(s: &State, addr: &str) -> Result<Arc<ConnOpts>, HomeError> {
        let mut server_name = s.home_cfg.tls.server_name.trim().to_string();
        if server_name.is_empty() {
            if s.home_cfg.tls.use_target_server_name {
                server_name = host_from_address(addr);
            } else {
                server_name = s.seed_host.trim().to_string();
            }
        }
        if server_name.is_empty() {
            server_name = s.home_cfg.host.trim().to_string();
        }
        let tls = new_tls_params(&s.home_cfg.tls, &server_name)?;
        let timeout = s.test_operation_timeout.unwrap_or(REDIS_OPERATION_TIMEOUT);
        Ok(Arc::new(ConnOpts {
            addr: addr.to_string(),
            tls,
            dial_timeout: timeout,
            read_timeout: timeout,
            write_timeout: timeout,
        }))
    }

    fn ensure_clients(&self) -> Result<(), HomeError> {
        if self.dispatch_fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        if !self.enabled() {
            return Err(HomeError::Disabled);
        }
        let mut s = self.state.lock();
        if self.dispatch_fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        let Some(addr) = Self::addr_locked(&s) else {
            return Err(HomeError::Other(format!(
                "home: invalid address (host={:?} port={})",
                s.home_cfg.host, s.home_cfg.port
            )));
        };
        if s.cmd.is_none() {
            let opts = Self::conn_opts_locked(&s, &addr)?;
            s.cmd_opts = Some(opts.clone());
            s.cmd = Some(Pool::new(opts, self.pool_tracker()));
        }
        if s.sub.is_none() {
            let opts = Self::conn_opts_locked(&s, &addr)?;
            s.sub = Some(Pool::new(opts, self.pool_tracker()));
        }
        Ok(())
    }

    fn command_client(&self) -> Result<Arc<Pool>, HomeError> {
        if self.dispatch_fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        self.ensure_clients()?;
        let s = self.state.lock();
        if self.dispatch_fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        s.cmd.clone().ok_or(HomeError::NotConnected)
    }

    fn subscription_client(&self) -> Result<Arc<Pool>, HomeError> {
        self.ensure_clients()?;
        self.state.lock().sub.clone().ok_or(HomeError::NotConnected)
    }

    /// Runs one command on a pooled connection; a broken connection is dropped.
    async fn exec(&self, pool: &Pool, args: &[Vec<u8>]) -> Result<Value, HomeError> {
        let mut conn = pool.get().await?;
        let result = conn.call(args).await;
        pool.put(conn);
        result
    }

    async fn exec_with_read_timeout(&self, pool: &Pool, args: &[Vec<u8>], timeout: Duration) -> Result<Value, HomeError> {
        let mut conn = pool.get().await?;
        let result = conn.call_with_timeout(args, timeout).await;
        pool.put(conn);
        result
    }

    async fn run(&self, args: &[Vec<u8>]) -> Result<Value, HomeError> {
        let pool = self.command_client()?;
        self.exec(&pool, args).await
    }

    pub async fn ping(&self) -> Result<(), HomeError> {
        self.run(&[arg("ping")]).await.map(|_| ())
    }

    // ---- cluster discovery and failover ----

    fn cluster_discovery_enabled(&self) -> bool {
        !self.state.lock().home_cfg.disable_cluster_discovery
    }

    async fn refresh_best_cluster_node(&self) -> Result<(), HomeError> {
        if !self.cluster_discovery_enabled() {
            return Ok(());
        }
        match self.refresh_cluster_nodes().await {
            Ok(switched) => {
                if switched && let Some(addr) = self.addr() {
                    tracing::info!("home cluster target switched to {addr}");
                }
                Ok(())
            }
            Err(e) => {
                tracing::debug!("home cluster nodes unavailable: {e}");
                Err(e)
            }
        }
    }

    /// Asks Home for its cluster nodes and retargets the least loaded one. Returns whether the
    /// target changed.
    pub async fn refresh_cluster_nodes(&self) -> Result<bool, HomeError> {
        if !self.cluster_discovery_enabled() {
            return Ok(false);
        }
        let pool = self
            .command_client()
            .map_err(|e| HomeError::ClusterDiscoveryTransport(Box::new(e)))?;
        let reply = match self.exec(&pool, &[arg("CLUSTER"), arg("NODES")]).await {
            Ok(v) => v,
            Err(e) if e.is_redis_reply() => return Err(e),
            Err(e) => return Err(HomeError::ClusterDiscoveryTransport(Box::new(e))),
        };
        let raw = reply
            .text()
            .ok_or_else(|| HomeError::Other(format!("redis: unexpected reply type {reply:?}")))?;
        let nodes = parse_cluster_nodes_payload(raw.as_bytes())?;
        if nodes.is_empty() {
            return Ok(false);
        }
        let mut s = self.state.lock();
        s.cluster_nodes = nodes.clone();
        s.reconnect_failures = 0;
        Ok(self.switch_to_node_locked(&mut s, &nodes[0].ip, nodes[0].port))
    }

    fn update_cluster_nodes_from_payload(&self, raw: &[u8]) -> Result<(), HomeError> {
        if !self.cluster_discovery_enabled() {
            return Ok(());
        }
        let nodes = parse_cluster_nodes_payload(raw)?;
        self.state.lock().cluster_nodes = nodes;
        Ok(())
    }

    fn switch_to_node_locked(&self, s: &mut State, host: &str, port: i64) -> bool {
        let host = host.trim();
        if host.is_empty() || port <= 0 {
            return false;
        }
        if s.home_cfg.host.trim() == host && s.home_cfg.port == port {
            return false;
        }
        s.home_cfg.host = host.to_string();
        s.home_cfg.port = port;
        if self
            .recovery_state
            .compare_exchange(RECOVERY_STABLE, RECOVERY_SWITCHING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            let _ = self.recovery_state.compare_exchange(
                RECOVERY_TAKEOVER_ELIGIBLE,
                RECOVERY_SWITCHING_TAKEOVER,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
        Self::detach_clients_locked(s);
        if let Some(release) = s.release.take() {
            release.close();
        }
        true
    }

    fn mark_reconnect_failure(&self, reason: &str) {
        let (switched, addr) = self.failover_after_reconnect_failure();
        if switched {
            tracing::warn!("home control center unavailable after repeated {reason} failures; switching to {addr}");
        }
    }

    /// Counts a reconnect failure; after three, moves to the next known node.
    pub fn failover_after_reconnect_failure(&self) -> (bool, String) {
        let mut s = self.state.lock();
        if s.home_cfg.disable_cluster_discovery {
            s.reconnect_failures = 0;
            return (false, String::new());
        }
        s.reconnect_failures += 1;
        if s.reconnect_failures < RECONNECT_FAILOVER_THRESHOLD {
            return (false, String::new());
        }
        s.reconnect_failures = 0;
        self.switch_to_next_node_locked(&mut s)
    }

    fn failover_after_subscription_timeout(&self) -> (bool, String) {
        let mut s = self.state.lock();
        if s.home_cfg.disable_cluster_discovery {
            s.reconnect_failures = 0;
            return (false, String::new());
        }
        s.reconnect_failures = 0;
        self.switch_to_next_node_locked(&mut s)
    }

    fn switch_to_next_node_locked(&self, s: &mut State) -> (bool, String) {
        let current_host = s.home_cfg.host.trim().to_string();
        let current_port = s.home_cfg.port;
        let mut candidates: Vec<(String, i64)> = s.cluster_nodes.iter().map(|n| (n.ip.clone(), n.port)).collect();
        if !s.seed_host.trim().is_empty() && s.seed_port > 0 {
            candidates.push((s.seed_host.clone(), s.seed_port));
        }
        for (host, port) in candidates {
            let host = host.trim().to_string();
            if host.is_empty() || port <= 0 || (host == current_host && port == current_port) {
                continue;
            }
            if self.switch_to_node_locked(s, &host, port) {
                return (true, Self::addr_locked(s).unwrap_or_default());
            }
        }
        (false, String::new())
    }

    fn mark_subscription_timeout(&self) {
        let (switched, addr) = self.failover_after_subscription_timeout();
        if switched {
            tracing::warn!("home subscription heartbeat timeout; switching to {addr}");
        }
    }

    fn reset_reconnect_failures(&self) {
        self.state.lock().reconnect_failures = 0;
    }

    /// Test hook: seeds cluster failover state.
    pub fn set_cluster_state(&self, nodes: Vec<ClusterNode>, reconnect_failures: u32) {
        let mut s = self.state.lock();
        s.cluster_nodes = nodes;
        s.reconnect_failures = reconnect_failures;
    }

    pub fn cluster_nodes(&self) -> Vec<ClusterNode> {
        self.state.lock().cluster_nodes.clone()
    }

    pub fn reconnect_failures(&self) -> u32 {
        self.state.lock().reconnect_failures
    }

    // ---- plain reads ----

    pub async fn get_config(&self) -> Result<Vec<u8>, HomeError> {
        if let Err(e) = self.refresh_best_cluster_node().await
            && matches!(e, HomeError::ClusterDiscoveryTransport(_))
        {
            return Err(e);
        }
        match bulk_opt(self.run(&[arg("get"), arg(KEY_CONFIG)]).await?)? {
            None => Err(HomeError::ConfigNotFound),
            Some(raw) if raw.is_empty() => Err(HomeError::EmptyResponse),
            Some(raw) => Ok(raw),
        }
    }

    pub async fn get_models(
        &self,
        headers: &http::HeaderMap,
        query: &[(String, String)],
    ) -> Result<Vec<u8>, HomeError> {
        let req = ModelsRequest {
            kind: "models".into(),
            headers: headers_to_lower_map(headers),
            query: query_to_lower_map(query),
        };
        let key = serde_json::to_vec(&req).map_err(HomeError::other)?;
        match bulk_opt(self.run(&[arg("get"), key]).await?)? {
            None => Err(HomeError::ModelsNotFound),
            Some(raw) if raw.is_empty() => Err(HomeError::EmptyResponse),
            Some(raw) => Ok(raw),
        }
    }

    // ---- key/value store ----

    pub async fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, HomeError> {
        bulk_opt(self.run(&[arg("get"), arg(key)]).await?)
    }

    /// `SET`; `Ok(false)` when a NX/XX condition was not met.
    pub async fn kv_set(&self, key: &str, value: &[u8], opts: KvSetOptions) -> Result<bool, HomeError> {
        self.command_client()?;
        let mut args = vec![arg("SET")];
        args.extend(build_kv_set_args(key, value, opts)?);
        Ok(!matches!(self.run(&args).await?, Value::Nil))
    }

    pub async fn kv_set_nx(&self, key: &str, value: &[u8], ttl: Duration) -> Result<bool, HomeError> {
        let mut opts = KvSetOptions { nx: true, ..Default::default() };
        if !ttl.is_zero() {
            opts.ex = ttl;
        }
        self.kv_set(key, value, opts).await
    }

    /// Atomically replaces a value only when its current state matches the expected state:
    /// `CAS <key> <expected-exists 0|1> <expected-value> <new-value> [PX <ttl-ms>]`. Home
    /// replies integer 1 on swap, 0 on mismatch. A Home that predates CAS rejects the command,
    /// which latches [`HomeError::CompareAndSwapUnsupported`] for this client lifetime.
    pub async fn kv_compare_and_swap(
        &self,
        key: &str,
        expected: &[u8],
        expected_exists: bool,
        value: &[u8],
        ttl: Duration,
    ) -> Result<bool, HomeError> {
        if self.cas_unsupported.load(Ordering::SeqCst) {
            return Err(HomeError::CompareAndSwapUnsupported);
        }
        self.command_client()?;
        let mut args = vec![
            arg("CAS"),
            arg(key),
            arg(if expected_exists { "1" } else { "0" }),
            expected.to_vec(),
            value.to_vec(),
        ];
        let ms = duration_ceil(ttl, Duration::from_millis(1));
        if ms > 0 {
            args.push(arg("PX"));
            args.push(arg(ms.to_string()));
        }
        match self.run(&args).await.and_then(int_reply) {
            Ok(n) => Ok(n == 1),
            Err(e) if e.is_command_unsupported() => {
                if !self.cas_unsupported.swap(true, Ordering::SeqCst) {
                    tracing::warn!(
                        "home kv: this Home does not implement the CAS command; Antigravity and Codex reasoning replay are disabled until Home is upgraded"
                    );
                }
                Err(HomeError::CompareAndSwapUnsupported)
            }
            Err(e) => Err(e),
        }
    }

    pub async fn kv_del(&self, keys: &[String]) -> Result<i64, HomeError> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut args = vec![arg("del")];
        args.extend(keys.iter().map(arg));
        int_reply(self.run(&args).await?)
    }

    pub async fn kv_expire(&self, key: &str, ttl: Duration) -> Result<bool, HomeError> {
        // go-redis formats the TTL in whole seconds, sub-second positive values as 1.
        let mut secs = ttl.as_secs();
        if secs == 0 && !ttl.is_zero() {
            secs = 1;
        }
        Ok(int_reply(self.run(&[arg("expire"), arg(key), arg(secs.to_string())]).await?)? == 1)
    }

    /// `TTL`: `(ttl, exists)`; a key without expiry reports `(0, true)`.
    pub async fn kv_ttl(&self, key: &str) -> Result<(Duration, bool), HomeError> {
        let n = int_reply(self.run(&[arg("ttl"), arg(key)]).await?)?;
        Ok(match n {
            n if n <= -2 => (Duration::ZERO, false),
            -1 => (Duration::ZERO, true),
            n => (Duration::from_secs(n as u64), true),
        })
    }

    pub async fn kv_incr_by(&self, key: &str, delta: i64) -> Result<i64, HomeError> {
        int_reply(self.run(&[arg("incrby"), arg(key), arg(delta.to_string())]).await?)
    }

    /// `MGET`: per key the value and whether it exists.
    pub async fn kv_mget(&self, keys: &[String]) -> Result<Vec<Option<Vec<u8>>>, HomeError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec![arg("mget")];
        args.extend(keys.iter().map(arg));
        let Value::Array(items) = self.run(&args).await? else {
            return Err(HomeError::other("home kv: unexpected MGET reply"));
        };
        items
            .into_iter()
            .map(|item| match item {
                Value::Nil => Ok(None),
                Value::Bulk(b) => Ok(Some(b.to_vec())),
                Value::Simple(s) => Ok(Some(s.into_bytes())),
                other => Err(HomeError::Other(format!("home kv: unsupported MGET item type {other:?}"))),
            })
            .collect()
    }

    /// `MSET` with keys in sorted order.
    pub async fn kv_mset(&self, pairs: &BTreeMap<String, Vec<u8>>) -> Result<(), HomeError> {
        if pairs.is_empty() {
            return Ok(());
        }
        let mut args = vec![arg("MSET")];
        for (k, v) in pairs {
            args.push(arg(k));
            args.push(v.clone());
        }
        self.run(&args).await.map(|_| ())
    }

    // ---- auth dispatch ----

    /// Requests a credential from Home (`RPOP <json>` on a dedicated, probed connection).
    /// Transport failures after the request was written are [`HomeError::AmbiguousDispatch`].
    pub async fn rpop_auth(&self, params: &DispatchParams<'_>) -> Result<Vec<u8>, HomeError> {
        if self.dispatch_fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        let model = params.model.trim();
        if model.is_empty() {
            return Err(HomeError::other("home: requested model is empty"));
        }
        let mut params = params.clone();
        params.model = model;
        let req = new_auth_dispatch_request(&params);
        let key = serde_json::to_vec(&req).map_err(HomeError::other)?;
        let pool = self.command_client()?;
        if self.dispatch_fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        let mut conn = pool.get().await?;
        if self.dispatch_fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        // The probe proves the connection is alive before the request is issued, so a failure
        // here is deterministic (nothing was sent).
        conn.call(&[arg("ping")]).await?;
        if self.dispatch_fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        let result = conn.call(&[arg("rpop"), key]).await;
        pool.put(conn);
        match result {
            Ok(value) => match bulk_opt(value)? {
                None => Err(HomeError::AuthNotFound),
                Some(raw) if raw.is_empty() => Err(HomeError::EmptyResponse),
                Some(raw) => Ok(raw),
            },
            Err(e) if e.is_redis_reply() => Err(e),
            Err(e) => Err(HomeError::AmbiguousDispatch(Box::new(e))),
        }
    }

    /// Asks Home for refreshed credentials of `auth_index` (35s read timeout).
    pub async fn get_refresh_auth(&self, auth_index: &str, access_token_sha256: &str) -> Result<Vec<u8>, HomeError> {
        let pool = self.command_client()?;
        let auth_index = auth_index.trim();
        if auth_index.is_empty() {
            return Err(HomeError::other("home: auth_index is empty"));
        }
        let req = RefreshRequest {
            kind: "refresh".into(),
            auth_index: auth_index.to_string(),
            observed_access_token_sha256: access_token_sha256.trim().to_string(),
        };
        let key = serde_json::to_vec(&req).map_err(HomeError::other)?;
        let value = self
            .exec_with_read_timeout(&pool, &[arg("get"), key], REFRESH_OPERATION_TIMEOUT)
            .await?;
        match bulk_opt(value)? {
            None => Err(HomeError::AuthNotFound),
            Some(raw) if raw.is_empty() => Err(HomeError::EmptyResponse),
            Some(raw) => Ok(raw),
        }
    }

    // ---- reporting ----

    async fn push(&self, command: &str, key: &str, payload: &[u8], skip_empty: bool) -> Result<(), HomeError> {
        let pool = self.command_client()?;
        if skip_empty && payload.is_empty() {
            return Ok(());
        }
        self.exec(&pool, &[arg(command), arg(key), payload.to_vec()]).await.map(|_| ())
    }

    pub async fn lpush_usage(&self, payload: &[u8]) -> Result<(), HomeError> {
        self.push("lpush", KEY_USAGE, payload, true).await
    }

    /// Publishes a bounded in-flight observation frame.
    pub async fn lpush_in_flight_snapshot(&self, payload: &[u8]) -> Result<(), HomeError> {
        self.push("lpush", KEY_IN_FLIGHT_SNAPSHOT, payload, false).await
    }

    pub async fn rpush_request_log(&self, payload: &[u8]) -> Result<(), HomeError> {
        self.push("rpush", KEY_REQUEST_LOG, payload, true).await
    }

    pub async fn rpush_app_log(&self, payload: &[u8]) -> Result<(), HomeError> {
        self.push("rpush", KEY_APP_LOG, payload, true).await
    }

    pub async fn rpush_plugin_status(&self, payload: &[u8]) -> Result<(), HomeError> {
        self.push("rpush", KEY_PLUGIN_STATUS, payload, true).await
    }

    /// Sends one cumulative concurrency release frame through the independent release pool.
    pub async fn push_concurrency_release(&self, frame: &ConcurrencyReleaseFrame) -> Result<(), HomeError> {
        if frame.credential_id.is_empty() || frame.model.is_empty() || frame.release_seq <= 0 {
            return Err(HomeError::other("invalid concurrency release frame"));
        }
        let pool = self.concurrency_release_client()?;
        let payload = serde_json::to_vec(frame)
            .map_err(|e| HomeError::Other(format!("marshal concurrency release frame: {e}")))?;
        self.exec(&pool, &[arg("LPUSH"), arg(KEY_CONCURRENCY_RELEASE), payload]).await.map(|_| ())
    }

    fn concurrency_release_client(&self) -> Result<Arc<Pool>, HomeError> {
        if self.dispatch_fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        let not_ready = |state: u32| {
            matches!(state, RECOVERY_TAKEOVER_ELIGIBLE | RECOVERY_SWITCHING | RECOVERY_SWITCHING_TAKEOVER)
        };
        if not_ready(self.recovery_state()) {
            return Err(HomeError::NotConnected);
        }
        if !self.enabled() {
            return Err(HomeError::Disabled);
        }
        let mut s = self.state.lock();
        if self.dispatch_fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        if not_ready(self.recovery_state()) {
            return Err(HomeError::NotConnected);
        }
        if let Some(release) = &s.release {
            return Ok(release.clone());
        }
        let Some(addr) = Self::addr_locked(&s) else {
            return Err(HomeError::Other(format!(
                "home: invalid address (host={:?} port={})",
                s.home_cfg.host, s.home_cfg.port
            )));
        };
        let opts = Self::conn_opts_locked(&s, &addr)?;
        // The release pool is never tracked by the dispatch fence.
        let pool = Pool::new(opts, Arc::new(Tracker::new(Arc::new(AtomicBool::new(false)))));
        s.release = Some(pool.clone());
        Ok(pool)
    }

    /// Whether the release pool exists (tests).
    pub fn has_release_client(&self) -> bool {
        self.state.lock().release.is_some()
    }

    // ---- plugins ----

    pub async fn get_plugin_tasks(&self) -> Result<Vec<PluginTask>, HomeError> {
        match bulk_opt(self.run(&[arg("get"), arg(KEY_PLUGIN_TASKS)]).await?)? {
            None => Ok(Vec::new()),
            Some(raw) if raw.is_empty() => Ok(Vec::new()),
            Some(raw) => serde_json::from_slice(&raw).map_err(HomeError::other),
        }
    }

    /// Fetches the plugin sync response for `request` (JSON) on a dedicated connection with a
    /// two minute read timeout. The raw response is returned; an `unsupported` reply from an
    /// older Home becomes [`HomeError::PluginSyncUnsupported`].
    pub async fn get_plugin_sync(&self, request: &[u8]) -> Result<Vec<u8>, HomeError> {
        self.ensure_clients()?;
        let opts = {
            let s = self.state.lock();
            s.cmd_opts.clone().ok_or(HomeError::NotConnected)?
        };
        let mut sync_opts = (*opts).clone();
        sync_opts.read_timeout = PLUGIN_SYNC_OPERATION_TIMEOUT;
        let mut conn = Conn::dial(Arc::new(sync_opts), None).await?;
        let reply = conn.call(&[arg("get"), arg(KEY_PLUGIN_SYNC), request.to_vec()]).await;
        drop(conn);
        let value = match reply {
            Ok(v) => v,
            Err(e) => {
                if let Some(message) = plugin_sync_unsupported_message(&e.to_string()) {
                    return Err(HomeError::PluginSyncUnsupported(message));
                }
                return Err(e);
            }
        };
        let raw = bulk_opt(value)?.ok_or_else(|| HomeError::Other("redis: nil".into()))?;
        if raw.is_empty() {
            return Err(HomeError::EmptyResponse);
        }
        if let Some(message) = plugin_sync_unsupported_response(&raw) {
            return Err(HomeError::PluginSyncUnsupported(message));
        }
        Ok(raw)
    }

    // ---- lifecycle config and subscription ----

    /// Applies Home's authoritative lifecycle settings (defaults filled, then validated).
    pub fn set_lifecycle_config(&self, cfg: CredentialConcurrencyConfig) -> Result<(), HomeError> {
        let cfg = cfg.with_defaults();
        cfg.validate().map_err(|e| {
            HomeError::Other(format!("validate credential concurrency lifecycle config: {e}"))
        })?;
        self.state.lock().lifecycle = cfg.clone();
        *self.limiter.write() = Some(Arc::new(cfg));
        Ok(())
    }

    /// The latest validated Home limiter configuration (defaults before any config arrived).
    pub fn limiter_config(&self) -> CredentialConcurrencyConfig {
        match &*self.limiter.read() {
            Some(cfg) => (**cfg).clone(),
            None => CredentialConcurrencyConfig::default().with_defaults(),
        }
    }

    /// `SUBSCRIBE` arguments and the receive timeout (Go: `subscriptionParameters`).
    pub fn subscription_parameters(&self) -> (Vec<String>, Duration) {
        let s = self.state.lock();
        let cfg = s.lifecycle.clone().with_defaults();
        let mut timeout = cfg.cpa_heartbeat_timeout.to_std();
        if let Some(test) = s.test_operation_timeout
            && cfg.lifecycle_config_revision == 0
        {
            timeout = test;
        }
        let mut args = vec![CHANNEL_CONFIG.to_string()];
        if cfg.lifecycle_config_revision > 0 {
            args.push(cfg.lifecycle_config_revision.to_string());
            if s.legacy_membership {
                return (args, timeout);
            }
            let state = self.recovery_state();
            if state == RECOVERY_TAKEOVER_ELIGIBLE || state == RECOVERY_SWITCHING_TAKEOVER {
                args.push("takeover".into());
            }
            args.push(s.instance_id.clone());
        }
        (args, timeout)
    }

    fn mark_membership_takeover_eligible(&self) {
        if self
            .recovery_state
            .compare_exchange(RECOVERY_STABLE, RECOVERY_TAKEOVER_ELIGIBLE, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            let _ = self.recovery_state.compare_exchange(
                RECOVERY_SWITCHING,
                RECOVERY_SWITCHING_TAKEOVER,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }

    /// Drops the bootstrap command pool, then proves a fresh one works (Go:
    /// `rebuildCommandPoolAndProbe`).
    async fn rebuild_command_pool_and_probe(&self) -> Result<(), HomeError> {
        self.promote_subscription();
        self.ping().await?;
        self.recovery_state.store(RECOVERY_STABLE, Ordering::SeqCst);
        Ok(())
    }

    /// Go: `promoteSubscription`.
    pub fn promote_subscription(&self) {
        let cmd = {
            let mut s = self.state.lock();
            s.cmd_opts = None;
            s.cmd.take()
        };
        if let Some(cmd) = cmd {
            cmd.close();
        }
    }

    fn handle_subscription_payload(
        &self,
        channel: &str,
        payload: &str,
        on_config: &mut (dyn FnMut(&[u8]) -> Result<(), HomeError> + Send),
    ) -> Result<(), HomeError> {
        let payload = payload.trim();
        if payload.is_empty() {
            return Ok(());
        }
        match channel.trim().to_lowercase().as_str() {
            CHANNEL_CONFIG => on_config(payload.as_bytes()),
            CHANNEL_CLUSTER => self.update_cluster_nodes_from_payload(payload.as_bytes()),
            _ => Ok(()),
        }
    }

    fn end_lifetime(&self, err: HomeError) -> HomeError {
        self.heartbeat_ok.store(false, Ordering::SeqCst);
        if !self.managed_lifetime() {
            self.close();
        }
        err
    }

    /// One GET, SUBSCRIBE and receive lifetime (Go: `RunConfigSubscriberLifetime`).
    /// Reconnection is owned by the service so each replacement can install a new client
    /// lifetime. Returns when the heartbeat is lost, a stage fails or `cancel` fires.
    pub async fn run_config_subscriber_lifetime(
        &self,
        cancel: &Cancel,
        on_config: &mut (dyn FnMut(&[u8]) -> Result<(), HomeError> + Send),
        on_ready: &mut (dyn FnMut() + Send),
    ) -> Result<(), HomeError> {
        if !self.enabled() {
            return Err(HomeError::Disabled);
        }
        let canceled = || cancel.is_dead();
        self.close_bootstrap_pools();
        if let Err(e) = self.ensure_clients() {
            if !canceled() {
                self.mark_reconnect_failure("connect");
            }
            return Err(self.end_lifetime(e));
        }

        let raw = match with_cancel(cancel, self.get_config()).await {
            Ok(raw) => raw,
            Err(e) => {
                if !canceled() {
                    self.mark_reconnect_failure("config fetch");
                }
                return Err(self.end_lifetime(e));
            }
        };
        if let Err(e) = on_config(&raw) {
            return Err(self.end_lifetime(e));
        }

        let sub_pool = match self.subscription_client() {
            Ok(p) => p,
            Err(e) => {
                if !canceled() {
                    self.mark_reconnect_failure("subscribe client");
                }
                return Err(self.end_lifetime(e));
            }
        };
        let (args, receive_timeout) = self.subscription_parameters();
        // The subscription gets its own connection, tracked by the subscription pool.
        let mut conn = match with_cancel(cancel, Conn::dial(sub_pool.opts.clone(), Some(sub_pool.tracker.clone()))).await {
            Ok(c) => c,
            Err(e) => {
                if !canceled() {
                    self.mark_reconnect_failure("subscribe");
                }
                return Err(self.end_lifetime(e));
            }
        };
        let mut wire = vec![arg("subscribe")];
        wire.extend(args.iter().map(arg));
        let ack = async {
            conn.send(&wire).await?;
            self.receive_subscription_ack(&mut conn, receive_timeout, &args[0]).await
        };
        if let Err(e) = with_cancel(cancel, ack).await {
            if !canceled() {
                self.mark_reconnect_failure("subscribe");
            }
            drop(conn);
            return Err(self.end_lifetime(e));
        }
        // A protocol-one ACK means Home already committed this membership. Preserve it if the
        // command probe fails.
        if args.len() > 1 {
            self.mark_membership_takeover_eligible();
        }

        if let Err(e) = with_cancel(cancel, self.rebuild_command_pool_and_probe()).await {
            if !canceled() {
                self.mark_reconnect_failure("command probe");
            }
            drop(conn);
            return Err(self.end_lifetime(e));
        }
        self.reset_reconnect_failures();
        self.heartbeat_ok.store(true, Ordering::SeqCst);
        on_ready();

        loop {
            let (_, receive_timeout) = self.subscription_parameters();
            let event = with_cancel(cancel, conn.recv(receive_timeout)).await.and_then(parse_pubsub);
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    if !canceled() {
                        if self.heartbeat_ok.load(Ordering::SeqCst) {
                            self.mark_membership_takeover_eligible();
                        }
                        if e.is_timeout() {
                            self.mark_subscription_timeout();
                        } else {
                            self.mark_reconnect_failure("subscription");
                        }
                    }
                    drop(conn);
                    return Err(self.end_lifetime(e));
                }
            };
            match event {
                PubSubEvent::Message { channel, payload } => {
                    if self.handle_subscription_payload(&channel, &payload, on_config).is_err() {
                        if channel.trim().eq_ignore_ascii_case(CHANNEL_CLUSTER) {
                            tracing::warn!("failed to apply cluster update from home control center, ignoring");
                        } else {
                            tracing::warn!("failed to apply config update from home control center, ignoring");
                        }
                    }
                }
                PubSubEvent::Pong => self.reset_reconnect_failures(),
                PubSubEvent::Subscription { .. } => {}
                PubSubEvent::Other => tracing::debug!("home subscription returned unsupported message type"),
            }
        }
    }

    async fn receive_subscription_ack(&self, conn: &mut Conn, timeout: Duration, channel: &str) -> Result<(), HomeError> {
        let value = conn.recv(timeout).await?;
        match parse_pubsub(value)? {
            PubSubEvent::Subscription { kind, channel: got, count } if kind == "subscribe" && got == channel && count == 1 => {
                Ok(())
            }
            _ => Err(HomeError::other("invalid Home subscription ACK")),
        }
    }
}

/// Runs `fut` until it finishes or `cancel` fires (Go: context cancellation).
async fn with_cancel<T>(cancel: &Cancel, fut: impl std::future::Future<Output = Result<T, HomeError>>) -> Result<T, HomeError> {
    tokio::select! {
        _ = cancel.wait() => Err(HomeError::Other("context canceled".into())),
        r = fut => r,
    }
}

fn host_from_address(addr: &str) -> String {
    let addr = addr.trim();
    match addr.rsplit_once(':') {
        Some((host, _)) => host.trim_start_matches('[').trim_end_matches(']').trim().to_string(),
        None => addr.to_string(),
    }
}

fn plugin_sync_unsupported_code(code: &str) -> bool {
    code.trim().eq_ignore_ascii_case(PLUGIN_SYNC_UNSUPPORTED_ERROR_TYPE)
}

/// Go: `pluginSyncUnsupportedMessage`.
pub fn plugin_sync_unsupported_message(message: &str) -> Option<String> {
    let message = message.trim().to_lowercase();
    let message = message.strip_prefix("err ").unwrap_or(&message).trim().to_string();
    match message.as_str() {
        PLUGIN_SYNC_UNSUPPORTED_ERROR_TYPE | "unsupported key" | "wrong number of arguments for 'get' command" => Some(message),
        _ => None,
    }
}

/// Go: `pluginSyncUnsupportedResponse`.
pub fn plugin_sync_unsupported_response(raw: &[u8]) -> Option<String> {
    #[derive(Deserialize, Default)]
    struct Detail {
        #[serde(default)]
        code: String,
        #[serde(default, rename = "type")]
        kind: String,
        #[serde(default)]
        message: String,
    }
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        error: Detail,
    }
    let response: Response = serde_json::from_slice(raw).ok()?;
    let d = response.error;
    if plugin_sync_unsupported_code(&d.code) || plugin_sync_unsupported_code(&d.kind) {
        let message = d.message.trim();
        return Some(if message.is_empty() { PLUGIN_SYNC_UNSUPPORTED_ERROR_TYPE.into() } else { message.into() });
    }
    plugin_sync_unsupported_message(&d.message)
}

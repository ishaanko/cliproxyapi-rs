//! `cliproxy`: server entry point (Go: cmd/server/main.go).
//!
//! The binary wires config, the auth store and the HTTP server. Executor registration, auth file
//! loading and the management API are supplied by the service layer, which embeds
//! `cpa_server::build_router_with_management`; run standalone this binary serves the proxy surface
//! with whatever the process-wide registry and `Manager` contain.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cpa_auth::OAuthSessions;
use cpa_config::Config;
use cpa_runtime::service::{ServiceBuilder, StoreBackend};
use cpa_runtime::usage::UsageTracker;
use cpa_server::aistudio;
use cpa_server::cli::{self, Command, ParseOutcome};
use cpa_server::logging::{self, LogControl};
use cpa_server::redis_protocol::RedisProtocol;
use cpa_server::reqlog::RequestLogger;
use cpa_management::ManagementState;
use cpa_plugin::adapters::service::ServiceHooks;
use cpa_server::{AppState, BuildInfo, KeepAlive, ServerModelExecutor, build_router_with_management, safemode, serve};

#[cfg(feature = "pgo-dump")]
mod pgo_dump;

// mimalloc cut CPU per request by ~20% and raised throughput ~30% against glibc malloc in the
// bench harness (jemalloc: ~17%, with more resident memory). Plugins are unaffected: only
// host-allocated buffers use libc `malloc`/`free` (the plugin releases them through the host's
// `free_buffer`); buffers a plugin returns go back through the plugin's own `free_buffer`.
//
// mimalloc reserves its first 1 GiB arena fully committed on Linux, so freed memory is reset
// (MADV_FREE) instead of decommitted and the resident set sits ~17 MB higher at idle (more under
// load) with no CPU benefit. `.cargo/config.toml` compiles libmimalloc-sys with
// `-DMI_DEFAULT_ARENA_EAGER_COMMIT=0`; the runtime option `MIMALLOC_ARENA_EAGER_COMMIT` still
// overrides it.
#[cfg(all(feature = "mimalloc", not(feature = "alloc-stats")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// Benchmark hook (`--features alloc-stats`): counts allocations around the chosen allocator.
#[cfg(all(feature = "mimalloc", feature = "alloc-stats"))]
#[global_allocator]
static GLOBAL: cpa_allocstats::Counting<mimalloc::MiMalloc> = cpa_allocstats::Counting::new(mimalloc::MiMalloc);

#[cfg(all(not(feature = "mimalloc"), feature = "alloc-stats"))]
#[global_allocator]
static GLOBAL: cpa_allocstats::Counting<std::alloc::System> = cpa_allocstats::Counting::new(std::alloc::System);

/// `pprof` feature: samples all threads for `CPA_PPROF_SECS` seconds and writes folded stacks
/// (root first, `;` separated, one line per distinct stack) to `CPA_PPROF_OUT`.
#[cfg(feature = "pprof")]
fn start_pprof() {
    use std::io::Write;
    let (Ok(out), Ok(secs)) = (std::env::var("CPA_PPROF_OUT"), std::env::var("CPA_PPROF_SECS")) else {
        return;
    };
    let Ok(secs) = secs.parse::<u64>() else {
        return;
    };
    let Ok(guard) = pprof::ProfilerGuardBuilder::default().frequency(999).blocklist(&["libc", "libgcc", "pthread", "vdso"]).build() else {
        return;
    };
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        let Ok(report) = guard.report().build() else {
            return;
        };
        let Ok(mut file) = std::fs::File::create(out) else {
            return;
        };
        for (frames, count) in &report.data {
            let mut line = frames.thread_name.clone();
            for symbols in frames.frames.iter().rev() {
                for sym in symbols.iter().rev() {
                    line.push(';');
                    line.push_str(&sym.name());
                }
            }
            let _ = writeln!(file, "{line} {count}");
        }
    });
}

fn build_info() -> BuildInfo {
    BuildInfo {
        version: option_env!("CPA_VERSION").unwrap_or("dev").to_string(),
        commit: option_env!("CPA_COMMIT").unwrap_or("none").to_string(),
        build_date: option_env!("CPA_BUILD_DATE").unwrap_or("unknown").to_string(),
    }
}

/// Polls a worker runs between checks of the I/O driver and timers (tokio's `event_interval`,
/// default 61). A longer interval polls the driver less often under load, which saves CPU on
/// streaming workloads; the price is that under saturation (a worker never idle) timers and I/O
/// readiness can be noticed up to this many polls late. `CPA_EVENT_INTERVAL` overrides the
/// default; zero or unparsable values are ignored.
fn event_interval() -> u32 {
    const DEFAULT: u32 = 1024;
    std::env::var("CPA_EVENT_INTERVAL").ok().and_then(|v| v.trim().parse().ok()).filter(|&n| n > 0).unwrap_or(DEFAULT)
}

fn main() {
    #[cfg(feature = "alloc-stats")]
    cpa_allocstats::serve_from_env();
    #[cfg(feature = "pprof")]
    start_pprof();
    #[cfg(feature = "pgo-dump")]
    pgo_dump::start();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().event_interval(event_interval()).build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(run());
    // Match Go: stopping closes everything immediately.
    std::process::exit(code);
}

async fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let program = std::env::args().next().unwrap_or_else(|| "cliproxy".into());
    let build = build_info();
    if args.first().map(String::as_str) == Some("discover") {
        return cpa_server::discover_cmd::run_subcommand(&args[1..], &build).await;
    }
    let json_discover = args.iter().any(|a| a.trim_start_matches('-') == "discover-json");
    if !json_discover {
        println!("CLIProxyAPI Version: {}, Commit: {}, BuiltAt: {}", build.version, build.commit, build.build_date);
    }
    let log = logging::init();

    // Plugins are loaded from the config on disk first so the flags they declare parse like
    // built-in ones (Go: `pluginHost.ApplyConfig` + `RegisterCommandLineFlags` before `flag.Parse`).
    let plugin_host = cpa_plugin::Host::new();
    let mut plugin_flags: Vec<cli::PluginFlag> = Vec::new();
    if !json_discover && !args.iter().any(|a| matches!(a.trim_start_matches('-'), "discover" | "discover-json")) {
        let bootstrap = load_plugin_bootstrap_config(&plugin_bootstrap_config_path(&args));
        plugin_host.apply_config(&cpa_plugin::CallCtx::background(), Some(Arc::new(bootstrap))).await;
        let declared = plugin_host.register_command_line_flags(&cpa_plugin::CallCtx::background(), &cli::builtin_flag_names()).await;
        plugin_flags = declared
            .into_iter()
            .map(|d| cli::PluginFlag {
                is_bool: d.kind == "bool",
                default: plugin_host.command_line_flag_default(&d.name),
                name: d.name,
                usage: d.usage,
            })
            .collect();
    }

    let cli = match cli::parse_with(&args, &plugin_flags) {
        ParseOutcome::Run(c) => *c,
        ParseOutcome::Help => {
            eprint!("{}", cli::usage_with(&program, &plugin_flags));
            return 0;
        }
        ParseOutcome::Error(msg) => {
            eprintln!("{msg}");
            eprint!("{}", cli::usage_with(&program, &plugin_flags));
            return 2;
        }
    };
    if cli.discover || cli.discover_json {
        return cpa_server::discover_cmd::run_flags(&cli).await;
    }
    for (name, value) in &cli.plugin_flags {
        if let Err(e) = plugin_host.set_command_line_flag(name, value) {
            eprintln!("invalid value {value:?} for flag -{name}: {e}");
            eprint!("{}", cli::usage_with(&program, &plugin_flags));
            return 2;
        }
    }
    if let Some(flag) = cli.unsupported.first() {
        eprintln!("flag -{flag} is not supported by this build");
        return 2;
    }
    let mut cli = cli;
    // Go: `lookupEnv("HOME_JWT", "home_jwt")` when the flag is empty.
    if cli.home_jwt.trim().is_empty() {
        cli.home_jwt = ["HOME_JWT", "home_jwt"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .map(|v| v.trim().to_string())
            .find(|v| !v.is_empty())
            .unwrap_or_default();
    }

    let wd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("failed to get working directory: {e}");
            return 0;
        }
    };
    if let Err(e) = cpa_config::load_dotenv(&wd) {
        tracing::warn!("failed to load .env file: {e}");
    }
    let cloud_deploy = std::env::var("DEPLOY").is_ok_and(|v| v == "cloud");

    let mut config_path = if cli.config.is_empty() {
        wd.join("config.yaml")
    } else {
        std::path::PathBuf::from(&cli.config)
    };
    let home_mode = !cli.home_jwt.trim().is_empty();
    // PGSTORE_* / OBJECTSTORE_* / GITSTORE_*: the remote store owns the spool config and auth dir.
    // Home mode disables local stores.
    let store_backend = match cpa_store::open_from_env(&wd, home_mode) {
        Ok(Some(opened)) => {
            config_path = opened.config_path;
            cli::set_token_store(opened.backend.store.clone());
            Some(opened.backend)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::error!("{e}");
            return 0;
        }
    };
    let mut home_boot = None;
    let mut cfg = if home_mode {
        match boot_home(&cli, &plugin_host).await {
            Ok(boot) => {
                let cfg = boot.cfg.clone();
                home_boot = Some(boot);
                cfg
            }
            Err(()) => return 0,
        }
    } else {
        match cpa_config::load_config_optional(&config_path, cloud_deploy) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to load config: {e}");
                return 0;
            }
        }
    };

    if cloud_deploy && !home_mode {
        let usable = match std::fs::metadata(&config_path) {
            Err(_) => {
                tracing::info!("Cloud deploy mode: No configuration file detected; standing by for configuration");
                false
            }
            Ok(m) if m.is_dir() => {
                tracing::info!("Cloud deploy mode: Config path is a directory; standing by for configuration");
                false
            }
            Ok(_) if cfg.port == 0 => {
                tracing::info!("Cloud deploy mode: Configuration file is empty or invalid; standing by for valid configuration");
                false
            }
            Ok(_) => {
                tracing::info!("Cloud deploy mode: Configuration file detected; starting service");
                true
            }
        };
        if !usable && cli.command().is_none() {
            tracing::info!("Cloud deploy mode: No config found; standing by for configuration. API server is not started. Press Ctrl+C to exit.");
            shutdown_signal().await;
            tracing::info!("Cloud deploy mode: Shutdown signal received; exiting");
            return 0;
        }
    }

    if let Err(e) = log.apply_config(&cfg) {
        tracing::error!("failed to configure log output: {e}");
        return 0;
    }
    tracing::info!("CLIProxyAPI Version: {}, Commit: {}, BuiltAt: {}", build.version, build.commit, build.build_date);
    if let Some(backend) = &store_backend {
        cfg.auth_dir = backend.auth_dir.to_string_lossy().into_owned();
    }
    if let Err(e) = cli::resolve_auth_dir(&mut cfg) {
        tracing::error!("failed to resolve auth directory: {e}");
        return 0;
    }

    plugin_host.apply_config(&cpa_plugin::CallCtx::background(), Some(Arc::new(cfg.clone()))).await;
    if let Some(boot) = home_boot.take()
        && !finish_home_boot(boot, &plugin_host).await
    {
        return 0;
    }
    if plugin_host.has_triggered_command_line_flags() {
        let builtin = cli.builtin_flag_values();
        let (code, handled) = plugin_host
            .execute_command_line(&cpa_plugin::CallCtx::background(), &program, &args, &config_path.to_string_lossy(), &builtin)
            .await;
        if handled {
            plugin_host.shutdown_all(&cpa_plugin::CallCtx::background()).await;
            return code;
        }
    }

    if let Some(command) = cli.command() {
        return match command {
            Command::VertexImport => {
                cli::run_vertex_import(&cfg, &cli.vertex_import, &cli.vertex_import_prefix);
                0
            }
            Command::Login(kind) => cli::run_login(&cfg, &cli, kind).await,
        };
    }

    if cli.local_model && (!cli.tui || cli.standalone) {
        tracing::info!("Local model mode: using embedded model catalogs, remote model updates disabled");
    }
    if cli.tui {
        return if cli.standalone {
            run_standalone_tui(cfg, config_path, store_backend, &cli, build, log, plugin_host).await
        } else {
            // Pure management client: the proxy server must already be running.
            let base_url = resolve_management_base_url(&cli.management_base_url, &cfg);
            if let Err(e) = cpa_tui::run_with_base_url(&base_url, &cli.password, None).await {
                eprintln!("TUI error: {e}");
            }
            0
        };
    }
    let local = LocalManagement {
        password: cli.password.clone(),
        keep_alive: !cli.password.is_empty(),
        handle_signals: true,
    };
    serve_proxy(cfg, config_path, store_backend, build, log, local, None, plugin_host).await
}

/// `resolveManagementBaseURL`: flag, then `remote-management.base-url`, then localhost.
fn resolve_management_base_url(flag_url: &str, cfg: &Config) -> String {
    let flag_url = flag_url.trim();
    if !flag_url.is_empty() {
        return flag_url.to_string();
    }
    let configured = cfg.remote_management.base_url.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    let port = if cfg.port > 0 { cfg.port } else { 8317 };
    format!("http://127.0.0.1:{port}")
}

/// `--tui --standalone`: runs the server in-process, waits for the management API, then attaches
/// the TUI (logs flow to it through the log tap) and stops the server when the TUI exits.
async fn run_standalone_tui(
    cfg: Config,
    config_path: std::path::PathBuf,
    store_backend: Option<StoreBackend>,
    cli: &cli::Cli,
    build: BuildInfo,
    log: Arc<LogControl>,
    plugin_host: Arc<cpa_plugin::Host>,
) -> i32 {
    let hook = cpa_tui::LogHook::new(2000);
    let tap_hook = hook.clone();
    log.attach_tui(Arc::new(move |line| tap_hook.push(line)));
    // Like Go, point stdout/stderr at /dev/null so nothing the embedded server prints can draw
    // over the TUI; the TUI keeps the original terminal.
    let stdio = StdioRedirect::new().ok();
    let tui_output: Box<dyn std::io::Write> = match stdio.as_ref().and_then(|s| s.terminal().ok()) {
        Some(terminal) => Box::new(terminal),
        None => Box::new(std::io::stdout()),
    };

    let password = if cli.password.is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("tui-{}-{}", std::process::id(), nanos)
    } else {
        cli.password.clone()
    };
    let port = if cfg.port > 0 { cfg.port } else { 8317 };

    // No keep-alive endpoint here: the TUI's lifetime owns the server.
    // The embedded server also ignores SIGINT/SIGTERM (Go's background service only stops on
    // cancel); the TUI handles them and then stops the server.
    let local = LocalManagement { password: password.clone(), keep_alive: false, handle_signals: false };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_proxy(cfg, config_path, store_backend, build, log.clone(), local, Some(stop_rx), plugin_host));

    let client = cpa_tui::Client::new(port, &password);
    let mut ready = false;
    let mut backoff = Duration::from_millis(100);
    for _ in 0..30 {
        if client.get_config().await.is_ok() {
            ready = true;
            break;
        }
        // The server task ending (bind failure, bad config) means it will never become ready.
        if server.is_finished() {
            break;
        }
        tokio::time::sleep(backoff).await;
        if backoff < Duration::from_secs(1) {
            backoff = backoff.mul_f64(1.5);
        }
    }

    if !ready {
        log.detach_tui();
        let _ = stop_tx.send(());
        let _ = server.await;
        drop(tui_output);
        if let Some(stdio) = stdio {
            stdio.restore();
        }
        eprintln!("TUI error: embedded server is not ready");
        return 0;
    }
    let tui_result = cpa_tui::run_with_output(port, &password, Some(hook), tui_output).await;
    log.detach_tui();
    if let Some(stdio) = stdio {
        stdio.restore();
    }
    if let Err(e) = tui_result {
        eprintln!("TUI error: {e}");
    }
    let _ = stop_tx.send(());
    let _ = server.await;
    0
}

/// Redirects fd 1 and 2 to /dev/null (Go reassigns `os.Stdout`/`os.Stderr` the same way) and keeps
/// the originals so the TUI can draw to the real terminal and the streams can be restored.
#[cfg(unix)]
struct StdioRedirect {
    out: std::os::fd::OwnedFd,
    err: std::os::fd::OwnedFd,
}

#[cfg(unix)]
impl StdioRedirect {
    fn new() -> std::io::Result<Self> {
        let out = rustix::io::dup(std::io::stdout())?;
        let err = rustix::io::dup(std::io::stderr())?;
        let devnull = std::fs::OpenOptions::new().read(true).write(true).open("/dev/null")?;
        rustix::stdio::dup2_stdout(&devnull)?;
        rustix::stdio::dup2_stderr(&devnull)?;
        Ok(StdioRedirect { out, err })
    }

    /// A handle on the original stdout for the TUI.
    fn terminal(&self) -> std::io::Result<std::fs::File> {
        Ok(std::fs::File::from(self.out.try_clone()?))
    }

    fn restore(self) {
        let _ = rustix::stdio::dup2_stdout(&self.out);
        let _ = rustix::stdio::dup2_stderr(&self.err);
    }
}

/// Windows has no fd-level redirect here; the TUI keeps using stdout.
#[cfg(not(unix))]
struct StdioRedirect;

#[cfg(not(unix))]
impl StdioRedirect {
    fn new() -> std::io::Result<Self> {
        Err(std::io::Error::other("unsupported"))
    }

    fn terminal(&self) -> std::io::Result<std::fs::File> {
        Err(std::io::Error::other("unsupported"))
    }

    fn restore(self) {}
}

/// What the Home boot hands to the rest of the start: the config plus the bootstrap client and
/// plugin report that are finished once the plugins are loaded.
struct HomeBoot {
    cfg: Config,
    client: Arc<cpa_home::Client>,
    node_id: String,
    report: cpa_pluginstore::homeplugins::SyncReport,
    report_ready: bool,
}

/// Go: the `-home-jwt` branch of `main`: enroll for mTLS, fetch the config from Home, install the
/// plugins Home assigns, report the result and hand the parsed config to the service. `Err`
/// means the process should exit after the error was logged.
async fn boot_home(cli: &cli::Cli, plugin_host: &Arc<cpa_plugin::Host>) -> Result<HomeBoot, ()> {
    let timeout = Duration::from_secs(30);
    let mut home_cfg = match tokio::time::timeout(timeout, cpa_home::certificate::config_from_jwt(&cli.home_jwt)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            tracing::error!("invalid -home-jwt: {e}");
            return Err(());
        }
        Err(_) => {
            tracing::error!("invalid -home-jwt: context deadline exceeded");
            return Err(());
        }
    };
    if cli.home_disable_cluster_discovery {
        home_cfg.disable_cluster_discovery = true;
    }
    let client = Arc::new(cpa_home::Client::new(home_cfg.clone()));
    let raw = match tokio::time::timeout(timeout, client.get_config()).await {
        Ok(Ok(raw)) => raw,
        Ok(Err(e)) => {
            tracing::error!("failed to fetch config from home: {e}");
            client.close();
            return Err(());
        }
        Err(_) => {
            tracing::error!("failed to fetch config from home: context deadline exceeded");
            client.close();
            return Err(());
        }
    };
    let mut parsed = match cpa_config::parse_config_bytes(&raw) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to parse config payload from home: {e}");
            client.close();
            return Err(());
        }
    };
    parsed.home = home_cfg.clone();
    parsed.port = cpa_config::normalize_home_port(parsed.port);
    parsed.usage_statistics_enabled = true;
    // The sync keeps `plugins.store-auth`; the running config must not.
    let sync_cfg = parsed.clone();
    parsed.plugins.store_auth.clear();
    cpa_runtime::service::force_home_runtime_config(&mut parsed);
    let sync = cpa_server::plugin_home::startup_sync(&client, &sync_cfg, plugin_host).await;
    if let Some(e) = &sync.error {
        tracing::error!("failed to sync plugins from home: {e}");
    }
    if sync.ready
        && let Err(e) = cpa_server::plugin_home::push_status(&client, &home_cfg.node_id, sync.report.clone()).await
    {
        tracing::warn!("failed to report home plugin sync status: {e}");
    }
    if sync.error.is_some() {
        client.close();
        return Err(());
    }
    Ok(HomeBoot { cfg: parsed, client, node_id: home_cfg.node_id, report: sync.report, report_ready: sync.ready })
}

/// Go: after `pluginHost.ApplyConfig(cfg)` in Home mode: record which synced plugins loaded, report
/// that and release the bootstrap client. `false` means the start must stop.
async fn finish_home_boot(boot: HomeBoot, plugin_host: &Arc<cpa_plugin::Host>) -> bool {
    let mut report = boot.report;
    let mut ok = true;
    if boot.report_ready {
        let inspector = cpa_server::plugin_home::HostRuntime(plugin_host.clone());
        let load_error = cpa_pluginstore::homeplugins::mark_load_results(&mut report, Some(&inspector));
        if let Err(e) = cpa_server::plugin_home::push_status(&boot.client, &boot.node_id, report).await {
            tracing::warn!("failed to report home plugin load status: {e}");
        }
        if let Some(e) = load_error {
            tracing::error!("failed to load home plugins: {e}");
            ok = false;
        }
    }
    // The bootstrap client is not owned by the service; release its connection.
    boot.client.close();
    ok
}

/// Starts the app-log forwarder with the Home lifetime (Go: `startHomeLogForwarder`).
struct ServerHomeHooks(cpa_server::home_app_log::HomeAppLogForwarder);

impl cpa_runtime::service::HomeHooks for ServerHomeHooks {
    fn bind(&self, client: Arc<cpa_home::Client>) {
        self.0.bind(client);
    }

    fn deactivate(&self, client: &Arc<cpa_home::Client>) {
        self.0.deactivate(client);
    }

    fn stop(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.0.stop())
    }
}

/// Local management password, whether the idle-shutdown keep-alive endpoint is enabled, and
/// whether SIGINT/SIGTERM stop the server.
struct LocalManagement {
    password: String,
    keep_alive: bool,
    handle_signals: bool,
}

/// Serves the proxy until the listener fails, a signal arrives, keep-alive idles out, or `stop`
/// fires (standalone TUI exit).
async fn serve_proxy(
    cfg: Config,
    config_path: std::path::PathBuf,
    store_backend: Option<StoreBackend>,
    build: BuildInfo,
    log: Arc<LogControl>,
    local: LocalManagement,
    stop: Option<tokio::sync::oneshot::Receiver<()>>,
    plugin_host: Arc<cpa_plugin::Host>,
) -> i32 {
    let safe_mode = !cfg.home.enabled && safemode::has_example_api_keys(&cfg.api_keys);
    if safe_mode {
        tracing::error!(
            api_keys = %safemode::example_api_keys(&cfg.api_keys).join(","),
            "unsafe example API key configured; proxy API endpoints disabled until api-keys is updated"
        );
    }

    // Control panel asset: snapshot the config, then start the periodic updater (Go: SetCurrentConfig
    // + StartAutoUpdater right before the service starts). A UI embedded in the binary replaces
    // the downloaded panel, so there is nothing to update then.
    cpa_managementasset::set_current_config(Some(Arc::new(cfg.clone())));
    if cpa_server::ui::index().is_none() {
        cpa_managementasset::start_auto_updater(tokio_util::sync::CancellationToken::new(), &config_path.to_string_lossy());
    }

    // The service owns config reload, the credential manager, the auth store and model
    // registration; executors are registered through its builder by the executor layer.
    let usage = Arc::new(UsageTracker::default());
    // Usage records and error events also feed the Redis-protocol output (Go: redisqueue plugin).
    cpa_runtime::usage_queue::install(&usage);
    cpa_home::queue::set_usage_statistics_enabled(cfg.usage_statistics_enabled);
    cpa_home::queue::set_retention_seconds(cfg.redis_usage_queue_retention_seconds);
    let (compat_factory, compat_slot) = cpa_executors::openai_compat::lazy_factory();
    let plugin_hooks = Arc::new(ServiceHooks(plugin_host.clone()));
    let mut builder = ServiceBuilder::new(&config_path)
        .dotenv_dir(None)
        .usage(usage.clone())
        .executor_factory(compat_factory)
        .plugins(plugin_hooks);
    if let Some(backend) = store_backend {
        builder = builder.store_backend(backend);
    }
    if cfg.home.enabled {
        builder = builder
            .home_plugins(cpa_server::plugin_home::ServerHomePlugins::new(plugin_host.clone()))
            .initial_config(cfg.clone())
            .home_hooks(Arc::new(ServerHomeHooks(cpa_server::home_app_log::HomeAppLogForwarder::start(0))));
    }
    let service = match builder.build() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!("failed to build proxy service: {e}");
            return 0;
        }
    };
    // Per-entry openai-compatibility executors are built on demand and need the live config.
    compat_slot.set(service.subscribe_config());
    // Plugins are loaded before the service starts so plugin auth files parse and plugin
    // models register on the first pass (Go: `pluginHost.ApplyConfig` in cmd/server).
    plugin_host.set_auth_manager(Some(service.manager()));
    plugin_host.sync_runtime_config(&service.config(), &service.manager(), &usage).await;
    refresh_plugin_routes(&plugin_host).await;
    if let Err(e) = service.start().await {
        tracing::error!("failed to build proxy service: {e}");
        return 0;
    }
    let config_rx = service.subscribe_config();
    cpa_managementasset::follow_config(config_rx.clone());
    for executor in cpa_executors::all_executors(config_rx.clone()) {
        service.register_executor(executor);
    }
    aistudio::install_relay_hooks(&service);
    aistudio::watch_ws_auth(config_rx.clone());
    plugin_host.sync_model_runtime(&service.manager(), service.registry()).await;
    service.refresh_model_registrations().await;
    let manager = service.manager();
    manager.set_error_event_sink(Some(Arc::new(|payload: Vec<u8>| cpa_home::queue::enqueue_error(&payload))));
    let store = service.store();
    let sessions = Arc::new(OAuthSessions::default());

    let mut state = AppState::new(config_rx.clone(), manager.clone(), store.clone(), sessions.clone(), usage.clone());
    state.build = build.clone();
    state.example_api_key_safe_mode = safe_mode;
    state.config_file_path = config_path.to_string_lossy().into_owned();
    state.plugins = Some(plugin_host.clone());
    plugin_host.set_model_executor(Some(ServerModelExecutor::new(state.clone())));
    if !cfg.commercial_mode {
        state.request_logger = Some(Arc::new(RequestLogger::new(config_rx.clone(), config_path.parent().map(|p| p.to_path_buf()))));
    }
    let mut idle_shutdown: Option<tokio::sync::mpsc::Receiver<()>> = None;
    if local.keep_alive {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        state.keep_alive = Some(KeepAlive { password: local.password.clone(), heartbeat: tx });
        idle_shutdown = Some(rx);
    }

    // Logins and management deletes go through the registered token store (Go: GetTokenStore).
    let token_store: Arc<dyn cpa_auth::Store> = service.token_store();
    let login = cpa_auth::Manager::new(token_store.clone()).with_sessions(sessions.clone());
    let reload_service = service.clone();
    let mut management = ManagementState::new(
        &config_path,
        config_rx.clone(),
        manager,
        store,
        sessions,
        login,
        usage,
        logging::resolve_log_directory(&cfg),
    )
    .with_token_store(token_store)
    .with_build_info(cpa_management::BuildInfo {
        version: build.version.clone(),
        commit: build.commit.clone(),
        build_date: build.build_date.clone(),
    })
    .with_plugin_host(plugin_host.clone())
    .with_reload_hook(Arc::new(move || {
        let service = reload_service.clone();
        Box::pin(async move {
            service.reload_config().await;
        })
    }));
    if !local.password.is_empty() {
        management = management.with_local_password(local.password.clone());
    }

    // Plugin runtime follows config reloads (Go: `applyConfigRuntime`).
    {
        let mut rx = config_rx.clone();
        let (host, service, usage) = (plugin_host.clone(), service.clone(), service.usage());
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let next = rx.borrow().clone();
                host.sync_runtime_config(&next, &service.manager(), &usage).await;
                refresh_plugin_routes(&host).await;
                host.sync_model_runtime(&service.manager(), service.registry()).await;
                service.refresh_model_registrations().await;
            }
        });
    }
    // The usage queue runs while management is available (or Home owns usage) and follows
    // config reloads like Go's `managementRoutesEnabled` bookkeeping.
    let has_secret = !cfg.remote_management.secret_key.is_empty() || management.has_env_secret() || management.has_local_password();
    let routes_enabled = Arc::new(AtomicBool::new(has_secret));
    cpa_home::queue::set_enabled(has_secret || cfg.home.enabled);
    {
        let mut rx = config_rx.clone();
        let routes_enabled = routes_enabled.clone();
        let env_secret = management.has_env_secret();
        let mut last = (cfg.usage_statistics_enabled, cfg.redis_usage_queue_retention_seconds);
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let next = rx.borrow().clone();
                let key = (next.usage_statistics_enabled, next.redis_usage_queue_retention_seconds);
                if key.0 != last.0 {
                    cpa_home::queue::set_usage_statistics_enabled(key.0);
                }
                if key.1 != last.1 {
                    cpa_home::queue::set_retention_seconds(key.1);
                }
                last = key;
                let enabled = env_secret || !next.remote_management.secret_key.is_empty();
                routes_enabled.store(enabled, Ordering::SeqCst);
                cpa_home::queue::set_enabled(enabled || next.home.enabled);
            }
        });
    }
    let redis = Arc::new(RedisProtocol { config: config_rx.clone(), management: management.clone(), routes_enabled });

    // Log level / destination follow config reloads.
    {
        let mut rx = config_rx.clone();
        let log = log.clone();
        let mut last = (cfg.debug, cfg.logging_to_file, cfg.logs_max_total_size_mb);
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let next = rx.borrow().clone();
                let key = (next.debug, next.logging_to_file, next.logs_max_total_size_mb);
                if key != last {
                    last = key;
                    if let Err(e) = log.apply_config(&next) {
                        tracing::error!("failed to reconfigure log output: {e}");
                    }
                }
            }
        });
    }

    // The provider redirect routes (`/anthropic/callback`, ...) live in the proxy router.
    let app = build_router_with_management(state, cpa_management::router(management));
    let server = serve::serve_with_redis(&cfg, app, Some(redis));
    let idle = async move {
        match idle_shutdown.as_mut() {
            None => std::future::pending::<()>().await,
            Some(rx) => loop {
                match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                    Ok(Some(())) => {}
                    _ => {
                        tracing::warn!("keep-alive endpoint idle for 10s, shutting down");
                        return;
                    }
                }
            },
        }
    };
    tokio::select! {
        result = server => {
            if let Err(e) = result {
                tracing::error!("proxy service exited with error: {e}");
            }
        }
        _ = async {
            if local.handle_signals {
                shutdown_signal().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {}
        _ = idle => {}
        _ = service.home_fatal() => {}
        _ = async {
            match stop {
                Some(rx) => {
                    let _ = rx.await;
                }
                None => std::future::pending::<()>().await,
            }
        } => {}
    }
    service.shutdown_home().await;
    service.shutdown_graceful().await;
    plugin_host.set_model_executor(None);
    plugin_host.shutdown_runtime(&service.manager(), service.registry()).await;
    0
}

/// Go `pluginBootstrapConfigPath`: the config file named by `-config` (else the default).
fn plugin_bootstrap_config_path(args: &[String]) -> std::path::PathBuf {
    let default = || std::env::current_dir().map(|d| d.join("config.yaml")).unwrap_or_else(|_| "config.yaml".into());
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--" => return default(),
            "-config" | "--config" => return args.get(i + 1).map(std::path::PathBuf::from).unwrap_or_else(default),
            _ => {}
        }
        if let Some(v) = arg.strip_prefix("-config=").or_else(|| arg.strip_prefix("--config=")) {
            return v.into();
        }
        i += 1;
    }
    default()
}

/// Go `loadPluginBootstrapConfig`: the config file, or an empty config when it is missing or
/// invalid.
fn load_plugin_bootstrap_config(path: &std::path::Path) -> Config {
    let empty = || {
        let mut cfg = Config::default();
        cfg.normalize_plugins_config();
        cfg
    };
    match std::fs::read(path) {
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("failed to read plugin bootstrap config: {e}");
            }
            empty()
        }
        Ok(raw) if raw.iter().all(u8::is_ascii_whitespace) => empty(),
        Ok(raw) => match cpa_config::parse_config_bytes(&raw) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("failed to parse plugin bootstrap config: {e}");
                empty()
            }
        },
    }
}

/// Go `RefreshPluginManagementRoutes`: plugin routes may not shadow built-in management routes.
async fn refresh_plugin_routes(host: &Arc<cpa_plugin::Host>) {
    let reserved: std::collections::HashSet<String> = cpa_management::GIN_ROUTES
        .iter()
        .filter(|(_, path)| path.starts_with("/v0/management/") || *path == "/v0/management")
        .map(|(method, path)| format!("{} {path}", method.to_uppercase()))
        .collect();
    host.register_management_routes(&cpa_plugin::CallCtx::background(), &reserved).await;
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

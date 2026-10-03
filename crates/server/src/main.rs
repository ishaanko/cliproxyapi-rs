//! `cliproxy`: server entry point (Go: cmd/server/main.go).
//!
//! The binary wires config, the auth store and the HTTP server. Executor registration, auth file
//! loading and the management API are supplied by the service layer, which embeds
//! `cpa_server::build_router_with_management`; run standalone this binary serves the proxy surface
//! with whatever the process-wide registry and `Manager` contain.

use std::sync::Arc;
use std::time::Duration;

use cpa_auth::OAuthSessions;
use cpa_config::Config;
use cpa_runtime::service::ServiceBuilder;
use cpa_runtime::usage::UsageTracker;
use cpa_server::cli::{self, Command, ParseOutcome};
use cpa_server::logging::{self, LogControl};
use cpa_server::reqlog::RequestLogger;
use cpa_management::ManagementState;
use cpa_server::{AppState, BuildInfo, KeepAlive, build_router_with_management, safemode, serve};

fn build_info() -> BuildInfo {
    BuildInfo {
        version: option_env!("CPA_VERSION").unwrap_or("dev").to_string(),
        commit: option_env!("CPA_COMMIT").unwrap_or("none").to_string(),
        build_date: option_env!("CPA_BUILD_DATE").unwrap_or("unknown").to_string(),
    }
}

fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
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
    let json_discover = args.iter().any(|a| a.trim_start_matches('-') == "discover-json");
    if !json_discover {
        println!("CLIProxyAPI Version: {}, Commit: {}, BuiltAt: {}", build.version, build.commit, build.build_date);
    }
    let log = logging::init();

    let cli = match cli::parse(&args) {
        ParseOutcome::Run(c) => *c,
        ParseOutcome::Help => {
            eprint!("{}", cli::usage(&program));
            return 0;
        }
        ParseOutcome::Error(msg) => {
            eprintln!("{msg}");
            eprint!("{}", cli::usage(&program));
            return 2;
        }
    };
    if let Some(flag) = cli.unsupported.first() {
        eprintln!("flag -{flag} is not supported by this build");
        return 2;
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

    let config_path = if cli.config.is_empty() {
        wd.join("config.yaml")
    } else {
        std::path::PathBuf::from(&cli.config)
    };
    let mut cfg = match cpa_config::load_config_optional(&config_path, cloud_deploy) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to load config: {e}");
            return 0;
        }
    };

    if cloud_deploy {
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
    if let Err(e) = cli::resolve_auth_dir(&mut cfg) {
        tracing::error!("failed to resolve auth directory: {e}");
        return 0;
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

    if cli.local_model {
        tracing::info!("Local model mode: using embedded model catalogs, remote model updates disabled");
    }
    serve_proxy(cfg, config_path, &cli, build, log).await
}

async fn serve_proxy(cfg: Config, config_path: std::path::PathBuf, cli: &cli::Cli, build: BuildInfo, log: Arc<LogControl>) -> i32 {
    let safe_mode = safemode::has_example_api_keys(&cfg.api_keys);
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
    let (compat_factory, compat_slot) = cpa_executors::openai_compat::lazy_factory();
    let service = match ServiceBuilder::new(&config_path)
        .dotenv_dir(None)
        .usage(usage.clone())
        .executor_factory(compat_factory)
        .build()
    {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!("failed to build proxy service: {e}");
            return 0;
        }
    };
    // Per-entry openai-compatibility executors are built on demand and need the live config.
    compat_slot.set(service.subscribe_config());
    if let Err(e) = service.start().await {
        tracing::error!("failed to build proxy service: {e}");
        return 0;
    }
    let config_rx = service.subscribe_config();
    cpa_managementasset::follow_config(config_rx.clone());
    for executor in cpa_executors::all_executors(config_rx.clone()) {
        service.register_executor(executor);
    }
    let manager = service.manager();
    let store = service.store();
    let sessions = Arc::new(OAuthSessions::default());

    let mut state = AppState::new(config_rx.clone(), manager.clone(), store.clone(), sessions.clone(), usage.clone());
    state.build = build.clone();
    state.example_api_key_safe_mode = safe_mode;
    state.config_file_path = config_path.to_string_lossy().into_owned();
    if !cfg.commercial_mode {
        state.request_logger = Some(Arc::new(RequestLogger::new(config_rx.clone(), config_path.parent().map(|p| p.to_path_buf()))));
    }
    let mut idle_shutdown: Option<tokio::sync::mpsc::Receiver<()>> = None;
    if !cli.password.is_empty() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        state.keep_alive = Some(KeepAlive { password: cli.password.clone(), heartbeat: tx });
        idle_shutdown = Some(rx);
    }

    let login = cpa_auth::Manager::new(store.clone()).with_sessions(sessions.clone());
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
    .with_build_info(cpa_management::BuildInfo {
        version: build.version.clone(),
        commit: build.commit.clone(),
        build_date: build.build_date.clone(),
    })
    .with_reload_hook(Arc::new(move || {
        let service = reload_service.clone();
        Box::pin(async move {
            service.reload_config().await;
        })
    }));
    if !cli.password.is_empty() {
        management = management.with_local_password(cli.password.clone());
    }

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
    let server = serve::serve(&cfg, app);
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
        _ = shutdown_signal() => {}
        _ = idle => {}
    }
    service.shutdown();
    0
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

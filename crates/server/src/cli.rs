//! Command line (Go: cmd/server/main.go flags, internal/cmd/*_login.go, vertex_import.go).
//!
//! Parsing follows Go's `flag` package: `-flag` and `--flag`, `-flag=value` or `-flag value`,
//! booleans as `-flag` / `-flag=false`.

use std::path::PathBuf;
use std::sync::Arc;

use cpa_auth::{AuthFlowError, FileTokenStore, LoginOptions, Provider, SaveOptions, Store};
use cpa_config::Config;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cli {
    pub config: String,
    pub codex_login: bool,
    pub codex_device_login: bool,
    pub claude_login: bool,
    pub antigravity_login: bool,
    pub kimi_login: bool,
    pub kimi_ai_login: bool,
    pub xai_login: bool,
    pub devin_login: bool,
    pub meta_login: bool,
    pub no_browser: bool,
    pub oauth_callback_port: i64,
    pub vertex_import: String,
    pub vertex_import_prefix: String,
    pub password: String,
    pub local_model: bool,
    /// `-home-jwt`: Home control plane JWT (config and credentials come from Home).
    pub home_jwt: String,
    pub home_disable_cluster_discovery: bool,
    /// `-tui`: start the terminal management UI instead of the server.
    pub tui: bool,
    /// `-standalone`: with `-tui`, run an embedded server in-process.
    pub standalone: bool,
    /// `-management-base-url`: remote management API for TUI client mode.
    pub management_base_url: String,
    /// Flags accepted for compatibility but not implemented in this build.
    pub unsupported: Vec<String>,
    /// Plugin-declared flags given on the command line, in order, with their raw values.
    pub plugin_flags: Vec<(String, String)>,
}

/// A flag a plugin declared (Go: registered on `flag.CommandLine` by the plugin host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFlag {
    pub name: String,
    pub usage: String,
    /// `bool` flags may omit their value.
    pub is_bool: bool,
    /// The flag's default as the host renders it.
    pub default: String,
}

/// Names of the flags this binary owns (what `flag.Lookup` finds before plugin flags exist).
pub fn builtin_flag_names() -> std::collections::HashSet<String> {
    BOOL_FLAGS.iter().chain(VALUE_FLAGS).map(|n| n.to_string()).collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseOutcome {
    Run(Box<Cli>),
    /// `-h` / `-help`: print usage, exit 0.
    Help,
    /// Bad flag: message for stderr, exit 2.
    Error(String),
}

const BOOL_FLAGS: &[&str] = &[
    "codex-login",
    "codex-device-login",
    "claude-login",
    "antigravity-login",
    "kimi-login",
    "kimi-ai-login",
    "xai-login",
    "devin-login",
    "meta-login",
    "no-browser",
    "local-model",
    "tui",
    "standalone",
    "discover",
    "discover-json",
    "home-disable-cluster-discovery",
];

const VALUE_FLAGS: &[&str] = &[
    "config",
    "oauth-callback-port",
    "vertex-import",
    "vertex-import-prefix",
    "password",
    "management-base-url",
    "home-jwt",
    "discover-timeout",
    "discover-service-type",
    "discover-include",
    "discover-exclude",
];

/// Flags that exist in Go but have no counterpart here.
const UNSUPPORTED: &[&str] = &[
    "discover",
    "discover-json",
    "discover-timeout",
    "discover-service-type",
    "discover-include",
    "discover-exclude",
];

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// Parses `args` (without the program name).
pub fn parse(args: &[String]) -> ParseOutcome {
    parse_with(args, &[])
}

/// [`parse`] accepting the plugin-declared flags `extra` as well.
pub fn parse_with(args: &[String], extra: &[PluginFlag]) -> ParseOutcome {
    let mut cli = Cli::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        // Flag parsing stops at the first non-flag argument or a lone `--`.
        if arg == "--" {
            break;
        }
        if arg == "-" || !arg.starts_with('-') {
            break;
        }
        let trimmed = arg.trim_start_matches('-');
        if arg.starts_with("---") || trimmed.is_empty() || trimmed.starts_with('=') {
            return ParseOutcome::Error(format!("bad flag syntax: {arg}"));
        }
        let (name, inline_value) = match trimmed.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (trimmed, None),
        };
        i += 1;
        if name == "h" || name == "help" {
            return ParseOutcome::Help;
        }
        let plugin_flag = extra.iter().find(|f| f.name == name);
        let value = if plugin_flag.is_some_and(|f| f.is_bool) {
            match inline_value {
                None => "true".to_string(),
                Some(v) => match parse_bool(&v) {
                    Some(_) => v,
                    None => return ParseOutcome::Error(format!("invalid boolean value {v:?} for -{name}: parse error")),
                },
            }
        } else if plugin_flag.is_some() {
            match inline_value {
                Some(v) => v,
                None => {
                    if i >= args.len() {
                        return ParseOutcome::Error(format!("flag needs an argument: -{name}"));
                    }
                    i += 1;
                    args[i - 1].clone()
                }
            }
        } else if BOOL_FLAGS.contains(&name) {
            match inline_value {
                None => "true".to_string(),
                Some(v) => match parse_bool(&v) {
                    Some(_) => v,
                    None => return ParseOutcome::Error(format!("invalid boolean value {v:?} for -{name}: parse error")),
                },
            }
        } else if VALUE_FLAGS.contains(&name) {
            match inline_value {
                Some(v) => v,
                None => {
                    if i >= args.len() {
                        return ParseOutcome::Error(format!("flag needs an argument: -{name}"));
                    }
                    i += 1;
                    args[i - 1].clone()
                }
            }
        } else {
            return ParseOutcome::Error(format!("flag provided but not defined: -{name}"));
        };
        let flag_on = || parse_bool(&value).unwrap_or(false);
        match name {
            "config" => cli.config = value,
            "codex-login" => cli.codex_login = flag_on(),
            "codex-device-login" => cli.codex_device_login = flag_on(),
            "claude-login" => cli.claude_login = flag_on(),
            "antigravity-login" => cli.antigravity_login = flag_on(),
            "kimi-login" => cli.kimi_login = flag_on(),
            "kimi-ai-login" => cli.kimi_ai_login = flag_on(),
            "xai-login" => cli.xai_login = flag_on(),
            "devin-login" => cli.devin_login = flag_on(),
            "meta-login" => cli.meta_login = flag_on(),
            "no-browser" => cli.no_browser = flag_on(),
            "local-model" => cli.local_model = flag_on(),
            "tui" => cli.tui = flag_on(),
            "standalone" => cli.standalone = flag_on(),
            "management-base-url" => cli.management_base_url = value,
            "oauth-callback-port" => match value.parse::<i64>() {
                Ok(n) => cli.oauth_callback_port = n,
                Err(e) => return ParseOutcome::Error(format!("invalid value {value:?} for flag -{name}: parse error ({e})")),
            },
            "vertex-import" => cli.vertex_import = value,
            "vertex-import-prefix" => cli.vertex_import_prefix = value,
            "password" => cli.password = value,
            other if plugin_flag.is_some() => cli.plugin_flags.push((other.to_string(), value)),
            "home-jwt" => cli.home_jwt = value,
            "home-disable-cluster-discovery" => cli.home_disable_cluster_discovery = flag_on(),
            other if UNSUPPORTED.contains(&other) => {
                let enabled = !BOOL_FLAGS.contains(&other) || flag_on();
                if enabled {
                    cli.unsupported.push(other.to_string());
                }
            }
            _ => {}
        }
    }
    ParseOutcome::Run(Box::new(cli))
}

/// Usage text (the hidden `-password` flag is not listed).
pub fn usage(program: &str) -> String {
    usage_with(program, &[])
}

/// [`usage`] listing the plugin-declared flags among the built-in ones, sorted by name.
pub fn usage_with(program: &str, extra: &[PluginFlag]) -> String {
    let rows: &[(&str, &str, &str)] = &[
        ("antigravity-login", "", "Login to Antigravity using OAuth"),
        ("claude-login", "", "Login to Claude using OAuth"),
        ("codex-device-login", "", "Login to Codex using device code flow"),
        ("codex-login", "", "Login to Codex using OAuth"),
        ("config", "string", "Configure File Path"),
        ("devin-login", "", "Login to Devin using OAuth"),
        ("home-disable-cluster-discovery", "", "Disable Home CLUSTER NODES discovery and keep using the configured -home-jwt address"),
        ("home-jwt", "string", "Home control plane JWT for mTLS certificate bootstrap and connection"),
        ("kimi-ai-login", "", "Login to Kimi.ai using OAuth"),
        ("kimi-login", "", "Login to Kimi (.com) using OAuth"),
        ("local-model", "", "Use embedded models.json and codex_client_models.json only, skip remote model catalog fetching"),
        ("management-base-url", "string", "Base URL of remote management API for TUI client mode (e.g. https://proxy.example.com)"),
        ("meta-login", "", "Login to Meta using OAuth"),
        ("no-browser", "", "Don't open browser automatically for OAuth"),
        ("oauth-callback-port", "int", "Override OAuth callback port (defaults to provider-specific port)"),
        ("standalone", "", "In TUI mode, start an embedded local server"),
        ("tui", "", "Start with terminal management UI"),
        ("vertex-import", "string", "Import Vertex service account key JSON file"),
        ("vertex-import-prefix", "string", "Prefix for Vertex model namespacing (use with -vertex-import)"),
        ("xai-login", "", "Login to xAI using OAuth"),
    ];
    // Plugin flags are `flag.Var` values: Go shows the placeholder `value` unless they are bool.
    let mut all: Vec<(String, String, String, String)> = rows
        .iter()
        .map(|(n, t, h)| (n.to_string(), t.to_string(), h.to_string(), String::new()))
        .collect();
    for f in extra {
        let ty = if f.is_bool { "" } else { "value" };
        all.push((f.name.clone(), ty.to_string(), f.usage.clone(), f.default.clone()));
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = format!("Usage of {program}\n");
    for (name, ty, help, default) in &all {
        out.push_str(&format!("  -{name}"));
        if !ty.is_empty() {
            out.push_str(&format!(" {ty}"));
        }
        out.push_str(&format!("\n    {help}"));
        if !matches!(default.as_str(), "" | "false" | "0") {
            out.push_str(&format!(" (default {default})"));
        }
        out.push('\n');
    }
    out
}

/// Which login (or import) the flags select, in Go's precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    VertexImport,
    Login(LoginKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginKind {
    Antigravity,
    Codex,
    CodexDevice,
    Claude,
    Kimi,
    KimiAi,
    Xai,
    Devin,
    Meta,
}

impl Cli {
    /// The built-in flags as `(name, value, set)` for plugin command-line executions (Go:
    /// `flag.CommandLine.VisitAll`): parsed values, defaults elsewhere.
    pub fn builtin_flag_values(&self) -> Vec<(String, String, bool)> {
        let b = |v: bool| v.to_string();
        let rows: Vec<(&str, String)> = vec![
            ("config", self.config.clone()),
            ("codex-login", b(self.codex_login)),
            ("codex-device-login", b(self.codex_device_login)),
            ("claude-login", b(self.claude_login)),
            ("antigravity-login", b(self.antigravity_login)),
            ("kimi-login", b(self.kimi_login)),
            ("kimi-ai-login", b(self.kimi_ai_login)),
            ("xai-login", b(self.xai_login)),
            ("devin-login", b(self.devin_login)),
            ("meta-login", b(self.meta_login)),
            ("no-browser", b(self.no_browser)),
            ("oauth-callback-port", self.oauth_callback_port.to_string()),
            ("vertex-import", self.vertex_import.clone()),
            ("vertex-import-prefix", self.vertex_import_prefix.clone()),
            ("password", self.password.clone()),
            ("local-model", b(self.local_model)),
            ("tui", b(self.tui)),
            ("standalone", b(self.standalone)),
            ("management-base-url", self.management_base_url.clone()),
        ];
        rows.into_iter().map(|(n, v)| (n.to_string(), v, false)).collect()
    }

    /// `commandMode` selection: first matching branch wins.
    pub fn command(&self) -> Option<Command> {
        if !self.vertex_import.is_empty() {
            return Some(Command::VertexImport);
        }
        let kinds = [
            (self.antigravity_login, LoginKind::Antigravity),
            (self.codex_login, LoginKind::Codex),
            (self.codex_device_login, LoginKind::CodexDevice),
            (self.claude_login, LoginKind::Claude),
            (self.kimi_login, LoginKind::Kimi),
            (self.kimi_ai_login, LoginKind::KimiAi),
            (self.xai_login, LoginKind::Xai),
            (self.devin_login, LoginKind::Devin),
            (self.meta_login, LoginKind::Meta),
        ];
        kinds.into_iter().find(|(on, _)| *on).map(|(_, k)| Command::Login(k))
    }
}

impl LoginKind {
    fn provider(self) -> Provider {
        match self {
            LoginKind::Antigravity => Provider::Antigravity,
            LoginKind::Codex | LoginKind::CodexDevice => Provider::Codex,
            LoginKind::Claude => Provider::Claude,
            LoginKind::Kimi => Provider::Kimi,
            LoginKind::KimiAi => Provider::KimiAi,
            LoginKind::Xai => Provider::Xai,
            LoginKind::Devin => Provider::Devin,
            LoginKind::Meta => Provider::Meta,
        }
    }

    /// Name used in the success / failure messages.
    fn display(self) -> &'static str {
        match self {
            LoginKind::Antigravity => "Antigravity",
            LoginKind::Codex => "Codex",
            LoginKind::CodexDevice => "Codex device",
            LoginKind::Claude => "Claude",
            LoginKind::Kimi => "Kimi",
            LoginKind::KimiAi => "Kimi.ai",
            LoginKind::Xai => "xAI",
            LoginKind::Devin => "Devin",
            LoginKind::Meta => "Meta",
        }
    }
}

/// Exit code convention of the Go commands: 0 unless the callback port is taken (13).
pub type ExitCode = i32;

fn auth_store(cfg: &Config) -> Arc<FileTokenStore> {
    Arc::new(FileTokenStore::with_dir(&cfg.auth_dir))
}

/// `DoXLogin`: runs one login through `cpa_auth` and prints what the Go commands print.
pub async fn run_login(cfg: &Config, cli: &Cli, kind: LoginKind) -> ExitCode {
    let manager = cpa_auth::Manager::new(auth_store(cfg));
    let mut opts = LoginOptions::cli();
    opts.no_browser = cli.no_browser;
    opts.proxy_url = cfg.proxy_url.clone();
    opts.prompt = Some(cpa_auth::login::stdin_prompt());
    // The device-code and kimi flows ignore the callback port.
    if cli.oauth_callback_port > 0 && !matches!(kind, LoginKind::Kimi | LoginKind::KimiAi) {
        opts.callback_port = u16::try_from(cli.oauth_callback_port).ok();
    }
    if kind == LoginKind::CodexDevice {
        opts.metadata.insert("codex_login_mode".into(), "device".into());
    }
    if matches!(kind, LoginKind::Kimi | LoginKind::KimiAi) {
        opts.prompt = None;
    }

    let name = kind.display();
    let session = match manager.start_login(kind.provider(), opts).await {
        Ok(s) => s,
        Err(e) => return report_login_error(kind, e),
    };
    cpa_auth::login::announce(&session, cli.no_browser);
    let outcome = match session.wait().await {
        Ok(o) => o,
        Err(e) => return report_login_error(kind, e),
    };
    if let Some(path) = &outcome.saved_path {
        println!("Authentication saved to {}", path.display());
    }
    let with_label = !matches!(kind, LoginKind::Claude | LoginKind::Codex | LoginKind::CodexDevice);
    if with_label && !outcome.auth.label.is_empty() {
        println!("Authenticated as {}", outcome.auth.label);
    }
    println!("{name} authentication successful!");
    0
}

fn report_login_error(kind: LoginKind, err: AuthFlowError) -> ExitCode {
    let name = kind.display();
    match (&err, kind) {
        (AuthFlowError::Authentication { kind: ek, .. }, LoginKind::Claude | LoginKind::Codex | LoginKind::CodexDevice) => {
            tracing::error!("{}", err.user_friendly_message());
            if *ek == cpa_auth::error::AuthErrorKind::PortInUse {
                return i32::from(ek.code());
            }
            0
        }
        (_, LoginKind::Claude | LoginKind::Codex | LoginKind::CodexDevice) => {
            println!("{name} authentication failed: {err}");
            0
        }
        _ => {
            tracing::error!("{name} authentication failed: {err}");
            0
        }
    }
}

/// `DoVertexImport`.
pub fn run_vertex_import(cfg: &Config, key_path: &str, prefix: &str) {
    let raw_path = key_path.trim();
    if raw_path.is_empty() {
        tracing::error!("vertex-import: missing service account key path");
        return;
    }
    let data = match std::fs::read(raw_path) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("vertex-import: read file failed: {e}");
            return;
        }
    };
    if serde_json::from_slice::<serde_json::Value>(&data).is_err() {
        tracing::error!("vertex-import: invalid service account json");
        return;
    }
    let imported = match cpa_auth::vertex::import_service_account(&data, "us-central1", Some(prefix)) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!("vertex-import: {e}");
            return;
        }
    };
    if imported.email.trim().is_empty() {
        tracing::warn!("vertex-import: client_email missing in service account json");
    }
    let store = auth_store(cfg);
    let mut auth = imported.auth;
    match store.save(&mut auth, SaveOptions { creation_intent: true }) {
        Ok(Some(path)) => println!("Vertex credentials imported: {}", path.display()),
        Ok(None) => {}
        Err(e) => tracing::error!("vertex-import: save credential failed: {e}"),
    }
}

/// `resolveAuthDir`: `~` expansion; the resolved path is stored back into the config.
pub fn resolve_auth_dir(cfg: &mut Config) -> Result<PathBuf, String> {
    let dir = cpa_config::resolve_auth_dir(&cfg.auth_dir).map_err(|e| e.to_string())?;
    cfg.auth_dir = dir.to_string_lossy().into_owned();
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn run(s: &str) -> Cli {
        match parse(&args(s)) {
            ParseOutcome::Run(c) => *c,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn single_and_double_dash_forms() {
        let c = run("-config a.yaml --no-browser -oauth-callback-port=1455 --claude-login=true");
        assert_eq!(c.config, "a.yaml");
        assert!(c.no_browser && c.claude_login);
        assert_eq!(c.oauth_callback_port, 1455);
        let c = run("--config=b.yaml -local-model");
        assert_eq!((c.config.as_str(), c.local_model), ("b.yaml", true));
    }

    #[test]
    fn boolean_false_disables() {
        let c = run("-codex-login=false");
        assert!(!c.codex_login);
        assert_eq!(c.command(), None);
    }

    #[test]
    fn errors_match_go_flag_package() {
        assert_eq!(parse(&args("-nope")), ParseOutcome::Error("flag provided but not defined: -nope".into()));
        assert_eq!(parse(&args("-config")), ParseOutcome::Error("flag needs an argument: -config".into()));
        assert_eq!(parse(&args("-help")), ParseOutcome::Help);
        assert!(matches!(parse(&args("-no-browser=maybe")), ParseOutcome::Error(_)));
    }

    #[test]
    fn parsing_stops_at_first_positional_argument() {
        let c = run("-config x.yaml serve -no-browser");
        assert!(!c.no_browser);
    }

    #[test]
    fn login_precedence_follows_go_order() {
        let c = run("-meta-login -codex-login -vertex-import f.json");
        assert_eq!(c.command(), Some(Command::VertexImport));
        let c = run("-meta-login -claude-login");
        assert_eq!(c.command(), Some(Command::Login(LoginKind::Claude)));
        let c = run("-codex-device-login -codex-login");
        assert_eq!(c.command(), Some(Command::Login(LoginKind::Codex)));
        assert_eq!(run("-antigravity-login -codex-login").command(), Some(Command::Login(LoginKind::Antigravity)));
    }

    #[test]
    fn unsupported_flags_are_recorded() {
        let c = run("-tui -standalone=false -discover tok");
        assert_eq!(c.unsupported, vec!["discover"]);
        assert!(c.tui && !c.standalone);
    }

    #[test]
    fn home_flags_are_parsed() {
        let c = run("-home-jwt tok -home-disable-cluster-discovery");
        assert_eq!((c.home_jwt.as_str(), c.home_disable_cluster_discovery), ("tok", true));
        assert!(c.unsupported.is_empty());
    }
}

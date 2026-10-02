//! Login flows behind one API: `start_login` returns the authorization URL and state immediately and
//! runs the rest on a background task, so the same code serves the CLI (`LoginSession::wait`) and the
//! management API (poll `OAuthSessions`, feed callbacks, cancel). Mirrors sdk/auth/*.go (CLI) and
//! internal/api/handlers/management/auth_files_provider_oauth.go (management).
//!
//! Two modes differ in the same ways the Go code paths do:
//! - `Cli`: a local callback server is started on the provider's port, errors are returned, the
//!   user may paste the callback URL after a delay, and the browser is opened by the caller.
//! - `Management`: no local server; the redirect arrives through [`OAuthSessions`] (in-process
//!   inbox or callback file), and failures become short session error messages for polling.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::antigravity::{self, AntigravityAuth};
use crate::callback_server::{CallbackServer, Flavor};
use crate::claude::{self, ClaudeAuth};
use crate::codex::{self, CodexAuth};
use crate::devin::{self, DevinAuthService, ManualPaste, ManualPasteError};
use crate::error::{AuthErrorKind, AuthFlowError, Result};
use crate::kimi::{self, KimiAuth};
use crate::manager::{PostAuthHook, save_login_record};
use crate::meta::{self, MetaAuth};
use crate::oauth::{generate_state, parse_oauth_callback};
use crate::pkce::{PkceCodes, generate_pkce_codes, generate_pkce_codes_short};
use crate::sessions::{CallbackPayload, OAuthSessions};
use crate::storage::TokenStorage;
use crate::store::Store;
use crate::types::Auth;
use crate::xai::{self, XaiAuth};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Asks the user for a line of input (manual callback paste). `Err` aborts the login.
pub type Prompt = Arc<dyn Fn(String) -> BoxFuture<std::result::Result<String, String>> + Send + Sync>;

/// Prompt that prints the message and reads one line from stdin.
pub fn stdin_prompt() -> Prompt {
    Arc::new(|message: String| {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                use std::io::{BufRead, Write};
                print!("{message}");
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line).map(|_| line).map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| e.to_string())?
        })
    })
}

/// Providers that have a login flow (the `Authenticator` registry of sdk/auth).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Claude,
    Codex,
    Antigravity,
    Xai,
    Kimi,
    KimiAi,
    /// Legacy `kimi.ai` authenticator key.
    KimiAiDot,
    Devin,
    Meta,
}

impl Provider {
    /// Accepts the CLI/management spellings and aliases.
    pub fn parse(s: &str) -> Option<Provider> {
        Some(match s.trim().to_lowercase().as_str() {
            "claude" | "anthropic" => Provider::Claude,
            "codex" | "openai" => Provider::Codex,
            "antigravity" | "anti-gravity" => Provider::Antigravity,
            "xai" | "x-ai" | "x.ai" | "grok" => Provider::Xai,
            "kimi" => Provider::Kimi,
            "kimi-ai" => Provider::KimiAi,
            "kimi.ai" => Provider::KimiAiDot,
            "devin" | "cognition" => Provider::Devin,
            "meta" | "muse" => Provider::Meta,
            _ => return None,
        })
    }

    /// `Authenticator.Provider()`: the `type` written to credential files.
    pub fn key(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Antigravity => "antigravity",
            Provider::Xai => "xai",
            Provider::Kimi => "kimi",
            Provider::KimiAi => "kimi-ai",
            Provider::KimiAiDot => "kimi.ai",
            Provider::Devin => "devin",
            Provider::Meta => "meta",
        }
    }

    /// Provider name used by OAuth sessions and callback routes (`anthropic` for Claude).
    pub fn session_name(self) -> &'static str {
        match self {
            Provider::Claude => "anthropic",
            Provider::Kimi => "kimi",
            Provider::KimiAi | Provider::KimiAiDot => "kimi-ai",
            other => other.key(),
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Antigravity => "Antigravity",
            Provider::Xai => "xAI",
            Provider::Kimi => "Kimi",
            Provider::KimiAi | Provider::KimiAiDot => "Kimi.ai",
            Provider::Devin => "Devin",
            Provider::Meta => "Meta (Muse)",
        }
    }

    /// `Authenticator.RefreshLead()`: how long before expiry the auto-refresh loop acts.
    pub fn refresh_lead(self) -> Option<Duration> {
        match self {
            Provider::Claude => Some(claude::REFRESH_LEAD),
            Provider::Codex => Some(codex::REFRESH_LEAD),
            Provider::Antigravity => Some(antigravity::REFRESH_LEAD),
            Provider::Xai => Some(xai::REFRESH_LEAD),
            Provider::Kimi | Provider::KimiAi | Provider::KimiAiDot => Some(kimi::REFRESH_LEAD),
            Provider::Devin | Provider::Meta => None,
        }
    }
}

/// Which Go code path to mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoginMode {
    #[default]
    Cli,
    Management,
}

/// `LoginOptions` plus the transport knobs the Go call sites pass separately.
#[derive(Clone, Default)]
pub struct LoginOptions {
    pub mode: LoginMode,
    pub no_browser: bool,
    /// Overrides the local callback listener port (CLI only).
    pub callback_port: Option<u16>,
    /// e.g. `codex_login_mode = device`.
    pub metadata: BTreeMap<String, String>,
    /// Per-request proxy override, `""` inherits the global/environment proxy.
    pub proxy_url: String,
    /// Manual callback paste (CLI).
    pub prompt: Option<Prompt>,
    /// Kimi only: `kimi.ai` selects the kimi.ai account domain.
    pub kimi_domain: Option<String>,
    /// Devin in management mode: the main server's `/callback` URL used as redirect URI.
    pub devin_redirect_uri: Option<String>,
}

impl LoginOptions {
    pub fn cli() -> Self {
        Self::default()
    }

    pub fn management(proxy_url: &str) -> Self {
        Self { mode: LoginMode::Management, proxy_url: proxy_url.to_string(), ..Default::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowKind {
    /// Redirect back to a callback URL.
    Browser,
    /// Device code: the user enters a code at a verification URL.
    Device,
}

/// What the caller needs to send the user off to authorize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginStart {
    pub provider: Provider,
    pub state: String,
    pub url: String,
    pub flow: FlowKind,
    pub user_code: Option<String>,
    /// Seconds until the device code expires.
    pub expires_in: Option<u64>,
    /// Local callback listener port (CLI browser flows).
    pub callback_port: Option<u16>,
}

impl LoginStart {
    /// Management response body: `{"status":"ok","url","state"}` plus device-flow extras.
    pub fn to_json(&self) -> Value {
        let mut v = json!({"status": "ok", "url": self.url, "state": self.state});
        if self.flow == FlowKind::Device {
            v["flow"] = "device".into();
            if let Some(code) = &self.user_code {
                v["user_code"] = code.clone().into();
            }
            if let Some(n) = self.expires_in {
                v["expires_in"] = n.into();
            }
        }
        v
    }
}

/// Result of a completed login.
#[derive(Debug, Clone)]
pub struct LoginOutcome {
    pub auth: Auth,
    pub saved_path: Option<PathBuf>,
}

/// Pollable view of a login's progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStatus {
    Wait,
    Done,
    Failed(String),
    /// Cancelled, expired or never registered.
    Gone,
}

/// A running login. Dropping it does not cancel the background task; call [`cancel`](Self::cancel).
pub struct LoginSession {
    start: LoginStart,
    sessions: Arc<OAuthSessions>,
    auth_dir: Option<PathBuf>,
    task: JoinHandle<std::result::Result<LoginOutcome, AuthFlowError>>,
}

impl LoginSession {
    pub fn start_info(&self) -> &LoginStart {
        &self.start
    }

    pub fn state(&self) -> &str {
        &self.start.state
    }

    pub fn status(&self) -> LoginStatus {
        match self.sessions.get(&self.start.state) {
            None => LoginStatus::Gone,
            Some(s) if s.completed => LoginStatus::Done,
            Some(s) if !s.status.is_empty() => LoginStatus::Failed(s.status),
            Some(_) => LoginStatus::Wait,
        }
    }

    /// Management poll body (`GET /oauth/status`): `(http status, json)`.
    pub fn poll_json(&self) -> (u16, Value) {
        self.sessions.poll_status(&self.start.state)
    }

    /// Feeds a redirect (pasted URL, management callback endpoint) to the running login.
    pub fn submit_callback(&self, payload: CallbackPayload) -> std::result::Result<(), crate::sessions::CallbackError> {
        self.sessions.submit_callback(
            self.auth_dir.as_deref(),
            self.start.provider.session_name(),
            &self.start.state,
            &payload.code,
            &payload.error,
        )
    }

    /// Cancels a pending login; the background task stops without saving. Returns whether it was
    /// still pending.
    pub fn cancel(&self) -> bool {
        self.sessions.cancel(&self.start.state)
    }

    /// Waits for the login to finish (CLI path).
    pub async fn wait(self) -> Result<LoginOutcome> {
        match self.task.await {
            Ok(r) => r,
            Err(e) => Err(AuthFlowError::other(format!("login task failed: {e}"))),
        }
    }
}

/// Failure of a flow: the error to return and the short message shown to pollers.
struct Fail {
    err: AuthFlowError,
    session_msg: String,
}

fn fail(err: AuthFlowError, session_msg: impl Into<String>) -> Fail {
    Fail { err, session_msg: session_msg.into() }
}

impl From<AuthFlowError> for Fail {
    fn from(err: AuthFlowError) -> Self {
        let session_msg = err.to_string();
        Fail { err, session_msg }
    }
}

/// `oauthSessionErrorWithCause`.
fn with_cause(message: &str, cause: &dyn std::fmt::Display) -> String {
    let detail = cause.to_string();
    let detail = detail.trim();
    if detail.is_empty() { message.to_string() } else { format!("{message}: {detail}") }
}

/// Everything a background flow needs.
struct Env {
    store: Arc<dyn Store>,
    sessions: Arc<OAuthSessions>,
    hook: Option<PostAuthHook>,
    opts: LoginOptions,
    provider: Provider,
    state: String,
    inbox: mpsc::UnboundedReceiver<CallbackPayload>,
    auth_dir: Option<PathBuf>,
}

impl Env {
    fn mgmt(&self) -> bool {
        self.opts.mode == LoginMode::Management
    }

    fn pending(&self) -> bool {
        self.sessions.is_pending(&self.state, self.provider.session_name())
    }

    /// `guardOAuthSessionPendingForSave` + persist + `Complete`. A cancel mid-exchange prevents saving.
    async fn finalize(&self, mut auth: Auth, save_fail_msg: &str) -> std::result::Result<LoginOutcome, Fail> {
        if !self.pending() {
            return Err(fail(AuthFlowError::Cancelled, ""));
        }
        let (store, hook) = (self.store.clone(), self.hook.clone());
        let saved = tokio::task::spawn_blocking(move || {
            let path = save_login_record(store.as_ref(), hook.as_ref(), &mut auth);
            (auth, path)
        })
        .await
        .map_err(|e| fail(AuthFlowError::other(format!("save task failed: {e}")), save_fail_msg))?;
        let (auth, path) = saved;
        let saved_path = path.map_err(|e| fail(e, save_fail_msg))?;
        self.sessions.complete(&self.state);
        Ok(LoginOutcome { auth, saved_path })
    }
}

/// Where a browser flow's result came from.
enum Waited {
    Redirect(Redirect),
    /// Devin only: a session token pasted directly.
    Token(String),
}

#[derive(Debug, Clone, Default)]
struct Redirect {
    code: String,
    state: String,
    error: String,
    /// Only from a manually pasted URL.
    description: String,
}

/// Parses pasted input; `Ok(None)` means "keep waiting".
type PasteParser = Box<dyn Fn(&str) -> std::result::Result<Option<Waited>, AuthFlowError> + Send + Sync>;

struct WaitCfg {
    timeout: Duration,
    timeout_err: AuthFlowError,
    timeout_msg: &'static str,
    /// Prompt text and delay before it is shown; `None` disables manual paste.
    prompt: Option<(&'static str, Duration)>,
    parse_paste: PasteParser,
}

fn default_paste_parser() -> PasteParser {
    Box::new(|input: &str| match parse_oauth_callback(input) {
        Err(e) => Err(AuthFlowError::other(e)),
        Ok(None) => Ok(None),
        Ok(Some(p)) => Ok(Some(Waited::Redirect(Redirect {
            code: p.code,
            state: p.state,
            error: p.error,
            description: p.error_description,
        }))),
    })
}

/// Waits for the redirect from the local server, the session inbox, a callback file (management)
/// or a manual paste, whichever comes first; fails on timeout or when the session stops being
/// pending (cancel).
async fn wait_for_redirect(env: &mut Env, server: &mut Option<CallbackServer>, cfg: WaitCfg) -> std::result::Result<Waited, Fail> {
    let deadline = tokio::time::Instant::now() + cfg.timeout;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let prompt_timer = tokio::time::sleep(cfg.prompt.map(|(_, d)| d).unwrap_or(Duration::MAX / 4));
    tokio::pin!(prompt_timer);
    let mut prompt_armed = cfg.prompt.is_some() && env.opts.prompt.is_some();
    let mut prompt_fut: Option<BoxFuture<std::result::Result<String, String>>> = None;

    loop {
        let prompt_idle = prompt_fut.is_none();
        let server_next = async {
            match server.as_mut() {
                Some(s) => s.next_result().await,
                None => std::future::pending().await,
            }
        };
        let prompt_next = async {
            match prompt_fut.as_mut() {
                Some(f) => f.await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = server_next => match r {
                Some(r) => {
                    return Ok(Waited::Redirect(Redirect { code: r.code, state: r.state, error: r.error, description: String::new() }));
                }
                None => return Err(AuthFlowError::other("OAuth callback server stopped").into()),
            },
            p = env.inbox.recv() => {
                if let Some(p) = p {
                    return Ok(Waited::Redirect(Redirect { code: p.code, state: p.state, error: p.error, description: String::new() }));
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Err(fail(cfg.timeout_err, cfg.timeout_msg));
            }
            _ = tick.tick() => {
                if !env.pending() {
                    return Err(fail(AuthFlowError::Cancelled, ""));
                }
                if let Some(dir) = env.auth_dir.clone().filter(|_| env.mgmt()) {
                    if let Some(p) = crate::sessions::take_callback_file(&dir, env.provider.session_name(), &env.state) {
                        return Ok(Waited::Redirect(Redirect { code: p.code, state: p.state, error: p.error, description: String::new() }));
                    }
                }
            }
            _ = &mut prompt_timer, if prompt_armed && prompt_idle => {
                // A browser result that is already ready wins over asking the user.
                if let Some(r) = server.as_mut().and_then(|s| s.try_result()) {
                    return Ok(Waited::Redirect(Redirect { code: r.code, state: r.state, error: r.error, description: String::new() }));
                }
                if let (Some(prompt), Some((label, _))) = (env.opts.prompt.clone(), cfg.prompt) {
                    prompt_fut = Some(prompt(label.to_string()));
                }
                prompt_armed = false;
            }
            input = prompt_next => {
                prompt_fut = None;
                let input = input.map_err(|e| AuthFlowError::other(e))?;
                if let Some(w) = (cfg.parse_paste)(&input)? {
                    return Ok(w);
                }
            }
        }
    }
}

// ---- Entry point ----

pub(crate) async fn start_login(
    store: Arc<dyn Store>,
    sessions: Arc<OAuthSessions>,
    hook: Option<PostAuthHook>,
    provider: Provider,
    opts: LoginOptions,
) -> Result<LoginSession> {
    let provider = match (provider, opts.kimi_domain.as_deref()) {
        (Provider::Kimi, Some(d)) if kimi::is_kimi_ai_domain(d) => Provider::KimiAi,
        (p, _) => p,
    };
    let auth_dir = store.base_dir();
    let (tx, inbox) = mpsc::unbounded_channel();
    let mut env = Env { store, sessions: sessions.clone(), hook, opts, provider, state: String::new(), inbox, auth_dir: auth_dir.clone() };

    let (start, runner): (LoginStart, Runner) = match provider {
        Provider::Claude => claude_start(&mut env).await?,
        Provider::Codex if is_codex_device(&env.opts) => codex_device_start(&mut env).await?,
        Provider::Codex => codex_start(&mut env).await?,
        Provider::Antigravity => antigravity_start(&mut env).await?,
        Provider::Xai => xai_start(&mut env).await?,
        Provider::Kimi | Provider::KimiAi | Provider::KimiAiDot => kimi_start(&mut env).await?,
        Provider::Devin => devin_start(&mut env).await?,
        Provider::Meta => meta_start(&mut env).await?,
    };
    env.state = start.state.clone();
    sessions.register_with_inbox(&start.state, provider.session_name(), tx);

    let state = start.state.clone();
    let sessions_for_task = sessions.clone();
    let task = tokio::spawn(async move {
        match runner(env).await {
            Ok(outcome) => Ok(outcome),
            Err(Fail { err, session_msg }) => {
                if !matches!(err, AuthFlowError::Cancelled) {
                    sessions_for_task.set_error(&state, &session_msg);
                }
                Err(err)
            }
        }
    });
    Ok(LoginSession { start, sessions, auth_dir, task })
}

type Runner = Box<dyn FnOnce(Env) -> BoxFuture<std::result::Result<LoginOutcome, Fail>> + Send>;

fn runner<F, Fut>(f: F) -> Runner
where
    F: FnOnce(Env) -> Fut + Send + 'static,
    Fut: Future<Output = std::result::Result<LoginOutcome, Fail>> + Send + 'static,
{
    Box::new(move |env| Box::pin(f(env)))
}

fn browser_start(provider: Provider, state: &str, url: String, port: Option<u16>) -> LoginStart {
    LoginStart { provider, state: state.to_string(), url, flow: FlowKind::Browser, user_code: None, expires_in: None, callback_port: port }
}

fn nanos_state(prefix: &str) -> String {
    format!("{prefix}-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default())
}

const CALLBACK_WAIT: Duration = Duration::from_secs(5 * 60);
const MANUAL_PROMPT_DELAY: Duration = Duration::from_secs(15);

// ---- Claude ----

async fn claude_start(env: &mut Env) -> Result<(LoginStart, Runner)> {
    let pkce = generate_pkce_codes();
    let state = generate_state();
    let svc = ClaudeAuth::new(&env.opts.proxy_url)?;
    let mut port = None;
    let server = if env.mgmt() {
        None
    } else {
        let s = CallbackServer::start(Flavor::Claude, env.opts.callback_port.unwrap_or(claude::DEFAULT_CALLBACK_PORT)).await?;
        port = Some(s.port());
        Some(s)
    };
    let url = svc.generate_auth_url(&state, &pkce);
    let start = browser_start(Provider::Claude, &state, url, port);
    let st = state.clone();
    Ok((start, runner(move |env| claude_run(env, svc, pkce, st, server))))
}

async fn claude_run(
    mut env: Env,
    svc: ClaudeAuth,
    pkce: PkceCodes,
    state: String,
    mut server: Option<CallbackServer>,
) -> std::result::Result<LoginOutcome, Fail> {
    let cfg = WaitCfg {
        timeout: CALLBACK_WAIT,
        timeout_err: AuthFlowError::authentication(AuthErrorKind::CallbackTimeout, "timeout waiting for OAuth callback"),
        timeout_msg: "Timeout waiting for OAuth callback",
        prompt: Some(("Paste the Claude callback URL (or press Enter to keep waiting): ", MANUAL_PROMPT_DELAY)),
        parse_paste: default_paste_parser(),
    };
    let Waited::Redirect(r) = wait_for_redirect(&mut env, &mut server, cfg).await? else {
        return Err(AuthFlowError::other("unexpected token paste").into());
    };
    drop(server);

    if !r.error.is_empty() {
        return Err(fail(AuthFlowError::OAuth { code: r.error, description: r.description }, "Bad request"));
    }
    if r.state != state {
        let err = AuthFlowError::authentication(AuthErrorKind::InvalidState, if env.mgmt() { format!("expected {state}, got {}", r.state) } else { "state mismatch".into() });
        return Err(fail(err, "State code error"));
    }

    // Management drops the `#state` suffix; the CLI passes the code through and lets the
    // exchange honor the fragment state.
    let code = if env.mgmt() { r.code.split('#').next().unwrap_or("").to_string() } else { r.code };
    let bundle = svc.exchange_code_for_tokens(&code, &state, &pkce).await.map_err(|e| {
        tracing::error!("Failed to exchange authorization code for tokens: {e}");
        fail(AuthFlowError::authentication(AuthErrorKind::CodeExchangeFailed, &e), "Failed to exchange authorization code for tokens")
    })?;

    let storage = svc.create_token_storage(&bundle);
    if storage.email.is_empty() {
        let e = AuthFlowError::other("claude token storage missing account information");
        return Err(fail(e, "Failed to exchange authorization code for tokens: missing account email"));
    }
    let file_name = claude::credential_file_name(&storage.email, &storage.organization_uuid, &storage.account_uuid);
    let mut auth = Auth::new(file_name, "claude");
    auth.metadata.insert("email".into(), storage.email.clone().into());
    for (k, v) in [
        ("account_uuid", &storage.account_uuid),
        ("organization_uuid", &storage.organization_uuid),
        ("organization_name", &storage.organization_name),
    ] {
        if !v.is_empty() {
            auth.metadata.insert(k.into(), v.clone().into());
        }
    }
    if !storage.device_ids.is_empty() {
        auth.metadata.insert(claude::DEVICE_IDS_METADATA_KEY.into(), storage.device_ids.clone().into());
    }
    auth.storage = Some(TokenStorage::Claude(storage));
    env.finalize(auth, "Failed to save authentication tokens").await
}

// ---- Codex (browser) ----

async fn codex_start(env: &mut Env) -> Result<(LoginStart, Runner)> {
    let pkce = generate_pkce_codes();
    let state = generate_state();
    let svc = CodexAuth::new(&env.opts.proxy_url)?;
    let mut port = None;
    let server = if env.mgmt() {
        None
    } else {
        let s = CallbackServer::start(Flavor::Codex, env.opts.callback_port.unwrap_or(codex::DEFAULT_CALLBACK_PORT)).await?;
        port = Some(s.port());
        Some(s)
    };
    let url = svc.generate_auth_url(&state, &pkce);
    let start = browser_start(Provider::Codex, &state, url, port);
    let st = state.clone();
    Ok((start, runner(move |env| codex_run(env, svc, pkce, st, server))))
}

async fn codex_run(
    mut env: Env,
    svc: CodexAuth,
    pkce: PkceCodes,
    state: String,
    mut server: Option<CallbackServer>,
) -> std::result::Result<LoginOutcome, Fail> {
    let cfg = WaitCfg {
        timeout: CALLBACK_WAIT,
        timeout_err: AuthFlowError::authentication(AuthErrorKind::CallbackTimeout, "timeout waiting for OAuth callback"),
        timeout_msg: "Timeout waiting for OAuth callback",
        prompt: Some(("Paste the Codex callback URL (or press Enter to keep waiting): ", MANUAL_PROMPT_DELAY)),
        parse_paste: default_paste_parser(),
    };
    let Waited::Redirect(r) = wait_for_redirect(&mut env, &mut server, cfg).await? else {
        return Err(AuthFlowError::other("unexpected token paste").into());
    };
    drop(server);

    if !r.error.is_empty() {
        return Err(fail(AuthFlowError::OAuth { code: r.error, description: r.description }, "Bad Request"));
    }
    if r.state != state {
        let cause = if env.mgmt() { format!("expected {state}, got {}", r.state) } else { "state mismatch".into() };
        return Err(fail(AuthFlowError::authentication(AuthErrorKind::InvalidState, cause), "State code error"));
    }
    let bundle = svc.exchange_code_for_tokens(&r.code, &pkce).await.map_err(|e| {
        fail(
            AuthFlowError::authentication(AuthErrorKind::CodeExchangeFailed, &e),
            with_cause("Failed to exchange authorization code for tokens", &e),
        )
    })?;
    let auth = codex::build_auth_record(&svc, &bundle, env.mgmt()).map_err(|e| fail(e, "Failed to exchange authorization code for tokens"))?;
    env.finalize(auth, "Failed to save authentication tokens").await
}

// ---- Codex (device code) ----

fn is_codex_device(opts: &LoginOptions) -> bool {
    opts.metadata
        .get(codex::LOGIN_MODE_METADATA_KEY)
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(codex::LOGIN_MODE_DEVICE))
}

async fn codex_device_start(env: &mut Env) -> Result<(LoginStart, Runner)> {
    let svc = CodexAuth::new(&env.opts.proxy_url)?;
    let code = svc.request_device_user_code().await?;
    let start = LoginStart {
        provider: Provider::Codex,
        state: nanos_state("codex"),
        url: codex::DEVICE_VERIFICATION_URL.to_string(),
        flow: FlowKind::Device,
        user_code: Some(code.user_code.clone()),
        expires_in: None,
        callback_port: None,
    };
    Ok((start, runner(move |env| codex_device_run(env, svc, code))))
}

async fn codex_device_run(env: Env, svc: CodexAuth, code: codex::DeviceUserCode) -> std::result::Result<LoginOutcome, Fail> {
    let token = cancellable(&env, svc.poll_device_token(&code)).await?.map_err(|e| fail(e.clone(), with_cause("Authentication failed", &e)))?;
    let bundle = svc.exchange_device_code(&token).await.map_err(|e| fail(e.clone(), with_cause("Failed to exchange authorization code for tokens", &e)))?;
    let auth = codex::build_auth_record(&svc, &bundle, env.mgmt()).map_err(|e| fail(e, "Failed to exchange token"))?;
    env.finalize(auth, "Failed to save authentication tokens").await
}

/// Runs `fut` but gives up (silently) once the session stops being pending (checked every 2 s,
/// like `watchOAuthSessionCancel`).
async fn cancellable<T>(env: &Env, fut: impl Future<Output = T>) -> std::result::Result<T, Fail> {
    tokio::pin!(fut);
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.tick().await;
    loop {
        tokio::select! {
            r = &mut fut => {
                if !env.pending() {
                    return Err(fail(AuthFlowError::Cancelled, ""));
                }
                return Ok(r);
            }
            _ = tick.tick() => {
                if !env.pending() {
                    return Err(fail(AuthFlowError::Cancelled, ""));
                }
            }
        }
    }
}

// ---- Antigravity ----

async fn antigravity_start(env: &mut Env) -> Result<(LoginStart, Runner)> {
    let svc = AntigravityAuth::new(&env.opts.proxy_url)?;
    svc.ensure_client_secret()?;
    let state = generate_state();
    let (server, redirect_uri, port) = if env.mgmt() {
        (None, antigravity::default_redirect_uri(), None)
    } else {
        let s = CallbackServer::start(Flavor::Antigravity, env.opts.callback_port.unwrap_or(antigravity::CALLBACK_PORT)).await?;
        let port = s.port();
        (Some(s), format!("http://localhost:{port}/oauth-callback"), Some(port))
    };
    let url = svc.build_auth_url(&state, &redirect_uri);
    let start = browser_start(Provider::Antigravity, &state, url, port);
    let st = state.clone();
    Ok((start, runner(move |env| antigravity_run(env, svc, st, redirect_uri, server))))
}

async fn antigravity_run(
    mut env: Env,
    svc: AntigravityAuth,
    state: String,
    redirect_uri: String,
    mut server: Option<CallbackServer>,
) -> std::result::Result<LoginOutcome, Fail> {
    let mgmt = env.mgmt();
    let cfg = WaitCfg {
        timeout: CALLBACK_WAIT,
        timeout_err: AuthFlowError::other("antigravity: authentication timed out"),
        timeout_msg: "OAuth flow timed out",
        prompt: Some(("Paste the antigravity callback URL (or press Enter to keep waiting): ", MANUAL_PROMPT_DELAY)),
        parse_paste: default_paste_parser(),
    };
    let Waited::Redirect(r) = wait_for_redirect(&mut env, &mut server, cfg).await? else {
        return Err(AuthFlowError::other("unexpected token paste").into());
    };
    drop(server);

    let (code, got_state, error) = (r.code.trim().to_string(), r.state.trim().to_string(), r.error.trim().to_string());
    if !error.is_empty() {
        return Err(fail(AuthFlowError::other(format!("antigravity: authentication failed: {error}")), "Authentication failed"));
    }
    // CLI requires an exact state; management only rejects a present, different one.
    let state_ok = if mgmt { got_state.is_empty() || got_state == state } else { got_state == state };
    if !state_ok {
        return Err(fail(AuthFlowError::other("antigravity: invalid state"), "Authentication failed: state mismatch"));
    }
    if code.is_empty() {
        return Err(fail(AuthFlowError::other("antigravity: missing authorization code"), "Authentication failed: code not found"));
    }

    let token = svc
        .exchange_code_for_tokens(&code, &redirect_uri)
        .await
        .map_err(|e| fail(AuthFlowError::other(format!("antigravity: token exchange failed: {e}")), "Failed to exchange token"))?;
    let access_token = token.access_token.trim().to_string();
    if access_token.is_empty() {
        return Err(fail(AuthFlowError::other("antigravity: token exchange returned empty access token"), "Failed to exchange token"));
    }
    let email = svc
        .fetch_user_info(&access_token)
        .await
        .map_err(|e| fail(AuthFlowError::other(format!("antigravity: fetch user info failed: {e}")), "Failed to fetch user info"))?;

    let project_id = match svc.fetch_project_id(&access_token).await {
        Ok(p) => p.trim().to_string(),
        Err(e) if mgmt => {
            tracing::warn!("antigravity: failed to fetch project ID: {e}");
            String::new()
        }
        Err(e) => {
            return Err(fail(AuthFlowError::other(format!("antigravity: failed to fetch project ID: {e}")), "Failed to fetch project ID"));
        }
    };
    if project_id.is_empty() && !mgmt {
        return Err(fail(AuthFlowError::other("antigravity: project ID discovery returned empty project"), "Failed to fetch project ID"));
    }
    let auth = antigravity::build_auth(&token, &email, &project_id);
    env.finalize(auth, "Failed to save token to file").await
}

// ---- xAI (device code) ----

async fn xai_start(env: &mut Env) -> Result<(LoginStart, Runner)> {
    let svc = XaiAuth::new(&env.opts.proxy_url)?;
    let device = svc.start_device_flow().await.map_err(|e| AuthFlowError::other(format!("xai: failed to start device flow: {e}")))?;
    let start = LoginStart {
        provider: Provider::Xai,
        state: nanos_state("xai"),
        url: device.verification_url(),
        flow: FlowKind::Device,
        user_code: Some(device.user_code.trim().to_string()).filter(|c| !c.is_empty()),
        expires_in: Some(if device.expires_in > 0 { device.expires_in as u64 } else { xai::MAX_POLL_DURATION.as_secs() }),
        callback_port: None,
    };
    Ok((start, runner(move |env| xai_run(env, svc, device))))
}

async fn xai_run(env: Env, svc: XaiAuth, device: xai::DeviceCodeResponse) -> std::result::Result<LoginOutcome, Fail> {
    let bundle = cancellable(&env, svc.wait_for_authorization(&device))
        .await?
        .map_err(|e| fail(AuthFlowError::other(format!("xai: {e}")), with_cause("Authentication failed", &e)))?;
    let storage = svc.create_token_storage(&bundle);
    let auth = xai::build_auth_record(storage).map_err(|e| fail(e, "Failed to exchange token"))?;
    env.finalize(auth, "Failed to save token to file").await
}

// ---- Kimi (device code) ----

async fn kimi_start(env: &mut Env) -> Result<(LoginStart, Runner)> {
    let provider = env.provider;
    let domain = match (provider, env.opts.kimi_domain.as_deref()) {
        (Provider::Kimi, Some(d)) if !d.trim().is_empty() => d.to_string(),
        (Provider::Kimi, _) => kimi::KIMI_DEFAULT_DOMAIN.to_string(),
        _ => kimi::KIMI_AI_DOMAIN.to_string(),
    };
    let svc = KimiAuth::new(&domain, &env.opts.proxy_url)?;
    let device = svc.start_device_flow().await.map_err(|e| AuthFlowError::other(format!("kimi: failed to start device flow: {e}")))?;
    let prefix = if kimi::is_kimi_ai_domain(&domain) { "kmi-ai" } else { "kmi" };
    let start = LoginStart {
        provider,
        state: nanos_state(prefix),
        url: device.verification_url().to_string(),
        flow: FlowKind::Device,
        user_code: Some(device.user_code.clone()).filter(|c| !c.is_empty()),
        expires_in: Some(device.expires_in as u64).filter(|n| *n > 0),
        callback_port: None,
    };
    Ok((start, runner(move |env| kimi_run(env, svc, device, domain))))
}

async fn kimi_run(env: Env, svc: KimiAuth, device: kimi::DeviceCodeResponse, domain: String) -> std::result::Result<LoginOutcome, Fail> {
    let bundle = cancellable(&env, svc.wait_for_authorization(&device))
        .await?
        .map_err(|e| fail(AuthFlowError::other(format!("kimi: {e}")), with_cause("Authentication failed", &e)))?;
    let storage = svc.create_token_storage(&bundle);
    let auth = kimi::build_auth_record(env.provider.key(), &domain, &bundle, storage);
    env.finalize(auth, "Failed to save authentication tokens").await
}

// ---- Devin ----

async fn devin_start(env: &mut Env) -> Result<(LoginStart, Runner)> {
    let pkce = generate_pkce_codes_short();
    let state = generate_state();
    let svc = DevinAuthService::new(&env.opts.proxy_url)?;

    if env.mgmt() {
        let redirect = env
            .opts
            .devin_redirect_uri
            .clone()
            .filter(|r| !r.trim().is_empty())
            .ok_or_else(|| AuthFlowError::Config("callback server unavailable".into()))?;
        let url = svc.build_authorization_url(&redirect, &pkce.code_challenge, &state);
        let start = browser_start(Provider::Devin, &state, url, None);
        let st = state.clone();
        return Ok((start, runner(move |env| devin_run(env, svc, pkce, st, None, false))));
    }

    if env.opts.no_browser {
        if env.opts.prompt.is_none() {
            return Err(AuthFlowError::other("devin authentication in no-browser mode requires an interactive prompt"));
        }
        let url = svc.build_authorization_url("", &pkce.code_challenge, &state);
        let start = browser_start(Provider::Devin, &state, url, None);
        let st = state.clone();
        return Ok((start, runner(move |env| devin_run(env, svc, pkce, st, None, true))));
    }

    let server = CallbackServer::start(Flavor::Devin, env.opts.callback_port.unwrap_or(0))
        .await
        .map_err(|e| AuthFlowError::other(format!("failed to start devin oauth callback server: {e}")))?;
    let port = server.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let url = svc.build_authorization_url(&redirect, &pkce.code_challenge, &state);
    let start = browser_start(Provider::Devin, &state, url, Some(port));
    let st = state.clone();
    Ok((start, runner(move |env| devin_run(env, svc, pkce, st, Some(server), false))))
}

async fn devin_run(
    mut env: Env,
    svc: DevinAuthService,
    pkce: PkceCodes,
    state: String,
    mut server: Option<CallbackServer>,
    paste_only: bool,
) -> std::result::Result<LoginOutcome, Fail> {
    let mgmt = env.mgmt();
    let expected = state.clone();
    let parse_paste: PasteParser = Box::new(move |input: &str| match devin::parse_manual_paste(input, &expected) {
        Ok(ManualPaste::Empty) => Ok(None),
        Ok(ManualPaste::Token(t)) => Ok(Some(Waited::Token(t))),
        Ok(ManualPaste::Code(c)) => Ok(Some(Waited::Redirect(Redirect { code: c, state: expected.clone(), ..Default::default() }))),
        Err(ManualPasteError::Unrecognized) => Ok(None),
        Err(e) => Err(AuthFlowError::other(e.to_string())),
    });

    let waited = if paste_only {
        // No-browser mode: one prompt, no local server.
        let Some(prompt) = env.opts.prompt.clone() else {
            return Err(AuthFlowError::other("devin authentication in no-browser mode requires an interactive prompt").into());
        };
        let input = prompt("Paste the Devin authorization code or session token directly: ".to_string())
            .await
            .map_err(|e| AuthFlowError::other(format!("failed to read devin input: {e}")))?;
        match parse_paste(&input)? {
            Some(w) => w,
            None => return Err(AuthFlowError::other("devin authentication canceled: empty input received").into()),
        }
    } else {
        let cfg = WaitCfg {
            timeout: CALLBACK_WAIT,
            timeout_err: AuthFlowError::other("devin oauth callback failed: devin authentication timed out"),
            timeout_msg: "Timeout waiting for OAuth callback",
            prompt: Some((
                "Paste the Devin callback URL, authorization code, or session token directly (or press Enter to keep waiting): ",
                Duration::from_secs(5),
            )),
            parse_paste,
        };
        wait_for_redirect(&mut env, &mut server, cfg).await?
    };
    drop(server);

    let session_token = match waited {
        Waited::Token(raw) => devin::format_session_token(&raw),
        Waited::Redirect(r) => {
            if mgmt {
                if r.state != state {
                    return Err(fail(AuthFlowError::other("devin oauth state mismatch (possible CSRF)"), "State code error"));
                }
                if !r.error.is_empty() {
                    return Err(fail(AuthFlowError::other(format!("devin oauth error: {}", r.error)), "Devin authorization denied"));
                }
            } else {
                if !r.error.is_empty() {
                    return Err(AuthFlowError::other(format!("devin oauth error: {}", r.error)).into());
                }
                if !state.is_empty() && r.state != state {
                    return Err(AuthFlowError::other("devin oauth state mismatch (possible CSRF)").into());
                }
            }
            if r.code.trim().is_empty() {
                return Err(fail(AuthFlowError::other("no authorization code or token received"), "Missing authorization code"));
            }
            let token = svc.exchange_code_for_token(&r.code, &pkce.code_verifier).await.map_err(|e| {
                fail(
                    AuthFlowError::other(format!("failed to exchange devin authorization code: {e}")),
                    "Failed to exchange authorization code for tokens",
                )
            })?;
            if token.trim().is_empty() {
                return Err(fail(AuthFlowError::other("empty token"), "Failed to exchange authorization code for tokens"));
            }
            devin::format_session_token(&token)
        }
    };
    let auth = svc
        .create_auth_record(&session_token)
        .await
        .map_err(|e| fail(e, "Failed to create Devin authentication record"))?;
    env.finalize(auth, "Failed to save authentication tokens").await
}

// ---- Meta (device code) ----

async fn meta_start(env: &mut Env) -> Result<(LoginStart, Runner)> {
    let svc = MetaAuth::new(&env.opts.proxy_url)?;
    let device = svc.start_device_flow().await.map_err(|e| AuthFlowError::other(format!("meta: failed to start device flow: {e}")))?;
    let start = LoginStart {
        provider: Provider::Meta,
        state: nanos_state("meta"),
        url: device.verification_url(),
        flow: FlowKind::Device,
        user_code: Some(device.user_code.trim().to_string()).filter(|c| !c.is_empty()),
        expires_in: Some(if device.expires_in > 0 { device.expires_in as u64 } else { meta::MAX_POLL_DURATION.as_secs() }),
        callback_port: None,
    };
    Ok((start, runner(move |env| meta_run(env, svc, device))))
}

async fn meta_run(env: Env, svc: MetaAuth, device: meta::DeviceCodeResponse) -> std::result::Result<LoginOutcome, Fail> {
    let bundle = cancellable(&env, svc.wait_for_authorization(&device))
        .await?
        .map_err(|e| fail(AuthFlowError::other(format!("meta: {e}")), with_cause("Authentication failed", &e)))?;
    let storage = svc.create_token_storage(&bundle);
    let auth = meta::build_auth_record(storage, &bundle).map_err(|e| fail(e, "Failed to exchange token"))?;
    env.finalize(auth, "Failed to save token to file").await
}

// ---- CLI presentation ----

/// Prints (or opens) the URL the way the Go CLI does. Call after `start_login`, before `wait`.
pub fn announce(session: &LoginSession, no_browser: bool) {
    let info = session.start_info();
    let name = info.provider.display_name();
    match info.flow {
        FlowKind::Browser => {
            let port = info.callback_port.unwrap_or(0);
            let show_manual = |reason: Option<String>| {
                if let Some(r) = reason {
                    tracing::warn!("{r}");
                }
                if port != 0 {
                    print!("{}", crate::browser::ssh_tunnel_instructions(port, &crate::browser::outbound_ip()));
                }
                println!("Visit the following URL to continue authentication:\n{}", info.url);
            };
            if no_browser {
                show_manual(None);
            } else {
                println!("Opening browser for {name} authentication");
                if !crate::browser::is_available() {
                    show_manual(Some("No browser available; please open the URL manually".into()));
                } else if let Err(e) = crate::browser::open_url(&info.url) {
                    show_manual(Some(format!("Failed to open browser automatically: {e}")));
                }
            }
            println!("Waiting for {name} authentication callback...");
        }
        FlowKind::Device => {
            println!("\nTo authenticate, please visit:\n{}\n", info.url);
            if let Some(code) = &info.user_code {
                println!("Then enter this code: {code}\n");
            }
            if !no_browser {
                if crate::browser::is_available() {
                    if let Err(e) = crate::browser::open_url(&info.url) {
                        tracing::warn!("Failed to open browser automatically: {e}");
                    }
                } else {
                    tracing::warn!("No browser available; please open the URL manually");
                }
            }
            println!("Waiting for authorization...");
            if let Some(n) = info.expires_in {
                println!("(This will timeout in {n} seconds if not authorized)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_parsing_and_names() {
        assert_eq!(Provider::parse("Anthropic"), Some(Provider::Claude));
        assert_eq!(Provider::parse("grok"), Some(Provider::Xai));
        assert_eq!(Provider::parse("kimi.ai"), Some(Provider::KimiAiDot));
        assert_eq!(Provider::parse("gemini"), None);
        assert_eq!(Provider::Claude.session_name(), "anthropic");
        assert_eq!(Provider::KimiAiDot.session_name(), "kimi-ai");
        assert_eq!(Provider::KimiAiDot.key(), "kimi.ai");
        assert_eq!(Provider::Codex.refresh_lead(), Some(Duration::from_secs(24 * 3600)));
        assert_eq!(Provider::Claude.refresh_lead(), Some(Duration::from_secs(4 * 3600)));
        assert_eq!(Provider::Antigravity.refresh_lead(), Some(Duration::from_secs(1800)));
        assert_eq!(Provider::Devin.refresh_lead(), None);
    }

    #[test]
    fn start_json_matches_management_responses() {
        let browser = browser_start(Provider::Claude, "st", "https://x".into(), None);
        assert_eq!(browser.to_json(), json!({"status": "ok", "url": "https://x", "state": "st"}));
        let device = LoginStart {
            provider: Provider::Xai,
            state: "xai-1".into(),
            url: "https://v".into(),
            flow: FlowKind::Device,
            user_code: Some("ABCD".into()),
            expires_in: Some(1800),
            callback_port: None,
        };
        assert_eq!(
            device.to_json(),
            json!({"status": "ok", "url": "https://v", "state": "xai-1", "flow": "device", "user_code": "ABCD", "expires_in": 1800})
        );
    }

    #[test]
    fn session_error_text_includes_cause() {
        assert_eq!(with_cause("Authentication failed", &"boom "), "Authentication failed: boom");
        assert_eq!(with_cause("Authentication failed", &"  "), "Authentication failed");
    }
}

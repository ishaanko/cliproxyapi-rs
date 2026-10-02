//! Provider executor contract (Go: sdk/cliproxy/executor + ProviderExecutor in
//! sdk/cliproxy/auth/conductor.go).
//!
//! An executor turns a translated request into an upstream call for one provider
//! (`identifier()`, e.g. "claude", "codex", "gemini", "openai-compatibility"). The conductor
//! picks an [`Auth`], calls the executor, and classifies the returned [`ExecError`] to drive
//! cooldown, retry and failover.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_translator::Format;
use http::HeaderMap;
use serde_json::Value;
use tokio::sync::mpsc;

/// Well-known metadata keys shared by handlers, the conductor and executors
/// (Go: executor.*MetadataKey constants).
pub mod meta {
    pub const REQUESTED_MODEL: &str = "requested_model";
    pub const REQUEST_PATH: &str = "request_path";
    pub const DISALLOW_FREE_AUTH: &str = "disallow_free_auth";
    pub const AUTH_SELECTION_MODEL: &str = "auth_selection_model";
    pub const REASONING_EFFORT: &str = "reasoning_effort";
    pub const SERVICE_TIER: &str = "service_tier";
    pub const GENERATE: &str = "generate";
    pub const PINNED_AUTH_ID: &str = "pinned_auth_id";
    pub const SELECTED_AUTH_ID: &str = "selected_auth_id";
    pub const SELECTED_AUTH_INDEX: &str = "selected_auth_index";
    pub const EXECUTION_SESSION_ID: &str = "execution_session_id";
    pub const DERIVED_SESSION_ID: &str = "derived_session_id";
    pub const CANONICAL_SESSION_ID: &str = "canonical_session_id";
    pub const PARENT_SESSION_ID: &str = "parent_session_id";
    pub const IS_FORK: &str = "is_fork";
    pub const IS_COMPACTION: &str = "is_compaction";
    pub const CALLER_SCOPE: &str = "caller_scope";
    pub const SESSION_AFFINITY_PROVIDER: &str = "session_affinity_provider";
    pub const SESSION_AFFINITY_MODEL: &str = "session_affinity_model";
}

/// Free-form execution hints (Go: `map[string]any`).
pub type Metadata = HashMap<String, Value>;

/// A translated request ready for an upstream provider.
#[derive(Debug, Clone)]
pub struct Request {
    /// Upstream model identifier after alias/prefix resolution.
    pub model: String,
    /// Provider-format JSON payload.
    pub payload: Bytes,
    /// Schema of `payload`.
    pub format: Format,
    pub metadata: Metadata,
}

/// Per-execution options (Go: executor.Options).
#[derive(Debug, Clone)]
pub struct Options {
    pub stream: bool,
    /// Gemini `alt` hint (e.g. "sse").
    pub alt: String,
    /// Inbound client headers forwarded to the request builder.
    pub headers: HeaderMap,
    /// Inbound query string pairs.
    pub query: Vec<(String, String)>,
    /// Inbound request bytes before translation.
    pub original_request: Bytes,
    /// Inbound client schema.
    pub source_format: Format,
    /// Downstream response schema; `None` means `source_format`.
    pub response_format: Option<Format>,
    pub metadata: Metadata,
    /// Per-execution proxy override; refresh/token exchange must ignore it.
    pub proxy_url: String,
}

impl Options {
    pub fn new(source_format: Format) -> Self {
        Options {
            stream: false,
            alt: String::new(),
            headers: HeaderMap::new(),
            query: Vec::new(),
            original_request: Bytes::new(),
            source_format,
            response_format: None,
            metadata: Metadata::new(),
            proxy_url: String::new(),
        }
    }

    /// Go: ResponseFormatOrSource.
    pub fn response_format_or_source(&self) -> Format {
        self.response_format.unwrap_or(self.source_format)
    }
}

/// A complete (non-streaming) response, already translated to the response format.
#[derive(Debug, Clone, Default)]
pub struct Response {
    pub payload: Bytes,
    pub metadata: Metadata,
    /// Upstream headers for passthrough.
    pub headers: HeaderMap,
}

/// Streaming response: upstream headers plus a channel of translated chunks. A chunk with
/// `Err` is terminal.
pub struct StreamResult {
    pub headers: HeaderMap,
    pub chunks: mpsc::Receiver<Result<Bytes, ExecError>>,
}

/// Error classes the conductor treats specially (Go: auth.ErrorCode* constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Tied to this request, not the credential: no failover, no cooldown.
    RequestScoped,
    ConnectionLifecycle,
    TransientTransport,
    ForceCooldown,
}

/// Executor failure carrying everything the conductor and handlers need: HTTP-like status,
/// upstream body/headers for passthrough, retry hints and classification flags.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ExecError {
    /// HTTP status (0 when unknown, e.g. transport errors).
    pub status: u16,
    pub message: String,
    /// Upstream error body to relay to the client when present.
    pub body: Option<Bytes>,
    pub headers: HeaderMap,
    pub retry_after: Option<Duration>,
    pub code: Option<ErrorCode>,
    /// Credential is unusable until re-login (Go: IsTerminalAuth).
    pub terminal_auth: bool,
    pub retryable: bool,
    /// The failure affects the whole credential across models, not just the requested model
    /// (Go: IsCredentialScoped, e.g. Claude unified 5h/7d limits).
    pub credential_scoped: bool,
    /// Conductor-level machine code (Go `auth.Error.Code`) for errors the conductor itself
    /// produces: `auth_not_found`, `auth_unavailable`, `model_cooldown`, `provider_not_found`,
    /// `executor_not_found`, `empty_stream`, `unauthorized`. `None` for upstream errors.
    pub auth_code: Option<String>,
    /// The request reached the upstream transport boundary (Go: upstream-attempt marker). Errors
    /// built by executors default to true; set false for failures before any request was sent
    /// so the conductor does not prefer them over a synthesized "no auth available".
    pub upstream_attempted: bool,
    /// Text of the underlying upstream error a conductor-generated error wraps (Go
    /// `WithCause`); used to render "last upstream error" details in the HTTP layer.
    pub cause_text: Option<String>,
}

impl ExecError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        ExecError {
            status,
            message: message.into(),
            body: None,
            headers: HeaderMap::new(),
            retry_after: None,
            code: None,
            terminal_auth: false,
            retryable: false,
            credential_scoped: false,
            auth_code: None,
            upstream_attempted: true,
            cause_text: None,
        }
    }

    pub fn with_body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn with_code(mut self, code: ErrorCode) -> Self {
        self.code = Some(code);
        self
    }

    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    pub fn with_credential_scope(mut self) -> Self {
        self.credential_scoped = true;
        self
    }

    pub fn is_request_scoped(&self) -> bool {
        self.code == Some(ErrorCode::RequestScoped) || self.auth_code.as_deref() == Some("request_scoped")
    }
}

/// Upstream provider integration (Go: ProviderExecutor).
#[async_trait]
pub trait Executor: Send + Sync {
    /// Provider key handled by this executor (matches `Auth::provider`).
    fn identifier(&self) -> &str;

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError>;

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError>;

    /// Refresh credentials, returning the updated auth.
    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError>;

    async fn count_tokens(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError>;

    /// Release per-session resources (e.g. pooled websockets) when a client session ends.
    async fn close_execution_session(&self, _session_id: &str) {}

    /// Execution-local view without OAuth-only configuration, used for API-key credentials
    /// (Go: APIKeyConfigExecutor.ForAPIKey). `None` means the executor itself is used.
    fn for_api_key(&self) -> Option<DynExecutor> {
        None
    }

    /// Whether the credential needs a request-time update before use (Go:
    /// RequestAuthPreparer.ShouldPrepareRequestAuth, e.g. minting a Meta API key).
    fn should_prepare_request_auth(&self, _auth: &Auth) -> bool {
        false
    }

    /// Request-time credential update; the conductor merges and persists the returned auth.
    /// `Ok(None)` keeps the credential unchanged.
    async fn prepare_request_auth(&self, _auth: &Auth) -> Result<Option<Auth>, ExecError> {
        Ok(None)
    }

    /// Whether this executor's tool contract supports the Codex `apply_patch` tool for `model`
    /// (Go: ApplyPatchSupport).
    fn supports_apply_patch(&self, _model: &str) -> bool {
        false
    }
}

pub type DynExecutor = Arc<dyn Executor>;

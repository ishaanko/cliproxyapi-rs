//! Messages flowing through the app (Go: the bubbletea `tea.Msg` types) and the command context
//! that runs async work and feeds its result back as a message (Go: `tea.Cmd`).

use std::future::Future;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::client::Client;
use crate::keys::Key;

#[derive(Debug)]
pub enum Msg {
    Key(Key),
    Paste(String),
    Resize(u16, u16),
    /// Result of the password gate's `GetConfig` probe.
    AuthConnect(Result<Value, String>),
    LocaleChanged,
    DashboardData {
        config: Value,
        auth_files: Vec<Value>,
        api_keys: Vec<String>,
        err: Option<String>,
    },
    ConfigData {
        config: Value,
        err: Option<String>,
    },
    ConfigUpdate {
        path: String,
        value: Option<Value>,
        err: Option<String>,
    },
    AuthFiles {
        files: Vec<Value>,
        err: Option<String>,
    },
    AuthAction {
        action: String,
        err: Option<String>,
    },
    KeysData(Box<KeysData>),
    KeyAction {
        action: String,
        err: Option<String>,
    },
    OAuthStart(OAuthStart),
    OAuthPoll(OAuthPoll),
    OAuthCallbackSubmit {
        err: Option<String>,
    },
    LogsPoll {
        lines: Vec<String>,
        latest: i64,
        err: Option<String>,
    },
    LogsTick,
    LogLine(String),
    /// The terminal input reader failed; the loop ends with this error.
    InputClosed(String),
}

/// Everything the API Keys tab shows.
#[derive(Debug, Default)]
pub struct KeysData {
    pub api_keys: Vec<String>,
    pub gemini: Vec<Value>,
    pub interactions: Vec<Value>,
    pub claude: Vec<Value>,
    pub codex: Vec<Value>,
    pub xai: Vec<Value>,
    pub vertex: Vec<Value>,
    pub openai: Vec<Value>,
    pub err: Option<String>,
}

#[derive(Debug, Default)]
pub struct OAuthStart {
    pub url: String,
    pub state: String,
    pub provider_name: String,
    pub user_code: String,
    pub device_flow: bool,
    pub expires_in: i64,
    pub generation: u64,
    pub err: Option<String>,
}

#[derive(Debug, Default)]
pub struct OAuthPoll {
    pub state: String,
    pub generation: u64,
    pub done: bool,
    pub message: String,
    pub err: Option<String>,
}

/// Handle tabs use to start background work. Each future may yield one follow-up message.
#[derive(Clone)]
pub struct Ctx {
    tx: UnboundedSender<Msg>,
    pub client: Arc<Client>,
}

impl Ctx {
    pub fn new(tx: UnboundedSender<Msg>, client: Arc<Client>) -> Self {
        Ctx { tx, client }
    }

    /// Runs `fut` on the tokio runtime and delivers its message, if any.
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = Option<Msg>> + Send + 'static,
    {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Some(msg) = fut.await {
                let _ = tx.send(msg);
            }
        });
    }

    /// Delivers a message immediately.
    pub fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }
}

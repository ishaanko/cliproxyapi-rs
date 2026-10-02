//! Scripted behavior of the mock upstream. The harness posts a `Script` to the control endpoint
//! before each scenario; every inbound upstream request consumes the first matching `Step`
//! (or the script's `fallback` once the steps are used up).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What kind of assistant output a successful reply carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    /// Plain text answer.
    #[default]
    Text,
    /// A single tool/function call (name taken from the first tool in the request).
    ToolCall,
    /// Reasoning/thinking followed by a text answer.
    Thinking,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    /// Successful response in the family's native shape (SSE or JSON depending on the request).
    Ok { content: Content },
    /// HTTP error. A `None` body uses the family's native error shape for the status.
    Error {
        status: u16,
        #[serde(default)]
        headers: Vec<(String, String)>,
        #[serde(default)]
        body: Option<Value>,
    },
    /// Success status, but the stream (or JSON body) is cut after `after` events. With `abort`
    /// the connection is dropped mid-response, otherwise it ends cleanly without a terminal event.
    Cut { content: Content, after: usize, abort: bool },
    /// Stream that emits `after` events and then a family-specific in-band error event.
    StreamError { content: Content, after: usize },
    /// Verbatim response, for malformed or unusual upstream behavior.
    Raw { status: u16, content_type: String, body: String },
}

impl Reply {
    pub fn ok(content: Content) -> Self {
        Reply::Ok { content }
    }

    pub fn error(status: u16) -> Self {
        Reply::Error { status, headers: vec![], body: None }
    }

    pub fn error_with(status: u16, headers: &[(&str, &str)], body: Option<Value>) -> Self {
        let headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Reply::Error { status, headers, body }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    /// Only matches requests whose credential (api key) contains this string.
    #[serde(default)]
    pub credential: Option<String>,
    /// How many requests this step answers; `None` means unlimited.
    #[serde(default)]
    pub times: Option<u32>,
    /// Wait this long before answering (to exercise keep-alives).
    #[serde(default)]
    pub delay_ms: u64,
    /// For streams: pause this long after the first chunk (to exercise keep-alives).
    #[serde(default)]
    pub stall_ms: u64,
    /// Extra response headers added to whatever reply this step produces.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub reply: Reply,
}

impl Step {
    pub fn once(reply: Reply) -> Self {
        Step { credential: None, times: Some(1), delay_ms: 0, stall_ms: 0, headers: vec![], reply }
    }

    pub fn always(reply: Reply) -> Self {
        Step { credential: None, times: None, delay_ms: 0, stall_ms: 0, headers: vec![], reply }
    }

    /// Answer `times` requests (any credential) with `reply`.
    pub fn times(times: u32, reply: Reply) -> Self {
        Step { credential: None, times: Some(times), delay_ms: 0, stall_ms: 0, headers: vec![], reply }
    }


    pub fn delayed(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    pub fn stalled(mut self, stall_ms: u64) -> Self {
        self.stall_ms = stall_ms;
        self
    }

    pub fn with_headers(mut self, headers: &[(&str, &str)]) -> Self {
        self.headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Script {
    #[serde(default)]
    pub steps: Vec<Step>,
    pub fallback: Reply,
}

impl Default for Script {
    fn default() -> Self {
        Script { steps: vec![], fallback: Reply::ok(Content::Text) }
    }
}

impl Script {
    /// Every upstream request succeeds with `content`.
    pub fn ok(content: Content) -> Self {
        Script { steps: vec![], fallback: Reply::ok(content) }
    }

    pub fn steps(steps: Vec<Step>) -> Self {
        Script { steps, ..Script::default() }
    }

}

/// The reply chosen for one request, with its step's delay and extra headers.
pub struct Pick {
    pub reply: Reply,
    pub delay_ms: u64,
    pub stall_ms: u64,
    pub headers: Vec<(String, String)>,
}

/// Mutable script cursor: remaining uses per step.
pub struct ScriptState {
    script: Script,
    remaining: Vec<Option<u32>>,
}

impl ScriptState {
    pub fn new(script: Script) -> Self {
        let remaining = script.steps.iter().map(|s| s.times).collect();
        ScriptState { script, remaining }
    }

    /// Picks the reply for a request made with `credential`, consuming a step use.
    pub fn next(&mut self, credential: &str) -> Pick {
        for (i, step) in self.script.steps.iter().enumerate() {
            if self.remaining[i] == Some(0) {
                continue;
            }
            if step.credential.as_deref().is_some_and(|c| !credential.contains(c)) {
                continue;
            }
            if let Some(n) = self.remaining[i].as_mut() {
                *n -= 1;
            }
            return Pick { reply: step.reply.clone(), delay_ms: step.delay_ms, stall_ms: step.stall_ms, headers: step.headers.clone() };
        }
        Pick { reply: self.script.fallback.clone(), delay_ms: 0, stall_ms: 0, headers: vec![] }
    }
}

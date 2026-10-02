//! Request bodies for each client dialect, and the upstream families they are routed to.

use serde_json::{Value, json};

use crate::mock::script::Content;

/// Upstream provider family a model routes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Claude,
    Codex,
    Gemini,
    Compat,
}

pub const FAMILIES: [Family; 4] = [Family::Claude, Family::Codex, Family::Gemini, Family::Compat];

impl Family {
    pub fn label(self) -> &'static str {
        match self {
            Family::Claude => "claude",
            Family::Codex => "codex",
            Family::Gemini => "gemini",
            Family::Compat => "compat",
        }
    }

    /// A model served by this family (catalog model, or the alias for the compat provider).
    pub fn model(self) -> &'static str {
        match self {
            Family::Claude => "claude-sonnet-4-5-20250929",
            Family::Codex => "gpt-5.5",
            Family::Gemini => "gemini-2.5-flash",
            Family::Compat => "compat-gpt-4o",
        }
    }
}

/// What the scenario asks the model to do; also selects the mock's reply content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Tool,
    Thinking,
}

pub const KINDS: [Kind; 3] = [Kind::Text, Kind::Tool, Kind::Thinking];

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Tool => "tool",
            Kind::Thinking => "thinking",
        }
    }

    pub fn content(self) -> Content {
        match self {
            Kind::Text => Content::Text,
            Kind::Tool => Content::ToolCall,
            Kind::Thinking => Content::Thinking,
        }
    }
}

fn weather_schema() -> Value {
    json!({"type":"object","properties":{"city":{"type":"string","description":"City name"}},"required":["city"]})
}

/// OpenAI `/v1/chat/completions` body.
pub fn chat(model: &str, stream: bool, kind: Kind) -> Value {
    let mut b = json!({"model": model, "messages": [{"role":"user","content":"What is the weather in Paris?"}], "stream": stream});
    if stream {
        b["stream_options"] = json!({"include_usage": true});
    }
    match kind {
        Kind::Text => {}
        Kind::Tool => {
            b["tools"] = json!([{"type":"function","function":{"name":"get_weather","description":"Get the weather","parameters":weather_schema()}}]);
        }
        Kind::Thinking => b["reasoning_effort"] = json!("high"),
    }
    b
}

/// OpenAI `/v1/completions` body.
pub fn completions(model: &str, stream: bool) -> Value {
    json!({"model": model, "prompt": "Say hello", "max_tokens": 32, "temperature": 0.5, "stream": stream})
}

/// OpenAI `/v1/responses` body.
pub fn responses(model: &str, stream: bool, kind: Kind) -> Value {
    let mut b = json!({"model": model, "input": "What is the weather in Paris?", "stream": stream});
    match kind {
        Kind::Text => {}
        Kind::Tool => {
            b["tools"] = json!([{"type":"function","name":"get_weather","description":"Get the weather","parameters":weather_schema()}]);
        }
        Kind::Thinking => b["reasoning"] = json!({"effort":"high"}),
    }
    b
}

/// Claude `/v1/messages` body.
pub fn claude(model: &str, stream: bool, kind: Kind) -> Value {
    let mut b = json!({"model": model, "max_tokens": 4096, "messages": [{"role":"user","content":"What is the weather in Paris?"}], "stream": stream});
    match kind {
        Kind::Text => {}
        Kind::Tool => {
            b["tools"] = json!([{"name":"get_weather","description":"Get the weather","input_schema":weather_schema()}]);
        }
        Kind::Thinking => b["thinking"] = json!({"type":"enabled","budget_tokens":2048}),
    }
    b
}

/// Gemini `generateContent` / `streamGenerateContent` body.
pub fn gemini(kind: Kind) -> Value {
    let mut b = json!({"contents":[{"role":"user","parts":[{"text":"What is the weather in Paris?"}]}]});
    match kind {
        Kind::Text => {}
        Kind::Tool => {
            b["tools"] = json!([{"functionDeclarations":[{"name":"get_weather","description":"Get the weather","parameters":weather_schema()}]}]);
        }
        Kind::Thinking => b["generationConfig"] = json!({"thinkingConfig":{"thinkingBudget":1024,"includeThoughts":true}}),
    }
    b
}

/// Gemini route for a model and method (`generateContent`, `streamGenerateContent`, `countTokens`).
pub fn gemini_path(model: &str, method: &str) -> String {
    format!("/v1beta/models/{model}:{method}")
}

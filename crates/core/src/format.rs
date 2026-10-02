//! API dialects understood by the proxy (Go: sdk/translator formats, internal/constant).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Format {
    OpenAI,
    OpenAIResponse,
    Claude,
    Gemini,
    Codex,
    Antigravity,
    Interactions,
}

impl Format {
    pub const ALL: [Format; 7] = [
        Format::OpenAI,
        Format::OpenAIResponse,
        Format::Claude,
        Format::Gemini,
        Format::Codex,
        Format::Antigravity,
        Format::Interactions,
    ];

    /// The Go string identifier (`"openai-response"` etc.).
    pub fn as_str(self) -> &'static str {
        match self {
            Format::OpenAI => "openai",
            Format::OpenAIResponse => "openai-response",
            Format::Claude => "claude",
            Format::Gemini => "gemini",
            Format::Codex => "codex",
            Format::Antigravity => "antigravity",
            Format::Interactions => "interactions",
        }
    }

    pub fn parse(s: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.as_str() == s)
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

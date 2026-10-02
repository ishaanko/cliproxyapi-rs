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

/// Provider and format identifier strings (Go: internal/constant).
pub mod constant {
    /// Google Gemini provider.
    pub const GEMINI: &str = "gemini";
    /// Native Google Interactions API provider.
    pub const GEMINI_INTERACTIONS: &str = "gemini-interactions";
    /// OpenAI Codex provider.
    pub const CODEX: &str = "codex";
    /// Anthropic Claude provider.
    pub const CLAUDE: &str = "claude";
    /// OpenAI provider.
    pub const OPENAI: &str = "openai";
    /// OpenAI Responses format.
    pub const OPENAI_RESPONSE: &str = "openai-response";
    /// Antigravity format.
    pub const ANTIGRAVITY: &str = "antigravity";
    /// Google Interactions API format.
    pub const INTERACTIONS: &str = "interactions";
}

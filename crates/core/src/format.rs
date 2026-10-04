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
    /// A format name only plugins know (Go accepts any string as a `sdktranslator.Format`);
    /// built from [`Format::intern`] so the variant stays `Copy`.
    Custom(&'static str),
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
            Format::Custom(name) => name,
        }
    }

    /// Built-in formats only; plugin format names go through [`Format::intern`].
    pub fn parse(s: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.as_str() == s)
    }

    /// The built-in format for `s`, else a custom format carrying the exact name. Empty names
    /// have no format. Custom names are interned (leaked once per distinct name; plugin format
    /// vocabularies are tiny and process-lived).
    pub fn intern(s: &str) -> Option<Format> {
        if s.is_empty() {
            return None;
        }
        if let Some(f) = Format::parse(s) {
            return Some(f);
        }
        static NAMES: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
        let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = names.iter().find(|n| **n == s) {
            return Some(Format::Custom(n));
        }
        let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
        names.push(leaked);
        Some(Format::Custom(leaked))
    }

    pub fn is_custom(self) -> bool {
        matches!(self, Format::Custom(_))
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

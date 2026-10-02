//! Thinking-signature validation and replay policy (Go: internal/signature).
//!
//! Pure functions over strings and JSON bodies; there is no global state. Signature validation
//! walks protobuf wire data by hand ([`protowire`]), like Go. Everything is re-exported flat,
//! like the single Go package.
//!
//! Conventions: Go functions taking/returning JSON payloads take `&[u8]` and return `Vec<u8>`
//! (unchanged payloads come back byte-identical, changed ones are re-serialized compactly).
//! Go's variadic option structs become a by-value options struct (`Default` for "no options").
//! Go `error` returns become [`SignatureError`], whose message text matches Go's.
//!
//! Layout: `claude` (CAIS/E/R validation), `gemini` (thought signature validation and tool
//! pairing), `gpt`, `grok`, `kimi` (opaque-blob validators), `provider` (detection and the
//! replay compatibility policy), `claude_sanitize` / `gemini_sanitize` (JSON sanitizers).

pub mod protowire;

mod b64;
mod claude;
mod claude_sanitize;
mod gemini;
mod gemini_sanitize;
mod gpt;
mod grok;
mod kimi;
mod provider;

#[cfg(test)]
mod tests;

pub use claude::*;
pub use claude_sanitize::*;
pub use gemini::*;
pub use gemini_sanitize::*;
pub use gpt::*;
pub use grok::*;
pub use kimi::*;
pub use provider::*;

/// Validation failure; the message is the Go error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureError(String);

impl SignatureError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SignatureError {}

pub type Result<T> = std::result::Result<T, SignatureError>;

/// Go `%q`: double-quoted with Go escapes (`strconv.Quote`).
pub(crate) fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if c.is_control() || (c as u32) >= 0x80 && !is_go_printable(c) => {
                if (c as u32) < 0x10000 {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                } else {
                    out.push_str(&format!("\\U{:08x}", c as u32));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Approximation of Go's `strconv.IsPrint` for non-ASCII characters.
fn is_go_printable(c: char) -> bool {
    !c.is_control() && !c.is_whitespace() || c == ' '
}

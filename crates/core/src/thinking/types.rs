//! Canonical thinking types (Go: internal/thinking/types.go, errors.go).

use std::fmt;

use crate::registry::ModelInfo;

/// Kind of thinking configuration. `Budget` is the zero value, so a default
/// [`ThinkingConfig`] means "no config" (see [`ThinkingConfig::has_config`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingMode {
    /// Numeric budget (suffix `(1000)`).
    #[default]
    Budget,
    /// Discrete level (suffix `(high)`).
    Level,
    /// Thinking disabled (suffix `(none)` or budget 0).
    None,
    /// Automatic/dynamic thinking (suffix `(auto)` or budget -1).
    Auto,
}

impl ThinkingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingMode::Budget => "budget",
            ThinkingMode::Level => "level",
            ThinkingMode::None => "none",
            ThinkingMode::Auto => "auto",
        }
    }
}

impl fmt::Display for ThinkingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Level names (Go: `ThinkingLevel` constants). Levels are plain strings because request bodies
/// may carry arbitrary values that validation later accepts or rejects.
pub mod level {
    pub const NONE: &str = "none";
    pub const AUTO: &str = "auto";
    pub const MINIMAL: &str = "minimal";
    pub const LOW: &str = "low";
    pub const MEDIUM: &str = "medium";
    pub const HIGH: &str = "high";
    pub const XHIGH: &str = "xhigh";
    /// Used by Claude adaptive thinking (opus supports `max`).
    pub const MAX: &str = "max";
}

/// Unified thinking configuration passed between components. Depending on `mode`:
/// - `None`: budget 0, level ignored
/// - `Auto`: budget -1, level ignored
/// - `Budget`: budget is a positive integer, level ignored
/// - `Level`: level is a valid level, budget ignored
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThinkingConfig {
    pub mode: ThinkingMode,
    pub budget: i64,
    pub level: String,
}

impl ThinkingConfig {
    pub fn none() -> Self {
        Self {
            mode: ThinkingMode::None,
            budget: 0,
            level: String::new(),
        }
    }

    pub fn auto() -> Self {
        Self {
            mode: ThinkingMode::Auto,
            budget: -1,
            level: String::new(),
        }
    }

    pub fn level(level: impl Into<String>) -> Self {
        Self {
            mode: ThinkingMode::Level,
            budget: 0,
            level: level.into(),
        }
    }

    pub fn budget(budget: i64) -> Self {
        Self {
            mode: ThinkingMode::Budget,
            budget,
            level: String::new(),
        }
    }

    /// Go: `hasThinkingConfig`. The zero value means "no config".
    pub fn has_config(&self) -> bool {
        self.mode != ThinkingMode::Budget || self.budget != 0 || !self.level.is_empty()
    }

    /// Mode None with no budget or level: thinking is fully off.
    pub fn is_fully_disabled(&self) -> bool {
        self.mode == ThinkingMode::None && self.budget == 0 && self.level.is_empty()
    }
}

/// Result of [`parse_suffix`](super::parse_suffix): `model(value)` split into its parts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuffixResult {
    /// Model name with the suffix removed (the whole input when there is none).
    pub model_name: String,
    pub has_suffix: bool,
    /// Content inside the parentheses; empty when `has_suffix` is false.
    pub raw_suffix: String,
}

/// Converts a canonical [`ThinkingConfig`] into one provider's wire format.
///
/// Implementations expect a config already validated by
/// [`validate_config`](super::validate_config) (except for user-defined models, which skip
/// validation), must be idempotent, and return a modified copy of the body.
pub trait ProviderApplier: Send + Sync {
    fn apply(
        &self,
        body: &[u8],
        config: &ThinkingConfig,
        model_info: Option<&ModelInfo>,
    ) -> Result<Vec<u8>, ThinkingError>;
}

/// Machine-readable thinking error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Suffix format cannot be parsed.
    InvalidSuffix,
    /// Level value is not in the valid list.
    UnknownLevel,
    /// Model does not support thinking.
    ThinkingNotSupported,
    /// Model does not support the requested level.
    LevelNotSupported,
    /// Budget is outside the model's range.
    BudgetOutOfRange,
    /// Provider does not match the model.
    ProviderMismatch,
    /// A provider applier could not write its fields (Go: plain errors from the Kimi applier, not
    /// a `ThinkingError`; reported as a server error).
    ApplyFailed,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidSuffix => "INVALID_SUFFIX",
            ErrorCode::UnknownLevel => "UNKNOWN_LEVEL",
            ErrorCode::ThinkingNotSupported => "THINKING_NOT_SUPPORTED",
            ErrorCode::LevelNotSupported => "LEVEL_NOT_SUPPORTED",
            ErrorCode::BudgetOutOfRange => "BUDGET_OUT_OF_RANGE",
            ErrorCode::ProviderMismatch => "PROVIDER_MISMATCH",
            ErrorCode::ApplyFailed => "APPLY_FAILED",
        }
    }
}

/// Error from thinking processing. Displays as the bare message (no code prefix); handlers map it
/// to HTTP 400, or 500 for `ApplyFailed` ([`ThinkingError::status_code`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingError {
    pub code: ErrorCode,
    pub message: String,
    /// Model the error relates to (may be empty).
    pub model: String,
}

impl ThinkingError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            model: String::new(),
        }
    }

    pub fn with_model(
        code: ErrorCode,
        message: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            model: model.into(),
        }
    }

    pub fn status_code(&self) -> u16 {
        if self.code == ErrorCode::ApplyFailed {
            500
        } else {
            400
        }
    }
}

impl fmt::Display for ThinkingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ThinkingError {}

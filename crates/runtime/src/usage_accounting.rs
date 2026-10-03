//! Token usage detail and the canonical non-overlapping token breakdown (Go:
//! sdk/cliproxy/usage `Detail` and accounting.go). Shared by the executors' usage reporter and the
//! conductor's usage records.

use serde::Serialize;

/// Canonical token accounting schema version.
pub const TOKEN_ACCOUNTING_SCHEMA_VERSION: i64 = 2;

/// How confidently a token total could be classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenAccountingQuality {
    /// Go's zero value is the empty string, which is not a valid quality.
    #[default]
    #[serde(rename = "")]
    Unset,
    Complete,
    Inconsistent,
    Unclassified,
}

/// Mutually exclusive input buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct TokenInputBreakdown {
    pub total_tokens: i64,
    pub uncached_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
}

/// Mutually exclusive output buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct TokenOutputBreakdown {
    pub total_tokens: i64,
    pub non_reasoning_tokens: i64,
    pub reasoning_tokens: i64,
}

/// The canonical, non-overlapping token accounting contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct TokenBreakdown {
    pub schema_version: i64,
    pub quality: TokenAccountingQuality,
    pub total_tokens: i64,
    pub input: TokenInputBreakdown,
    pub output: TokenOutputBreakdown,
    pub unclassified_tokens: i64,
}

/// Token usage of one request (Go: usage.Detail).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Detail {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub cached_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    pub token_breakdown: TokenBreakdown,
    pub response_service_tier: String,
}

impl Detail {
    /// Any token counter or the breakdown total is non-zero.
    pub fn has_token_usage(&self) -> bool {
        self.input_tokens != 0
            || self.output_tokens != 0
            || self.reasoning_tokens != 0
            || self.cached_tokens != 0
            || self.cache_read_tokens != 0
            || self.cache_creation_tokens != 0
            || self.total_tokens != 0
            || self.token_breakdown.total_tokens != 0
    }
}

/// Sum of non-negative values; `None` on a negative value or overflow.
fn non_negative_sum(values: &[i64]) -> Option<i64> {
    values.iter().try_fold(0i64, |total, &v| if v < 0 { None } else { total.checked_add(v) })
}

/// Go's `nonNegativeSum`: the sum, or `(0, false)` on a negative value or overflow.
fn sum_ok(values: &[i64]) -> (i64, bool) {
    non_negative_sum(values).map_or((0, false), |s| (s, true))
}

impl TokenBreakdown {
    /// Whether the breakdown satisfies the v2 accounting invariants.
    pub fn valid(&self) -> bool {
        if self.schema_version != TOKEN_ACCOUNTING_SCHEMA_VERSION || self.quality == TokenAccountingQuality::Unset {
            return false;
        }
        let (i, o) = (&self.input, &self.output);
        if [
            self.total_tokens,
            self.unclassified_tokens,
            i.total_tokens,
            i.uncached_tokens,
            i.cache_read_tokens,
            i.cache_write_tokens,
            o.total_tokens,
            o.non_reasoning_tokens,
            o.reasoning_tokens,
        ]
        .iter()
        .any(|v| *v < 0)
        {
            return false;
        }
        if non_negative_sum(&[i.uncached_tokens, i.cache_read_tokens, i.cache_write_tokens]) != Some(i.total_tokens) {
            return false;
        }
        if non_negative_sum(&[o.non_reasoning_tokens, o.reasoning_tokens]) != Some(o.total_tokens) {
            return false;
        }
        if non_negative_sum(&[i.total_tokens, o.total_tokens, self.unclassified_tokens]) != Some(self.total_tokens) {
            return false;
        }
        !(self.quality == TokenAccountingQuality::Complete && self.unclassified_tokens != 0)
    }
}

fn inconsistent(total: i64, fallback: i64) -> TokenBreakdown {
    let resolved = if total <= 0 { fallback } else { total }.max(0);
    TokenBreakdown {
        schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
        quality: TokenAccountingQuality::Inconsistent,
        total_tokens: resolved,
        unclassified_tokens: resolved,
        ..Default::default()
    }
}

fn resolve_total(total: i64, expected: i64) -> Option<i64> {
    if total < 0 || expected < 0 {
        None
    } else if total == 0 {
        Some(expected)
    } else {
        (total == expected).then_some(total)
    }
}

fn complete(total: i64, input: TokenInputBreakdown, output: TokenOutputBreakdown) -> TokenBreakdown {
    TokenBreakdown {
        schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
        quality: TokenAccountingQuality::Complete,
        total_tokens: total,
        input,
        output,
        unclassified_tokens: 0,
    }
}

/// Protocols where cache tokens are included in the input total and reasoning in the output total.
pub fn new_subset_token_breakdown(
    input_total: i64,
    cache_read: i64,
    cache_write: i64,
    output_total: i64,
    reasoning: i64,
    total: i64,
) -> TokenBreakdown {
    let cache_total = non_negative_sum(&[cache_read, cache_write]);
    let expected = non_negative_sum(&[input_total, output_total]);
    let (Some(cache_total), Some(expected)) = (cache_total, expected) else {
        return inconsistent(total, expected.unwrap_or(0));
    };
    if reasoning < 0 || cache_total > input_total || reasoning > output_total {
        return inconsistent(total, expected);
    }
    let Some(resolved) = resolve_total(total, expected) else {
        return inconsistent(total, expected);
    };
    complete(
        resolved,
        TokenInputBreakdown {
            total_tokens: input_total,
            uncached_tokens: input_total - cache_total,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
        },
        TokenOutputBreakdown {
            total_tokens: output_total,
            non_reasoning_tokens: output_total - reasoning,
            reasoning_tokens: reasoning,
        },
    )
}

/// Keeps known subset buckets and assigns an authoritative remainder to `unclassified`.
pub fn new_partial_subset_token_breakdown(
    input_total: i64,
    cache_read: i64,
    cache_write: i64,
    output_total: i64,
    reasoning: i64,
    total: i64,
) -> TokenBreakdown {
    let cache_total = non_negative_sum(&[cache_read, cache_write]);
    let expected = non_negative_sum(&[input_total, output_total]);
    let (Some(cache_total), Some(expected)) = (cache_total, expected) else {
        return inconsistent(total, expected.unwrap_or(0));
    };
    if input_total < 0 || output_total < 0 || reasoning < 0 || cache_total > input_total || reasoning > output_total || total < 0 {
        return inconsistent(total, expected);
    }
    let resolved = if total == 0 { expected } else { total };
    if resolved < expected {
        return inconsistent(total, expected);
    }
    let unclassified = resolved - expected;
    TokenBreakdown {
        schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
        quality: if unclassified > 0 { TokenAccountingQuality::Unclassified } else { TokenAccountingQuality::Complete },
        total_tokens: resolved,
        input: TokenInputBreakdown {
            total_tokens: input_total,
            uncached_tokens: input_total - cache_total,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
        },
        output: TokenOutputBreakdown {
            total_tokens: output_total,
            non_reasoning_tokens: output_total - reasoning,
            reasoning_tokens: reasoning,
        },
        unclassified_tokens: unclassified,
    }
}

/// Protocols where uncached input, cache reads/writes, non-reasoning output and reasoning are
/// separate counters (Claude).
pub fn new_independent_token_breakdown(
    uncached_input: i64,
    cache_read: i64,
    cache_write: i64,
    non_reasoning_output: i64,
    reasoning: i64,
    total: i64,
) -> TokenBreakdown {
    let (input_total, ok_input) = sum_ok(&[uncached_input, cache_read, cache_write]);
    let (output_total, ok_output) = sum_ok(&[non_reasoning_output, reasoning]);
    let (expected, ok_expected) = sum_ok(&[input_total, output_total]);
    if !(ok_input && ok_output && ok_expected) {
        return inconsistent(total, expected);
    }
    let Some(resolved) = resolve_total(total, expected) else {
        return inconsistent(total, expected);
    };
    complete(
        resolved,
        TokenInputBreakdown {
            total_tokens: input_total,
            uncached_tokens: uncached_input,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
        },
        TokenOutputBreakdown {
            total_tokens: output_total,
            non_reasoning_tokens: non_reasoning_output,
            reasoning_tokens: reasoning,
        },
    )
}

/// Protocols where cache tokens are part of the input total while reasoning is separate from
/// ordinary output (Gemini family).
pub fn new_separate_reasoning_token_breakdown(
    input_total: i64,
    cache_read: i64,
    cache_write: i64,
    non_reasoning_output: i64,
    reasoning: i64,
    total: i64,
) -> TokenBreakdown {
    let Some(cache_total) = non_negative_sum(&[cache_read, cache_write]) else {
        return inconsistent(total, 0);
    };
    if input_total < 0 || cache_total > input_total {
        return inconsistent(total, 0);
    }
    let (output_total, ok_output) = sum_ok(&[non_reasoning_output, reasoning]);
    let (expected, ok_expected) = sum_ok(&[input_total, output_total]);
    if !(ok_output && ok_expected) {
        return inconsistent(total, expected);
    }
    let Some(resolved) = resolve_total(total, expected) else {
        return inconsistent(total, expected);
    };
    complete(
        resolved,
        TokenInputBreakdown {
            total_tokens: input_total,
            uncached_tokens: input_total - cache_total,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
        },
        TokenOutputBreakdown {
            total_tokens: output_total,
            non_reasoning_tokens: non_reasoning_output,
            reasoning_tokens: reasoning,
        },
    )
}

/// Keeps an authoritative total without guessing how an unknown protocol partitions it.
pub fn new_unclassified_token_breakdown(total: i64) -> TokenBreakdown {
    if total <= 0 {
        return TokenBreakdown {
            schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
            quality: if total < 0 { TokenAccountingQuality::Inconsistent } else { TokenAccountingQuality::Complete },
            ..Default::default()
        };
    }
    TokenBreakdown {
        schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
        quality: TokenAccountingQuality::Unclassified,
        total_tokens: total,
        unclassified_tokens: total,
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Semantics {
    Unknown,
    Subset,
    Independent,
    SeparateReasoning,
}

fn semantics_for(provider: &str, executor_type: &str) -> Semantics {
    let provider = provider.trim().to_lowercase();
    let executor = executor_type.trim().to_lowercase();
    let value = format!("{provider} {executor}");
    let value = value.trim();
    if value.is_empty() || value == "unknown" || value == "unknown unknown" {
        return Semantics::Unknown;
    }
    if executor == "openaicompatexecutor" || provider == "openai-compatibility" || provider.starts_with("openai-compatible-") {
        return Semantics::Subset;
    }
    if value.contains("claude") || value.contains("anthropic") {
        return Semantics::Independent;
    }
    if ["gemini", "aistudio", "antigravity", "vertex", "interaction"].iter().any(|m| value.contains(m)) {
        return Semantics::SeparateReasoning;
    }
    if ["openai", "codex", "xai", "grok", "kimi", "qwen", "deepseek", "openrouter"].iter().any(|m| value.contains(m)) {
        return Semantics::Subset;
    }
    Semantics::Unknown
}

fn unclassified_lower_bound(d: &Detail) -> Option<i64> {
    let cache = non_negative_sum(&[d.cache_read_tokens, d.cache_creation_tokens])?;
    if d.input_tokens < 0 || d.output_tokens < 0 || d.reasoning_tokens < 0 || d.cached_tokens < 0 {
        return None;
    }
    let input_total = d.input_tokens.max(cache).max(d.cached_tokens);
    let output_total = d.output_tokens.max(d.reasoning_tokens);
    non_negative_sum(&[input_total, output_total])
}

fn breakdown_for_semantics(d: &Detail, semantics: Semantics) -> TokenBreakdown {
    if d.total_tokens == 0 && d.input_tokens == 0 && d.output_tokens == 0 {
        let Some(total) = unclassified_lower_bound(d) else {
            return inconsistent(d.total_tokens, 0);
        };
        let any_cache = d.cache_read_tokens > 0 || d.cache_creation_tokens > 0 || d.cached_tokens > 0;
        if total > 0
            && (matches!(semantics, Semantics::Unknown | Semantics::Subset)
                || (semantics == Semantics::SeparateReasoning && any_cache))
        {
            return new_unclassified_token_breakdown(total);
        }
    }
    let args = (
        d.input_tokens,
        d.cache_read_tokens,
        d.cache_creation_tokens,
        d.output_tokens,
        d.reasoning_tokens,
        d.total_tokens,
    );
    match semantics {
        Semantics::Subset => new_subset_token_breakdown(args.0, args.1, args.2, args.3, args.4, args.5),
        Semantics::Independent => new_independent_token_breakdown(args.0, args.1, args.2, args.3, args.4, args.5),
        Semantics::SeparateReasoning => {
            new_separate_reasoning_token_breakdown(args.0, args.1, args.2, args.3, args.4, args.5)
        }
        Semantics::Unknown => {
            let mut total = d.total_tokens;
            if total == 0 {
                match unclassified_lower_bound(d) {
                    Some(t) => total = t,
                    None => return inconsistent(d.total_tokens, 0),
                }
            }
            new_unclassified_token_breakdown(total)
        }
    }
}

/// Attaches a valid v2 breakdown to a detail using the provider's token semantics (unknown
/// providers stay unclassified instead of guessing how their buckets overlap).
pub fn ensure_token_breakdown_for_provider(mut detail: Detail, provider: &str, executor_type: &str) -> Detail {
    if !detail.token_breakdown.valid() {
        let semantics = semantics_for(provider, executor_type);
        if detail.cache_read_tokens == 0
            && detail.cached_tokens > 0
            && detail.input_tokens == 0
            && detail.output_tokens == 0
            && detail.reasoning_tokens == 0
            && detail.cache_creation_tokens == 0
            && detail.total_tokens == 0
            && matches!(semantics, Semantics::Subset | Semantics::SeparateReasoning)
        {
            detail.cache_read_tokens = detail.cached_tokens;
        }
        detail.token_breakdown = breakdown_for_semantics(&detail, semantics);
    }
    if detail.total_tokens == 0 {
        detail.total_tokens = detail.token_breakdown.total_tokens;
    }
    detail
}

/// [`ensure_token_breakdown_for_provider`] with no provider knowledge.
pub fn ensure_token_breakdown(detail: Detail) -> Detail {
    ensure_token_breakdown_for_provider(detail, "", "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenAccountingQuality::{Complete, Inconsistent, Unclassified};

    // Ports of sdk/cliproxy/usage/accounting_test.go.

    #[test]
    fn subset_avoids_cache_and_reasoning_double_count() {
        let b = new_subset_token_breakdown(100, 40, 10, 30, 12, 130);
        assert!(b.valid(), "{b:?}");
        assert_eq!((b.input.uncached_tokens, b.output.non_reasoning_tokens, b.total_tokens), (50, 18, 130));
    }

    #[test]
    fn partial_subset_preserves_known_buckets() {
        let b = new_partial_subset_token_breakdown(10, 4, 0, 0, 0, 15);
        assert!(b.valid(), "{b:?}");
        assert_eq!((b.quality, b.input.total_tokens, b.unclassified_tokens), (Unclassified, 10, 5));
    }

    #[test]
    fn independent_keeps_claude_cache_buckets_independent() {
        let b = new_independent_token_breakdown(30, 7, 13, 5, 0, 55);
        assert!(b.valid(), "{b:?}");
        assert_eq!((b.input.total_tokens, b.total_tokens), (50, 55));
    }

    #[test]
    fn separate_reasoning_adds_reasoning_to_output() {
        let b = new_separate_reasoning_token_breakdown(20, 5, 0, 7, 3, 30);
        assert!(b.valid(), "{b:?}");
        assert_eq!((b.output.total_tokens, b.total_tokens), (10, 30));
    }

    #[test]
    fn contradictory_parents_are_inconsistent() {
        let b = new_subset_token_breakdown(10, 4, 0, 3, 1, 20);
        assert!(b.valid(), "{b:?}");
        assert_eq!((b.quality, b.unclassified_tokens), (Inconsistent, 20));
    }

    #[test]
    fn unclassified_does_not_guess_buckets() {
        let b = new_unclassified_token_breakdown(42);
        assert!(b.valid(), "{b:?}");
        assert_eq!((b.quality, b.unclassified_tokens), (Unclassified, 42));
    }

    #[test]
    fn ensure_for_provider_uses_known_semantics() {
        let detail = Detail {
            input_tokens: 100,
            output_tokens: 30,
            reasoning_tokens: 12,
            cache_read_tokens: 40,
            cache_creation_tokens: 10,
            ..Default::default()
        };
        // (name, provider, executor type, total, input total, output total)
        let cases = [
            ("OpenAI subsets cache and reasoning", "openai", "", 130, 100, 30),
            ("OpenAI compatible executor takes precedence", "anthropic", "OpenAICompatExecutor", 130, 100, 30),
            ("Gemini keeps reasoning separate", "gemini", "", 142, 100, 42),
            ("Claude keeps cache and reasoning independent", "anthropic", "", 192, 150, 42),
        ];
        for (name, provider, executor, total, input, output) in cases {
            let d = ensure_token_breakdown_for_provider(detail.clone(), provider, executor);
            let b = &d.token_breakdown;
            assert!(b.valid() && b.quality == Complete, "{name}: {b:?}");
            assert_eq!(
                (d.total_tokens, b.total_tokens, b.input.total_tokens, b.output.total_tokens),
                (total, total, input, output),
                "{name}"
            );
        }
    }

    #[test]
    fn ensure_for_unknown_provider_does_not_guess_reasoning() {
        let d = ensure_token_breakdown_for_provider(
            Detail { input_tokens: 100, output_tokens: 30, reasoning_tokens: 12, ..Default::default() },
            "plugin-provider",
            "",
        );
        assert_eq!((d.total_tokens, d.token_breakdown.quality, d.token_breakdown.unclassified_tokens), (130, Unclassified, 130));
    }

    #[test]
    fn ensure_for_unknown_provider_preserves_auxiliary_only_usage() {
        let d = ensure_token_breakdown_for_provider(
            Detail { reasoning_tokens: 12, cache_read_tokens: 7, ..Default::default() },
            "plugin-provider",
            "",
        );
        assert_eq!((d.total_tokens, d.token_breakdown.quality, d.token_breakdown.unclassified_tokens), (19, Unclassified, 19));
    }

    #[test]
    fn ensure_for_gemini_classifies_reasoning_only_usage() {
        let d = ensure_token_breakdown_for_provider(Detail { reasoning_tokens: 12, ..Default::default() }, "gemini", "");
        assert_eq!(
            (d.total_tokens, d.token_breakdown.quality, d.token_breakdown.output.reasoning_tokens),
            (12, Complete, 12)
        );
    }

    #[test]
    fn ensure_preserves_legacy_cached_only_usage() {
        let d = ensure_token_breakdown_for_provider(Detail { cached_tokens: 13, ..Default::default() }, "openai", "");
        assert_eq!(
            (d.total_tokens, d.cache_read_tokens, d.token_breakdown.quality, d.token_breakdown.unclassified_tokens),
            (13, 13, Unclassified, 13)
        );
    }

    #[test]
    fn ensure_does_not_override_canonical_zero_cache_read() {
        let d = ensure_token_breakdown_for_provider(
            Detail { cached_tokens: 13, cache_creation_tokens: 13, ..Default::default() },
            "openai",
            "",
        );
        assert_eq!(d.cache_read_tokens, 0);
    }

    #[test]
    fn valid_rejects_arithmetic_overflow() {
        // MaxInt64 + MaxInt64 + 2 wraps to 0 in int64 arithmetic.
        let input = TokenBreakdown {
            schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
            quality: Complete,
            input: TokenInputBreakdown {
                total_tokens: 0,
                uncached_tokens: i64::MAX,
                cache_read_tokens: i64::MAX,
                cache_write_tokens: 2,
            },
            ..Default::default()
        };
        assert!(!input.valid(), "{input:?}");
        let output = TokenBreakdown {
            schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
            quality: Complete,
            output: TokenOutputBreakdown { total_tokens: -2, non_reasoning_tokens: i64::MAX, reasoning_tokens: i64::MAX },
            ..Default::default()
        };
        assert!(!output.valid(), "{output:?}");
    }

    #[test]
    fn subset_and_separate_reasoning_reject_overflow() {
        let subset = new_subset_token_breakdown(i64::MAX, i64::MAX, i64::MAX, 0, 0, i64::MAX);
        assert!(subset.quality != Complete && subset.input.uncached_tokens >= 0, "{subset:?}");
        let separate = new_separate_reasoning_token_breakdown(i64::MAX, i64::MAX, i64::MAX, 0, 0, i64::MAX);
        assert!(separate.quality != Complete && separate.input.uncached_tokens >= 0, "{separate:?}");
    }
}

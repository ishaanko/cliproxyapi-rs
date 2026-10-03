//! Cheap pre-check for reasoning-summary intent.
//!
//! `cpa_core::thinking` reads summary visibility from a handful of paths (it fully parses and
//! validates the body first, which dominates the cost of translating a large request). Every one
//! of those paths starts with one of [`SUMMARY_ROOT_KEYS`], so a body whose top-level object has
//! none of them has no summary intent in any dialect and the core call can be skipped.

use std::cell::Cell;
use std::fmt;

use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};

use super::fast::Str;
use cpa_core::thinking::{self, SummaryConfig};

/// First path segments of every summary field `cpa_core::thinking::summary` reads or writes
/// (OpenAI chat extras and `reasoning_effort`, Responses `reasoning`, Claude `thinking`, Gemini
/// `generationConfig`, Antigravity `request`, Interactions `generation_config`).
/// Keep in sync with that module.
const SUMMARY_ROOT_KEYS: &[&str] = &[
    "extra_body",
    "google",
    "thinking",
    "reasoning",
    "reasoning_effort",
    "include_reasoning",
    "generationConfig",
    "generation_config",
    "request",
];

/// Walks the top-level members (values skipped unvalidated by content) until a summary key shows up.
struct Probe<'a>(&'a Cell<bool>);

impl<'de> Visitor<'de> for Probe<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<Str<'de>>()? {
            if SUMMARY_ROOT_KEYS.contains(&&*key) {
                self.0.set(true);
                return Ok(());
            }
            map.next_value::<IgnoredAny>()?;
        }
        Ok(())
    }
}

/// False only when `body` is a well-formed JSON object without any summary-related top-level key.
/// Anything else (a hit, a non-object, malformed input) answers true and the caller consults
/// `cpa_core::thinking`, which then makes the final decision exactly as before.
pub fn may_have_summary_intent(body: &[u8]) -> bool {
    let found = Cell::new(false);
    let mut de = serde_json::Deserializer::from_slice(body);
    let ok = de.deserialize_map(Probe(&found)).is_ok();
    found.get() || !ok
}

/// `thinking::extract_translated_summary_config`, skipping the body parse when it cannot matter.
pub fn extract_translated_summary_config(body: &[u8], source_format: &str, target_format: &str) -> SummaryConfig {
    if !may_have_summary_intent(body) {
        return SummaryConfig::default();
    }
    thinking::extract_translated_summary_config(body, source_format, target_format)
}

/// `thinking::apply_translated_summary_to_claude`, skipping the source parse when it cannot matter.
pub fn apply_translated_summary_to_claude(out: &[u8], source: &[u8], source_format: &str, model: &str) -> Vec<u8> {
    if !may_have_summary_intent(source) {
        return out.to_vec();
    }
    thinking::apply_translated_summary_to_claude(out, source, source_format, model)
}

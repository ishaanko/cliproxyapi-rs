//! gjson-style leniency for the response and request converters.
//!
//! The Go code reads payloads with gjson, which accepts malformed documents (mismatched
//! closers, truncation) and is sometimes combined with an explicit validity check
//! (`gjson.ValidBytes`). `cpa_json::parse` is already that tolerant parser. Original-text
//! lookups use `cpa_json::raw_at` / `raw_children`.

use std::collections::HashMap;

use cpa_json::{Res, Value};

/// gjson `ValidBytes`.
pub(super) fn gjson_valid(raw: &[u8]) -> bool {
    cpa_json::valid(raw)
}

/// gjson `ParseBytes`: a document `Value` even for malformed input; `None` where gjson would
/// report a non-existent result (input that does not start like JSON).
pub(super) fn parse_gjson(raw: &[u8]) -> Option<Value> {
    let parsed = cpa_json::parse(raw);
    (!parsed.is_null() || cpa_json::valid(raw)).then_some(parsed)
}

/// Original text of request tool output values, keyed by the address of the value inside the
/// parsed request. The request pipeline hands out borrowed `Res` nodes of that document, so
/// the address identifies one output (or output element) even when several are equal. Values
/// the pipeline cloned (owned `Res`) are not found and fall back to their compact form.
#[derive(Default)]
pub(super) struct RawTexts<'a>(HashMap<usize, &'a str>);

impl<'a> RawTexts<'a> {
    fn insert(&mut self, value: &Value, text: &'a str) {
        self.0.insert(std::ptr::from_ref(value) as usize, text);
    }

    /// The original text of `value`, or its compact serialization when unknown.
    pub(super) fn restore(&self, value: &Res<'_>) -> String {
        value
            .v()
            .and_then(|v| self.0.get(&(std::ptr::from_ref(v) as usize)))
            .map_or_else(|| value.raw(), |text| (*text).to_string())
    }
}

/// Collects the original text of every object/array `output` of the request's `input` items and
/// of the elements of array outputs, so `$ref` results can be stringified as sent (Go copies
/// `Result.Raw`). `root` is the parsed `src`.
pub(super) fn collect_output_raws<'a>(src: &'a [u8], root: &Value) -> RawTexts<'a> {
    let mut raws = RawTexts::default();
    let Some(items) = root.get("input").and_then(Value::as_array) else { return raws };
    let item_raws = cpa_json::raw_children(src, "input");
    for (item, item_raw) in items.iter().zip(&item_raws) {
        let Some(output) = item.get("output") else { continue };
        if !matches!(output, Value::Object(_) | Value::Array(_)) {
            continue;
        }
        let Some(output_raw) = cpa_json::raw_at(item_raw.as_bytes(), "output") else { continue };
        raws.insert(output, output_raw.trim());
        if let Value::Array(elements) = output {
            for (element, element_raw) in elements.iter().zip(cpa_json::raw_children(output_raw.as_bytes(), "")) {
                raws.insert(element, element_raw.trim());
            }
        }
    }
    raws
}

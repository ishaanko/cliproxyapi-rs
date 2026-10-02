//! Original-text lookups that stay linear on large bodies.
//!
//! `cpa_json::raw_at` scans from the start of its source on every call, so calling it once
//! per array element is quadratic. Resolve the element slices once with
//! `cpa_json::raw_children(src, "messages")` and do relative lookups inside each slice.

/// `raw_at` relative to a raw item slice (as returned by `cpa_json::raw_children`); `None` when
/// the item itself is missing.
pub fn raw_in<'a>(item: Option<&&'a str>, path: &str) -> Option<&'a str> {
    cpa_json::raw_at(item?.as_bytes(), path)
}

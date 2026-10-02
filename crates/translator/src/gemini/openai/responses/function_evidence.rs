//! Function call evidence tracking for the Gemini -> Responses direction (Go:
//! geminiRecordFunctionEvidence / geminiPendingIdentityError).
//!
//! Gemini may repeat a function call across frames. Full snapshots are evidence, never source
//! prefixes: stable ids and explicit part indexes identify repeated snapshots without merging
//! distinct unkeyed calls. Conflicts matter for apply_patch calls, which must fail the response.

use std::collections::HashMap;

use cpa_core::util::ResponsesToolIdentity;
use cpa_json::{Res};

use crate::common::ApplyPatchCallState;

#[derive(Debug, Default)]
pub(super) struct FunctionCallEvidence {
    pub part_index: i64,
    pub has_part_index: bool,
    pub apply_patch: bool,
    pub raw_name: String,
    pub upstream_id: String,
    pub input: String,
    pub has_input: bool,
    pub err: Option<String>,
    pub patch_call: Option<ApplyPatchCallState>,
}

/// All evidence of one response. `entries` owns every evidence record; `keys` maps the Go map
/// keys ("part:N", "id:X", "unknown:N") to entry indexes, `order` lists stored entries in first
/// seen order (Go iterates its map in random order).
#[derive(Debug, Default)]
pub(super) struct EvidenceStore {
    pub entries: Vec<FunctionCallEvidence>,
    keys: HashMap<String, usize>,
    order: Vec<usize>,
}

/// Records one function call snapshot and returns the index of its evidence entry.
pub(super) fn record_function_evidence(
    store: &mut EvidenceStore,
    tool_identity_map: &HashMap<String, ResponsesToolIdentity>,
    fc: &Res<'_>,
    args_raw: &str,
    part_index: i64,
    valid_json: bool,
) -> usize {
    let name = fc.g("name").str();
    let id = fc.g("id").str();
    let mut keys: Vec<String> = Vec::new();
    if part_index >= 0 {
        keys.push(format!("part:{part_index}"));
    }
    if !id.is_empty() {
        keys.push(format!("id:{id}"));
    }
    // Never guess which later named call owns an unkeyed nameless snapshot.
    if keys.is_empty() && name.is_empty() {
        keys.push(format!("unknown:{}", store.keys.len()));
    }
    let mut evidence: Option<usize> = None;
    let mut conflict = false;
    let mut patch_related = tool_identity_map.get(&name).is_some_and(|i| i.apply_patch);
    for key in &keys {
        if let Some(&prior) = store.keys.get(key) {
            patch_related = patch_related || store.entries[prior].apply_patch;
            match evidence {
                None => evidence = Some(prior),
                Some(e) if e != prior => conflict = true,
                _ => {}
            }
        }
    }
    if conflict {
        let err = "conflicting apply_patch call indexes".to_string();
        if patch_related {
            // Reject before rebinding either established call's aliases or provenance.
            store.entries.push(FunctionCallEvidence { apply_patch: true, err: Some(err), ..Default::default() });
            return store.entries.len() - 1;
        }
        // Ordinary-only cross-key reuse keeps the legacy first-match behavior.
        if let Some(e) = evidence {
            store.entries[e].err = Some(err);
        }
    }
    let idx = match evidence {
        Some(e) => e,
        None => {
            store.entries.push(FunctionCallEvidence::default());
            store.entries.len() - 1
        }
    };
    fn record_error(e: &mut FunctionCallEvidence, err: impl Into<String>) {
        if e.err.is_none() {
            e.err = Some(err.into());
        }
    }
    if part_index >= 0 {
        let e = &mut store.entries[idx];
        if e.has_part_index && e.part_index != part_index {
            record_error(e, "conflicting apply_patch part index");
        } else {
            e.part_index = part_index;
            e.has_part_index = true;
        }
    }
    if tool_identity_map.get(&name).is_some_and(|i| i.apply_patch) {
        store.entries[idx].apply_patch = true;
    }
    for key in keys {
        if store.keys.insert(key, idx).is_none() && !store.order.contains(&idx) {
            store.order.push(idx);
        }
    }
    let e = &mut store.entries[idx];
    if !name.is_empty() {
        if !e.raw_name.is_empty() && e.raw_name != name {
            record_error(e, "conflicting apply_patch call name");
        } else {
            e.raw_name = name;
        }
    }
    if !id.is_empty() {
        if !e.upstream_id.is_empty() && e.upstream_id != id {
            record_error(e, "conflicting apply_patch call ID");
        } else {
            e.upstream_id = id;
        }
    }
    let mut snapshot = ApplyPatchCallState::default();
    let finished = snapshot.finish_arguments(args_raw);
    if !valid_json {
        record_error(e, "invalid Gemini apply_patch response JSON");
    }
    match finished {
        Err(err) => record_error(e, err),
        Ok((_, input)) => {
            if e.has_input && e.input != input {
                record_error(e, "conflicting apply_patch complete snapshots");
            }
            e.has_input = true;
            e.input = input;
        }
    }
    idx
}

/// With apply_patch enabled, a stored call that never got a name is an error (its own error, or
/// an unresolved identity).
pub(super) fn pending_identity_error(store: &EvidenceStore, tool_identity_map: &HashMap<String, ResponsesToolIdentity>) -> Option<String> {
    if !tool_identity_map.values().any(|i| i.apply_patch) {
        return None;
    }
    for &idx in &store.order {
        let evidence = &store.entries[idx];
        if evidence.raw_name.is_empty() {
            return Some(evidence.err.clone().unwrap_or_else(|| "unresolved Gemini apply_patch call identity".to_string()));
        }
    }
    None
}

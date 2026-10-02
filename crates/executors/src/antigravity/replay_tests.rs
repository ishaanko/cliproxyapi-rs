//! Replay differential test: Go-recorded outputs of the replay functions over randomized histories.

use std::collections::HashMap;

use serde_json::Value;

use super::replay::{
    Index, apply_reasoning_replay_items, degrade_claude_tool_provenance_ids, payload_has_claude_tool_provenance_id,
    repair_unsigned_first_function_calls,
};
use super::signature::normalize_function_response_roles;

use super::tests::gunzip;

fn list(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

/// 500 randomized mutations of a multi-turn Gemini history (deleted/changed signatures, ids,
/// args, roles, reserved Claude ids, schema defaults), run through the Go replay functions
/// (apply, ledger extraction, degrade, repair, role normalization). The recorded outputs must
/// equal this port's. Regenerate with `testdata/oracle/run.sh`.
#[test]
fn replay_functions_match_go_on_randomized_histories() {
    let doc: Value = serde_json::from_slice(&gunzip(include_bytes!("testdata/replay.json.gz"))).expect("replay fixture");
    let items: Vec<Vec<u8>> = list(&doc["items"]).iter().map(cpa_json::to_vec).collect();
    let schemas: HashMap<String, Value> = doc["schemas"]
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();

    // The ledger extracted from the base history is the same as Go's.
    let base_index = Index::new(&doc["base"]);
    assert_eq!(base_index.reasoning_replay_items_from_request().unwrap_or_default(), list(&doc["items"]));

    let mut diffs = Vec::new();
    let mut applied_cases = 0;
    let no_schemas = HashMap::new();
    for (i, case) in list(&doc["cases"]).iter().enumerate() {
        let payload = &case["payload"];
        let bytes = cpa_json::to_vec(payload);
        let sch = if case["use_schemas"].as_bool().unwrap_or(false) { &schemas } else { &no_schemas };
        let (applied, changed) = apply_reasoning_replay_items(&bytes, &items, sch);
        if changed != case["changed"].as_bool().unwrap_or(false) {
            diffs.push(format!("case {i}: changed rust={changed} go={}", case["changed"]));
        }
        applied_cases += usize::from(changed);
        let got = cpa_json::parse(&applied);
        if got != case["applied"] {
            diffs.push(format!("case {i}: applied differs\n  go:   {}\n  rust: {got}", case["applied"]));
        }

        let got_items = Index::new(payload).reasoning_replay_items_from_request().unwrap_or_default();
        if got_items != list(&case["items_from_request"]) {
            diffs.push(format!(
                "case {i}: items_from_request differ\n  go:   {}\n  rust: {}",
                case["items_from_request"],
                Value::Array(got_items)
            ));
        }

        let mut degraded = payload.clone();
        let count = degrade_claude_tool_provenance_ids(&mut degraded);
        if degraded != case["degraded"] || count as i64 != case["degraded_count"].as_i64().unwrap_or(-1) {
            diffs.push(format!("case {i}: degrade differs (count rust={count} go={})", case["degraded_count"]));
        }
        let mut repaired = payload.clone();
        repair_unsigned_first_function_calls(&mut repaired);
        if repaired != case["repaired"] {
            diffs.push(format!("case {i}: repair differs"));
        }
        let roles = cpa_json::parse(&normalize_function_response_roles(bytes));
        if roles != case["roles_normalized"] {
            diffs.push(format!("case {i}: roles differ\n  go:   {}\n  rust: {roles}", case["roles_normalized"]));
        }
        if payload_has_claude_tool_provenance_id(payload) != case["has_reserved"].as_bool().unwrap_or(false) {
            diffs.push(format!("case {i}: reserved-id detection differs"));
        }
    }
    assert!(applied_cases > 50, "differential should apply replay items often, got {applied_cases}");
    assert!(
        diffs.is_empty(),
        "{} replay mismatches:\n{}",
        diffs.len(),
        diffs.iter().take(6).cloned().collect::<Vec<_>>().join("\n")
    );
}

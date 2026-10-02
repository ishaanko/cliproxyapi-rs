use cpa_core::registry::get_devin_models;

use super::*;
use crate::devin::test_support::golden;

#[test]
fn chat_model_uids_match_go() {
    // Hand-picked edge cases followed by the full catalog swept over every effort.
    let cases = golden()["models"].as_array().unwrap();
    assert!(cases.len() > 300, "catalog sweep present");
    for c in cases {
        let (model, level) = (c["model"].as_str().unwrap(), c["level"].as_str().unwrap());
        let budget = c["budget"].as_i64().unwrap_or(0);
        assert_eq!(
            resolve_chat_model_uid(model, level, budget),
            c["out"].as_str().unwrap(),
            "model {model:?} level {level:?} budget {budget}"
        );
    }
}

#[test]
fn catalog_thinking_models_resolve_to_effort_variants() {
    let models = get_devin_models();
    assert!(!models.is_empty());
    for m in models {
        let base = m.id.trim_start_matches("devin/");
        for effort in ["", "none", "low", "medium", "high", "xhigh", "max"] {
            let resolved = resolve_chat_model_uid(&format!("devin/{base}"), effort, 0);
            assert!(!resolved.is_empty(), "{base} {effort}");
            let has_levels = m.thinking.as_ref().is_some_and(|t| !t.levels.is_empty());
            let exempt = [
                "swe-1-7",
                "glm-5-2",
                "glm-5-2-1m",
                "swe-1-6-slow",
                "model_claude_4_5_opus",
            ];
            if has_levels && !exempt.contains(&base) {
                assert!(
                    has_effort_suffix(&resolved),
                    "{base} with {effort:?} resolved to bare {resolved}"
                );
            }
        }
    }
}

#[test]
fn budget_maps_to_effort_buckets() {
    for (budget, want) in [
        (1, "low"),
        (4096, "low"),
        (4097, "medium"),
        (16384, "medium"),
        (16385, "high"),
        (32768, "high"),
        (32769, "max"),
        (0, ""),
    ] {
        assert_eq!(
            normalize_thinking_level("", budget),
            want,
            "budget {budget}"
        );
    }
    assert_eq!(normalize_thinking_level(" Adaptive ", 0), "high");
    assert_eq!(normalize_thinking_level("OFF", 100), "none");
}

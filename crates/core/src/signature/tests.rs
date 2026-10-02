//! Signature tests: a differential corpus recorded from the Go implementation plus focused unit
//! tests for tricky protobuf/base64 behavior.
//!
//! `testdata/golden.jsonl.gz` holds ~35k cases (signature pool x functions, JSON payloads x
//! sanitizers) with the exact Go outputs. Regenerate with `testdata/regen.sh` (runs the Go
//! generator `testdata/zz_golden_test.go.txt` against the reference repo).

use std::io::Read;

use cpa_json::{json, Value};
use flate2::read::GzDecoder;

use super::protowire::{self, append, ParseError, WireType};
use super::*;

fn golden() -> Vec<Value> {
    let gz = include_bytes!("testdata/golden.jsonl.gz");
    let mut text = String::new();
    GzDecoder::new(&gz[..]).read_to_string(&mut text).expect("gunzip golden");
    text.lines()
        .map(|line| serde_json::from_str(line).expect("golden line"))
        .collect()
}

fn provider_of(s: &str) -> SignatureProvider {
    match s {
        "claude" => SignatureProvider::Claude,
        "gemini" => SignatureProvider::Gemini,
        "gemini_bypass" => SignatureProvider::GeminiBypass,
        "gpt" => SignatureProvider::Gpt,
        "kimi" => SignatureProvider::Kimi,
        "grok" => SignatureProvider::Grok,
        "swe" => SignatureProvider::Swe,
        "unknown" => SignatureProvider::Unknown,
        other => panic!("provider {other}"),
    }
}

fn kind_of(s: &str) -> SignatureBlockKind {
    match s {
        "unknown" | "" => SignatureBlockKind::Unknown,
        "claude_thinking" => SignatureBlockKind::ClaudeThinking,
        "gemini_model_part" => SignatureBlockKind::GeminiModelPart,
        "gemini_function_call" => SignatureBlockKind::GeminiFunctionCall,
        "gpt_reasoning" => SignatureBlockKind::GptReasoning,
        other => panic!("kind {other}"),
    }
}

fn claude_opts(i: u64) -> ClaudeSignatureValidationOptions {
    let mut o = ClaudeSignatureValidationOptions::default();
    match i {
        0 => {}
        1 => o.strict = true,
        2 => o.prefix_only = true,
        3 => o.base64_only = true,
        other => panic!("claude opts {other}"),
    }
    o
}

fn gemini_opts(i: u64) -> GeminiThoughtSignatureValidationOptions {
    let (a, k, m) = match i {
        0 => (false, false, false),
        1 => (true, false, false),
        2 => (false, true, false),
        3 => (false, false, true),
        4 => (true, true, true),
        other => panic!("gemini opts {other}"),
    };
    GeminiThoughtSignatureValidationOptions {
        allow_bypass_sentinel: a,
        require_known_envelope: k,
        require_observed_marker: m,
    }
}

fn gemini_payload_opts(i: u64) -> GeminiThoughtSignatureValidationOptions {
    match i {
        0 => gemini_opts(0),
        1 => gemini_opts(1),
        2 => GeminiThoughtSignatureValidationOptions {
            allow_bypass_sentinel: true,
            require_known_envelope: true,
            require_observed_marker: false,
        },
        3 => GeminiThoughtSignatureValidationOptions {
            allow_bypass_sentinel: true,
            require_known_envelope: false,
            require_observed_marker: true,
        },
        other => panic!("gemini payload opts {other}"),
    }
}

fn err_val<T>(r: &Result<T>) -> Value {
    match r {
        Ok(_) => Value::Null,
        Err(e) => Value::String(e.to_string()),
    }
}

fn decision_json(d: &SignatureCompatibilityDecision) -> Value {
    json!({
        "target": d.target_provider.as_str(), "detected": d.detected_provider.as_str(),
        "kind": d.block_kind.as_str(), "compatible": d.compatible, "action": d.action.as_str(),
        "replacement": d.replacement_signature, "normalized": d.normalized_signature,
        "reason": d.reason,
    })
}

fn report_json(out: &[u8], rep: &SignatureSanitizeReport) -> Value {
    json!({
        "value": String::from_utf8_lossy(out), "target": rep.target_provider.as_str(),
        "preserved": rep.preserved, "droppedBlocks": rep.dropped_blocks,
        "droppedSignatures": rep.dropped_signatures, "replaced": rep.replaced_signatures,
        "decisions": rep.decisions.iter().map(decision_json).collect::<Vec<_>>(),
    })
}

fn tree_json(t: &Result<ClaudeSignatureTree>) -> Value {
    let mut out = json!({ "err": err_val(t) });
    if let Ok(t) = t {
        out["tree"] = json!({
            "layers": t.encoding_layers, "chan": t.channel_id, "field2": t.field2,
            "routing": t.routing_class, "infra": t.infrastructure_class, "schema": t.schema_features,
            "model": t.model_text, "legacy": t.legacy_route_hint, "f7": t.has_field7,
        });
    }
    out
}

/// Runs one recorded case through the Rust port and returns the output in the golden schema.
fn run_case(case: &Value, pool: &[String]) -> Value {
    let fn_name = case["fn"].as_str().unwrap();
    let sig = || pool[case["sig"].as_u64().unwrap() as usize].as_str();
    let payload = || case["payload"].as_str().unwrap().as_bytes();
    let s = |key: &str| case[key].as_str().unwrap_or_default().to_string();
    match fn_name {
        "detect" => json!({"provider": detect_signature_provider_for_block(sig(), kind_of(&s("kind"))).as_str()}),
        "decide" => decision_json(&decide_signature_compatibility_for_model(
            provider_of(&s("target")),
            &s("model"),
            sig(),
            kind_of(&s("kind")),
        )),
        "normalizeClaude" => {
            let r = normalize_claude_thinking_signature(sig(), claude_opts(case["opts"].as_u64().unwrap()));
            json!({"value": r.as_deref().unwrap_or(""), "err": err_val(&r)})
        }
        "isValidClaude" => json!({"ok": is_valid_claude_thinking_signature(sig(), claude_opts(case["opts"].as_u64().unwrap()))}),
        "normalizeNative" => {
            let r = normalize_claude_provider_native_thinking_signature(
                sig(),
                claude_opts(case["opts"].as_u64().unwrap()),
            );
            json!({"value": r.as_deref().unwrap_or(""), "err": err_val(&r)})
        }
        "decodable" => json!({
            "ok": has_decodable_claude_thinking_signature(sig()),
            "prefix": has_claude_thinking_signature_prefix(sig()),
        }),
        "inspectCAIS" => {
            let r = inspect_claude_cais_signature(sig());
            let mut out = json!({"err": err_val(&r)});
            if let Ok(i) = &r {
                out["info"] = json!({
                    "first": i.first_byte, "env": i.envelope_version, "chan": i.channel_id,
                    "model": i.model_text, "kind": i.block_kind, "ctx": i.context_id,
                    "siglen": i.signature_len,
                });
            }
            out
        }
        "inspectSingle" => tree_json(&inspect_claude_single_layer_signature(sig())),
        "inspectDouble" => tree_json(&inspect_claude_double_layer_signature(sig())),
        "inspectGemini" => {
            let r = inspect_gemini_thought_signature(sig(), gemini_opts(case["opts"].as_u64().unwrap()));
            let mut out = json!({"err": err_val(&r)});
            if let Ok(i) = &r {
                out["info"] = json!({
                    "bypass": i.is_bypass_sentinel, "sentinel": i.bypass_sentinel, "dlen": i.decoded_len,
                    "first": i.first_byte, "marker": i.has_observed_marker, "known": i.known_envelope,
                    "envelope": i.envelope.as_str(), "records": i.record_count, "opaque": i.opaque_payload_len,
                });
            }
            out
        }
        "inspectGPT" => {
            let r = inspect_gpt_reasoning_signature(sig());
            let mut out = json!({"err": err_val(&r)});
            if let Ok(i) = &r {
                out["info"] = json!({"dlen": i.decoded_len, "clen": i.ciphertext_len});
            }
            out
        }
        "inspectGrok" => {
            let r = inspect_grok_encrypted_content(sig());
            let mut out = json!({"err": err_val(&r)});
            if let Ok(i) = &r {
                out["info"] = json!({"raw": i.raw_len, "dlen": i.decoded_len});
            }
            out
        }
        "inspectKimi" => {
            let r = inspect_kimi_thinking_signature(sig());
            let mut out = json!({"err": err_val(&r)});
            if let Ok(i) = &r {
                out["info"] = json!({"raw": i.raw_len, "dlen": i.decoded_len, "mode": i.mode.as_str()});
            }
            out
        }
        "compatAG" => {
            let r = compatible_antigravity_claude_thinking_signature(sig());
            json!({"value": r.clone().unwrap_or_default(), "ok": r.is_some()})
        }
        "recognized" => json!({"ok": is_recognized_reasoning_signature(sig())}),
        "split" => {
            let r = split_signature_provider_prefix(sig());
            json!({
                "provider": r.as_ref().map_or("unknown", |(p, _)| p.as_str()),
                // Go returns the untrimmed input as `rest` when there is no provider prefix.
                "rest": r.as_ref().map_or_else(|| sig().to_string(), |(_, rest)| rest.clone()),
                "ok": r.is_some(),
                "payload": signature_payload_without_provider_prefix(sig()),
            })
        }
        "geminiReplay" => {
            let kind = if s("kind") == "fc" {
                SignatureBlockKind::GeminiFunctionCall
            } else {
                SignatureBlockKind::GeminiModelPart
            };
            json!({"value": gemini_replay_signature_or_bypass(sig(), kind)})
        }
        "providerFromModel" => json!({"provider": signature_provider_from_model_name(&s("model")).as_str()}),
        "providerFromPrefix" => json!({"provider": signature_provider_from_cache_prefix(&s("prefix")).as_str()}),
        "stripInvalid" => {
            let o = claude_opts(case["opts"].as_u64().unwrap());
            json!({"value": String::from_utf8_lossy(&strip_invalid_claude_thinking_blocks(payload(), o))})
        }
        "stripInvalidEmpty" => {
            let o = claude_opts(case["opts"].as_u64().unwrap());
            json!({"value": String::from_utf8_lossy(&strip_invalid_claude_thinking_blocks_and_empty_messages(payload(), o))})
        }
        "stripAllowEmpty" => {
            let o = ClaudeSignatureValidationOptions {
                base64_only: true,
                allow_empty_signature_with_empty_text: true,
                ..Default::default()
            };
            json!({"value": String::from_utf8_lossy(&strip_invalid_claude_thinking_blocks(payload(), o))})
        }
        "validateClaude" => {
            let o = claude_opts(case["opts"].as_u64().unwrap());
            json!({"err": err_val(&validate_claude_thinking_signatures(payload(), o))})
        }
        "sanitizeForModel" => {
            let (out, rep) = sanitize_claude_messages_signatures_for_model(payload(), &s("model"));
            report_json(&out, &rep)
        }
        "sanitizeForClaudeUpstream" => {
            let (out, rep) = sanitize_claude_messages_for_claude_upstream(
                payload(),
                &s("model"),
                case["preserve"].as_bool().unwrap(),
            );
            report_json(&out, &rep)
        }
        "sanitizeForTargetGemini" | "sanitizeForTargetClaudeKeepTools" | "sanitizeForTargetGPT"
        | "sanitizeForTargetClaudePlaceholders" => {
            let drop = case["drop"].as_bool().unwrap();
            let opts = match fn_name {
                "sanitizeForTargetGemini" => ClaudeMessagesSignatureSanitizeOptions {
                    target_provider: SignatureProvider::Gemini,
                    target_model: "gemini-3".into(),
                    drop_empty_messages: drop,
                    ..Default::default()
                },
                "sanitizeForTargetClaudeKeepTools" => ClaudeMessagesSignatureSanitizeOptions {
                    target_provider: SignatureProvider::Claude,
                    target_model: "claude-x".into(),
                    drop_empty_messages: drop,
                    ..Default::default()
                },
                "sanitizeForTargetGPT" => ClaudeMessagesSignatureSanitizeOptions {
                    target_provider: SignatureProvider::Gpt,
                    drop_empty_messages: drop,
                    ..Default::default()
                },
                _ => ClaudeMessagesSignatureSanitizeOptions {
                    target_provider: SignatureProvider::Claude,
                    target_model: "claude-x".into(),
                    drop_empty_messages: drop,
                    ..Default::default()
                },
            };
            let (out, rep) = sanitize_claude_messages_signatures_for_target(payload(), &opts);
            report_json(&out, &rep)
        }
        "validateGemini" => {
            let o = gemini_payload_opts(case["opts"].as_u64().unwrap());
            json!({"err": err_val(&validate_gemini_thought_signatures(payload(), o))})
        }
        "pairing" => json!({"err": err_val(&validate_gemini_function_call_pairing(payload()))}),
        "sanitizeGemini" => {
            let out = sanitize_gemini_request_thought_signatures(payload(), &s("path"));
            json!({"value": String::from_utf8_lossy(&out)})
        }
        other => panic!("unknown golden fn {other}"),
    }
}

/// Go's protobuf errors randomly use a non-breaking space after `proto:`; normalize it.
fn scrub_proto_prefix(v: &mut Value) {
    match v {
        Value::String(s) => *s = s.replace("proto:\u{a0}", "proto: "),
        Value::Object(m) => m.iter_mut().for_each(|(_, v)| scrub_proto_prefix(v)),
        Value::Array(a) => a.iter_mut().for_each(scrub_proto_prefix),
        _ => {}
    }
}

/// Resolves `{"$sig": i}` references in recorded outputs.
fn resolve_refs(v: &Value, pool: &[String]) -> Value {
    match v {
        Value::Object(m) if m.len() == 1 && m.contains_key("$sig") => {
            Value::String(pool[m["$sig"].as_u64().unwrap() as usize].clone())
        }
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), resolve_refs(v, pool))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(|v| resolve_refs(v, pool)).collect()),
        other => other.clone(),
    }
}

/// JSON payload outputs are compared after compact re-serialization (key order preserved), since
/// Go leaves untouched regions byte-identical and Rust re-serializes.
fn normalize_payload_strings(v: &mut Value) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(s)) = m.get_mut("value") {
                if cpa_json::valid(s.as_bytes()) {
                    *s = cpa_json::to_string(&cpa_json::parse_str(s));
                }
            }
            for (_, v) in m.iter_mut() {
                normalize_payload_strings(v);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(normalize_payload_strings),
        _ => {}
    }
}

#[test]
fn matches_go_golden_corpus() {
    let cases = golden();
    let pool: Vec<String> = cases[0]["sigs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let mut failures = Vec::new();
    let (mut checked, mut mismatches) = (0usize, 0usize);
    for case in &cases[1..] {
        let mut want = resolve_refs(&case["out"], &pool);
        let mut got = run_case(case, &pool);
        scrub_proto_prefix(&mut want);
        normalize_payload_strings(&mut want);
        normalize_payload_strings(&mut got);
        checked += 1;
        if want != got {
            mismatches += 1;
        }
        if want != got && failures.len() < 12 {
            let mut shown = case.clone();
            if let Some(i) = case.get("sig").and_then(Value::as_u64) {
                let s = &pool[i as usize];
                shown["sig"] = Value::String(s.chars().take(80).collect());
            }
            failures.push(format!("case {shown}\n  want {want}\n  got  {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{mismatches} of {checked} cases differ, first {}:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ---- focused unit tests

fn varint_bytes(v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    append::varint(&mut out, v);
    out
}

#[test]
fn protowire_varint_edges() {
    assert_eq!(protowire::consume_varint(&[]), Err(ParseError::Truncated));
    assert_eq!(protowire::consume_varint(&[0x80]), Err(ParseError::Truncated));
    assert_eq!(protowire::consume_varint(&[0x7f]), Ok((127, 1)));
    for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
        let enc = varint_bytes(v);
        assert_eq!(protowire::consume_varint(&enc), Ok((v, enc.len())));
    }
    // Tenth byte above 1 overflows.
    let mut overflow = vec![0xff; 9];
    overflow.push(0x02);
    assert_eq!(protowire::consume_varint(&overflow), Err(ParseError::Overflow));
    // Field number zero and truncated length-delimited values are rejected.
    assert_eq!(protowire::consume_tag(&[0x00]), Err(ParseError::FieldNumber));
    assert_eq!(protowire::consume_bytes(&[0x05, 1, 2]), Err(ParseError::Truncated));
}

#[test]
fn protowire_group_skipping() {
    // field 1 start-group, inner varint field 2, end-group field 1
    let mut b = Vec::new();
    append::tag(&mut b, 2, WireType::Varint);
    append::varint(&mut b, 7);
    append::tag(&mut b, 1, WireType::EndGroup);
    assert_eq!(
        protowire::consume_field_value(1, WireType::StartGroup, &b),
        Ok(b.len())
    );
    assert_eq!(
        protowire::consume_field_value(9, WireType::StartGroup, &b),
        Err(ParseError::EndGroup)
    );
    assert_eq!(
        protowire::consume_field_value(1, WireType::Other(6), &b),
        Err(ParseError::Reserved)
    );
}

#[test]
fn go_quote_matches_strconv_quote() {
    assert_eq!(go_quote("E"), "\"E\"");
    assert_eq!(go_quote("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    assert_eq!(go_quote("\u{7f}\u{1}"), "\"\\x7f\\x01\"");
    assert_eq!(go_quote("\u{a0}"), "\"\\u00a0\"");
    assert_eq!(go_quote("é"), "\"é\"");
}

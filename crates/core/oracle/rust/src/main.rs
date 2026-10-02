//! Rust side of the base-layer differential harness: reads the same JSONL ops as the Go oracle
//! (`../go/base_oracle.go.txt`) and prints one JSON result per line in the same shape.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use cpa_core::registry::{self, ClientModelProjection, ModelInfo};
use cpa_core::{applypatch, misc, util};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, Default)]
#[serde(default)]
struct Op {
    op: String,
    #[serde(rename = "in")]
    in1: String,
    in2: String,
    in3: String,
    flag: bool,
    steps: Vec<Value>,
}

/// Go marshals nil maps as null and sorts keys.
fn map_or_null(m: std::collections::HashMap<String, String>) -> Value {
    if m.is_empty() {
        return Value::Null;
    }
    let sorted: BTreeMap<_, _> = m.into_iter().collect();
    json!(sorted)
}

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::BufWriter::new(std::io::stdout());
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let o: Op = match serde_json::from_str(&line) {
            Ok(o) => o,
            Err(e) => {
                let _ = writeln!(out, "{}", json!({"error": e.to_string()}));
                continue;
            }
        };
        let res = std::panic::catch_unwind(|| run(o)).unwrap_or_else(|_| json!({"panic": "rust panic"}));
        let _ = writeln!(out, "{res}");
    }
}

fn run(o: Op) -> Value {
    match o.op.as_str() {
        "clean_gemini" => json!(util::clean_json_schema_for_gemini(&o.in1)),
        "clean_gemini_json_schema" => json!(util::clean_json_schema_for_gemini_json_schema(&o.in1)),
        "clean_antigravity" => json!(util::clean_json_schema_for_antigravity(&o.in1)),
        "clean_antigravity_tool" => json!(util::clean_json_schema_for_antigravity_tool(&o.in1, o.flag)),
        "clean_antigravity_response" => json!(util::clean_json_schema_for_antigravity_response(&o.in1)),
        "inline_local_refs" => json!(util::inline_local_refs(&o.in1)),
        "normalize_claude_schema" => {
            json!(String::from_utf8_lossy(&util::normalize_claude_tool_input_schema(o.in1.as_bytes())))
        }
        "unicode_escape" => json!(util::has_unsupported_unicode_property_escape(&o.in1)),
        "fix_json" => json!(util::fix_json(&o.in1)),
        "sanitize_function_name" => json!(util::sanitize_function_name(&o.in1)),
        "sanitize_claude_function_name" => json!(util::sanitize_claude_function_name(&o.in1)),
        "sanitize_claude_tool_id" => json!(util::sanitize_claude_tool_id(&o.in1)),
        "gemini_claude_tool_use_id" => json!(util::gemini_claude_tool_use_id(&o.in1, &o.in2, &o.in3)),
        "is_gemini_claude_tool_use_id" => json!(util::is_gemini_claude_tool_use_id(&o.in1)),
        "tool_maps" => {
            let b = o.in1.as_bytes();
            json!({
                "claude": map_or_null(util::tool_name_map_from_claude_request(b)),
                "sanitized_fn": map_or_null(util::sanitized_function_name_map(b)),
                "disambiguated": map_or_null(util::disambiguated_tool_name_map(b)),
                "sanitized_tool": map_or_null(util::sanitized_tool_name_map(b)),
            })
        }
        "dedupe" => json!(String::from_utf8_lossy(&util::deduplicate_function_declarations(o.in1.as_bytes()))),
        "unwrap_custom" => json!(util::unwrap_responses_custom_tool_input(&o.in1)),
        "strip_attribution" => {
            json!(String::from_utf8_lossy(&util::strip_claude_code_attribution_system(o.in1.as_bytes())))
        }
        "tool_result" => {
            let v = serde_json::from_str::<Value>(&o.in1).ok();
            let r = util::convert_claude_tool_result_content(v.as_ref());
            let images: Vec<Value> =
                r.images.iter().map(|i| json!({"MimeType": i.mime_type, "Data": i.data})).collect();
            json!({"result": r.result, "raw": r.result_is_raw, "images": images})
        }
        "responses_tools" => {
            let root = cpa_json::parse_str(&o.in1);
            let (decls, fwd, rev) = util::build_gemini_function_declarations(&root);
            let ds: Vec<String> = decls.iter().map(|d| d.to_string()).collect();
            let rev_out: BTreeMap<String, Value> = rev
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        json!({"name": v.name, "namespace": v.namespace, "custom": v.custom, "apply_patch": v.apply_patch}),
                    )
                })
                .collect();
            let mut winners: Vec<String> = util::collect_responses_tool_winners(&root)
                .into_iter()
                .map(|(k, v)| {
                    format!(
                        "{}|{}|{}|{}|{}|{}|{}",
                        k, v.local_name, v.namespace, v.tool_type, v.source_priority, v.direct, v.order
                    )
                })
                .collect();
            winners.sort();
            let reverse_from_raw = util::responses_tool_reverse_identity_map(o.in1.as_bytes()).len();
            json!({"decls": ds, "forward": map_or_null(fwd), "reverse": rev_out, "winners": winners, "reverse_from_raw": reverse_from_raw})
        }
        "tool_choice" => {
            let fwd: std::collections::HashMap<String, String> = serde_json::from_str(&o.in2).unwrap_or_default();
            let tc = serde_json::from_str::<Value>(&o.in1).ok();
            let cfg = util::convert_responses_tool_choice_to_gemini(tc.as_ref(), &fwd);
            json!({"cfg": cfg.as_ref().map(|c| c.to_string()).unwrap_or_default(), "ok": cfg.is_some()})
        }
        "qualify" => json!(util::qualify_responses_namespace_tool_name(&o.in1, &o.in2)),
        "hide_api_key" => json!(util::hide_api_key(&o.in1)),
        "mask_query" => json!(util::mask_sensitive_query(&o.in1)),
        "mask_header" => json!(util::mask_sensitive_header_value(&o.in1, &o.in2)),
        "openai_compat_key" => json!(util::openai_compatible_provider_key(&o.in1)),
        "applypatch" => {
            let tool = cpa_json::parse_str(&o.in1);
            let un = applypatch::unwrap_input(&o.in2);
            json!({
                "is_custom": applypatch::is_custom_tool(&tool),
                "desc": applypatch::description(&tool),
                "wrap": applypatch::wrap_input(&o.in3),
                "escape": applypatch::escape_input_fragment(&o.in3),
                "unwrap": un.clone().unwrap_or_default(),
                "unwrap_err": if un.is_err() { "err" } else { "" },
                "params": String::from_utf8_lossy(&applypatch::parameters()),
            })
        }
        "oauth_callback" => match misc::parse_oauth_callback(&o.in1) {
            Err(e) => json!({"err": e}),
            Ok(None) => json!({"nil": true}),
            Ok(Some(cb)) => {
                json!({"code": cb.code, "state": cb.state, "error": cb.error, "desc": cb.error_description})
            }
        },
        "antigravity_ua" => json!({
            "req": misc::antigravity_request_user_agent(&o.in1),
            "onboard": misc::antigravity_onboard_user_user_agent(&o.in1),
            "version": misc::antigravity_version_from_user_agent(&o.in1),
        }),
        "mime" => {
            let v = misc::mime_type_for_extension(&o.in1);
            json!({"v": v.unwrap_or(""), "ok": v.is_some()})
        }
        "catalogs" => json!({
            "claude": registry::get_claude_models(), "gemini": registry::get_gemini_models(),
            "vertex": registry::get_gemini_vertex_models(), "aistudio": registry::get_ai_studio_models(),
            "codex_free": registry::get_codex_free_models(), "codex_team": registry::get_codex_team_models(),
            "codex_plus": registry::get_codex_plus_models(), "codex_pro": registry::get_codex_pro_models(),
            "kimi": registry::get_kimi_models(), "antigravity": registry::get_antigravity_models(),
            "xai": registry::get_xai_models(), "meta": registry::get_meta_models(),
            "devin": registry::get_devin_models(),
        }),
        "lookup_static" => json!(registry::lookup_static_model_info(&o.in1)),
        "lookup_devin" => json!(registry::lookup_devin_model(&o.in1)),
        "lookup_static_channel" => json!(registry::lookup_static_model_info_by_channel(&o.in1, &o.in2)),
        "native_flags" => {
            let mut out = Vec::new();
            for m in registry::get_codex_pro_models().into_iter().chain(registry::get_claude_models()) {
                let caps = m.native_capabilities.as_ref().map(|c| match c.web_search {
                    Some(b) => json!({"web_search": b}),
                    None => json!({}),
                });
                out.push(json!({"ID": m.id, "NativeCaps": caps, "ConfigUpd": m.support_configuration_update}));
            }
            json!(out)
        }
        "codex_client" => {
            let (d, rev) = registry::get_codex_client_models_snapshot();
            json!({"len": d.len(), "rev": rev, "devin_rev": registry::get_devin_models_revision()})
        }
        "registry_scenario" => scenario(&o),
        other => json!({"unknown_op": other}),
    }
}

fn null_if_empty(v: Value) -> Value {
    match &v {
        Value::Array(a) if a.is_empty() => Value::Null,
        _ => v,
    }
}

fn scenario(o: &Op) -> Value {
    let reg = registry::global_registry();
    let mut outs: Vec<Value> = Vec::new();
    let mut touched = std::collections::BTreeSet::new();
    for s in &o.steps {
        let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let client = g("client");
        let epoch_for = |c: &str| -> u64 {
            let off = s.get("epoch_offset").and_then(Value::as_i64).unwrap_or(0);
            (reg.client_registration_epoch(c) as i64 + off) as u64
        };
        match g("op").as_str() {
            "register" => {
                touched.insert(client.clone());
                let models: Vec<ModelInfo> = s
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|m| serde_json::from_value(m.clone()).ok()).collect())
                    .unwrap_or_default();
                reg.register_client(&client, &g("provider"), &models);
                outs.push(Value::Null);
            }
            "unregister" => {
                reg.unregister_client(&client);
                outs.push(Value::Null);
            }
            "suspend" => {
                reg.suspend_client_model(&client, &g("model"), &g("reason"));
                outs.push(Value::Null);
            }
            "resume" => {
                reg.resume_client_model(&client, &g("model"));
                outs.push(Value::Null);
            }
            "quota" => {
                reg.set_model_quota_exceeded(&client, &g("model"));
                outs.push(Value::Null);
            }
            "clear_quota" => {
                reg.clear_model_quota_exceeded(&client, &g("model"));
                outs.push(Value::Null);
            }
            "project" => {
                let projs: Vec<ClientModelProjection> = s
                    .get("projections")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|p| ClientModelProjection {
                                model_id: p.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
                                suspended: p.get("suspended").and_then(Value::as_bool).unwrap_or(false),
                                suspend_reason: p.get("reason").and_then(Value::as_str).unwrap_or("").to_string(),
                                quota_exceeded: p.get("quota").and_then(Value::as_bool).unwrap_or(false),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let generation = s.get("generation").and_then(Value::as_u64).unwrap_or(0);
                outs.push(json!(reg.apply_client_model_projections(&client, epoch_for(&client), generation, &projs)));
            }
            "capabilities" => {
                let ws = s.get("web_search").and_then(Value::as_bool).unwrap_or(false);
                outs.push(json!(reg.apply_client_model_capabilities(&client, epoch_for(&client), |_, info| {
                    info.supports_web_search = ws
                })));
            }
            "cleanup" => {
                reg.cleanup_expired_quotas();
                outs.push(Value::Null);
            }
            "query" => outs.push(snapshot(reg, s)),
            _ => {}
        }
    }
    for c in touched {
        reg.unregister_client(&c);
    }
    json!(outs)
}

fn strs(s: &Value, k: &str) -> Vec<String> {
    s.get(k)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn snapshot(reg: &registry::ModelRegistry, s: &Value) -> Value {
    let mut q = serde_json::Map::new();
    for h in ["openai", "claude", "gemini", ""] {
        let mut list = reg.get_available_models(h);
        let key = |m: &serde_json::Map<String, Value>| {
            m.get("id")
                .and_then(Value::as_str)
                .or_else(|| m.get("name").and_then(Value::as_str))
                .unwrap_or("")
                .to_string()
        };
        list.sort_by_key(|m| key(m));
        q.insert(format!("avail_{h}"), null_if_empty(json!(list)));
    }
    q.insert("avail_infos".into(), json!(reg.get_available_model_infos()));
    let model_ids = strs(s, "model_ids");
    let providers = strs(s, "providers");
    let clients = strs(s, "clients");
    let (mut counts, mut provs, mut info_by) = (serde_json::Map::new(), serde_json::Map::new(), serde_json::Map::new());
    for m in &model_ids {
        counts.insert(m.clone(), json!(reg.get_model_count(m)));
        provs.insert(m.clone(), null_if_empty(json!(reg.get_model_providers(m))));
        info_by.insert(format!("{m}|"), json!(reg.get_model_info(m, "")));
        for p in &providers {
            info_by.insert(format!("{m}|{p}"), json!(reg.get_model_info(m, p)));
        }
        q.insert(format!("ws_{m}"), json!(reg.get_responses_web_search_capability(m)));
    }
    q.insert("counts".into(), Value::Object(counts));
    q.insert("providers".into(), Value::Object(provs));
    q.insert("info_by".into(), Value::Object(info_by));
    let mut by_prov = serde_json::Map::new();
    for p in &providers {
        by_prov.insert(p.clone(), null_if_empty(json!(reg.get_available_models_by_provider(p))));
    }
    q.insert("by_provider".into(), Value::Object(by_prov));
    let mut cl = serde_json::Map::new();
    for c in &clients {
        let (models, epoch) = reg.get_models_and_epoch_for_client(c);
        cl.insert(c.clone(), json!({"models": null_if_empty(json!(models)), "epoch": epoch}));
        for m in &model_ids {
            cl.insert(
                format!("{c}|{m}"),
                json!([
                    reg.client_supports_model(c, m),
                    reg.is_model_suspended_for_client(c, m),
                    reg.is_model_quota_exceeded_for_client(c, m)
                ]),
            );
        }
    }
    q.insert("clients".into(), Value::Object(cl));
    q.insert(
        "first".into(),
        json!(reg.get_first_available_model("openai").unwrap_or_else(|_| "ERR".into())),
    );
    Value::Object(q)
}

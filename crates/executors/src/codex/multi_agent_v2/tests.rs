//! Ports of optimize_multi_agent_v2_test.go (translate wrappers, gin-context and benchmark cases
//! excluded: they exercise code that does not exist here).

use super::*;
use cpa_core::registry::ThinkingSupport;

/// Serializes the tests that register clients in the process-wide model registry.
static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn headers_with_ua(ua: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("user-agent", ua.parse().unwrap());
    h
}

fn enabled_cfg() -> Config {
    let mut cfg = Config::default();
    cfg.client.codex.optimize_multi_agent_v2 = true;
    cfg
}

fn parsed(bytes: &[u8]) -> Value {
    cpa_json::parse(bytes)
}

fn gstr(bytes: &[u8], path: &str) -> String {
    parsed(bytes).g(path).str()
}

fn gexists(bytes: &[u8], path: &str) -> bool {
    parsed(bytes).g(path).exists()
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => panic!("not an object"),
    }
}

fn optimize(headers: &HeaderMap, payload: &str, cfg: Option<&Config>) -> (Vec<u8>, bool) {
    optimize_request(&RequestCtx::default(), headers, payload.as_bytes(), cfg)
}

#[test]
fn is_codex_multi_agent_client() {
    let cases = [
        (
            "Codex Desktop/0.146.0-alpha.3 (Mac OS 26.5.2; arm64) unknown (Codex Desktop; 26.721.30844)",
            true,
        ),
        (
            "codex-tui/0.154.0 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.154.0)",
            true,
        ),
        (
            "codex_cli_rs/0.144.1 (Mac OS 26.3.1; arm64) iTerm.app/3.6.9",
            true,
        ),
        ("codex_cli_rs", true),
        (
            "codex_exec/0.153.2 (Mac OS 26.6.2; arm64) unknown (codex_exec; 0.153.2)",
            true,
        ),
        ("curl/8.7.1", false),
        ("proxy Codex Desktop/0.146.0", false),
    ];
    for (ua, want) in cases {
        assert_eq!(is_codex_client_user_agent(ua), want, "{ua}");
    }
}

#[test]
fn spawn_agent_models_from_sources_includes_model_metadata() {
    let catalog = br#"{"models":[
        {"slug":"model-template","display_name":"Template","description":"Template model.","default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"}],"service_tiers":[{"id":"priority"}],"priority":1},
        {"slug":"gpt-5.5","display_name":"Default","description":"Default model.","default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}],"service_tiers":[{"id":"priority"}],"priority":2}
    ]}"#;
    let available = vec![
        obj(
            cpa_json::json!({"id": "custom-model", "display_name": "Custom", "description": "Registry description."}),
        ),
        obj(cpa_json::json!({"id": "model-template"})),
        obj(cpa_json::json!({"id": "custom-model", "description": "duplicate"})),
    ];
    let lookup = |model_id: &str| {
        (model_id == "custom-model").then(|| ModelInfo {
            description: "Dynamic model.".into(),
            thinking: Some(ThinkingSupport {
                levels: ["none", "low", "medium", "high"].map(String::from).to_vec(),
                ..Default::default()
            }),
            ..Default::default()
        })
    };

    let models = spawn_agent_models_from_sources(&available, catalog, &lookup);
    assert_eq!(models.len(), 2);
    let got = &models[0];
    assert_eq!(got.id, "model-template");
    assert_eq!(got.description, "Template model.");
    assert_eq!(got.default_reasoning_effort, "low");
    assert_eq!(models[0].service_tiers.join(","), "priority");
    let custom = &models[1];
    assert_eq!(custom.id, "custom-model");
    assert_eq!(custom.description, "Dynamic model.");
    assert_eq!(custom.reasoning_efforts.join(","), "none,low,medium,high");
    assert_eq!(custom.default_reasoning_effort, "medium");
    assert!(custom.service_tiers.is_empty());
}

#[test]
fn decode_home_available_models_sorts_and_dedupes() {
    let raw = br#"{
        "codex":[{"id":"model-b","display_name":"Model B"},{"id":"model-a"}],
        "other":[{"name":"models/model-c","displayName":"Model C"},{"id":"model-a","display_name":"duplicate"}]
    }"#;
    let models = decode_home_available_models(raw);
    assert_eq!(models.len(), 3);
    assert_eq!(map_string(&models[0], "id"), "model-a");
    assert_eq!(map_string(&models[1], "description"), "Model B");
    assert_eq!(map_string(&models[2], "id"), "model-c");
    assert!(decode_home_available_models(br#"{"error":{"type":"no_credentials"}}"#).is_empty());
}

/// The Home models query goes out with the client headers and an empty `client_version`, and the
/// answer is decoded like Go's codexHomeAvailableModels. Uses a local client, not the global one.
#[tokio::test]
async fn home_models_query_sends_headers_and_decodes_the_answer() {
    use cpa_home::testing::{MockHome, bulk};

    let raw = r#"{"codex":[{"id":"model-b","display_name":"Model B"},{"id":"model-a"}]}"#;
    let mock = MockHome::start(move |_| bulk(raw)).await;
    let cfg = cpa_config::HomeConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        port: i64::from(mock.port()),
        ..Default::default()
    };
    let client = cpa_home::Client::new(cfg);
    client.set_heartbeat_ok_for_tests(true);

    let models = query_home_models(&client, &headers_with_ua("codex_cli_rs/0.144.1"))
        .await
        .expect("models");
    assert_eq!(models.len(), 2);
    assert_eq!(map_string(&models[0], "id"), "model-a");
    assert_eq!(map_string(&models[1], "description"), "Model B");

    let commands = mock.commands();
    let get = commands
        .iter()
        .find(|c| c.first().is_some_and(|n| n.eq_ignore_ascii_case("get")))
        .expect("get command");
    let request = parsed(get[1].as_bytes());
    assert_eq!(request.g("type").str(), "models");
    assert_eq!(
        request.g("headers.user-agent").str(),
        "codex_cli_rs/0.144.1"
    );
    assert!(request.g("query.client_version").exists());
    assert_eq!(request.g("query.client_version").str(), "");
}

#[test]
fn rewrite_spawn_agent_description_normalizes_model_list() {
    let payload = br#"{
        "input":[{
            "type":"additional_tools",
            "role":"developer",
            "tools":[{
                "type":"namespace",
                "name":"collaboration",
                "tools":[
                    {"type":"function","name":"send_message","description":"unchanged"},
                    {"type":"function","name":"spawn_agent","description":"\n        Available model overrides (optional; inherited parent model is preferred):\n- old duplicate\n- old duplicate\n        Spawns an agent to work on a task.","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
                ]
            }]
        }]
    }"#;
    let models = vec![
        SpawnAgentModel {
            id: "model-alpha".into(),
            description: "Alpha model.".into(),
            reasoning_efforts: ["low", "medium", "high"].map(String::from).to_vec(),
            default_reasoning_effort: "medium".into(),
            service_tiers: vec!["priority".into()],
            ..Default::default()
        },
        SpawnAgentModel {
            id: "model-beta".into(),
            description: "Beta model".into(),
            reasoning_efforts: ["low", "high"].map(String::from).to_vec(),
            default_reasoning_effort: "low".into(),
            ..Default::default()
        },
    ];
    let got = rewrite_spawn_agent_description_with_models(payload, &models);
    let description = gstr(&got, "input.0.tools.0.tools.1.description");
    let want_alpha = "- `model-alpha`: Alpha model. Reasoning efforts: low, medium (default), high. Service tiers: priority.";
    let want_beta = "- `model-beta`: Beta model. Reasoning efforts: low (default), high.";
    assert!(
        description.contains(want_alpha) && description.contains(want_beta),
        "{description}"
    );
    assert!(!description.contains("old duplicate"), "{description}");
    for id in ["model-alpha", "model-beta"] {
        assert_eq!(description.matches(&format!("`{id}`")).count(), 1);
    }
    assert!(description.find("`model-beta`") < description.find(SPAWN_AGENT_DESCRIPTION_MARKER));
    assert_eq!(
        gstr(&got, "input.0.tools.0.tools.0.description"),
        "unchanged"
    );
    assert!(!gexists(
        &got,
        "input.0.tools.0.tools.1.parameters.properties.message.encrypted"
    ));
}

#[test]
fn rewrite_spawn_agent_description_top_level_without_marker() {
    let payload = br#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Create a worker."}]}]}"#;
    let models = vec![SpawnAgentModel {
        id: "model-a".into(),
        description: "Model A.".into(),
        reasoning_efforts: vec!["medium".into()],
        default_reasoning_effort: "medium".into(),
        ..Default::default()
    }];
    let got = rewrite_spawn_agent_description_with_models(payload, &models);
    let description = gstr(&got, "tools.0.tools.0.description");
    let want_suffix = format!(
        "{SPAWN_AGENT_MODELS_HEADING}\n- `model-a`: Model A. Reasoning efforts: medium (default)."
    );
    assert!(
        description.starts_with("Create a worker.\n\n") && description.ends_with(&want_suffix),
        "{description:?}"
    );
}

#[test]
fn spawn_agent_tool_paths_ignore_invalid_containers() {
    let payload = br#"{
        "input":[{"type":"message","tools":[{"type":"function","name":"spawn_agent","description":"message"}]}],
        "tools":[
            {"type":"function","name":"wrapper","tools":[{"type":"function","name":"spawn_agent","description":"child"}]},
            {"type":"custom","name":"spawn_agent","description":"custom"},
            {"type":"namespace","name":"spawn_agent","description":"namespace"}
        ]
    }"#;
    assert!(spawn_agent_tool_paths(&parsed(payload)).is_empty());
}

#[test]
fn optimize_skips_namespace_conflict() {
    let payload = r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]},{"type":"namespace","name":"collaboration-optimize","tools":[]}]}"#;
    let cfg = enabled_cfg();
    let (got, optimized) = optimize(&headers_with_ua("codex-tui/0.154.0"), payload, Some(&cfg));
    assert!(!optimized);
    assert_eq!(got, payload.as_bytes());
    assert!(has_namespace_conflict(payload.as_bytes()));
}

#[test]
fn optimize_skips_dot_prefix_conflict() {
    let payload = r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]},{"type":"function","name":"collaboration-optimize.tool"}]}"#;
    let cfg = enabled_cfg();
    let (got, optimized) = optimize(&headers_with_ua("codex-tui/0.154.0"), payload, Some(&cfg));
    assert!(!optimized);
    assert_eq!(got, payload.as_bytes());
}

#[test]
fn optimize_collaboration_namespace_without_models() {
    let payload = r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent"}]}]}"#;
    let mut root = parsed(payload.as_bytes());
    let paths = spawn_agent_tool_paths(&root);
    let (_, optimized) = optimize_collaboration_namespace(&mut root, &paths);
    assert!(optimized);
    assert_eq!(
        root.g("tools.0.name").str(),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
}

#[test]
fn rewrite_spawn_agent_description_without_models_still_removes_encrypted() {
    let payload = br#"{"tools":[{"type":"function","name":"spawn_agent","description":"unchanged","parameters":{"properties":{"message":{"encrypted":true}}}}]}"#;
    let got = rewrite_spawn_agent_description_with_models(payload, &[]);
    assert_eq!(gstr(&got, "tools.0.description"), "unchanged");
    assert!(!gexists(
        &got,
        "tools.0.parameters.properties.message.encrypted"
    ));
}

#[test]
fn rewrite_spawn_agent_description_leaves_payload_without_tool_unchanged() {
    let payload = br#"{"tools":[{"type":"function","name":"other","description":"unchanged"}]}"#;
    let models = vec![SpawnAgentModel {
        id: "model-a".into(),
        description: "Model A.".into(),
        ..Default::default()
    }];
    assert_eq!(
        rewrite_spawn_agent_description_with_models(payload, &models),
        payload
    );
}

#[test]
fn rewrite_spawn_agent_description_enabled_optimizes_tool() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let model_id = "codex-spawn-agent-test-model";
    let client_id = "codex-spawn-agent-test-client";
    let registry = global_registry();
    registry.register_client(
        client_id,
        "codex",
        &[ModelInfo {
            id: model_id.into(),
            description: "Test agent model.".into(),
            thinking: Some(ThinkingSupport {
                levels: ["low", "medium", "high"].map(String::from).to_vec(),
                ..Default::default()
            }),
            ..Default::default()
        }],
    );

    let payload = r#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"type":"string","encrypted":true}}}}]}]}"#;
    let cfg = enabled_cfg();
    let (got, optimized) = optimize(
        &headers_with_ua("Codex Desktop/0.146.0-alpha.3"),
        payload,
        Some(&cfg),
    );
    registry.unregister_client(client_id);

    assert!(optimized);
    assert_eq!(
        gstr(&got, "tools.0.name"),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
    let description = gstr(&got, "tools.0.tools.0.description");
    let want = format!(
        "- `{model_id}`: Test agent model. Reasoning efforts: low, medium (default), high."
    );
    assert!(description.contains(&want), "{description:?}");
    assert!(!gexists(
        &got,
        "tools.0.tools.0.parameters.properties.message.encrypted"
    ));
}

#[test]
fn prepare_tools_only_prepares_tool_definitions() {
    let payload = br#"{
        "input":[
            {"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"task"}]},
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}},
                    {"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}}
                ]}
            ]}
        ]
    }"#;
    let (got, prepared) = prepare_tools(
        &RequestCtx::default(),
        &headers_with_ua("codex_cli_rs/0.144.1"),
        payload,
        true,
        false,
    );
    assert!(prepared);
    assert_eq!(gstr(&got, "input.0.content.0.type"), "encrypted_content");
    assert_eq!(gstr(&got, "input.1.tools.0.name"), COLLABORATION_NAMESPACE);
    for path in ["input.1.tools.0.tools.0", "input.1.tools.0.tools.1"] {
        assert!(
            !gexists(
                &got,
                &format!("{path}.parameters.properties.message.encrypted")
            ),
            "{path}"
        );
    }
}

#[test]
fn optimize_skips_prepared_tool_refresh() {
    let payload = "{\"tools\":[{\"type\":\"namespace\",\"name\":\"collaboration\",\"tools\":[{\"type\":\"function\",\"name\":\"spawn_agent\",\"description\":\"Available model overrides (optional; inherited parent model is preferred):\\n- old-model: Old model.\\nSpawns an agent.\",\"parameters\":{\"properties\":{\"message\":{\"encrypted\":true}}}}]}]}";
    let cfg = enabled_cfg();
    let ctx = RequestCtx {
        tools_prepared: true,
        ..Default::default()
    };
    let (got, optimized) = optimize_request(
        &ctx,
        &headers_with_ua("codex_cli_rs/0.144.1"),
        payload.as_bytes(),
        Some(&cfg),
    );
    assert!(optimized);
    assert!(gstr(&got, "tools.0.tools.0.description").contains("old-model"));
    assert!(!gexists(
        &got,
        "tools.0.tools.0.parameters.properties.message.encrypted"
    ));
}

#[test]
fn optimize_normalizes_agent_message_content_only() {
    let payload = r#"{"input":[{"type":"agent_message","id":"amsg_1","author":"/root","recipient":"/root/worker","content":[{"type":"input_text","text":"Payload:\n"},{"type":"encrypted_content","encrypted_content":"delegated task"}],"internal_chat_message_metadata_passthrough":{"turn_id":"turn_1"}}]}"#;
    let headers = headers_with_ua("Codex Desktop/0.146.0-alpha.3");
    let cfg = enabled_cfg();
    let (got, namespace_optimized) = optimize(&headers, payload, Some(&cfg));
    assert!(!namespace_optimized);
    let message = parsed(&got).g("input.0").value();
    assert_eq!(message.g("type").str(), "agent_message");
    assert!(!message.g("role").exists());
    assert_eq!(message.g("content.1.type").str(), "input_text");
    assert_eq!(message.g("content.1.text").str(), "delegated task");
    assert!(!message.g("content.1.encrypted_content").exists());
    assert_eq!(message.g("author").str(), "/root");
    assert_eq!(message.g("recipient").str(), "/root/worker");
    assert_eq!(
        message
            .g("internal_chat_message_metadata_passthrough.turn_id")
            .str(),
        "turn_1"
    );

    let default_cfg = Config::default();
    let curl = headers_with_ua("curl/8.7.1");
    for (headers, cfg) in [(&headers, &default_cfg), (&curl, &cfg)] {
        let (unchanged, _) = optimize(headers, payload, Some(cfg));
        assert_eq!(unchanged, payload.as_bytes());
    }
}

#[test]
fn restore_response_maps_only_tool_call_namespaces() {
    let payload = br#"{
        "type":"response.completed",
        "response":{
            "output":[
                {"type":"function_call","name":"spawn_agent","namespace":"collaboration-optimize","arguments":{"namespace":"collaboration-optimize","name":"collaboration-optimize__opaque"}},
                {"type":"function_call","name":"collaboration-optimize__send_message"},
                {"type":"message","namespace":"collaboration-optimize","name":"collaboration-optimize__plain"}
            ],
            "tools":[{"type":"namespace","name":"collaboration-optimize"}]
        }
    }"#;
    let got = restore_response(payload, true);
    assert_eq!(
        gstr(&got, "response.output.0.namespace"),
        COLLABORATION_NAMESPACE
    );
    assert_eq!(
        gstr(&got, "response.output.1.name"),
        "collaboration__send_message"
    );
    assert_eq!(gstr(&got, "response.tools.0.name"), COLLABORATION_NAMESPACE);
    assert_eq!(
        gstr(&got, "response.output.0.arguments.namespace"),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
    assert_eq!(
        gstr(&got, "response.output.2.namespace"),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
    assert_eq!(
        gstr(&got, "response.output.2.name"),
        "collaboration-optimize__plain"
    );
    assert_eq!(restore_response(payload, false), payload);
}

#[test]
fn restore_response_restores_dotted_flat_tool_name() {
    let payload = br#"{
        "type":"response.completed",
        "response":{
            "output":[{
                "type":"function_call",
                "name":"collaboration-optimize.spawn_agent",
                "namespace":null,
                "arguments":"{}",
                "call_id":"call_1"
            },{
                "type":"custom_tool_call",
                "name":"collaboration-optimize.list_agents",
                "input":"{}",
                "call_id":"call_2"
            }]
        }
    }"#;
    let got = restore_response(payload, true);
    assert_eq!(
        gstr(&got, "response.output.0.namespace"),
        COLLABORATION_NAMESPACE
    );
    assert_eq!(gstr(&got, "response.output.0.name"), "spawn_agent");
    assert_eq!(
        gstr(&got, "response.output.1.namespace"),
        COLLABORATION_NAMESPACE
    );
    assert_eq!(gstr(&got, "response.output.1.name"), "list_agents");
}

#[test]
fn restore_response_reencodes_like_go_marshal() {
    // A changed payload goes through Go's map round trip: sorted keys, HTML escaping, numbers kept.
    let payload = br#"{"z":1.50,"response":{"output":[{"type":"function_call","namespace":"collaboration-optimize","name":"a<b"}]},"a":"x&y"}"#;
    let got = restore_response(payload, true);
    assert_eq!(
        String::from_utf8(got).unwrap(),
        r#"{"a":"x\u0026y","response":{"output":[{"name":"a\u003cb","namespace":"collaboration","type":"function_call"}]},"z":1.50}"#
    );
}

const AGENT_MESSAGE_PAYLOAD: &str = r#"{"model":"gpt-5.4","input":[{
    "type":"agent_message",
    "id":"amsg_019f92ae-84fd-76f0-aa66-5a722dee382e",
    "author":"/root",
    "recipient":"/root/arithmetic_problem",
    "content":[
        {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/arithmetic_problem\nSender: /root\nPayload:\n"},
        {"type":"encrypted_content","encrypted_content":"请出一道四则运算题，并给出答案。全程使用简体中文，题目简洁。"}
    ],
    "internal_chat_message_metadata_passthrough":{"turn_id":"019f92ae-7eae-7371-957e-8f6f734edddc"}
}]}"#;

#[test]
fn rewrite_input_rewrites_agent_message() {
    let cfg = enabled_cfg();
    let headers = headers_with_ua("Codex Desktop/0.146.0-alpha.3");
    let got = rewrite_input(
        &headers,
        AGENT_MESSAGE_PAYLOAD.as_bytes(),
        Some(&cfg),
        false,
    );
    assert_eq!(gstr(&got, "input.0.type"), "message");
    assert_eq!(gstr(&got, "input.0.role"), "user");
    assert_eq!(gstr(&got, "input.0.content.1.type"), "input_text");
    assert_eq!(
        gstr(&got, "input.0.content.1.text"),
        "请出一道四则运算题，并给出答案。全程使用简体中文，题目简洁。"
    );
    assert!(!gexists(&got, "input.0.content.1.encrypted_content"));
    assert_eq!(gstr(&got, "input.0.author"), "/root");
    assert_eq!(
        gstr(
            &got,
            "input.0.internal_chat_message_metadata_passthrough.turn_id"
        ),
        "019f92ae-7eae-7371-957e-8f6f734edddc"
    );
}

#[test]
fn rewrite_input_strips_author_and_recipient_issue_6136() {
    let payload = br#"{"model":"gpt-5.4","input":[{
        "type":"agent_message",
        "id":"amsg_1",
        "author":"/root",
        "recipient":"/root/worker",
        "content":[
            {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\n"},
            {"type":"encrypted_content","encrypted_content":"test task"}
        ],
        "internal_chat_message_metadata_passthrough":{"turn_id":"turn_1"}
    },{
        "type":"message",
        "role":"user",
        "id":"msg_2",
        "author":"/root/worker",
        "recipient":"/root",
        "content":"regular user message with author",
        "internal_chat_message_metadata_passthrough":{"turn_id":"turn_2"}
    },{
        "type":"message",
        "role":"assistant",
        "content":"clean assistant message"
    }]}"#;
    let cfg = enabled_cfg();

    // Compat mode strips from all items even with a non-Codex user agent.
    let got = rewrite_input(&headers_with_ua("curl/8.7.1"), payload, Some(&cfg), true);
    assert_eq!(gstr(&got, "input.0.type"), "message");
    assert_eq!(gstr(&got, "input.0.role"), "user");
    for i in 0..2 {
        for field in [
            "author",
            "recipient",
            "internal_chat_message_metadata_passthrough",
        ] {
            assert!(
                !gexists(&got, &format!("input.{i}.{field}")),
                "input.{i}.{field}"
            );
        }
    }
    assert_eq!(gstr(&got, "input.1.type"), "message");
    assert_eq!(gstr(&got, "input.1.role"), "user");
    assert_eq!(gstr(&got, "input.2.content"), "clean assistant message");

    // Non-compat mode keeps the metadata on every item.
    let got = rewrite_input(
        &headers_with_ua("Codex Desktop/0.146.0-alpha.3"),
        payload,
        Some(&cfg),
        false,
    );
    assert_eq!(gstr(&got, "input.0.author"), "/root");
    assert_eq!(gstr(&got, "input.0.recipient"), "/root/worker");
    assert_eq!(
        gstr(
            &got,
            "input.0.internal_chat_message_metadata_passthrough.turn_id"
        ),
        "turn_1"
    );
    assert_eq!(gstr(&got, "input.1.author"), "/root/worker");
    assert_eq!(gstr(&got, "input.1.recipient"), "/root");
}

#[test]
fn rewrite_input_compat_mode_without_optimize_issue_6233() {
    let payload = br#"{"model":"gpt-6-luna","input":[{
        "type":"agent_message",
        "id":"amsg_probe",
        "author":"/root/worker",
        "recipient":"/root",
        "content":[
            {"type":"input_text","text":"Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\ndone"},
            {"type":"encrypted_content","encrypted_content":"test task payload"}
        ],
        "internal_chat_message_metadata_passthrough":{"turn_id":"turn_probe"}
    }]}"#;
    let cfg = Config::default();
    let got = rewrite_input(
        &headers_with_ua("Codex Desktop/0.158.0-alpha.2.1"),
        payload,
        Some(&cfg),
        true,
    );
    assert_eq!(gstr(&got, "input.0.type"), "message");
    assert_eq!(gstr(&got, "input.0.role"), "user");
    assert_eq!(gstr(&got, "input.0.content.1.type"), "input_text");
    assert_eq!(gstr(&got, "input.0.content.1.text"), "test task payload");
    for field in [
        "author",
        "recipient",
        "internal_chat_message_metadata_passthrough",
    ] {
        assert!(!gexists(&got, &format!("input.0.{field}")), "{field}");
    }
}

#[test]
fn rewrite_input_conditions() {
    let payload = br#"{"input":[{"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"task"}]}]}"#;
    let enabled = enabled_cfg();
    let disabled = Config::default();
    let cases: [(&str, Option<&Config>, &str, bool); 5] = [
        (
            "Codex Desktop enabled",
            Some(&enabled),
            "Codex Desktop/0.146.0-alpha.3",
            true,
        ),
        (
            "codex tui enabled",
            Some(&enabled),
            "codex-tui/0.154.0",
            true,
        ),
        (
            "optimization disabled",
            Some(&disabled),
            "codex-tui/0.154.0",
            false,
        ),
        ("unrelated client", Some(&enabled), "curl/8.7.1", false),
        ("nil config", None, "Codex Desktop/0.146.0-alpha.3", false),
    ];
    for (name, cfg, ua, want) in cases {
        let got = rewrite_input(&headers_with_ua(ua), payload, cfg, false);
        assert_eq!(gstr(&got, "input.0.type") == "message", want, "{name}");
    }
}

#[test]
fn rewrite_spawn_agent_description_disabled_leaves_payload_unchanged() {
    let payload = br#"{"tools":[{"type":"function","name":"spawn_agent","description":"unchanged","parameters":{"properties":{"message":{"encrypted":true}}}}]}"#;
    let cfg = Config::default();
    let got = rewrite_spawn_agent_description(
        &RequestCtx::default(),
        &headers_with_ua("codex-tui/0.154.0"),
        payload,
        Some(&cfg),
    );
    assert_eq!(got, payload);
}

#[test]
fn rewrite_spawn_agent_description_ignores_other_user_agent() {
    let payload =
        br#"{"tools":[{"type":"function","name":"spawn_agent","description":"unchanged"}]}"#;
    let cfg = enabled_cfg();
    let got = rewrite_spawn_agent_description(
        &RequestCtx::default(),
        &headers_with_ua("curl/8.7.1"),
        payload,
        Some(&cfg),
    );
    assert_eq!(got, payload);
}

#[test]
fn replace_spawn_agent_models_normalizes_sections_and_preserves_instructions() {
    let description = format!(
        "{SPAWN_AGENT_MODELS_HEADING}\n- `old-model`: old\nKeep this multi-agent instruction.\nSpawns an agent.\n{SPAWN_AGENT_MODELS_HEADING}"
    );
    let got = replace_spawn_agent_models(&description, "- `new-model`: New model.");
    assert!(!got.contains("old-model"), "{got:?}");
    assert_eq!(
        got.matches(SPAWN_AGENT_MODELS_HEADING).count(),
        1,
        "{got:?}"
    );
    assert!(
        got.contains("Keep this multi-agent instruction."),
        "{got:?}"
    );
}

#[test]
fn collaboration_message_tool_paths_finds_all_three_tools() {
    let payload = br#"{
        "tools":[
            {"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"properties":{"message":{"encrypted":true}}}},
                {"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}},
                {"type":"function","name":"followup_task","parameters":{"properties":{"message":{"encrypted":true}}}},
                {"type":"function","name":"unrelated_tool","parameters":{"properties":{"message":{"encrypted":true}}}}
            ]}
        ]
    }"#;
    assert_eq!(collaboration_message_tool_paths(&parsed(payload)).len(), 3);
}

#[test]
fn collaboration_message_tool_paths_additional_tools() {
    let payload = br#"{
        "input":[
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true}}}},
                    {"type":"function","name":"followup_task","parameters":{"properties":{"message":{"encrypted":true}}}}
                ]}
            ]}
        ]
    }"#;
    assert_eq!(collaboration_message_tool_paths(&parsed(payload)).len(), 2);
}

#[test]
fn remove_encryption_all_tools() {
    let payload = br#"{
        "tools":[
            {"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
            ]}
        ]
    }"#;
    let mut root = parsed(payload);
    let paths = collaboration_message_tool_paths(&root);
    assert!(remove_collaboration_message_encryption(&mut root, &paths));
    for tool_path in ["tools.0.tools.0", "tools.0.tools.1", "tools.0.tools.2"] {
        assert!(
            !root
                .g(&format!(
                    "{tool_path}.parameters.properties.message.encrypted"
                ))
                .exists()
        );
        assert_eq!(
            root.g(&format!("{tool_path}.parameters.properties.message.type"))
                .str(),
            "string"
        );
    }
}

#[test]
fn remove_encryption_preserves_unrelated_encrypted_fields() {
    let payload = br#"{
        "tools":[
            {"type":"function","name":"send_message","parameters":{"properties":{"message":{"type":"string","encrypted":true},"data":{"encrypted":"keep-me"}}}},
            {"type":"function","name":"unrelated_tool","parameters":{"properties":{"message":{"encrypted":true}}}}
        ]
    }"#;
    let mut root = parsed(payload);
    let paths = collaboration_message_tool_paths(&root);
    remove_collaboration_message_encryption(&mut root, &paths);
    assert!(
        !root
            .g("tools.0.parameters.properties.message.encrypted")
            .exists()
    );
    assert_eq!(
        root.g("tools.0.parameters.properties.data.encrypted").str(),
        "keep-me"
    );
    assert!(
        root.g("tools.1.parameters.properties.message.encrypted")
            .exists()
    );
}

#[test]
fn remove_encryption_no_op_without_encrypted() {
    let payload = br#"{
        "tools":[
            {"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string"}}}}
        ]
    }"#;
    let mut root = parsed(payload);
    let paths = collaboration_message_tool_paths(&root);
    assert!(!remove_collaboration_message_encryption(&mut root, &paths));
}

#[test]
fn optimize_removes_encryption_without_spawn_agent() {
    let payload = r#"{
        "tools":[
            {"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
            ]}
        ]
    }"#;
    let cfg = enabled_cfg();
    let (got, optimized) = optimize(
        &headers_with_ua("Codex Desktop/0.146.0-alpha.3"),
        payload,
        Some(&cfg),
    );
    assert!(!optimized);
    for path in ["tools.0.tools.0", "tools.0.tools.1"] {
        assert!(
            !gexists(
                &got,
                &format!("{path}.parameters.properties.message.encrypted")
            ),
            "{path}"
        );
    }
}

#[test]
fn optimize_removes_encryption_in_additional_tools() {
    let payload = r#"{
        "input":[
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                    {"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
                ]}
            ]}
        ]
    }"#;
    let cfg = enabled_cfg();
    let (got, _) = optimize(&headers_with_ua("codex-tui/0.154.0"), payload, Some(&cfg));
    for path in ["input.0.tools.0.tools.0", "input.0.tools.0.tools.1"] {
        assert!(
            !gexists(
                &got,
                &format!("{path}.parameters.properties.message.encrypted")
            ),
            "{path}"
        );
    }
}

#[test]
fn optimize_removes_encryption_from_all_three_tools_with_spawn_agent() {
    let payload = r#"{
        "tools":[
            {"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"send_message","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                {"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}}
            ]}
        ]
    }"#;
    let cfg = enabled_cfg();
    let (got, optimized) = optimize(
        &headers_with_ua("Codex Desktop/0.146.0-alpha.3"),
        payload,
        Some(&cfg),
    );
    assert!(optimized);
    for path in ["tools.0.tools.0", "tools.0.tools.1", "tools.0.tools.2"] {
        assert!(
            !gexists(
                &got,
                &format!("{path}.parameters.properties.message.encrypted")
            ),
            "{path}"
        );
    }
    assert_eq!(
        gstr(&got, "tools.0.name"),
        OPTIMIZED_COLLABORATION_NAMESPACE
    );
}

#[test]
fn spawn_agent_models_cache_invalidation() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let registry = global_registry();
    let (client1, client2) = ("cache-invalidation-client-1", "cache-invalidation-client-2");
    let formatted = || {
        spawn_agent_models_and_markdown_for_request(&HeaderMap::new(), false)
            .markdown
            .clone()
    };
    let alpha = |levels: &[&str]| ModelInfo {
        id: "test-spawn-model-alpha".into(),
        display_name: "Test Spawn Model Alpha".into(),
        description: "Initial description.".into(),
        thinking: Some(ThinkingSupport {
            levels: levels.iter().map(|l| l.to_string()).collect(),
            ..Default::default()
        }),
        ..Default::default()
    };

    registry.register_client(client1, "openai", &[alpha(&["low", "medium"])]);
    let first = formatted();
    assert!(first.contains("test-spawn-model-alpha"), "{first}");
    assert!(first.contains("Reasoning efforts: low, medium"), "{first}");
    assert_eq!(formatted(), first);

    registry.register_client(
        client2,
        "openai",
        &[ModelInfo {
            id: "test-spawn-model-beta".into(),
            display_name: "Test Spawn Model Beta".into(),
            description: "Second model.".into(),
            ..Default::default()
        }],
    );
    assert!(formatted().contains("test-spawn-model-beta"));

    registry.register_client(
        client1,
        "openai",
        &[alpha(&["low", "medium", "high", "max"])],
    );
    assert!(formatted().contains("low, medium (default), high, max"));

    registry.unregister_client(client2);
    assert!(!formatted().contains("test-spawn-model-beta"));
    registry.unregister_client(client1);
}

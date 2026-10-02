//! Registry behavior tests on isolated registries. The semantics (availability clause, epoch and
//! generation guards, provider tallies) were cross-checked against the Go registry with randomized
//! register/suspend/quota/projection scenarios.

use std::sync::{Arc, Mutex};

use super::*;

fn model(id: &str, created: i64) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        object: "model".into(),
        created,
        owned_by: "o".into(),
        ..Default::default()
    }
}

fn ids(registry: &ModelRegistry, handler: &str) -> Vec<String> {
    registry
        .get_available_models(handler)
        .iter()
        .map(|m| {
            m.get("id")
                .or_else(|| m.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[test]
fn counts_and_provider_ordering_follow_registrations() {
    let r = ModelRegistry::new();
    r.register_client(
        "a",
        "Gemini",
        &[model("m", 1), model("m", 1), model("only-a", 2)],
    );
    r.register_client("b", "claude", &[model("m", 1)]);
    r.register_client("c", "claude", &[model("m", 1)]);

    // Duplicate ids within one client count twice (Go parity).
    assert_eq!(r.get_model_count("m"), 4);
    assert_eq!(
        r.get_model_providers("m"),
        ["claude", "gemini"].map(String::from),
        "ordered by count desc"
    );
    assert_eq!(ids(&r, "openai"), ["m", "only-a"]);

    r.unregister_client("b");
    assert_eq!(r.get_model_count("m"), 3);
    r.unregister_client("a");
    assert_eq!(r.get_model_providers("m"), ["claude"]);
    assert_eq!(r.get_model_count("only-a"), 0);
    assert_eq!(ids(&r, "openai"), ["m"]);
}

#[test]
fn quota_and_cooldown_keep_models_listed_but_hard_suspension_hides_them() {
    let r = ModelRegistry::new();
    r.register_client("a", "gemini", &[model("m", 1)]);

    r.set_model_quota_exceeded("a", "m");
    assert_eq!(
        r.get_model_count("m"),
        0,
        "the only client is inside its quota window"
    );
    assert_eq!(ids(&r, "openai"), ["m"], "still listed: only quota trouble");
    assert!(
        r.get_first_available_model("openai").is_err(),
        "but nothing can serve it"
    );

    r.clear_model_quota_exceeded("a", "m");
    r.suspend_client_model("a", "m", "Quota");
    assert_eq!(
        ids(&r, "openai"),
        ["m"],
        "quota-reason suspension keeps it listed"
    );

    r.resume_client_model("a", "m");
    r.suspend_client_model("a", "m", "auth");
    assert!(
        ids(&r, "openai").is_empty(),
        "hard suspension hides the model"
    );
    assert!(r.is_model_suspended_for_client("a", "m"));

    // Re-registering the binding resets transient scheduling state.
    r.register_client("a", "gemini", &[model("m", 1)]);
    assert!(!r.is_model_suspended_for_client("a", "m"));
    assert_eq!(ids(&r, "openai"), ["m"]);
}

#[test]
fn projections_are_guarded_by_epoch_and_generation() {
    let r = ModelRegistry::new();
    r.register_client("a", "gemini", &[model("m", 1)]);
    let epoch = r.client_registration_epoch("a");
    let suspend = |reason: &str| {
        [ClientModelProjection {
            model_id: "m".into(),
            suspended: true,
            suspend_reason: reason.into(),
            quota_exceeded: false,
        }]
    };

    assert!(
        !r.apply_client_model_projections("a", epoch + 1, 0, &suspend("x")),
        "wrong epoch"
    );
    assert!(
        !r.apply_client_model_projections("ghost", 0, 0, &suspend("x")),
        "unknown client"
    );
    let unowned = [ClientModelProjection {
        model_id: "other".into(),
        suspended: true,
        ..Default::default()
    }];
    assert!(
        !r.apply_client_model_projections("a", epoch, 0, &unowned),
        "no owned model"
    );

    assert!(r.apply_client_model_projections("a", epoch, 5, &suspend("x")));
    assert!(r.is_model_suspended_for_client("a", "m"));
    assert!(
        !r.apply_client_model_projections("a", epoch, 4, &suspend("y")),
        "stale generation"
    );
    let resume = [ClientModelProjection {
        model_id: "m".into(),
        ..Default::default()
    }];
    assert!(r.apply_client_model_projections("a", epoch, 5, &resume));
    assert!(!r.is_model_suspended_for_client("a", "m"));

    // Re-registration bumps the epoch and resets the generation.
    r.register_client("a", "gemini", &[model("m", 1)]);
    assert!(!r.apply_client_model_projections("a", epoch, 9, &suspend("x")));
    assert!(r.apply_client_model_projections(
        "a",
        r.client_registration_epoch("a"),
        0,
        &suspend("x")
    ));
}

#[test]
fn provider_change_moves_tallies_and_variants() {
    let r = ModelRegistry::new();
    let mut gemini_variant = model("m", 1);
    gemini_variant.display_name = "gemini flavour".into();
    let mut claude_variant = model("m", 1);
    claude_variant.display_name = "claude flavour".into();

    r.register_client("a", "gemini", &[gemini_variant]);
    assert_eq!(
        r.get_model_info("m", "gemini").unwrap().display_name,
        "gemini flavour"
    );

    r.register_client("a", "claude", &[claude_variant]);
    assert_eq!(r.get_model_providers("m"), ["claude"]);
    assert_eq!(
        r.get_model_info("m", "claude").unwrap().display_name,
        "claude flavour"
    );
    // The old provider has no bindings left: fall back to the global (last registered) info.
    assert_eq!(
        r.get_model_info("m", "gemini").unwrap().display_name,
        "claude flavour"
    );
    assert_eq!(r.get_available_models_by_provider("gemini").len(), 0);
    assert_eq!(r.get_available_models_by_provider("Claude").len(), 1);
}

#[test]
fn empty_registration_unregisters_the_client() {
    let r = ModelRegistry::new();
    r.register_client("a", "gemini", &[model("m", 1)]);
    let epoch = r.client_registration_epoch("a");
    r.register_client("a", "gemini", &[]);
    assert!(ids(&r, "openai").is_empty());
    assert!(r.get_models_for_client("a").is_empty());
    assert!(r.client_registration_epoch("a") > epoch);
    assert!(!r.client_supports_model("a", "m"));
}

#[test]
fn web_search_support_follows_clients() {
    let r = ModelRegistry::new();
    let mut with_search = model("m", 1);
    with_search.supports_web_search = true;
    r.register_client("a", "antigravity", &[with_search]);
    r.register_client("b", "antigravity", &[model("m", 1)]);
    assert!(
        r.get_model_info("m", "").unwrap().supports_web_search,
        "any client supporting it counts"
    );

    r.unregister_client("a");
    assert!(!r.get_model_info("m", "").unwrap().supports_web_search);

    let epoch = r.client_registration_epoch("b");
    assert!(
        r.apply_client_model_capabilities("b", epoch, |_, info| info.supports_web_search = true)
    );
    assert!(r.get_model_info("m", "").unwrap().supports_web_search);
    assert!(
        !r.apply_client_model_capabilities("b", epoch + 1, |_, _| {}),
        "stale epoch"
    );
}

#[test]
fn web_search_capability_resolution_is_conservative() {
    let route = |provider: &str, web_search: Option<bool>| NativeCapabilityRoute {
        provider: provider.into(),
        native_capabilities: Some(NativeCapabilities { web_search }),
    };
    assert_eq!(resolve_responses_web_search_capability(&[]), None);
    assert_eq!(
        resolve_responses_web_search_capability(&[route("codex", Some(true))]),
        Some(true)
    );
    assert_eq!(
        resolve_responses_web_search_capability(&[route("codex", None)]),
        None,
        "no catalog data: unknown"
    );
    assert_eq!(
        resolve_responses_web_search_capability(&[
            route("codex", Some(true)),
            route("gemini", Some(true))
        ]),
        Some(false)
    );
    assert_eq!(
        resolve_responses_web_search_capability(&[route("codex", Some(false))]),
        Some(false)
    );
    assert_eq!(
        resolve_responses_web_search_capability(&[route("mystery", Some(true))]),
        None
    );
    assert_eq!(
        resolve_responses_web_search_capability(&[route("openai-compatible-x", None)]),
        Some(false)
    );
}

#[test]
fn list_shapes_per_handler_type() {
    let r = ModelRegistry::new();
    let mut m = model("claude-x", 1_759_276_800);
    m.display_name = "Claude X".into();
    m.context_length = 123;
    m.input_token_limit = 7;
    m.name = "models/claude-x".into();
    r.register_client("a", "claude", &[m, model("bare", 0)]);

    let claude = r.get_available_models("claude");
    assert_eq!(
        serde_json::to_string(&claude[1]).unwrap(),
        r#"{"created_at":"2025-10-01T00:00:00Z","display_name":"Claude X","id":"claude-x","max_input_tokens":123,"max_tokens":64000,"object":"model","owned_by":"o","type":"model"}"#
    );
    assert_eq!(
        serde_json::to_string(&claude[0]).unwrap(),
        r#"{"display_name":"bare","id":"bare","max_input_tokens":200000,"max_tokens":64000,"object":"model","owned_by":"o","type":"model"}"#
    );
    let gemini = r.get_available_models("gemini");
    assert_eq!(
        serde_json::to_string(&gemini[1]).unwrap(),
        r#"{"displayName":"Claude X","inputTokenLimit":7,"name":"models/claude-x"}"#
    );
    let generic = r.get_available_models("");
    assert_eq!(
        serde_json::to_string(&generic[0]).unwrap(),
        r#"{"id":"bare","object":"model","owned_by":"o"}"#
    );
    // Newest `created` wins for `auto`.
    assert_eq!(r.get_first_available_model("openai").unwrap(), "claude-x");
}

#[test]
fn listing_cache_is_invalidated_by_changes() {
    let r = ModelRegistry::new();
    r.register_client("a", "gemini", &[model("m1", 1)]);
    assert_eq!(ids(&r, "openai"), ["m1"]);
    let generation = r.get_generation();
    r.register_client("b", "gemini", &[model("m2", 2)]);
    assert!(r.get_generation() > generation);
    assert_eq!(ids(&r, "openai"), ["m1", "m2"]);
}

#[test]
fn hooks_observe_registration_changes_on_a_separate_thread() {
    struct Recorder(Mutex<Vec<String>>);
    impl ModelRegistryHook for Recorder {
        fn on_models_registered(&self, provider: &str, client_id: &str, models: Vec<ModelInfo>) {
            self.0
                .lock()
                .unwrap()
                .push(format!("reg {provider} {client_id} {}", models.len()));
        }
        fn on_models_unregistered(&self, provider: &str, client_id: &str) {
            self.0
                .lock()
                .unwrap()
                .push(format!("unreg {provider} {client_id}"));
        }
    }
    let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
    let r = ModelRegistry::new();
    r.set_hook(Some(recorder.clone()));
    r.register_client(
        "a",
        "Gemini",
        &[model("m", 1), model("m", 1), model("n", 1)],
    );
    // Wait for the registration notification before unregistering to keep the order deterministic.
    for _ in 0..200 {
        if !recorder.0.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    r.unregister_client("a");
    for _ in 0..200 {
        if recorder.0.lock().unwrap().len() >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        *recorder.0.lock().unwrap(),
        ["reg gemini a 2", "unreg gemini a"]
    );
}

#[test]
fn lookup_prefers_registry_then_static_definitions() {
    let id = "zz-unit-test-model";
    assert!(lookup_model_info(id, None).is_none());
    assert!(lookup_model_info("  ", None).is_none());
    let mut info = model(id, 1);
    info.display_name = "dynamic".into();
    global_registry().register_client("lookup-test-client", "ProviderX", &[info]);
    assert_eq!(
        lookup_model_info(id, Some(" providerx "))
            .unwrap()
            .display_name,
        "dynamic"
    );
    global_registry().unregister_client("lookup-test-client");
    assert!(lookup_model_info(id, None).is_none());
    let static_id = crate::registry::get_claude_models()
        .first()
        .expect("claude catalog")
        .id
        .clone();
    assert!(lookup_model_info(&static_id, None).is_some());
}

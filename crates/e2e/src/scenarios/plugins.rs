//! Dynamic plugin scenarios. The Go example plugins (built by `crates/e2e/plugins/build.sh`)
//! are loaded by the server under test; each hook type is exercised through the observable
//! behavior of the example that implements it. Scenarios are skipped when the plugin
//! libraries are not built.

use serde_json::{Value, json};

use super::bodies::{self, Family, Kind};
use crate::client::{Auth, HttpReq, Step as Req};
use crate::config::{ConfigSpec, PluginSpec};
use crate::mock::script::{Content, Script};
use crate::scenario::Scenario;

const V0: &str = "/v0/management";
const V8: &str = "/v8/management";
/// Lets the asynchronous config reload triggered by a management write finish.
const SETTLE_MS: u64 = 700;
/// Plugin models and executors are registered shortly after the listener is up.
const REGISTER_MS: u64 = 1500;

fn mgmt(req: HttpReq) -> Req {
    Req::Http(req.auth(Auth::Mgmt))
}

fn get(path: &str) -> Req {
    mgmt(HttpReq::get(path))
}

fn script() -> Script {
    Script::ok(Content::Text)
}

fn chat(family: Family) -> HttpReq {
    HttpReq::post("/v1/chat/completions", bodies::chat(family.model(), false, Kind::Text))
}

fn chat_stream(family: Family) -> HttpReq {
    HttpReq::post("/v1/chat/completions", bodies::chat(family.model(), true, Kind::Text))
}

fn claude_messages() -> HttpReq {
    HttpReq::post("/v1/messages", bodies::claude(Family::Claude.model(), false, Kind::Text))
}

macro_rules! plugin_profile {
    ($name:ident, $id:literal) => {
        fn $name(s: &mut ConfigSpec) {
            s.plugins.push(PluginSpec::new($id));
        }
    };
}

plugin_profile!(simple, "simple");
plugin_profile!(model, "model");
plugin_profile!(auth_plugin, "auth");
plugin_profile!(frontend_auth, "frontend-auth");
plugin_profile!(frontend_auth_exclusive, "frontend-auth-exclusive");
plugin_profile!(request_normalizer, "request-normalizer");
plugin_profile!(request_translator, "request-translator");
plugin_profile!(response_normalizer, "response-normalizer");
plugin_profile!(response_translator, "response-translator");
plugin_profile!(thinking, "thinking");
plugin_profile!(management_api, "management-api");
plugin_profile!(host_callback, "host-callback");
plugin_profile!(host_model_callback, "host-model-callback");
plugin_profile!(host_auth_files, "host-callback-auth-files");

fn request_lifecycle(s: &mut ConfigSpec) {
    s.plugins.push(PluginSpec::new("request-lifecycle").priority(100).setting("max_concurrency", json!(2)).setting("reject_keyword", json!("blocked")));
}

fn simple_with_auth(s: &mut ConfigSpec) {
    simple(s);
    s.auth_files.push(("plugin-example.json", r#"{"type":"plugin-example","email":"plugin@example.test"}"#));
}

fn auth_with_file(s: &mut ConfigSpec) {
    auth_plugin(s);
    s.auth_files.push(("example-auth.json", r#"{"type":"example-auth-go","token":"t","email":"auth@example.test"}"#));
}

fn exclusive_ready(s: &mut ConfigSpec) {
    frontend_auth_exclusive(s);
    s.ready_headers.push(("X-Example-Frontend-Auth", "exclusive"));
}

fn codex_service_tier(s: &mut ConfigSpec) {
    s.plugins.push(PluginSpec::new("codex-service-tier").priority(1).setting("fast", json!(true)));
}

fn scheduler_pick(s: &mut ConfigSpec) {
    s.plugins.push(PluginSpec::new("scheduler").priority(1).setting("auth_id", json!("")).setting("delegate", json!("fill-first")));
}

fn scheduler_round_robin(s: &mut ConfigSpec) {
    s.plugins.push(PluginSpec::new("scheduler").priority(1).setting("delegate", json!("round-robin")));
}

fn scheduler_deny(s: &mut ConfigSpec) {
    s.plugins.push(PluginSpec::new("scheduler").priority(1).setting("deny", json!(true)));
}

fn usage_with_statistics(s: &mut ConfigSpec) {
    s.usage_statistics = true;
    s.plugins.push(PluginSpec::new("usage"));
}

fn two_plugins(s: &mut ConfigSpec) {
    s.plugins.push(PluginSpec::new("request-normalizer").priority(1));
    s.plugins.push(PluginSpec::new("response-normalizer").priority(2));
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    management(&mut out);
    resources(&mut out);
    models(&mut out);
    auth_files(&mut out);
    access(&mut out);
    translation(&mut out);
    execution(&mut out);
    lifecycle(&mut out);
    out
}

fn management(out: &mut Vec<Scenario>) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("plugins.mgmt.{id}"), desc, script(), steps);
    out.push(
        s(
            "list_simple",
            "GET /plugins with the all-capability example loaded",
            vec![get(&format!("{V0}/plugins")), get(&format!("{V8}/plugins")), get(&format!("{V0}/plugins/simple/config")), get(&format!("{V0}/plugins/nope/config"))],
        )
        .profile(simple),
    );
    out.push(
        s(
            "list_many",
            "GET /plugins with several capability plugins",
            vec![get(&format!("{V0}/plugins")), get(&format!("{V0}/quota/providers"))],
        )
        .profile(|s| {
            for id in ["model", "executor", "management-api", "frontend-auth", "auth", "usage"] {
                s.plugins.push(PluginSpec::new(id));
            }
        }),
    );
    out.push(
        s(
            "config_edit",
            "plugin config GET, PUT, PATCH and enabled toggle",
            vec![
                get(&format!("{V0}/plugins/model/config")),
                mgmt(HttpReq::put(&format!("{V0}/plugins/model/config"), json!({"enabled": true, "priority": 3, "color": "blue"}))),
                Req::Pause(SETTLE_MS),
                get(&format!("{V0}/plugins/model/config")),
                mgmt(HttpReq::patch(&format!("{V0}/plugins/model/config"), json!({"color": null, "size": 2}))),
                Req::Pause(SETTLE_MS),
                get(&format!("{V0}/plugins/model/config")),
                mgmt(HttpReq::patch(&format!("{V0}/plugins/model/enabled"), json!({"enabled": false}))),
                Req::Pause(SETTLE_MS),
                get(&format!("{V0}/plugins")),
                mgmt(HttpReq::patch(&format!("{V0}/plugins/model/enabled"), json!({}))),
                mgmt(HttpReq::put(&format!("{V0}/plugins/bad id/config"), json!({}))),
            ],
        )
        .profile(model),
    );
    out.push(
        s(
            "delete",
            "DELETE removes the plugin file and its config",
            vec![
                mgmt(HttpReq::delete(&format!("{V0}/plugins/model"))),
                Req::Pause(SETTLE_MS),
                get(&format!("{V0}/plugins")),
                mgmt(HttpReq::delete(&format!("{V0}/plugins/model"))),
                mgmt(HttpReq::delete(&format!("{V8}/plugins/unknown-plugin"))),
            ],
        )
        .profile(model),
    );
    out.push(
        s(
            "quota_without_provider",
            "plugin quota endpoints without a quota provider",
            vec![
                get(&format!("{V0}/plugins/simple/quota?auth_index=missing")),
                get(&format!("{V0}/plugins/simple/quota")),
                mgmt(HttpReq::post(&format!("{V0}/quota/fetch"), json!({"auth_index": "missing"}))),
                mgmt(HttpReq::post(&format!("{V0}/quota/reset"), json!({}))),
                mgmt(HttpReq::post(&format!("{V0}/quota/reset"), json!({"auth_index": "missing"}))),
            ],
        )
        .profile(simple),
    );
    out.push(
        s(
            "auth_url_plugin_provider",
            "login URL of a plugin auth provider (v0 and v8 routes) and the pending session",
            vec![
                get(&format!("{V0}/auth-auth-url")),
                get(&format!("{V0}/example-auth-go-auth-url")),
                get(&format!("{V8}/oauth/auth-url?provider=auth")),
                get(&format!("{V8}/oauth/auth-url?provider=nope")),
                get(&format!("{V8}/oauth/status?state=example-state")),
            ],
        )
        .profile(auth_plugin),
    );
}

fn resources(out: &mut Vec<Scenario>) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("plugins.resource.{id}"), desc, script(), steps);
    out.push(
        s(
            "management_api",
            "plugin resource served without authentication, and unknown resources",
            vec![
                Req::Http(HttpReq::get("/v0/resource/plugins/management-api/status").auth(Auth::None)),
                Req::Http(HttpReq::post("/v0/resource/plugins/management-api/status", json!({})).auth(Auth::None)),
                Req::Http(HttpReq::get("/v0/resource/plugins/management-api/other").auth(Auth::None)),
                Req::Http(HttpReq::get("/v0/resource/plugins/unknown/status").auth(Auth::None)),
                get(&format!("{V0}/plugins")),
            ],
        )
        .profile(management_api),
    );
    out.push(
        s(
            "host_callback",
            "plugin resource that calls the host logger",
            vec![Req::Http(HttpReq::get("/v0/resource/plugins/host-callback/status").auth(Auth::None))],
        )
        .profile(host_callback),
    );
    out.push(
        s(
            "host_auth_files",
            "plugin resource over the host auth file callbacks",
            vec![
                Req::Http(HttpReq::get("/v0/resource/plugins/host-callback-auth-files/status").auth(Auth::None)),
                Req::Http(HttpReq::get("/v0/resource/plugins/host-callback-auth-files/status?mode=list").auth(Auth::None)),
            ],
        )
        .profile(host_auth_files),
    );
    out.push(
        s(
            "host_model_callback",
            "plugin resource that runs a model request through the host",
            vec![Req::Http(HttpReq::get("/v0/resource/plugins/host-model-callback/status").auth(Auth::None))],
        )
        .profile(host_model_callback),
    );
}

fn models(out: &mut Vec<Scenario>) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("plugins.models.{id}"), desc, script(), steps);
    out.push(
        s(
            "static_models",
            "models registered by a model plugin show up in the listings",
            vec![Req::Pause(REGISTER_MS), Req::Http(HttpReq::get("/v1/models")), Req::Http(HttpReq::get("/v1beta/models"))],
        )
        .profile(model),
    );
    out.push(
        s(
            "executor_models",
            "an executor plugin registers its provider models",
            vec![Req::Pause(REGISTER_MS), Req::Http(HttpReq::get("/v1/models"))],
        )
        .profile(simple),
    );
    out.push(
        s(
            "auth_models",
            "a plugin credential gets the models of its provider; the credential is listed",
            vec![Req::Pause(REGISTER_MS), Req::Http(HttpReq::get("/v1/models")), get(&format!("{V8}/credentials")), get(&format!("{V8}/credentials/models?name=plugin-example.json"))],
        )
        .profile(simple_with_auth),
    );
}

fn auth_files(out: &mut Vec<Scenario>) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("plugins.auth.{id}"), desc, script(), steps);
    out.push(
        s(
            "parse_file",
            "a credential file owned by a plugin auth provider is parsed by the plugin",
            vec![
                Req::Pause(REGISTER_MS),
                get(&format!("{V8}/credentials")),
                get(&format!("{V0}/auth-files")),
                get(&format!("{V8}/credentials/download?name=example-auth-go.json")),
            ],
        )
        .profile(auth_with_file),
    );
}

fn access(out: &mut Vec<Scenario>) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("plugins.access.{id}"), desc, script(), steps);
    out.push(
        s(
            "frontend_auth",
            "a frontend auth provider joins the client key check",
            vec![
                Req::Http(chat(Family::Compat).auth(Auth::Bearer("not-a-configured-key"))),
                Req::Http(chat(Family::Compat).auth(Auth::None)),
                Req::Http(chat(Family::Compat)),
                Req::Http(HttpReq::get("/v1/models").auth(Auth::Bearer("anything"))),
            ],
        )
        .profile(frontend_auth),
    );
    out.push(
        s(
            "frontend_auth_exclusive",
            "an exclusive frontend auth provider replaces the other providers",
            vec![
                Req::Http(chat(Family::Compat).auth(Auth::Bearer("not-a-configured-key"))),
                Req::Http(chat(Family::Compat)),
                Req::Http(chat(Family::Compat).auth(Auth::None).header("X-Example-Frontend-Auth", "exclusive")),
                Req::Http(HttpReq::get("/v1/models").auth(Auth::None).header("X-Example-Frontend-Auth", "exclusive")),
            ],
        )
        .profile(exclusive_ready),
    );
}

fn translation(out: &mut Vec<Scenario>) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("plugins.translate.{id}"), desc, script(), steps);
    for (name, profile) in [
        ("request_normalizer", request_normalizer as fn(&mut ConfigSpec)),
        ("request_translator", request_translator),
        ("response_normalizer", response_normalizer),
        ("response_translator", response_translator),
        ("thinking", thinking),
    ] {
        out.push(
            s(
                &format!("{name}_chat"),
                &format!("{name} plugin on a chat request routed to the compat upstream"),
                vec![Req::Http(chat(Family::Compat))],
            )
            .profile(profile),
        );
        out.push(
            s(
                &format!("{name}_claude"),
                &format!("{name} plugin on a Claude request routed to the Claude upstream"),
                vec![Req::Http(claude_messages())],
            )
            .profile(profile),
        );
    }
    out.push(
        s("response_normalizer_stream", "response normalizer on a streaming chat", vec![Req::Http(chat_stream(Family::Compat))]).profile(response_normalizer),
    );
    out.push(
        s("codex_service_tier", "service tier normalizer on a Codex request", vec![Req::Http(chat(Family::Codex))]).profile(codex_service_tier),
    );
    out.push(
        s("two_plugins", "request and response normalizers together", vec![Req::Http(chat(Family::Compat))]).profile(two_plugins),
    );
}

fn execution(out: &mut Vec<Scenario>) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("plugins.exec.{id}"), desc, script(), steps);
    let model_req = |model: &str, stream: bool| HttpReq::post("/v1/chat/completions", bodies::chat(model, stream, Kind::Text));
    out.push(
        s(
            "executor",
            "a plugin executor serves its own model (plain and streaming)",
            vec![
                Req::Pause(REGISTER_MS),
                Req::Http(model_req("plugin-example-model", false)),
                Req::Http(model_req("plugin-example-model", true)),
                Req::Http(model_req("plugin-example/plugin-example-model", false)),
                Req::Http(model_req("no-such-plugin-model", false)),
            ],
        )
        .profile(simple),
    );
    out.push(
        s(
            "executor_with_auth",
            "a plugin executor serving a request with a plugin credential",
            vec![Req::Pause(REGISTER_MS), Req::Http(model_req("plugin-example-model", false)), Req::Http(model_req("plugin-example-model", true))],
        )
        .profile(simple_with_auth),
    );
    out.push(
        s(
            "executor_claude_entry",
            "a Claude-dialect request routed to the plugin executor",
            vec![
                Req::Pause(REGISTER_MS),
                Req::Http(HttpReq::post("/v1/messages", bodies::claude("plugin-example-model", false, Kind::Text))),
                Req::Http(HttpReq::post("/v1/messages/count_tokens", bodies::claude("plugin-example-model", false, Kind::Text))),
            ],
        )
        .profile(simple),
    );
    out.push(
        s(
            "thinking_suffix",
            "a thinking suffix on a plugin model reaches the plugin thinking applier",
            vec![Req::Pause(REGISTER_MS), Req::Http(model_req("plugin-example-model(8192)", false))],
        )
        .profile(simple),
    );
    out.push(
        s(
            "scheduler_delegate",
            "a scheduler plugin delegating to the fill-first selector",
            vec![Req::Http(chat(Family::Claude)), Req::Http(chat(Family::Claude)), Req::Http(chat(Family::Claude))],
        )
        .profile(scheduler_pick),
    );
    out.push(
        s(
            "scheduler_round_robin",
            "a scheduler plugin delegating to the round-robin selector",
            vec![Req::Http(chat(Family::Claude)), Req::Http(chat(Family::Claude)), Req::Http(chat(Family::Claude)), Req::Http(chat(Family::Claude))],
        )
        .profile(scheduler_round_robin),
    );
    out.push(s("scheduler_deny", "a scheduler plugin that rejects every pick", vec![Req::Http(chat(Family::Claude))]).profile(scheduler_deny));
    out.push(s("usage_plugin", "a usage plugin observing a request", vec![Req::Http(chat(Family::Compat)), get(&format!("{V0}/usage-queue"))]).profile(usage_with_statistics));
}

fn lifecycle(out: &mut Vec<Scenario>) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("plugins.lifecycle.{id}"), desc, script(), steps);
    let keyword = |kw: &str| {
        let mut body = bodies::chat(Family::Compat.model(), false, Kind::Text);
        body["messages"][0]["content"] = Value::String(format!("please do {kw} things"));
        HttpReq::post("/v1/chat/completions", body)
    };
    out.push(
        s(
            "reject_keyword",
            "request interceptor terminating a request with a custom response",
            vec![Req::Http(chat(Family::Compat)), Req::Http(keyword("blocked")), Req::Http(chat(Family::Compat))],
        )
        .profile(request_lifecycle),
    );
    // The plugin admits two requests at a time: only the terminal `request.complete` events
    // keep a sequence of requests flowing.
    let mut sequence = vec![];
    for i in 0..5 {
        sequence.push(Req::Http(if i % 2 == 0 { chat(Family::Compat) } else { chat_stream(Family::Compat) }));
        sequence.push(Req::Pause(150));
    }
    out.push(s("slot_release", "completion events release the interceptor's concurrency slots", sequence).profile(request_lifecycle));
    out.push(
        s(
            "reject_keyword_stream",
            "request interceptor terminating a streaming request",
            vec![Req::Http(HttpReq::post("/v1/chat/completions", {
                let mut b = bodies::chat(Family::Compat.model(), true, Kind::Text);
                b["messages"][0]["content"] = Value::String("blocked request".into());
                b
            }))],
        )
        .profile(request_lifecycle),
    );
}

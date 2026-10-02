//! Client authentication, model listings and miscellaneous routes (no upstream involved unless
//! noted).

use serde_json::json;

use super::bodies::{self, Family, Kind};
use super::profiles;
use crate::client::{Auth, HttpReq};
use crate::config::CLIENT_KEY_2;
use crate::mock::script::{Content, Reply, Script, Step};
use crate::scenario::Scenario;

fn ok() -> Script {
    Script::ok(Content::Text)
}

fn chat() -> HttpReq {
    HttpReq::post("/v1/chat/completions", bodies::chat(Family::Compat.model(), false, Kind::Text))
}

fn claude_messages() -> HttpReq {
    HttpReq::post("/v1/messages", bodies::claude(Family::Claude.model(), false, Kind::Text))
}

fn gemini_generate(path_suffix: &str) -> HttpReq {
    HttpReq::post(&format!("{}{path_suffix}", bodies::gemini_path(Family::Gemini.model(), "generateContent")), bodies::gemini(Kind::Text))
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    auth(&mut out);
    models(&mut out);
    routes(&mut out);
    out
}

fn auth(out: &mut Vec<Scenario>) {
    let mut add = |id: &str, desc: &str, reqs: Vec<HttpReq>| {
        out.push(Scenario::new(format!("auth.{id}"), desc, ok(), reqs.into_iter().map(Into::into).collect()));
    };
    add("missing.chat", "no credentials on chat", vec![chat().auth(Auth::None)]);
    add("missing.models", "no credentials on /v1/models", vec![HttpReq::get("/v1/models").auth(Auth::None)]);
    add("missing.claude", "no credentials on /v1/messages", vec![claude_messages().auth(Auth::None)]);
    add("missing.gemini", "no credentials on a Gemini route", vec![gemini_generate("").auth(Auth::None)]);
    add("missing.gemini_models", "no credentials on /v1beta/models", vec![HttpReq::get("/v1beta/models").auth(Auth::None)]);
    add("invalid.bearer", "wrong bearer key", vec![chat().auth(Auth::Bearer("wrong-key"))]);
    add("invalid.x_api_key", "wrong x-api-key", vec![claude_messages().auth(Auth::XApiKey("wrong-key"))]);
    add("invalid.query_key", "wrong ?key=", vec![gemini_generate("?key=wrong-key").auth(Auth::None)]);
    add("invalid.empty_bearer", "empty bearer token", vec![chat().auth(Auth::RawAuthorization("Bearer "))]);
    add("valid.raw_authorization", "key without the Bearer scheme", vec![chat().auth(Auth::RawAuthorization(crate::config::CLIENT_KEY))]);
    add("valid.lowercase_bearer", "lowercase bearer scheme", vec![chat().auth(Auth::RawAuthorization("bearer e2e-client-key"))]);
    add("valid.x_api_key", "x-api-key on /v1/messages", vec![claude_messages().auth(Auth::XApiKey(crate::config::CLIENT_KEY))]);
    add("valid.x_goog_api_key", "x-goog-api-key on a Gemini route", vec![gemini_generate("").auth(Auth::GoogKey(crate::config::CLIENT_KEY))]);
    add("valid.query_key", "?key= on a Gemini route", vec![gemini_generate("?key=e2e-client-key").auth(Auth::None)]);
    add("valid.query_auth_token", "?auth_token= on chat", vec![HttpReq::post("/v1/chat/completions?auth_token=e2e-client-key", bodies::chat(Family::Compat.model(), false, Kind::Text)).auth(Auth::None)]);
    add("valid.second_key", "the second configured client key", vec![chat().auth(Auth::Bearer(CLIENT_KEY_2))]);
    add(
        "precedence.invalid_then_valid",
        "an invalid Authorization header does not mask a valid x-api-key",
        vec![claude_messages().auth(Auth::Bearer("wrong-key")).header("x-api-key", crate::config::CLIENT_KEY)],
    );
    add("options.preflight", "OPTIONS bypasses auth with 204", vec![HttpReq::options("/v1/chat/completions").auth(Auth::None)]);
    out.push(
        Scenario::new("auth.open_access", "no client keys configured: everything is allowed", ok(), vec![chat().auth(Auth::None).into(), HttpReq::get("/v1/models").auth(Auth::None).into()])
            .profile(profiles::open_access),
    );
}

fn models(out: &mut Vec<Scenario>) {
    let mut add = |id: &str, desc: &str, reqs: Vec<HttpReq>| {
        out.push(Scenario::new(format!("models.{id}"), desc, ok(), reqs.into_iter().map(Into::into).collect()));
    };
    add("openai", "GET /v1/models, OpenAI shape", vec![HttpReq::get("/v1/models")]);
    add("claude_by_version_header", "Anthropic-Version header selects the Claude shape", vec![HttpReq::get("/v1/models").header("anthropic-version", "2023-06-01")]);
    add("claude_by_user_agent", "claude-cli User-Agent selects the Claude shape", vec![HttpReq::get("/v1/models").header("user-agent", "claude-cli/2.1.0 (external, cli)")]);
    add("codex_client_catalog", "?client_version= returns the Codex client catalog", vec![HttpReq::get("/v1/models?client_version=0.100.0")]);
    add("gemini_list", "GET /v1beta/models", vec![HttpReq::get("/v1beta/models")]);
    add("gemini_get", "GET /v1beta/models/<id>", vec![HttpReq::get("/v1beta/models/gemini-2.5-flash")]);
    add("gemini_get_missing", "GET /v1beta/models/<unknown>", vec![HttpReq::get("/v1beta/models/no-such-model")]);
    add("trailing_slash", "GET /v1/models/ redirects", vec![HttpReq::get("/v1/models/")]);
    out.push(Scenario::new(
        "models.after_all_keys_unauthorized",
        "models of a provider whose keys all got 401 are suspended and unlisted",
        Script::steps(vec![Step::always(Reply::error(401))]),
        vec![claude_messages().into(), HttpReq::get("/v1/models").into()],
    ));
    out.push(Scenario::new(
        "models.after_all_keys_rate_limited",
        "quota-limited models stay listed",
        Script::steps(vec![Step::always(Reply::error_with(429, &[("retry-after", "30")], None))]),
        vec![claude_messages().into(), HttpReq::get("/v1/models").into()],
    ));
}

fn routes(out: &mut Vec<Scenario>) {
    let mut add = |id: &str, desc: &str, reqs: Vec<HttpReq>| {
        out.push(Scenario::new(format!("misc.{id}"), desc, ok(), reqs.into_iter().map(Into::into).collect()));
    };
    add("root", "GET /", vec![HttpReq::get("/").auth(Auth::None)]);
    add("healthz", "GET /healthz", vec![HttpReq::get("/healthz").auth(Auth::None)]);
    add("not_found", "unknown route with and without credentials", vec![HttpReq::get("/v1/nope"), HttpReq::get("/v1/nope").auth(Auth::None)]);
    add("method_not_allowed", "GET on a POST route", vec![HttpReq::get("/v1/chat/completions"), HttpReq::get("/v1/messages")]);
    add("management_panel", "GET /management.html with the panel disabled", vec![HttpReq::get("/management.html").auth(Auth::None)]);
    add("keep_alive_route", "/keep-alive only exists with a local password", vec![HttpReq::get("/keep-alive").auth(Auth::None)]);
    add(
        "oauth_callbacks",
        "provider redirect landing routes",
        vec![
            HttpReq::get("/anthropic/callback?code=abc&state=nostate").auth(Auth::None),
            HttpReq::get("/codex/callback?error=access_denied").auth(Auth::None),
            HttpReq::get("/callback").auth(Auth::None),
            HttpReq::get("/callback?code=abc&state=nostate").auth(Auth::None),
        ],
    );
    add(
        "bad_bodies.chat",
        "malformed chat bodies",
        vec![
            chat().raw("{not json"),
            chat().raw(""),
            HttpReq::post("/v1/chat/completions", json!({"model": Family::Compat.model()})),
            HttpReq::post("/v1/chat/completions", json!({"messages": [{"role":"user","content":"hi"}]})),
            HttpReq::post("/v1/chat/completions", json!({"model": 7, "messages": []})),
            HttpReq::post("/v1/chat/completions", json!([1, 2, 3])),
        ],
    );
    add(
        "bad_bodies.responses",
        "malformed Responses bodies",
        vec![
            HttpReq::post("/v1/responses", json!({})).raw("{not json"),
            HttpReq::post("/v1/responses", json!({"input": "hi"})),
            HttpReq::post("/v1/responses/compact", json!({"model": Family::Codex.model(), "stream": true, "input": "hi"})),
        ],
    );
    add(
        "bad_bodies.claude",
        "malformed Claude bodies",
        vec![claude_messages().raw("{not json"), HttpReq::post("/v1/messages", json!({"max_tokens": 5})), HttpReq::post("/v1/messages/count_tokens", json!({}))],
    );
    add(
        "bad_bodies.gemini",
        "malformed Gemini requests",
        vec![
            gemini_generate("").raw("{not json"),
            HttpReq::post("/v1beta/models/gemini-2.5-flash", bodies::gemini(Kind::Text)),
            HttpReq::post("/v1beta/models/gemini-2.5-flash:noSuchMethod", bodies::gemini(Kind::Text)),
            HttpReq::post("/v1beta/models/a:b:c", bodies::gemini(Kind::Text)),
        ],
    );
    add(
        "chat_with_responses_body",
        "a Responses-shaped body (input, no messages) on the chat endpoint is converted",
        vec![HttpReq::post("/v1/chat/completions", json!({"model": Family::Compat.model(), "input": "hello", "instructions": "be brief"}))],
    );
    add(
        "stream_flag_variants",
        "what counts as stream:true per dialect",
        vec![
            HttpReq::post("/v1/chat/completions", json!({"model": Family::Compat.model(), "stream": "true", "messages": [{"role":"user","content":"hi"}]})),
            HttpReq::post("/v1/messages", json!({"model": Family::Claude.model(), "max_tokens": 64, "stream": null, "messages": [{"role":"user","content":"hi"}]})),
            HttpReq::post("/v1/messages", json!({"model": Family::Claude.model(), "max_tokens": 64, "stream": 0, "messages": [{"role":"user","content":"hi"}]})),
        ],
    );
    add(
        "gemini_alt_variants",
        "alt query parameter handling for streamGenerateContent",
        vec![
            HttpReq::post(&format!("{}?alt=json", bodies::gemini_path(Family::Gemini.model(), "streamGenerateContent")), bodies::gemini(Kind::Text)),
            HttpReq::post(&format!("{}?%24alt=sse", bodies::gemini_path(Family::Gemini.model(), "streamGenerateContent")), bodies::gemini(Kind::Text)),
        ],
    );
    out.push(
        Scenario::new(
            "misc.safe_mode.example_keys",
            "template client keys put the server in safe mode",
            ok(),
            vec![
                HttpReq::get("/").auth(Auth::None).into(),
                HttpReq::get("/v1/models").auth(Auth::Bearer("your-api-key-1")).into(),
                HttpReq::post("/v1/chat/completions", bodies::chat(Family::Compat.model(), false, Kind::Text)).auth(Auth::Bearer("your-api-key-1")).into(),
                HttpReq::get("/management.html?safe-mode=configure").auth(Auth::None).into(),
                HttpReq::get("/healthz").auth(Auth::None).into(),
            ],
        )
        .profile(profiles::example_keys),
    );
}

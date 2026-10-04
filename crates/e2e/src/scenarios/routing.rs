//! Credential selection, cooldown, retry policy, model routing and header handling scenarios.

use serde_json::json;

use super::bodies::{self, FAMILIES, Family, Kind};
use super::profiles;
use crate::client::{HttpReq, Step as Req};
use crate::mock::script::{Content, Reply, Script, Step};
use crate::scenario::Scenario;

fn chat(model: &str) -> HttpReq {
    HttpReq::post("/v1/chat/completions", bodies::chat(model, false, Kind::Text))
}

fn chat_stream(model: &str) -> HttpReq {
    HttpReq::post("/v1/chat/completions", bodies::chat(model, true, Kind::Text))
}

fn repeat(n: usize, req: HttpReq) -> Vec<Req> {
    (0..n).map(|_| Req::Http(req.clone())).collect()
}

fn ok() -> Script {
    Script::ok(Content::Text)
}

fn fail_once(status: u16) -> Script {
    Script::steps(vec![Step::once(Reply::error(status))])
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    selection(&mut out);
    model_routing(&mut out);
    retry_policy(&mut out);
    streaming_options(&mut out);
    headers(&mut out);
    other_endpoints(&mut out);
    out
}

/// Key rotation, strategies and priorities.
fn selection(out: &mut Vec<Scenario>) {
    for f in FAMILIES {
        out.push(Scenario::new(
            format!("route.rr.{}", f.label()),
            "round robin across two keys over four requests",
            ok(),
            repeat(4, chat(f.model())),
        ));
    }
    for f in [Family::Claude, Family::Compat] {
        out.push(
            Scenario::new(format!("route.fill_first.{}", f.label()), "fill-first sticks to one key", ok(), repeat(3, chat(f.model())))
                .profile(profiles::fill_first),
        );
    }
    out.push(
        Scenario::new("route.fill_first.claude_failover", "fill-first moves on after a 500, then sticks", fail_once(500), repeat(3, chat(Family::Claude.model())))
            .profile(profiles::fill_first),
    );
    for f in [Family::Claude, Family::Gemini] {
        out.push(
            Scenario::new(format!("route.wrr.{}", f.label()), "weighted round robin 3:1 over eight requests", ok(), repeat(8, chat(f.model())))
                .profile(profiles::weighted),
        );
    }
    out.push(
        Scenario::new("route.priority.healthy", "higher priority key serves every request", ok(), repeat(3, chat(Family::Claude.model())))
            .profile(profiles::priority),
    );
    out.push(
        Scenario::new("route.priority.fallback", "lower priority key serves after the preferred key fails", fail_once(500), repeat(3, chat(Family::Claude.model())))
            .profile(profiles::priority),
    );
}

/// Model naming: prefixes, aliases, exclusions, unknown models, thinking suffixes.
fn model_routing(out: &mut Vec<Scenario>) {
    let claude = Family::Claude.model();
    let prefixed = format!("team/{claude}");
    for (id, profile) in [("route.prefix.optional", profiles::prefix as fn(&mut _)), ("route.prefix.forced", profiles::prefix_forced)] {
        out.push(
            Scenario::new(
                id,
                "prefixed key reachable as team/<model>; plain model per force-model-prefix",
                ok(),
                vec![chat(&prefixed).into(), chat(claude).into(), chat(claude).into(), HttpReq::get("/v1/models").into()],
            )
            .profile(profile),
        );
    }
    out.push(
        Scenario::new(
            "route.excluded_models",
            "excluded models are unroutable and unlisted",
            ok(),
            vec![chat(claude).into(), chat("claude-haiku-4-5-20251001").into(), HttpReq::get("/v1/models").into()],
        )
        .profile(profiles::claude_excluded),
    );
    out.push(
        Scenario::new(
            "route.alias.claude",
            "configured model alias routes to the upstream name; the plain name is gone",
            ok(),
            vec![
                chat("sonnet-alias").into(),
                chat(claude).into(),
                HttpReq::post("/v1/messages", bodies::claude("sonnet-alias", false, Kind::Text)).into(),
                HttpReq::get("/v1/models").into(),
            ],
        )
        .profile(profiles::claude_alias),
    );
    out.push(Scenario::new(
        "route.alias.compat_upstream_name",
        "the compat upstream model name is not routable, only its alias",
        ok(),
        vec![chat("mock-gpt-4o").into(), chat("compat-gpt-4o").into()],
    ));
    for (id, req) in [
        ("route.unknown_model.chat", chat("no-such-model")),
        ("route.unknown_model.claude", HttpReq::post("/v1/messages", bodies::claude("no-such-model", false, Kind::Text))),
        ("route.unknown_model.responses", HttpReq::post("/v1/responses", bodies::responses("no-such-model", false, Kind::Text))),
        ("route.unknown_model.gemini", HttpReq::post(&bodies::gemini_path("no-such-model", "generateContent"), bodies::gemini(Kind::Text))),
    ] {
        out.push(Scenario::new(id, "model unknown to the registry", ok(), vec![req.into()]));
    }
    let suffixed = [
        (Family::Claude, "claude-sonnet-4-5-20250929(2048)"),
        (Family::Codex, "gpt-5.5(high)"),
        (Family::Gemini, "gemini-2.5-flash(1024)"),
        (Family::Compat, "compat-reason(high)"),
    ];
    for (f, model) in suffixed {
        out.push(Scenario::new(
            format!("route.thinking_suffix.{}", f.label()),
            "thinking suffix in the model name configures upstream thinking",
            Script::ok(Content::Thinking),
            vec![chat(model).into(), chat_stream(model).into()],
        ));
    }
}

/// Retry rounds, per-request credential cap and cooldown toggles.
fn retry_policy(out: &mut Vec<Scenario>) {
    let claude = Family::Claude.model();
    let always = |s: u16| Script::steps(vec![Step::always(Reply::error(s))]);
    out.push(
        Scenario::new("route.retry.none", "request-retry 0: one round only, failover within it", always(500), vec![chat(claude).into()])
            .profile(profiles::no_retry),
    );
    out.push(
        Scenario::new("route.retry.rounds", "request-retry 3 without transient cooldown: four immediate rounds", always(500), vec![chat(claude).into()])
            .profile(profiles::rounds_no_cooldown),
    );
    out.push(
        Scenario::new("route.retry.max_credentials", "max-retry-credentials 1 caps attempts per round", always(500), vec![chat(claude).into()])
            .profile(profiles::max_one_credential),
    );
    out.push(
        Scenario::new("route.cooling.disabled_500", "disable-cooling: failed keys stay eligible", always(500), repeat(2, chat(claude)))
            .profile(profiles::disable_cooling),
    );
    out.push(
        Scenario::new("route.cooling.disabled_429", "disable-cooling with 429s", always(429), repeat(2, chat(claude)))
            .profile(profiles::disable_cooling),
    );
    out.push(Scenario::new(
        "route.cooling.500_second_request",
        "a 500 cools the key; the next request uses the other key",
        fail_once(500),
        repeat(3, chat(claude)),
    ));
    // Every key cooling after upstream failures: the "no auth available" error carries
    // providers, model and the last upstream error.
    for status in [500, 401] {
        out.push(Scenario::new(
            format!("route.cooling.all_cooling_{status}"),
            "every key cooling down: auth_unavailable with the last upstream error",
            always(status),
            repeat(3, chat(claude)),
        ));
    }
    out.push(Scenario::new(
        "route.cooling.401_second_request",
        "a 401 cools the key for a long time",
        fail_once(401),
        repeat(3, chat(claude)),
    ));
    out.push(
        Scenario::new(
            "route.scoped.stop",
            "request-scoped-errors stop rule returns the 429 without failover or cooldown",
            Script::steps(vec![Step::once(Reply::error(429))]),
            repeat(2, chat(claude)),
        )
        .profile(profiles::scoped_stop),
    );
    out.push(
        Scenario::new(
            "route.scoped.continue_cooldown",
            "request-scoped-errors continue-and-cooldown rotates keys on a 400",
            Script::steps(vec![Step::once(Reply::error(400))]),
            repeat(2, chat(claude)),
        )
        .profile(profiles::scoped_continue_cooldown),
    );
    out.push(Scenario::new(
        "route.400_context_length",
        "context_length_exceeded is a request fault and is returned as is",
        Script::steps(vec![Step::always(Reply::error_with(
            400,
            &[],
            Some(json!({"error":{"message":"maximum context length exceeded","type":"invalid_request_error","code":"context_length_exceeded"}})),
        ))]),
        vec![chat(Family::Compat.model()).into()],
    ));
    out.push(Scenario::new(
        "route.upstream_html_500",
        "non-JSON upstream error body",
        Script::steps(vec![Step::always(Reply::Raw { status: 502, content_type: "text/html".into(), body: "<html>bad gateway</html>".into() })]),
        vec![chat(Family::Compat.model()).into()],
    ));
    out.push(Scenario::new(
        "route.upstream_garbage_200",
        "200 with a body that is not JSON",
        Script::steps(vec![Step::always(Reply::Raw { status: 200, content_type: "application/json".into(), body: "not json".into() })]),
        vec![chat(Family::Compat.model()).into()],
    ));
}

/// Streaming bootstrap retries and keep-alives.
fn streaming_options(out: &mut Vec<Scenario>) {
    let claude = Family::Claude.model();
    // The compat executor turns an in-band error as the first event into a bootstrap failure on
    // every key; the third attempt (a handler-level retry, if enabled) succeeds.
    let compat = Family::Compat.model();
    let two_failures = Script::steps(vec![Step::times(2, Reply::StreamError { content: Content::Text, after: 0 })]);
    out.push(
        Scenario::new("route.bootstrap.off", "stream fails when every key fails before the first payload", two_failures.clone(), vec![chat_stream(compat).into()])
            .profile(profiles::bootstrap_off),
    );
    out.push(
        Scenario::new("route.bootstrap.retry1", "bootstrap-retries 1 calls the executor again before the first byte", two_failures, vec![chat_stream(compat).into()])
            .profile(profiles::bootstrap_one),
    );
    // 1s keep-alive interval against a 3.5s stall: ticks at 1s, 2s, 3s, each 0.5s or more away
    // from the upstream's own timing.
    let stalled = |reply: Reply| Script::steps(vec![Step::always(reply).stalled(3500)]);
    let delayed = |reply: Reply| Script::steps(vec![Step::always(reply).delayed(3500)]);
    out.push(
        Scenario::new("route.keepalive.stream", "keepalive-seconds emits SSE comments while upstream stalls mid-stream", stalled(Reply::ok(Content::Text)), vec![chat_stream(claude).into()])
            .profile(profiles::stream_keepalive),
    );
    out.push(
        Scenario::new("route.keepalive.nonstream", "nonstream-keepalive-interval writes blank lines before the JSON body", delayed(Reply::ok(Content::Text)), vec![chat(claude).into()])
            .profile(profiles::nonstream_keepalive),
    );
    out.push(
        Scenario::new(
            "route.keepalive.nonstream_error",
            "an error after a keep-alive newline is delivered with status 200",
            delayed(Reply::error(400)),
            vec![chat(claude).into()],
        )
        .profile(profiles::nonstream_keepalive),
    );
}

/// Header handling in both directions.
fn headers(out: &mut Vec<Scenario>) {
    let upstream_headers = [("x-e2e-upstream", "from-upstream"), ("x-ratelimit-remaining", "5")];
    let with_headers = |reply: Reply| Script::steps(vec![Step::always(reply).with_headers(&upstream_headers)]);
    let claude = Family::Claude.model();
    out.push(Scenario::new("route.passthrough.off", "upstream response headers are not forwarded by default", with_headers(Reply::ok(Content::Text)), vec![chat(claude).into()]));
    out.push(
        Scenario::new("route.passthrough.on", "passthrough-headers forwards filtered upstream headers", with_headers(Reply::ok(Content::Text)), vec![chat(claude).into(), chat_stream(claude).into()])
            .profile(profiles::passthrough_headers),
    );
    out.push(
        Scenario::new("route.passthrough.on_error", "passthrough-headers on an upstream error", with_headers(Reply::error(400)), vec![chat(claude).into()])
            .profile(profiles::passthrough_headers),
    );
    for f in FAMILIES {
        out.push(
            Scenario::new(
                format!("route.headers.custom.{}", f.label()),
                "configured static and client-copied upstream headers",
                ok(),
                vec![chat(f.model()).header("X-Client-Tag", "tag-123").into()],
            )
            .profile(profiles::custom_headers),
        );
    }
    let sdk_headers = |r: HttpReq| {
        r.header("User-Agent", "e2e-sdk/9.9")
            .header("X-Stainless-Lang", "js")
            .header("X-Request-Id", "client-req-1")
            .header("Idempotency-Key", "idem-1")
    };
    out.push(Scenario::new(
        "route.client_headers.claude",
        "which client headers reach a Claude upstream",
        ok(),
        vec![sdk_headers(HttpReq::post("/v1/messages", bodies::claude(claude, false, Kind::Text)))
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "fine-grained-tool-streaming-2025-05-14")
            .into()],
    ));
    out.push(Scenario::new(
        "route.client_headers.codex",
        "which client headers reach a Codex upstream",
        ok(),
        vec![sdk_headers(HttpReq::post("/v1/responses", bodies::responses(Family::Codex.model(), false, Kind::Text)))
            .header("Originator", "codex_cli_rs")
            .header("Version", "0.99.0")
            .header("Session_id", "11111111-2222-3333-4444-555555555555")
            .header("X-Codex-Turn-Metadata", "{\"session_id\":\"abc\"}")
            .into()],
    ));
    out.push(Scenario::new(
        "route.client_headers.gemini",
        "which client headers reach a Gemini upstream",
        ok(),
        vec![sdk_headers(HttpReq::post(&bodies::gemini_path(Family::Gemini.model(), "generateContent"), bodies::gemini(Kind::Text)))
            .header("X-Goog-Api-Client", "genai-js/0.1")
            .into()],
    ));
    out.push(Scenario::new(
        "route.client_headers.compat",
        "which client headers reach an OpenAI-compatible upstream",
        ok(),
        vec![sdk_headers(chat(Family::Compat.model())).header("OpenAI-Organization", "org-1").header("OpenAI-Beta", "assistants=v2").into()],
    ));
}

/// Endpoints other than plain generation: compact, token counting, images.
fn other_endpoints(out: &mut Vec<Scenario>) {
    for f in FAMILIES {
        out.push(Scenario::new(
            format!("route.compact.{}", f.label()),
            "POST /v1/responses/compact",
            ok(),
            vec![HttpReq::post("/v1/responses/compact", json!({"model": f.model(), "input": "Summarize"})).into()],
        ));
        out.push(Scenario::new(
            format!("route.count_tokens.claude_dialect.{}", f.label()),
            "Claude count_tokens against each family",
            ok(),
            vec![HttpReq::post(
                "/v1/messages/count_tokens",
                json!({"model": f.model(), "messages": [{"role":"user","content":"What is the weather in Paris?"}]}),
            )
            .into()],
        ));
        out.push(Scenario::new(
            format!("route.count_tokens.gemini_dialect.{}", f.label()),
            "Gemini countTokens against each family",
            ok(),
            vec![HttpReq::post(&bodies::gemini_path(f.model(), "countTokens"), bodies::gemini(Kind::Text)).into()],
        ));
    }
    out.push(Scenario::new(
        "route.images.unsupported_model",
        "image generation with a non-image model",
        ok(),
        vec![HttpReq::post("/v1/images/generations", json!({"model": Family::Compat.model(), "prompt": "a cat"})).into()],
    ));
    out.push(Scenario::new(
        "route.completions.string_prompt_defaults",
        "legacy completions without a prompt",
        ok(),
        vec![HttpReq::post("/v1/completions", json!({"model": Family::Compat.model()})).into()],
    ));
}

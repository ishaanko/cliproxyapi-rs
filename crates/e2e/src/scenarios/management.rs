//! Management API scenarios (`/v0/management`, `/v8/management`).

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::bodies::{self, Family, Kind};
use super::profiles;
use crate::client::{Auth, HttpReq, Step as Req};
use crate::mock::script::{Content, Reply, Script, Step};
use crate::scenario::Scenario;

const V0: &str = "/v0/management";
const V8: &str = "/v8/management";
/// Lets the asynchronous config reload triggered by a management write finish.
const SETTLE_MS: u64 = 500;

fn mgmt(req: HttpReq) -> Req {
    Req::Http(req.auth(Auth::Mgmt))
}

fn get(path: &str) -> Req {
    mgmt(HttpReq::get(path))
}

fn script_ok() -> Script {
    Script::ok(Content::Text)
}

/// Stable credential id shown by management and trace headers:
/// first 8 bytes of sha256("<prefix>:<base_url>+<api_key>"), hex.
pub(super) fn auth_index(prefix: &str, mock_port: u16, family: &str, key: &str) -> String {
    let seed = format!("{prefix}:http://127.0.0.1:{mock_port}/{family}+{key}");
    Sha256::digest(seed.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn claude_chat() -> Req {
    Req::Http(HttpReq::post("/v1/chat/completions", bodies::chat(Family::Claude.model(), false, Kind::Text)))
}

pub fn scenarios(mock_port: u16) -> Vec<Scenario> {
    let mut out = vec![];
    auth(&mut out);
    config(&mut out);
    lists(&mut out);
    auth_files(&mut out, mock_port);
    observability(&mut out, mock_port);
    misc(&mut out, mock_port);
    out
}

fn auth(out: &mut Vec<Scenario>) {
    let config_path = format!("{V0}/config");
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("mgmt.auth.{id}"), desc, script_ok(), steps);
    out.push(s("missing_key", "no management key", vec![Req::Http(HttpReq::get(&config_path).auth(Auth::None))]));
    out.push(s("invalid_key", "wrong management key", vec![Req::Http(HttpReq::get(&config_path).auth(Auth::Bearer("wrong")))]));
    out.push(s("raw_authorization", "management key without Bearer", vec![Req::Http(HttpReq::get(&config_path).auth(Auth::RawAuthorization(crate::config::MGMT_SECRET)))]));
    out.push(s(
        "x_management_key",
        "X-Management-Key header",
        vec![Req::Http(HttpReq::get(&config_path).auth(Auth::None).header("X-Management-Key", crate::config::MGMT_SECRET))],
    ));
    out.push(s("client_key_rejected", "a proxy client key is not a management key", vec![Req::Http(HttpReq::get(&config_path))]));
    out.push(
        Scenario::new(
            "mgmt.auth.disabled_without_secret",
            "no secret-key: management routes 404",
            script_ok(),
            vec![get(&config_path), get(&format!("{V8}/config")), get(&format!("{V0}/api-keys"))],
        )
        .profile(profiles::no_management),
    );
    let mut ban: Vec<Req> = (0..5).map(|_| Req::Http(HttpReq::get(&config_path).auth(Auth::Bearer("wrong")))).collect();
    ban.push(get(&config_path));
    out.push(s("ip_ban", "five failures ban the client even for the right key", ban));
}

fn config(out: &mut Vec<Scenario>) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("mgmt.config.{id}"), desc, script_ok(), steps);
    out.push(s("v0_get", "GET /v0/management/config", vec![get(&format!("{V0}/config"))]));
    out.push(s("v8_get", "GET /v8/management/config", vec![get(&format!("{V8}/config"))]));
    out.push(s(
        "v8_subtree",
        "v8 config subtrees by key path",
        vec![
            get(&format!("{V8}/config/routing/retry")),
            get(&format!("{V8}/config/access/api-keys")),
            get(&format!("{V8}/config/routing/retry/request-retry")),
            get(&format!("{V8}/config/no/such/key")),
            get(&format!("{V8}/config/")),
        ],
    ));
    out.push(s("yaml_v0", "GET /v0/management/config.yaml", vec![get(&format!("{V0}/config.yaml"))]));
    out.push(s("yaml_v8", "GET /v8/management/config.yaml", vec![get(&format!("{V8}/config.yaml"))]));
    out.push(s(
        "v8_write",
        "PUT, PATCH and DELETE on the v8 config tree",
        vec![
            mgmt(HttpReq::put(&format!("{V8}/config/routing/retry/request-retry"), json!(5))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V8}/config/routing/retry")),
            mgmt(HttpReq::patch(&format!("{V8}/config/routing/retry"), json!({"max-retry-interval": 9}))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V8}/config/routing/retry")),
            mgmt(HttpReq::delete(&format!("{V8}/config/routing/retry/max-retry-interval"))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V8}/config/routing/retry")),
            get(&format!("{V0}/config")),
        ],
    ));
    out.push(s(
        "v8_write_errors",
        "invalid v8 config writes",
        vec![
            mgmt(HttpReq::put(&format!("{V8}/config/routing/retry/request-retry"), json!("many"))),
            mgmt(HttpReq::put(&format!("{V8}/config"), json!([1, 2]))),
            mgmt(HttpReq::put(&format!("{V8}/config/routing/retry"), json!({})).raw("{not json")),
            mgmt(HttpReq::delete(&format!("{V8}/config"))),
            mgmt(HttpReq::delete(&format!("{V8}/config/no/such"))),
            mgmt(HttpReq::patch(&format!("{V8}/config/credentials/concurrency/lifecycle-config-revision"), json!(9))),
            mgmt(HttpReq::put(&format!("{V8}/config/nonsense-root"), json!({"a": 1}))),
        ],
    ));
    out.push(s(
        "yaml_put_invalid",
        "invalid PUT /v0/management/config.yaml",
        vec![mgmt(HttpReq::put(&format!("{V0}/config.yaml"), Value::Null).raw("port: [unclosed")), get(&format!("{V0}/config"))],
    ));
    out.push(s(
        "scalars",
        "v0 scalar settings: GET, PUT, PATCH",
        vec![
            get(&format!("{V0}/request-retry")),
            mgmt(HttpReq::put(&format!("{V0}/request-retry"), json!({"value": 4}))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V0}/request-retry")),
            mgmt(HttpReq::patch(&format!("{V0}/max-retry-interval"), json!({"value": 7}))),
            mgmt(HttpReq::put(&format!("{V0}/debug"), json!({"value": true}))),
            mgmt(HttpReq::put(&format!("{V0}/proxy-url"), json!({"value": "socks5://127.0.0.1:1"}))),
            mgmt(HttpReq::delete(&format!("{V0}/proxy-url"))),
            get(&format!("{V0}/routing/strategy")),
            mgmt(HttpReq::put(&format!("{V0}/routing/strategy"), json!({"value": "ff"}))),
            get(&format!("{V0}/routing/strategy")),
            mgmt(HttpReq::put(&format!("{V0}/routing/strategy"), json!({"value": "bogus"}))),
            mgmt(HttpReq::put(&format!("{V0}/request-retry"), json!({"value": "x"}))),
            mgmt(HttpReq::put(&format!("{V0}/debug"), json!({}))),
            get(&format!("{V0}/logging-to-file")),
            get(&format!("{V0}/usage-statistics-enabled")),
        ],
    ));
}

/// Client api-keys and provider key lists.
fn lists(out: &mut Vec<Scenario>) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("mgmt.keys.{id}"), desc, script_ok(), steps);
    out.push(s(
        "api_keys_crud",
        "client api-keys: list, replace, patch, delete; the new key authenticates",
        vec![
            get(&format!("{V0}/api-keys")),
            mgmt(HttpReq::put(&format!("{V0}/api-keys"), json!(["e2e-client-key", "added-key"]))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V0}/api-keys")),
            Req::Http(HttpReq::get("/v1/models").auth(Auth::Bearer("added-key"))),
            mgmt(HttpReq::patch(&format!("{V0}/api-keys"), json!({"old": "added-key", "new": "renamed-key"}))),
            Req::Pause(SETTLE_MS),
            Req::Http(HttpReq::get("/v1/models").auth(Auth::Bearer("added-key"))),
            Req::Http(HttpReq::get("/v1/models").auth(Auth::Bearer("renamed-key"))),
            mgmt(HttpReq::delete(&format!("{V0}/api-keys?value=renamed-key"))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V0}/api-keys")),
            mgmt(HttpReq::delete(&format!("{V0}/api-keys"))),
            mgmt(HttpReq::put(&format!("{V0}/api-keys"), json!([]))),
        ],
    ));
    out.push(s(
        "provider_lists",
        "provider key lists with their auth-index",
        vec![
            get(&format!("{V0}/claude-api-key")),
            get(&format!("{V0}/codex-api-key")),
            get(&format!("{V0}/gemini-api-key")),
            get(&format!("{V0}/openai-compatibility")),
            get(&format!("{V0}/xai-api-key")),
            get(&format!("{V0}/vertex-api-key")),
            get(&format!("{V0}/oauth-excluded-models")),
            get(&format!("{V0}/oauth-model-alias")),
        ],
    ));
    out.push(s(
        "claude_key_edit",
        "patch, delete and re-add a claude key",
        vec![
            mgmt(HttpReq::patch(&format!("{V0}/claude-api-key"), json!({"match": "sk-claude-1", "value": {"prefix": "edited"}}))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V0}/claude-api-key")),
            mgmt(HttpReq::delete(&format!("{V0}/claude-api-key?api-key=sk-claude-2"))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V0}/claude-api-key")),
            mgmt(HttpReq::patch(&format!("{V0}/claude-api-key"), json!({"match": "missing", "value": {"prefix": "x"}}))),
            mgmt(HttpReq::delete(&format!("{V0}/claude-api-key"))),
            mgmt(HttpReq::put(&format!("{V0}/claude-api-key"), json!([{"api-key": "sk-new", "base-url": "http://127.0.0.1:1/x"}]))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V0}/claude-api-key")),
            Req::Http(HttpReq::get("/v1/models")),
        ],
    ));
}

fn auth_files(out: &mut Vec<Scenario>, mock_port: u16) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("mgmt.auth_files.{id}"), desc, script_ok(), steps);
    out.push(s(
        "list",
        "credential listing (v0 auth-files, v8 credentials, pagination)",
        vec![
            get(&format!("{V0}/auth-files")),
            get(&format!("{V8}/credentials")),
            get(&format!("{V0}/auth-files?page=1&page_size=3")),
            get(&format!("{V0}/auth-files?page=0")),
            get(&format!("{V0}/auth-files?page_size=x")),
            get(&format!("{V0}/auth-files?auth_index={}", auth_index("claude-api-key", mock_port, "anthropic", "sk-claude-1"))),
        ],
    ));
    let file = json!({"type": "claude", "email": "e2e@example.com", "disabled": true, "access_token": "sk-ant-oat-fake", "priority": 3});
    out.push(s(
        "lifecycle",
        "upload a disabled credential file, inspect, patch and delete it",
        vec![
            mgmt(HttpReq::post(&format!("{V0}/auth-files?name=e2e-claude.json"), file.clone())),
            Req::Pause(SETTLE_MS),
            get(&format!("{V0}/auth-files?name=e2e-claude.json")),
            get(&format!("{V0}/auth-files/download?name=e2e-claude.json")),
            mgmt(HttpReq::patch(&format!("{V0}/auth-files/fields"), json!({"name": "e2e-claude.json", "note": "hello", "priority": 7}))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V0}/auth-files/download?name=e2e-claude.json")),
            mgmt(HttpReq::patch(&format!("{V0}/auth-files/status"), json!({"name": "e2e-claude.json", "disabled": false}))),
            mgmt(HttpReq::delete(&format!("{V0}/auth-files?name=e2e-claude.json"))),
            Req::Pause(SETTLE_MS),
            get(&format!("{V0}/auth-files?name=e2e-claude.json")),
        ],
    ));
    out.push(s(
        "errors",
        "invalid credential file requests",
        vec![
            mgmt(HttpReq::post(&format!("{V0}/auth-files?name=notjson.txt"), json!({"type": "claude"}))),
            mgmt(HttpReq::post(&format!("{V0}/auth-files"), json!({"type": "claude"}))),
            mgmt(HttpReq::post(&format!("{V0}/auth-files?name=bad.json"), Value::Null).raw("{broken")),
            get(&format!("{V0}/auth-files/download?name=missing.json")),
            get(&format!("{V0}/auth-files/download?name=../etc/passwd")),
            get(&format!("{V0}/auth-files/models")),
            mgmt(HttpReq::delete(&format!("{V0}/auth-files?name=missing.json"))),
            mgmt(HttpReq::patch(&format!("{V0}/auth-files/status"), json!({"name": "missing.json", "disabled": true}))),
            mgmt(HttpReq::patch(&format!("{V0}/auth-files/fields"), json!({"note": "x"}))),
        ],
    ));
}

/// Usage counters, cooldown state and logs.
fn observability(out: &mut Vec<Scenario>, mock_port: u16) {
    out.push(Scenario::new(
        "mgmt.usage.api_key_usage",
        "per-key success/failed counters after mixed requests",
        Script::steps(vec![Step::once(Reply::ok(Content::Text)), Step::once(Reply::error(400))]),
        vec![claude_chat(), claude_chat(), get(&format!("{V0}/api-key-usage")), get(&format!("{V8}/observability/usage/api-keys"))],
    ));
    out.push(
        Scenario::new(
            "mgmt.usage.queue",
            "usage events popped from the queue",
            Script::steps(vec![Step::once(Reply::ok(Content::Text)), Step::once(Reply::error(400))]),
            vec![
                claude_chat(),
                claude_chat(),
                Req::Pause(SETTLE_MS),
                get(&format!("{V0}/usage-queue?count=5")),
                get(&format!("{V0}/usage-queue?count=0")),
                get(&format!("{V0}/usage-queue")),
            ],
        )
        .profile(profiles::usage_stats),
    );
    let idx1 = auth_index("claude-api-key", mock_port, "anthropic", "sk-claude-1");
    let idx2 = auth_index("claude-api-key", mock_port, "anthropic", "sk-claude-2");
    out.push(Scenario::new(
        "mgmt.cooldown.reset",
        "cooldown state is visible in credentials; reset clears it",
        Script::steps(vec![Step::always(Reply::error(500))]),
        vec![
            claude_chat(),
            get(&format!("{V0}/auth-files?auth_index={idx1}")),
            get(&format!("{V0}/auth-files?auth_index={idx2}")),
            mgmt(HttpReq::post(&format!("{V0}/reset-quota"), json!({"auth_index": idx1}))),
            mgmt(HttpReq::post(&format!("{V8}/routing/cooldown/reset"), json!({"auth_index": idx2}))),
            mgmt(HttpReq::post(&format!("{V0}/reset-quota"), json!({"auth_index": "ffffffffffffffff"}))),
            mgmt(HttpReq::post(&format!("{V0}/reset-quota"), json!({}))),
            get(&format!("{V0}/auth-files?auth_index={idx1}")),
        ],
    ));
    out.push(Scenario::new(
        "mgmt.logs.disabled",
        "log endpoints with logging-to-file off",
        script_ok(),
        vec![
            get(&format!("{V0}/logs")),
            get(&format!("{V0}/request-error-logs")),
            get(&format!("{V0}/request-error-logs/error-nope.log")),
            get(&format!("{V0}/request-log-by-id/abcdef12")),
            get(&format!("{V8}/observability/logs")),
        ],
    ));
}

fn misc(out: &mut Vec<Scenario>, mock_port: u16) {
    let s = |id: &str, desc: &str, steps: Vec<Req>| Scenario::new(format!("mgmt.{id}"), desc, script_ok(), steps);
    out.push(s(
        "model_definitions",
        "static model definitions per channel",
        vec![
            get(&format!("{V0}/model-definitions/claude")),
            get(&format!("{V0}/model-definitions/codex")),
            get(&format!("{V0}/model-definitions/nope")),
            get(&format!("{V8}/routing/model-definitions/gemini")),
        ],
    ));
    let idx = auth_index("openai-compatibility", mock_port, "compat", "sk-compat-1");
    out.push(s(
        "api_call",
        "api-call substitutes $TOKEN$ with the credential and proxies the request",
        vec![
            mgmt(HttpReq::post(
                &format!("{V0}/api-call"),
                json!({
                    "auth_index": idx,
                    "method": "GET",
                    "url": format!("http://127.0.0.1:{mock_port}/compat/models"),
                    "header": {"Authorization": "Bearer $TOKEN$", "X-E2E": "1"}
                }),
            )),
            mgmt(HttpReq::post(&format!("{V0}/api-call"), json!({"method": "GET"}))),
            mgmt(HttpReq::post(&format!("{V0}/api-call"), json!({"url": "http://127.0.0.1:1/x"}))),
            mgmt(HttpReq::post(&format!("{V0}/api-call"), json!({"method": "GET", "url": "not a url"}))),
        ],
    ));
    out.push(s(
        "oauth_status",
        "OAuth session endpoints without a started login",
        vec![
            get(&format!("{V0}/get-auth-status?state=unknown-state")),
            get(&format!("{V0}/get-auth-status")),
            get(&format!("{V0}/get-auth-status?state=bad/state")),
            mgmt(HttpReq::delete(&format!("{V0}/oauth-session?state=unknown-state"))),
            Req::Http(HttpReq::get(&format!("{V0}/oauth-callback?state=unknown-state&code=x&provider=codex")).auth(Auth::None)),
            Req::Http(HttpReq::post(&format!("{V0}/oauth-callback"), json!({"provider": "codex"})).auth(Auth::None)),
            get(&format!("{V8}/oauth/auth-url")),
        ],
    ));
    out.push(s("plugins", "plugin endpoints with plugins disabled", vec![get(&format!("{V0}/plugins")), get(&format!("{V8}/plugins")), get(&format!("{V0}/plugin-store"))]).profile(profiles::plugin_store_mock));
    out.push(s("unknown_route", "unknown management paths", vec![get(&format!("{V0}/no-such-thing")), get(&format!("{V8}/no-such-thing"))]));
}

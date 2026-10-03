//! Model list endpoints: `/v1/models` dispatch and the Gemini `/v1beta/models` pair. The payloads
//! come from `cpa_runtime::service::listing` (pure functions over the model registry); this
//! module routes requests to them and serializes like Go (`WriteModelListResponse`).

use axum::http::HeaderMap;
use cpa_config::Config;
use cpa_core::registry::global_registry;
use cpa_runtime::conductor::Manager;
use cpa_runtime::service::{
    ModelsRoute, claude_models_response, gemini_model_response, gemini_models_response, grok_models_response,
    openai_models_response, route_models_request,
};

use crate::error::error_response_json;
use crate::home_models;
use crate::reply::Reply;
use crate::req::ReqInfo;

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// `writeModelListResponse` for the registry-backed lists: a 200 body is also recorded as the
/// request log's API response (`WriteModelListResponse` sets `API_RESPONSE`).
fn recorded(info: &ReqInfo, reply: Reply) -> Reply {
    if reply.status == 200 {
        info.api_log.set_api_response(&reply.body);
    }
    reply
}

/// `GET /v1/models` (`unifiedModelsHandler`). With Home enabled every variant is answered from
/// Home's catalog instead of the local registry.
pub async fn unified_models(cfg: &Config, manager: &Manager, info: &ReqInfo) -> Reply {
    let registry = global_registry();
    let headers = &info.headers;
    let client_version = info.query.iter().find(|(k, _)| k == "client_version").map(|(_, v)| v.as_str());
    let route = route_models_request(
        client_version,
        header_str(headers, "anthropic-version"),
        header_str(headers, "user-agent"),
    );
    if cfg.home.enabled {
        return match route {
            ModelsRoute::Grok => home_models::grok_models_reply(info).await,
            ModelsRoute::CodexClient { client_version } => {
                home_models::codex_client_models_reply(cfg, manager, info, &client_version).await
            }
            ModelsRoute::Claude => home_models::models_reply(cfg, info, true).await,
            ModelsRoute::OpenAi => home_models::models_reply(cfg, info, false).await,
        };
    }
    let reply = match route {
        // Go serializes the Grok payload from structs, so keys keep declaration order.
        ModelsRoute::Grok => Reply::json(200, ordered_json(&grok_models_response(registry))),
        ModelsRoute::CodexClient { client_version } => {
            match crate::codex_models::build_client_models_body(&client_version, cfg, manager) {
                Ok(body) => Reply::json(200, body),
                Err(err) => Reply::json(
                    500,
                    error_response_json(&format!("Failed to encode model list: {err}"), "server_error"),
                ),
            }
        }
        ModelsRoute::Claude => claude_models(cfg),
        ModelsRoute::OpenAi => openai_models(),
    };
    recorded(info, reply)
}

/// Compact JSON in insertion order with Go's HTML escaping (`<`, `>`, `&`, U+2028/9).
pub(crate) fn ordered_json(value: &serde_json::Value) -> Vec<u8> {
    crate::logging::go_json_html_escape(serde_json::to_string(value).unwrap_or_else(|_| "null".into())).into_bytes()
}

pub fn openai_models() -> Reply {
    Reply::json_value(200, &openai_models_response(global_registry()))
}

/// `ClaudeModels`.
pub fn claude_models(cfg: &Config) -> Reply {
    let disable_cloaking = cfg.claude_code.disable_cloaking_model_list;
    Reply::json_value(200, &claude_models_response(global_registry(), disable_cloaking))
}

/// `GET /v1beta/models` (`geminiModelsHandler`).
pub async fn gemini_models(cfg: &Config, info: &ReqInfo) -> Reply {
    if cfg.home.enabled {
        return home_models::gemini_models_reply(info).await;
    }
    recorded(info, Reply::json_value(200, &gemini_models_response(global_registry())))
}

/// `GET /v1beta/models/<name>` (`geminiGetHandler`): the model, or the 404 `Not Found` JSON error.
pub async fn gemini_get_model(cfg: &Config, info: &ReqInfo, action: &str) -> Reply {
    if cfg.home.enabled {
        return home_models::gemini_model_reply(info, action).await;
    }
    match gemini_model_response(global_registry(), action) {
        Some(model) => Reply::json_value(200, &model),
        None => Reply::json(404, error_response_json("Not Found", "not_found")),
    }
}

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
use crate::reply::Reply;

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// `GET /v1/models` (`unifiedModelsHandler`).
pub fn unified_models(cfg: &Config, manager: &Manager, headers: &HeaderMap, query: &[(String, String)]) -> Reply {
    let registry = global_registry();
    let client_version = query.iter().find(|(k, _)| k == "client_version").map(|(_, v)| v.as_str());
    let route = route_models_request(
        client_version,
        header_str(headers, "anthropic-version"),
        header_str(headers, "user-agent"),
    );
    match route {
        ModelsRoute::Grok => Reply::json_value(200, &grok_models_response(registry)),
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
    }
}

pub fn openai_models() -> Reply {
    Reply::json_value(200, &openai_models_response(global_registry()))
}

/// `ClaudeModels`.
pub fn claude_models(cfg: &Config) -> Reply {
    let disable_cloaking = cfg.claude_code.disable_cloaking_model_list;
    Reply::json_value(200, &claude_models_response(global_registry(), disable_cloaking))
}

/// `GET /v1beta/models`.
pub fn gemini_models() -> Reply {
    Reply::json_value(200, &gemini_models_response(global_registry()))
}

/// `GET /v1beta/models/<name>`: the model, or the 404 `Not Found` JSON error.
pub fn gemini_get_model(action: &str) -> Reply {
    match gemini_model_response(global_registry(), action) {
        Some(model) => Reply::json_value(200, &model),
        None => Reply::json(404, error_response_json("Not Found", "not_found")),
    }
}

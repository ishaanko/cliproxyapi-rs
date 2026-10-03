//! Model capabilities Home attaches to a dispatch (Go: `attachResolvedHomeModelInfo` and
//! `homeAPIKeyModelOptions` in `api_key_model_capabilities.go`).

use cpa_auth::Auth;
use cpa_config::OpenAiCompatibilityModel;
use cpa_core::registry::ModelInfo;
use serde_json::Value;

use super::home_selection::HOME_UPSTREAM_MODEL_ATTRIBUTE;
use super::models::{
    RESOLVED_API_KEY_MODEL_INFO, RESOLVED_CODEX_OAUTH_MODEL_INFO, RESOLVED_HOME_MODEL_INFO, alias_lookup_candidates,
    model_info_value, rewrite_model_for_auth,
};
use super::util::parse_suffix;
use crate::executor::Request;

/// Request metadata key of the credential model options selected for the attempt.
pub const RESOLVED_HOME_MODEL_OPTIONS: &str = "cliproxy.resolved_home_model_options";

/// The selected credential's model configuration from Home's `credential_options.models`.
/// A present models list is authoritative, including empty lists and false defaults; `None`
/// means the credential carries no models list.
pub(crate) fn home_api_key_model_options(auth: &Auth, model: &str, route_model: &str) -> Option<OpenAiCompatibilityModel> {
    let raw = auth.metadata.get("credential_options")?;
    let models_raw = raw.get("models")?;
    if models_raw.is_null() {
        return None;
    }
    let models: Vec<OpenAiCompatibilityModel> = serde_json::from_value(models_raw.clone()).ok()?;
    let requested = model.trim();
    if requested.is_empty() {
        return Some(OpenAiCompatibilityModel::default());
    }
    let mut base = parse_suffix(requested).model_name.trim().to_string();
    if base.is_empty() {
        base = requested.to_string();
    }
    let (_, route_candidates) = alias_lookup_candidates(&rewrite_model_for_auth(route_model.trim(), auth));
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    for route in &route_candidates {
        for candidate in [requested, base.as_str()] {
            for configured in &models {
                let alias = configured.alias.trim();
                let mut name = configured.name.trim();
                if name.is_empty() {
                    name = alias;
                }
                if eq(name, candidate) && (eq(alias, route) || eq(name, route)) {
                    return Some(configured.clone());
                }
            }
        }
    }
    // Prefer upstream names over aliases and exact suffixes over base fallbacks.
    for use_alias in [false, true] {
        for candidate in [requested, base.as_str()] {
            if candidate.is_empty() {
                continue;
            }
            for configured in &models {
                let mut name = configured.name.trim();
                if use_alias || name.is_empty() {
                    name = configured.alias.trim();
                }
                if eq(name, candidate) {
                    return Some(configured.clone());
                }
            }
        }
    }
    Some(OpenAiCompatibilityModel::default())
}

/// Go: `attachResolvedHomeModelInfo`.
pub(crate) fn attach_resolved_home_model_info(
    req: &mut Request,
    auth: &Auth,
    route_model: &str,
    model_info: Option<ModelInfo>,
    support: Option<bool>,
) {
    let mut upstream_model = req.model.clone();
    if let Some(info) = &model_info {
        upstream_model.clone_from(&info.id);
    }
    let dispatched = auth.attr(HOME_UPSTREAM_MODEL_ATTRIBUTE);
    if !dispatched.is_empty() {
        upstream_model = dispatched;
    }
    let options = home_api_key_model_options(auth, &upstream_model, route_model);
    if model_info.is_none() && options.is_none() {
        return;
    }
    req.metadata.remove(RESOLVED_HOME_MODEL_OPTIONS);
    if let Some(options) = &options
        && let Ok(v) = serde_json::to_value(options)
    {
        req.metadata.insert(RESOLVED_HOME_MODEL_OPTIONS.into(), v);
    }
    let Some(mut selected) = model_info else { return };

    let local: Option<ModelInfo> = [RESOLVED_API_KEY_MODEL_INFO, RESOLVED_CODEX_OAUTH_MODEL_INFO]
        .iter()
        .find_map(|k| req.metadata.get(*k))
        .and_then(|v| {
            let mut info = serde_json::from_value::<ModelInfo>(v.clone()).ok()?;
            info.support_configuration_update =
                v.get("support_configuration_update").and_then(Value::as_bool).unwrap_or(false);
            Some(info)
        });
    let base_of = |id: &str| parse_suffix(id).model_name.trim().to_lowercase();
    let same_model = local.as_ref().is_some_and(|l| base_of(&l.id) == base_of(&selected.id));
    selected.support_configuration_update = match support {
        Some(s) => s,
        None => same_model && local.as_ref().is_some_and(|l| l.support_configuration_update),
    };
    if let Some(options) = &options {
        selected.is_compat = options.is_compat;
    }
    req.metadata.insert(RESOLVED_HOME_MODEL_INFO.into(), model_info_value(&selected));
}

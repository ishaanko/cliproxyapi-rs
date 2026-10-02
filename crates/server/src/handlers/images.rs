//! Image and video execution is not ported: after Go's request validation (generations: JSON body,
//! supported model, prompt) the endpoints answer 501 with an OpenAI-shaped error.
//! `disable-image-generation: true` keeps Go's bare 404 on the image routes.

use axum::extract::State;
use axum::response::Response;
use bytes::Bytes;
use cpa_config::DisableImageGenerationMode;
use cpa_core::registry::{OPENAI_IMAGE_MODEL_TYPE, lookup_model_info};
use cpa_json::J;

use super::{bad_request_message, read_request_body};
use crate::reply::Reply;
use crate::req::ReqInfo;
use crate::state::AppState;

const IMAGES_GENERATIONS_PATH: &str = "/v1/images/generations";
const IMAGES_EDITS_PATH: &str = "/v1/images/edits";
const DEFAULT_IMAGES_TOOL_MODEL: &str = "gpt-image-2";
const CODEX_IMAGE_MODELS: [&str; 5] = ["gpt-image-1.5", "gpt-image-2", "gpt-image-2.5-flare", "gpt-image-2.5-sunburst", "gpt-image-2.5"];
const XAI_IMAGE_MODELS: [&str; 3] = ["grok-imagine-image", "grok-imagine-image-quality", "grok-imagine-image-2.0"];

fn not_implemented(what: &str) -> Reply {
    let message = cpa_core::util::go_json_string(&format!("{what} are not supported by this server build"));
    Reply::json(
        501,
        format!(
            r#"{{"error":{{"message":{message},"type":"not_supported_error","param":null,"code":"endpoint_not_implemented"}}}}"#
        )
        .into_bytes(),
    )
}

/// `imagesModelParts`: text before and after the last `/`.
fn images_model_parts(model: &str) -> (&str, &str) {
    let model = model.trim();
    match model.rfind('/') {
        Some(idx) if idx + 1 < model.len() => (model[..idx].trim(), model[idx + 1..].trim()),
        _ => ("", model),
    }
}

/// `isSupportedImagesModel`: codex image tool models, xAI image models, configured compat models.
fn is_supported_images_model(model: &str) -> bool {
    let (prefix, base) = images_model_parts(model);
    let base = base.to_lowercase();
    if CODEX_IMAGE_MODELS.contains(&base.as_str()) {
        return true;
    }
    let prefix = prefix.to_lowercase();
    if XAI_IMAGE_MODELS.contains(&base.as_str()) && matches!(prefix.as_str(), "" | "xai" | "x-ai" | "grok") {
        return true;
    }
    let model = model.trim();
    !model.is_empty() && lookup_model_info(model, None).is_some_and(|info| info.r#type == OPENAI_IMAGE_MODEL_TYPE)
}

/// `rejectUnsupportedImagesModel` message.
fn unsupported_model_message(model: &str) -> String {
    format!(
        "Model {model} is not supported on {IMAGES_GENERATIONS_PATH} or {IMAGES_EDITS_PATH}. Use gpt-image-1.5, gpt-image-2, gpt-image-2.5-flare, gpt-image-2.5-sunburst, gpt-image-2.5, grok-imagine-image, grok-imagine-image-quality, grok-imagine-image-2.0, or a configured openai-compatibility image model."
    )
}

/// `POST /v1/images/generations` and `/v1/images/edits`.
pub async fn images(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    if st.cfg().disable_image_generation == DisableImageGenerationMode::All {
        return Reply::new(404).into_response();
    }
    if info.path == IMAGES_GENERATIONS_PATH {
        let raw = match read_request_body(&info, body) {
            Ok(b) => b,
            Err(reply) => return reply.into_response(),
        };
        if !cpa_json::valid(&raw) {
            return bad_request_message("Invalid request: body must be valid JSON").into_response();
        }
        let root = cpa_json::parse(&raw);
        let mut model = root.g("model").str().trim().to_string();
        if model.is_empty() {
            model = DEFAULT_IMAGES_TOOL_MODEL.to_string();
        }
        if !is_supported_images_model(&model) {
            return bad_request_message(&unsupported_model_message(&model)).into_response();
        }
        if root.g("prompt").str().trim().is_empty() {
            return bad_request_message("Invalid request: prompt is required").into_response();
        }
    }
    not_implemented("image endpoints").into_response()
}

/// Video routes (`/v1/videos*`, `/openai/v1/videos*`).
pub async fn videos(_info: ReqInfo) -> Response {
    not_implemented("video endpoints").into_response()
}

//! Image and video endpoints are not ported: they answer 501 with an OpenAI-shaped error.
//! `disable-image-generation: true` keeps Go's bare 404 on the image routes.

use axum::extract::State;
use axum::response::Response;
use cpa_config::DisableImageGenerationMode;

use crate::reply::Reply;
use crate::req::ReqInfo;
use crate::state::AppState;

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

/// `POST /v1/images/generations` and `/v1/images/edits`.
pub async fn images(State(st): State<AppState>, _info: ReqInfo) -> Response {
    if st.cfg().disable_image_generation == DisableImageGenerationMode::All {
        return Reply::new(404).into_response();
    }
    not_implemented("image endpoints").into_response()
}

/// Video routes (`/v1/videos*`, `/openai/v1/videos*`).
pub async fn videos(_info: ReqInfo) -> Response {
    not_implemented("video endpoints").into_response()
}

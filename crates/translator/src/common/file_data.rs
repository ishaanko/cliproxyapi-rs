//! OpenAI file content normalization (Go: common/file_data.go).

use cpa_core::misc::mime_type_for_extension;

/// Go `filepath.Ext` without the dot, lowercased: the suffix after the final dot of the last path
/// element.
fn lower_extension(filename: &str) -> String {
    let base = filename.rsplit('/').next().unwrap_or(filename);
    match base.rfind('.') {
        Some(dot) => base[dot + 1..].to_lowercase(),
        None => String::new(),
    }
}

/// The MIME type and raw base64 payload for OpenAI file content. `data:` URLs must carry a MIME
/// type and the `base64` flag; other strings are returned as-is with `fallback_mime_type`, or the
/// type implied by the filename extension. `None` when neither is available.
pub fn normalize_openai_file_data(
    filename: &str,
    fallback_mime_type: &str,
    file_data: &str,
) -> Option<(String, String)> {
    if file_data.is_empty() {
        return None;
    }

    let mut fallback = fallback_mime_type.to_string();
    if fallback.is_empty() {
        fallback = mime_type_for_extension(&lower_extension(filename))
            .unwrap_or_default()
            .to_string();
    }
    const DATA_URL_PREFIX: &str = "data:";
    let is_data_url = file_data
        .get(..DATA_URL_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(DATA_URL_PREFIX));
    if !is_data_url {
        if fallback.is_empty() {
            return None;
        }
        return Some((fallback, file_data.to_string()));
    }

    let (metadata, payload) = file_data[DATA_URL_PREFIX.len()..].split_once(',')?;
    if payload.is_empty() {
        return None;
    }
    let mut fields = metadata.split(';');
    let mime_type = fields.next().unwrap_or_default().trim();
    if mime_type.is_empty() {
        return None;
    }
    fields
        .any(|field| field.trim().eq_ignore_ascii_case("base64"))
        .then(|| (mime_type.to_string(), payload.to_string()))
}

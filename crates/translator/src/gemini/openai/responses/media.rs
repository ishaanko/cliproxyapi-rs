//! Media blocks (image, audio, video, file) of OpenAI Responses input mapped to Gemini
//! `inline_data` / `file_data` parts (Go: gemini_openai-responses_request.go, media helpers).

use base64::alphabet::STANDARD;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use cpa_core::misc::mime_type_for_extension;
use cpa_json::{json, Res, Value};

use super::lenient::RawTexts;
use crate::common::normalize_openai_file_data;

/// Go `base64.StdEncoding`: padding required, non-zero trailing bits tolerated.
const STD_PADDED: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::RequireCanonical).with_decode_allow_trailing_bits(true),
);
/// Go `base64.RawStdEncoding`.
const STD_RAW: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::RequireNone).with_decode_allow_trailing_bits(true),
);

pub(super) fn gemini_responses_inline_data_part(mime_type: &str, data: &str) -> Value {
    json!({"inline_data": {"mime_type": mime_type, "data": data}})
}

fn gemini_responses_file_data_part(mime_type: &str, file_uri: &str) -> Value {
    json!({"file_data": {"mime_type": mime_type, "file_uri": file_uri}})
}

/// The first non-blank value, trimmed.
fn first_non_empty(values: &[String]) -> String {
    values.iter().map(|v| v.trim()).find(|v| !v.is_empty()).unwrap_or_default().to_string()
}

fn is_data_url(raw: &str) -> bool {
    raw.trim().to_lowercase().starts_with("data:")
}

fn is_remote_url(u: &str) -> bool {
    let lower = u.trim().to_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("gs://")
}

fn is_generic_mime(mime_type: &str) -> bool {
    matches!(mime_type.trim().to_lowercase().as_str(), "" | "application/octet-stream" | "binary/octet-stream")
}

/// The first value that is non-blank and not a generic MIME type, trimmed.
fn first_non_generic_format(values: &[String]) -> String {
    values.iter().map(|v| v.trim()).find(|v| !v.is_empty() && !is_generic_mime(v)).unwrap_or_default().to_string()
}

/// MIME type and payload of a base64 `data:` URL; `None` unless it is valid base64 with data.
fn parse_openai_responses_data_url(raw_url: &str) -> Option<(String, String)> {
    let trimmed_raw = raw_url.trim();
    if trimmed_raw.len() < 5 || !trimmed_raw.is_char_boundary(5) || !trimmed_raw[..5].eq_ignore_ascii_case("data:") {
        return None;
    }
    let (metadata, payload) = trimmed_raw[5..].split_once(',')?;
    if payload.trim().is_empty() {
        return None;
    }
    let payload = payload.trim();
    let mut fields = metadata.split(';');
    let mime_type = fields.next().unwrap_or_default().trim().to_string();
    if !fields.any(|f| f.trim().eq_ignore_ascii_case("base64")) {
        return None;
    }
    // Go's decoder skips CR/LF.
    let stripped: Vec<u8> = payload.bytes().filter(|b| *b != b'\r' && *b != b'\n').collect();
    if STD_PADDED.decode(&stripped).is_err() && STD_RAW.decode(&stripped).is_err() {
        return None;
    }
    Some((mime_type, payload.to_string()))
}

/// File extension without the dot, original case (Go `strings.TrimPrefix(filepath.Ext(f), ".")`).
fn ext_of(filename: &str) -> String {
    let base = filename.rsplit('/').next().unwrap_or(filename);
    match base.rfind('.') {
        Some(dot) => base[dot + 1..].to_string(),
        None => String::new(),
    }
}

fn lower_ext_of(filename: &str) -> String {
    ext_of(filename).to_lowercase()
}

/// Go `filepath.Base`.
fn path_base(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// Percent-decodes `s` like Go's `url.PathUnescape`; `None` on a bad escape.
fn path_unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// The decoded path of an absolute URL (Go `url.Parse(...).Path`); `None` when parsing fails.
fn url_path(raw_url: &str) -> Option<String> {
    if raw_url.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let without_fragment = raw_url.split('#').next().unwrap_or(raw_url);
    let without_query = without_fragment.split('?').next().unwrap_or(without_fragment);
    let rest = without_query.split_once("://").map(|(_, r)| r).unwrap_or(without_query);
    let path = match rest.find('/') {
        Some(idx) => &rest[idx..],
        None => "",
    };
    path_unescape(path)
}

fn open_ai_responses_audio_mime_type(audio_format: &str) -> String {
    let audio_format = audio_format.trim();
    if is_generic_mime(audio_format) {
        return "audio/wav".to_string();
    }
    if audio_format.contains('/') {
        return audio_format.to_string();
    }
    let f_lower = audio_format.to_lowercase();
    match f_lower.as_str() {
        "wav" => "audio/wav".to_string(),
        "mp3" | "mpeg" => "audio/mpeg".to_string(),
        "ogg" => "audio/ogg".to_string(),
        "flac" => "audio/flac".to_string(),
        "aac" => "audio/aac".to_string(),
        "webm" => "audio/webm".to_string(),
        "pcm16" | "pcm" => "audio/pcm".to_string(),
        "g711_ulaw" | "g711_alaw" => "audio/basic".to_string(),
        "opus" => "audio/opus".to_string(),
        "m4a" => "audio/mp4".to_string(),
        "wma" => "audio/x-ms-wma".to_string(),
        _ => match mime_type_for_extension(&f_lower) {
            Some(mapped) if mapped.starts_with("audio/") => mapped.to_string(),
            _ => "audio/wav".to_string(),
        },
    }
}

fn open_ai_responses_video_mime_type(format: &str) -> String {
    let format = format.trim();
    if is_generic_mime(format) {
        return "video/mp4".to_string();
    }
    if format.contains('/') {
        return format.to_string();
    }
    let f_lower = format.to_lowercase();
    match f_lower.as_str() {
        "mp4" => "video/mp4".to_string(),
        "webm" => "video/webm".to_string(),
        "mov" | "quicktime" => "video/quicktime".to_string(),
        "avi" | "x-msvideo" => "video/x-msvideo".to_string(),
        "mpeg" => "video/mpeg".to_string(),
        "ogg" => "video/ogg".to_string(),
        "mkv" | "x-matroska" => "video/x-matroska".to_string(),
        "flv" | "x-flv" => "video/x-flv".to_string(),
        "3gpp" => "video/3gpp".to_string(),
        _ => match mime_type_for_extension(&f_lower) {
            Some(mapped) if mapped.starts_with("video/") => mapped.to_string(),
            _ => "video/mp4".to_string(),
        },
    }
}

/// Resolves a `data:` URL carrying a generic MIME type through an audio/video/... resolver:
/// the explicit format, else the filename extension, else `default`.
fn resolve_generic_data_url_mime(mime_type: String, format: &str, filename: &str, resolve: fn(&str) -> String, default: &str) -> String {
    if !is_generic_mime(&mime_type) {
        return mime_type;
    }
    if !format.is_empty() && !is_generic_mime(format) {
        resolve(format)
    } else if !filename.is_empty() {
        resolve(&ext_of(filename))
    } else {
        default.to_string()
    }
}

fn opt_str(block: &Res<'_>, path: &str) -> String {
    block.g(path).str()
}

fn open_ai_responses_audio_from_block(block: &Res<'_>) -> Option<(String, String)> {
    let b_type = block.g("type").str().trim().to_lowercase();
    if b_type != "input_audio" && b_type != "audio" {
        return None;
    }

    let filename = first_non_empty(&[opt_str(block, "filename"), opt_str(block, "file.filename")]);
    let mut audio_obj = block.g("input_audio");
    if !audio_obj.exists() {
        audio_obj = block.g("audio");
    }
    let mut audio_format = first_non_generic_format(&[
        audio_obj.g("format").str(),
        audio_obj.g("mime_type").str(),
        opt_str(block, "format"),
        opt_str(block, "mime_type"),
    ]);

    // 1. Nested input_audio object (standard OpenAI Responses schema)
    let mut audio_data = audio_obj.g("data").str();

    // 2. Flat data
    if audio_data.is_empty() {
        audio_data = opt_str(block, "data");
    }

    // 3. audio_url / url
    if audio_data.is_empty() {
        let audio_url = first_non_empty(&[opt_str(block, "audio_url.url"), opt_str(block, "audio_url"), opt_str(block, "url")]);
        if !audio_url.is_empty() {
            if is_data_url(&audio_url) {
                return match parse_openai_responses_data_url(&audio_url) {
                    Some((m_type, d)) if !d.is_empty() => {
                        Some((resolve_generic_data_url_mime(m_type, &audio_format, &filename, open_ai_responses_audio_mime_type, "audio/wav"), d))
                    }
                    _ => None,
                };
            } else if !is_remote_url(&audio_url) {
                let mut m_type = open_ai_responses_audio_mime_type(&audio_format);
                if is_generic_mime(&audio_format) && !filename.is_empty() {
                    m_type = open_ai_responses_audio_mime_type(&ext_of(&filename));
                }
                return Some((m_type, audio_url));
            }
        }
    }

    // 4. Source object (base64)
    if audio_data.is_empty() && block.g("source.type").str() == "base64" {
        audio_data = opt_str(block, "source.data");
        if audio_format.is_empty() {
            audio_format = opt_str(block, "source.media_type");
        }
    }

    if audio_data.is_empty() {
        return None;
    }

    if is_data_url(&audio_data) {
        return match parse_openai_responses_data_url(&audio_data) {
            Some((m_type, d)) if !d.is_empty() => {
                Some((resolve_generic_data_url_mime(m_type, &audio_format, &filename, open_ai_responses_audio_mime_type, "audio/wav"), d))
            }
            _ => None,
        };
    }

    let mut m_type = open_ai_responses_audio_mime_type(&audio_format);
    if is_generic_mime(&audio_format) && !filename.is_empty() {
        m_type = open_ai_responses_audio_mime_type(&ext_of(&filename));
    }
    Some((m_type, audio_data))
}

fn open_ai_responses_video_from_block(block: &Res<'_>) -> Option<(String, String)> {
    let b_type = block.g("type").str().trim().to_lowercase();
    if b_type != "input_video" && b_type != "video_url" && b_type != "video" {
        return None;
    }

    let filename = first_non_empty(&[opt_str(block, "filename"), opt_str(block, "file.filename")]);
    let mut video_obj = block.g("input_video");
    if !video_obj.exists() {
        video_obj = block.g("video");
    }
    let mut format = first_non_generic_format(&[
        video_obj.g("format").str(),
        video_obj.g("mime_type").str(),
        opt_str(block, "format"),
        opt_str(block, "mime_type"),
    ]);

    // 1. video_url (string or { "url": "..." }) or url
    let video_url = first_non_empty(&[opt_str(block, "video_url.url"), opt_str(block, "video_url"), opt_str(block, "url")]);
    if !video_url.is_empty() {
        if is_data_url(&video_url) {
            return match parse_openai_responses_data_url(&video_url) {
                Some((m_type, d)) if !d.is_empty() => {
                    Some((resolve_generic_data_url_mime(m_type, &format, &filename, open_ai_responses_video_mime_type, "video/mp4"), d))
                }
                _ => None,
            };
        } else if !is_remote_url(&video_url) {
            let mut m_type = open_ai_responses_video_mime_type(&format);
            if is_generic_mime(&format) && !filename.is_empty() {
                m_type = open_ai_responses_video_mime_type(&ext_of(&filename));
            }
            return Some((m_type, video_url));
        }
    }

    // 2. Nested input_video or video object data
    let mut video_data = video_obj.g("data").str();

    // 3. Flat data
    if video_data.is_empty() {
        video_data = opt_str(block, "data");
    }

    // 4. Source object (base64)
    if video_data.is_empty() && block.g("source.type").str() == "base64" {
        video_data = opt_str(block, "source.data");
        if format.is_empty() {
            format = opt_str(block, "source.media_type");
        }
    }

    if video_data.is_empty() {
        return None;
    }

    if is_data_url(&video_data) {
        return match parse_openai_responses_data_url(&video_data) {
            Some((m_type, d)) if !d.is_empty() => {
                Some((resolve_generic_data_url_mime(m_type, &format, &filename, open_ai_responses_video_mime_type, "video/mp4"), d))
            }
            _ => None,
        };
    }

    let mut m_type = open_ai_responses_video_mime_type(&format);
    if is_generic_mime(&format) && !filename.is_empty() {
        m_type = open_ai_responses_video_mime_type(&ext_of(&filename));
    }
    Some((m_type, video_data))
}

/// A MIME type from a short format name or extension; empty when unknown or generic.
fn normalize_format_to_mime(format: &str) -> String {
    let format = format.trim();
    if format.is_empty() || is_generic_mime(format) {
        return String::new();
    }
    if format.contains('/') {
        return format.to_string();
    }
    let f_lower = format.to_lowercase();
    match f_lower.as_str() {
        "jpg" | "jpeg" => return "image/jpeg".to_string(),
        "wav" => return "audio/wav".to_string(),
        "mp3" => return "audio/mpeg".to_string(),
        "mp4" => return "video/mp4".to_string(),
        "webm" => return "video/webm".to_string(),
        "pdf" => return "application/pdf".to_string(),
        _ => {}
    }
    mime_type_for_extension(&f_lower).unwrap_or_default().to_string()
}

fn open_ai_responses_file_from_block(block: &Res<'_>) -> Option<(String, String)> {
    let b_type = block.g("type").str().trim().to_lowercase();
    if b_type != "input_file" && b_type != "file" {
        return None;
    }

    let filename = first_non_empty(&[opt_str(block, "filename"), opt_str(block, "file.filename")]);
    let mut file_data = first_non_empty(&[opt_str(block, "file_data"), opt_str(block, "file.file_data"), opt_str(block, "data")]);
    if file_data.is_empty() {
        let file_url = first_non_empty(&[
            opt_str(block, "file_url.url"),
            opt_str(block, "file_url"),
            opt_str(block, "file.file_url"),
            opt_str(block, "url"),
        ]);
        if is_data_url(&file_url) {
            file_data = file_url;
        }
    }

    let file_obj = block.g("file");
    let mut fallback_mime = first_non_generic_format(&[
        opt_str(block, "mime_type"),
        file_obj.g("mime_type").str(),
        opt_str(block, "format"),
        file_obj.g("format").str(),
    ]);
    if !fallback_mime.is_empty() {
        fallback_mime = normalize_format_to_mime(&fallback_mime);
    }
    if is_generic_mime(&fallback_mime) && !filename.is_empty() {
        let ext = lower_ext_of(&filename);
        if !ext.is_empty() {
            fallback_mime = normalize_format_to_mime(&ext);
        }
    }

    if is_data_url(&file_data) {
        return match parse_openai_responses_data_url(&file_data) {
            Some((mut m_type, d)) if !d.is_empty() => {
                if is_generic_mime(&m_type) && !fallback_mime.is_empty() {
                    m_type = fallback_mime;
                }
                if is_generic_mime(&m_type) && !filename.is_empty() {
                    let ext = lower_ext_of(&filename);
                    if !ext.is_empty() {
                        let norm = normalize_format_to_mime(&ext);
                        if !norm.is_empty() {
                            m_type = norm;
                        }
                    }
                }
                if is_generic_mime(&m_type) {
                    m_type = "application/octet-stream".to_string();
                }
                Some((m_type, d))
            }
            _ => None,
        };
    }

    normalize_openai_file_data(&filename, &fallback_mime, &file_data)
}

/// MIME type and payload of any media block (image, audio, video, file), `None` when the block
/// is not media or carries no usable inline data.
pub(super) fn open_ai_responses_media_from_block(block: &Res<'_>) -> Option<(String, String)> {
    open_ai_responses_image_from_block(block)
        .or_else(|| open_ai_responses_audio_from_block(block))
        .or_else(|| open_ai_responses_video_from_block(block))
        .or_else(|| open_ai_responses_file_from_block(block))
}

/// A Gemini part for a content block: remote URLs become `file_data`, inline or data-URL media
/// becomes `inline_data`.
pub(super) fn open_ai_responses_part_from_block(block: &Res<'_>) -> Option<Value> {
    let b_type = block.g("type").str().trim().to_lowercase();

    // 1. Remote URLs (http://, https://, gs://)
    let raw_url = first_non_empty(&[
        opt_str(block, "video_url.url"),
        opt_str(block, "video_url"),
        opt_str(block, "audio_url.url"),
        opt_str(block, "audio_url"),
        opt_str(block, "image_url.url"),
        opt_str(block, "image_url"),
        opt_str(block, "file_url.url"),
        opt_str(block, "file_url"),
        opt_str(block, "file.file_url"),
        opt_str(block, "url"),
    ]);
    if is_remote_url(&raw_url) {
        let mut filename = first_non_empty(&[opt_str(block, "filename"), opt_str(block, "file.filename")]);
        if filename.is_empty() {
            if let Some(path) = url_path(&raw_url) {
                filename = path_base(&path);
            }
        }
        let format = first_non_generic_format(
            &[
                "format",
                "mime_type",
                "input_video.format",
                "input_video.mime_type",
                "video.format",
                "video.mime_type",
                "input_audio.format",
                "input_audio.mime_type",
                "audio.format",
                "audio.mime_type",
                "input_image.format",
                "input_image.mime_type",
                "image.format",
                "image.mime_type",
                "file.format",
                "file.mime_type",
            ]
            .map(|p| opt_str(block, p)),
        );
        let mime_type = match b_type.as_str() {
            "input_video" | "video_url" | "video" => {
                let mut m = String::new();
                if !format.is_empty() && !is_generic_mime(&format) {
                    m = open_ai_responses_video_mime_type(&format);
                } else if !filename.is_empty() {
                    let ext = lower_ext_of(&filename);
                    if !ext.is_empty() {
                        m = open_ai_responses_video_mime_type(&ext);
                    }
                }
                if is_generic_mime(&m) {
                    m = "video/mp4".to_string();
                }
                m
            }
            "input_audio" | "audio" => {
                let mut m = String::new();
                if !format.is_empty() && !is_generic_mime(&format) {
                    m = open_ai_responses_audio_mime_type(&format);
                } else if !filename.is_empty() {
                    let ext = lower_ext_of(&filename);
                    if !ext.is_empty() {
                        m = open_ai_responses_audio_mime_type(&ext);
                    }
                }
                if is_generic_mime(&m) {
                    m = "audio/wav".to_string();
                }
                m
            }
            "input_image" | "image_url" | "image" => open_ai_responses_image_mime_type(&format, &filename),
            _ => {
                let mut m = String::new();
                if !format.is_empty() {
                    m = normalize_format_to_mime(&format);
                }
                if is_generic_mime(&m) && !filename.is_empty() {
                    let ext = lower_ext_of(&filename);
                    if !ext.is_empty() {
                        m = normalize_format_to_mime(&ext);
                    }
                }
                if is_generic_mime(&m) {
                    m = "application/octet-stream".to_string();
                }
                m
            }
        };
        return Some(gemini_responses_file_data_part(&mime_type, &raw_url));
    }

    // 2. Inline base64 or data URL
    open_ai_responses_media_from_block(block).map(|(mime_type, data)| gemini_responses_inline_data_part(&mime_type, &data))
}

fn open_ai_responses_image_mime_type(format: &str, filename: &str) -> String {
    let format = format.trim();
    if !format.is_empty() && !is_generic_mime(format) {
        if format.contains('/') {
            return format.to_string();
        }
        let f_lower = format.to_lowercase();
        if f_lower == "jpg" || f_lower == "jpeg" {
            return "image/jpeg".to_string();
        }
        if let Some(mapped) = mime_type_for_extension(&f_lower) {
            return mapped.to_string();
        }
        return format!("image/{format}");
    }
    if !filename.is_empty() {
        let ext = lower_ext_of(filename);
        if ext == "jpg" || ext == "jpeg" {
            return "image/jpeg".to_string();
        }
        if !ext.is_empty() {
            if let Some(mapped) = mime_type_for_extension(&ext) {
                return mapped.to_string();
            }
        }
    }
    "image/png".to_string()
}

fn open_ai_responses_image_from_block(block: &Res<'_>) -> Option<(String, String)> {
    let block_type = block.g("type").str().trim().to_lowercase();
    if !matches!(block_type.as_str(), "input_image" | "image_url" | "image") {
        return None;
    }
    let mut format = first_non_generic_format(&[
        opt_str(block, "format"),
        opt_str(block, "mime_type"),
        opt_str(block, "input_image.format"),
        opt_str(block, "input_image.mime_type"),
        opt_str(block, "image.format"),
        opt_str(block, "image.mime_type"),
    ]);
    let filename = first_non_empty(&[opt_str(block, "filename"), opt_str(block, "file.filename")]);

    // 1. image_url
    let image_url = first_non_empty(&[opt_str(block, "image_url.url"), opt_str(block, "image_url"), opt_str(block, "url")]);
    if !image_url.is_empty() {
        if is_data_url(&image_url) {
            return match parse_openai_responses_data_url(&image_url) {
                Some((mut m_type, d)) if !d.is_empty() => {
                    if is_generic_mime(&m_type) {
                        m_type = open_ai_responses_image_mime_type(&format, &filename);
                    }
                    Some((m_type, d))
                }
                _ => None,
            };
        } else if !is_remote_url(&image_url) {
            return Some((open_ai_responses_image_mime_type(&format, &filename), image_url));
        }
    }

    // 2. source object (base64)
    let mut image_data = String::new();
    if block.g("source.type").str() == "base64" {
        image_data = opt_str(block, "source.data");
        if format.is_empty() {
            format = opt_str(block, "source.media_type");
        }
    }

    // 3. direct data
    if image_data.is_empty() && block.g("data").exists() {
        image_data = opt_str(block, "data");
    }

    if image_data.is_empty() {
        return None;
    }

    if is_data_url(&image_data) {
        return match parse_openai_responses_data_url(&image_data) {
            Some((mut m_type, d)) if !d.is_empty() => {
                if is_generic_mime(&m_type) {
                    m_type = open_ai_responses_image_mime_type(&format, &filename);
                }
                Some((m_type, d))
            }
            _ => None,
        };
    }

    Some((open_ai_responses_image_mime_type(&format, &filename), image_data))
}

struct OutputBlock {
    text: String,
    is_text: bool,
    raw: String,
}

/// Flattens an array tool output: media blocks become inline parts (second value of the result
/// tuple's images); text blocks collapse to a string, anything else stays raw JSON.
/// Returns (result, is_raw_json, media parts).
pub(super) fn parse_open_ai_responses_array_output(output_result: &Res<'_>, raws: &RawTexts<'_>) -> (String, bool, Vec<Value>) {
    let mut image_parts: Vec<Value> = Vec::new();
    let mut non_image_entries: Vec<OutputBlock> = Vec::new();
    let mut has_content_block = false;
    let mut has_non_text_block = false;

    for block in output_result.array() {
        if let Some((mime_type, data)) = open_ai_responses_media_from_block(&block) {
            has_content_block = true;
            image_parts.push(gemini_responses_inline_data_part(&mime_type, &data));
            continue;
        }
        let b_type = block.g("type").str();
        if b_type == "input_text" || b_type == "output_text" || b_type == "text" {
            has_content_block = true;
            non_image_entries.push(OutputBlock { text: block.g("text").str(), is_text: true, raw: raws.restore(&block) });
        } else if block.is_string() {
            non_image_entries.push(OutputBlock { text: block.str(), is_text: true, raw: raws.restore(&block) });
        } else {
            has_non_text_block = true;
            non_image_entries.push(OutputBlock { text: raws.restore(&block), is_text: false, raw: raws.restore(&block) });
        }
    }

    if !has_content_block {
        return (raws.restore(output_result), true, Vec::new());
    }

    match non_image_entries.len() {
        0 => (String::new(), false, image_parts),
        1 => {
            let e = non_image_entries.remove(0);
            if e.is_text { (e.text, false, image_parts) } else { (e.raw, true, image_parts) }
        }
        _ => {
            if !has_non_text_block {
                let texts: Vec<&str> = non_image_entries.iter().map(|e| e.text.as_str()).collect();
                return (texts.join("\n"), false, image_parts);
            }
            let raws: Vec<&str> = non_image_entries.iter().map(|e| e.raw.as_str()).collect();
            (format!("[{}]", raws.join(",")), true, image_parts)
        }
    }
}

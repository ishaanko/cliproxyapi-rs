//! Handler-level source type shared by the OpenAI-compatible and xAI executors.

/// Metadata key naming a handler-level source type (`openai-image`, `openai-video`) that the
/// translator `Format` enum cannot express. Go carries these as `SourceFormat` strings; callers
/// that route image or video requests set this key.
pub const META_HANDLER_TYPE: &str = "handler_type";

/// Go: `opts.SourceFormat.String()` including the handler-level types.
pub fn source_handler_type(opts: &cpa_runtime::executor::Options) -> String {
    match opts.metadata.get(META_HANDLER_TYPE).and_then(serde_json::Value::as_str) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => opts.source_format.as_str().to_string(),
    }
}

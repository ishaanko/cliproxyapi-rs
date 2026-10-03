//! Codex `apply_patch` bridge helpers for non-native executors (Go: helps/apply_patch.go).
//!
//! Translators carry the bridge state in the stream's [`Param`]; these helpers read its error
//! state, finalize a stream that ended early, and publish/forward the sanitized gateway error.
//! `upstream` / `client` follow `translate_stream(upstream, client, ..)`: the format the
//! response comes from and the format delivered to the client.

use bytes::Bytes;
use cpa_core::util::responses_tool_reverse_identity_map;
use cpa_runtime::executor::{ExecError, Options, Request};
use cpa_translator::{Ctx, Format, Param};
use tokio::sync::mpsc;

use super::usage::UsageReporter;

/// Deliberately excludes upstream JSON and patch text.
pub const APPLY_PATCH_UPSTREAM_ERROR_MESSAGE: &str = "Invalid apply_patch tool arguments received from upstream.";

/// The sanitized 502 every executor reports for a failed apply_patch bridge (Go: the per-executor
/// `statusErr{code: 502, msg: ApplyPatchUpstreamErrorMessage}`).
pub fn gateway_error() -> ExecError {
    ExecError::new(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)
}

/// Output side of a stream result (the sender half of `StreamResult::chunks`).
pub type ChunkSender = mpsc::Sender<Result<Bytes, ExecError>>;

/// The original conversion error recorded by the translator, if any.
pub fn apply_patch_translation_error(param: &Param) -> Option<&str> {
    param.tool_input_error.as_deref()
}

/// Lets the translator state fail a patch-enabled stream whose transport ended without the
/// source terminator; returns the failure frames to deliver (never a success terminator).
/// Safe to call for any pair: only the translator that owns the state reacts.
pub fn finalize_apply_patch_stream(param: &mut Param) -> Vec<Vec<u8>> {
    use cpa_translator::Format;
    // Go type-asserts the translator state; here each registered finalizer only reacts to its
    // own state type, so trying every Responses pair is equivalent.
    for upstream in [Format::OpenAI, Format::Claude, Format::Gemini, Format::Antigravity, Format::Interactions] {
        let frames = cpa_translator::global().finalize_stream(upstream, Format::OpenAIResponse, param);
        if !frames.is_empty() {
            return frames;
        }
    }
    Vec::new()
}

/// Publishes the failure when the translator recorded a tool input error, before delivery can
/// be canceled. Returns whether a failure was recorded.
pub fn record_apply_patch_stream_failure(param: &Param, reporter: &UsageReporter, gateway_err: &ExecError) -> bool {
    if apply_patch_translation_error(param).is_none() {
        return false;
    }
    reporter.publish_failure(gateway_err);
    true
}

/// The sanitized gateway error when the translator retained an apply_patch failure (published
/// first), for callers that deliver it themselves.
pub fn patch_failure(param: &Param, reporter: &UsageReporter) -> Option<ExecError> {
    let err = gateway_error();
    record_apply_patch_stream_failure(param, reporter, &err).then_some(err)
}

/// Propagates a retained failure after its one translated frame: records it and sends the
/// sanitized gateway error. Returns whether the stream failed. A closed receiver (client gone)
/// ends delivery silently.
pub async fn stop_apply_patch_stream(
    param: &Param,
    reporter: &UsageReporter,
    out: &ChunkSender,
    gateway_err: ExecError,
) -> bool {
    if !record_apply_patch_stream_failure(param, reporter, &gateway_err) {
        return false;
    }
    let _ = out.send(Err(gateway_err)).await;
    true
}

/// Checks EOF before any synthetic success or usage publication: sends the finalize frames,
/// then the gateway error if the stream failed. Returns true when the caller must stop (failure
/// or the client went away).
pub async fn end_apply_patch_stream(
    param: &mut Param,
    reporter: &UsageReporter,
    out: &ChunkSender,
    gateway_err: ExecError,
) -> bool {
    let chunks = finalize_apply_patch_stream(param);
    record_apply_patch_stream_failure(param, reporter, &gateway_err);
    for chunk in chunks {
        if out.send(Ok(Bytes::from(chunk))).await.is_err() {
            return true;
        }
    }
    stop_apply_patch_stream(param, reporter, out, gateway_err).await
}

/// Whether an original winning custom declaration is `apply_patch`.
pub fn apply_patch_requested(original: &[u8]) -> bool {
    responses_tool_reverse_identity_map(original).values().any(|identity| identity.apply_patch)
}

/// Whether `name` is the upstream name of the original request's `apply_patch` declaration.
pub fn is_apply_patch_upstream_tool(original: &[u8], name: &str) -> bool {
    responses_tool_reverse_identity_map(original).get(name).is_some_and(|i| i.apply_patch)
}

/// Prepares canonical translator state even for an empty EOF. An empty source has no progress
/// events and does not opt native Codex into bridging.
pub fn initialize_apply_patch_stream(
    upstream: Format,
    client: Format,
    model: &str,
    original: &[u8],
    effective: &[u8],
    param: &mut Param,
) {
    if client != Format::OpenAIResponse || !apply_patch_requested(original) {
        return;
    }
    let _ = cpa_translator::translate_stream(&Ctx::default(), upstream, client, model, original, effective, &[], param);
}

/// The request used to resolve source declarations: `opts.original_request`, else the payload.
pub fn apply_patch_original_request(req: &Request, opts: &Options) -> Bytes {
    if opts.original_request.is_empty() { req.payload.clone() } else { opts.original_request.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUSTOM_PATCH: &str = r#"{"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"x"}}]}"#;

    #[test]
    fn detects_original_apply_patch_declarations() {
        assert!(apply_patch_requested(CUSTOM_PATCH.as_bytes()));
        assert!(!apply_patch_requested(br#"{"tools":[{"type":"function","name":"apply_patch","parameters":{}}]}"#));
        assert!(!apply_patch_requested(b"not json"));
    }

    /// The helpers run inside spawned stream tasks, so their futures must be `Send`.
    #[test]
    fn helper_futures_are_send() {
        fn send<T: Send>(_: T) {}
        let reporter = UsageReporter::new("kimi", "KimiExecutor", "m", None, None);
        let (tx, _rx) = mpsc::channel(1);
        let mut param = Param::default();
        send(end_apply_patch_stream(&mut param, &reporter, &tx, gateway_error()));
        send(stop_apply_patch_stream(&param, &reporter, &tx, gateway_error()));
    }

    #[tokio::test]
    async fn failure_is_published_and_forwarded_once_state_has_error() {
        let reporter = UsageReporter::new("kimi", "KimiExecutor", "m", None, None);
        let (tx, mut rx) = mpsc::channel(4);
        let mut param = Param::default();
        let gateway = || ExecError::new(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE);
        // No translator error: nothing happens.
        assert!(!end_apply_patch_stream(&mut param, &reporter, &tx, gateway()).await);
        assert!(reporter.record().is_none());
        param.tool_input_error = Some("bad patch".into());
        assert!(end_apply_patch_stream(&mut param, &reporter, &tx, gateway()).await);
        assert_eq!(rx.recv().await.unwrap().unwrap_err().status, 502);
        let r = reporter.record().unwrap();
        assert!(r.failed && r.fail.status_code == 502);
    }
}

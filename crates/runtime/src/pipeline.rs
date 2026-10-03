//! Execution pipeline context and hooks (Go: sdk/cliproxy/pipeline/context.go).
//!
//! SDK-facing contract only: nothing in the proxy path constructs these yet, matching Go, where
//! the package is consumed by embedders. [`Context`] carries the state shared between middleware,
//! translators and executors for one execution; [`Hook`] observes it around the call.

use std::sync::Arc;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_translator::registry::Registry;

use crate::executor::{ExecError, Options, Request, Response};

/// One streamed chunk as hooks see it: payload bytes, or the terminal error.
pub type StreamChunk = Result<Bytes, ExecError>;

/// Execution state shared across middleware, translators and executors.
pub struct Context {
    /// The provider-facing request payload.
    pub request: Request,
    /// Execution flags (streaming, headers, ...).
    pub options: Options,
    /// The credential selected for execution.
    pub auth: Option<Arc<Auth>>,
    /// The translator registry schema adaptation runs against. (Go holds a `*Pipeline` that wraps
    /// a registry with request/response middleware; only the registry exists in this port.)
    pub translator: Option<&'static Registry>,
    /// Lets middleware customize the outbound transport per request.
    pub http_client: Option<reqwest::Client>,
}

/// Middleware callbacks around execution. All methods default to no-ops.
pub trait Hook: Send + Sync {
    fn before_execute(&self, _ctx: &mut Context) {}
    fn after_execute(&self, _ctx: &Context, _resp: &Response, _err: Option<&ExecError>) {}
    fn on_stream_chunk(&self, _ctx: &Context, _chunk: &StreamChunk) {}
}

type BeforeFn = Box<dyn Fn(&mut Context) + Send + Sync>;
type AfterFn = Box<dyn Fn(&Context, &Response, Option<&ExecError>) + Send + Sync>;
type StreamFn = Box<dyn Fn(&Context, &StreamChunk) + Send + Sync>;

/// Aggregates optional hook closures into a [`Hook`].
#[derive(Default)]
pub struct HookFunc {
    pub before: Option<BeforeFn>,
    pub after: Option<AfterFn>,
    pub stream: Option<StreamFn>,
}

impl Hook for HookFunc {
    fn before_execute(&self, ctx: &mut Context) {
        if let Some(f) = &self.before {
            f(ctx);
        }
    }

    fn after_execute(&self, ctx: &Context, resp: &Response, err: Option<&ExecError>) {
        if let Some(f) = &self.after {
            f(ctx, resp, err);
        }
    }

    fn on_stream_chunk(&self, ctx: &Context, chunk: &StreamChunk) {
        if let Some(f) = &self.stream {
            f(ctx, chunk);
        }
    }
}

/// Injects custom HTTP transports per auth entry (Go: `RoundTripperProvider`). The reqwest client
/// stands in for Go's `http.RoundTripper`.
pub trait RoundTripperProvider: Send + Sync {
    fn round_tripper_for(&self, auth: &Auth) -> Option<reqwest::Client>;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use cpa_core::format::Format;

    use super::*;

    fn context() -> Context {
        Context {
            request: Request { model: "m".into(), payload: Bytes::new(), format: Format::OpenAI, metadata: Default::default() },
            options: Options::new(Format::OpenAI),
            auth: None,
            translator: None,
            http_client: None,
        }
    }

    #[test]
    fn hook_func_calls_only_the_closures_that_are_set() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in = calls.clone();
        let hook = HookFunc {
            before: Some(Box::new(move |ctx| {
                calls_in.fetch_add(1, Ordering::SeqCst);
                ctx.request.model = "rewritten".into();
            })),
            ..Default::default()
        };
        let mut ctx = context();
        hook.before_execute(&mut ctx);
        hook.after_execute(&ctx, &Response::default(), None);
        hook.on_stream_chunk(&ctx, &Ok(Bytes::from_static(b"x")));
        assert_eq!((calls.load(Ordering::SeqCst), ctx.request.model.as_str()), (1, "rewritten"));
    }
}

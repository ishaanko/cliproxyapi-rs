//! Response body observer: forwards frames untouched, reports data chunks to a callback and
//! runs a completion callback exactly once when the body ends, errors or is dropped (client
//! disconnect). Used by the access log and the request-log writer.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes, HttpBody};
use http_body_util::BodyExt;

pub type OnChunk = Box<dyn FnMut(&Bytes) + Send>;
type OnDone = Box<dyn FnOnce() + Send>;

pub struct TeeBody {
    inner: Pin<Box<dyn HttpBody<Data = Bytes, Error = axum::Error> + Send>>,
    on_chunk: Option<OnChunk>,
    on_done: Option<OnDone>,
}

impl TeeBody {
    pub fn wrap(body: Body, on_chunk: Option<OnChunk>, on_done: OnDone) -> Body {
        Body::new(TeeBody {
            inner: Box::pin(body.map_err(axum::Error::new)),
            on_chunk,
            on_done: Some(on_done),
        })
    }

    fn finish(&mut self) {
        if let Some(done) = self.on_done.take() {
            done();
        }
    }
}

impl HttpBody for TeeBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = &mut *self;
        match this.inner.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(cb), Some(data)) = (this.on_chunk.as_mut(), frame.data_ref()) {
                    cb(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => {
                this.finish();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for TeeBody {
    fn drop(&mut self) {
        self.finish();
    }
}

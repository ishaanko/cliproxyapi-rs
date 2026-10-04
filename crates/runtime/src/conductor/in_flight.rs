//! Per-credential in-flight request counts for the `smart-quota` strategy (Rust-only).
//!
//! A pick under `smart-quota` takes an [`InFlightGuard`] for the chosen credential; the guard
//! travels with the attempt (and, for streams, with the chunk source) and releases the count when
//! dropped, so every exit path (error, failover, client hang-up) is covered.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::executor::{Chunk, ChunkRx, ChunkSource};

#[derive(Default)]
pub(crate) struct InFlight {
    counts: Mutex<HashMap<String, usize>>,
}

impl InFlight {
    /// Current number of requests running on `auth_id`.
    pub(crate) fn load(&self, auth_id: &str) -> usize {
        self.counts.lock().get(auth_id).copied().unwrap_or(0)
    }

    pub(crate) fn acquire(self: &Arc<Self>, auth_id: &str) -> InFlightGuard {
        *self.counts.lock().entry(auth_id.to_string()).or_insert(0) += 1;
        InFlightGuard {
            owner: self.clone(),
            auth_id: auth_id.to_string(),
        }
    }
}

/// Holds one in-flight slot of a credential until dropped.
pub(crate) struct InFlightGuard {
    owner: Arc<InFlight>,
    auth_id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut counts = self.owner.counts.lock();
        if let Some(n) = counts.get_mut(&self.auth_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.remove(&self.auth_id);
            }
        }
    }
}

/// A chunk source that keeps a guard alive for as long as the stream is held.
struct Guarded {
    inner: ChunkRx,
    _guard: InFlightGuard,
}

impl ChunkSource for Guarded {
    fn poll_chunk(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Chunk>> {
        self.inner.poll_recv(cx)
    }
}

/// Ties `guard` to the lifetime of `chunks`.
pub(crate) fn guard_stream(chunks: ChunkRx, guard: InFlightGuard) -> ChunkRx {
    ChunkRx::Source(Box::new(Guarded {
        inner: chunks,
        _guard: guard,
    }))
}

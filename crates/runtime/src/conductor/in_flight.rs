//! Per-credential in-flight request counts for the `smart-quota` strategy (Rust-only).
//!
//! A pick under `smart-quota` takes an [`InFlightGuard`] for the chosen credential; the guard
//! travels with the attempt (and, for streams, with the chunk source) and releases the count when
//! dropped, so every exit path (error, failover, client hang-up) is covered.
//!
//! The load read, the selection and the slot acquisition of one pick happen under a single lock
//! ([`InFlight::pick_and_acquire`]), so concurrent picks always see each other. Lock order is
//! state read lock, then `counts`, then the selector's rotation lock; dropping a guard takes
//! `counts` alone.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use super::quota_windows::Score;
use super::selector::Cand;
use crate::executor::{Chunk, ChunkRx, ChunkSource};

#[derive(Default)]
pub(crate) struct InFlight {
    counts: Mutex<HashMap<String, usize>>,
}

impl InFlight {
    /// Current number of requests running on `auth_id` (tests).
    #[cfg(test)]
    pub(crate) fn load(&self, auth_id: &str) -> usize {
        self.counts.lock().get(auth_id).copied().unwrap_or(0)
    }

    pub(crate) fn acquire(self: &Arc<Self>, auth_id: &str) -> InFlightGuard {
        let mut counts = self.counts.lock();
        self.acquire_locked(&mut counts, auth_id)
    }

    fn acquire_locked(
        self: &Arc<Self>,
        counts: &mut HashMap<String, usize>,
        auth_id: &str,
    ) -> InFlightGuard {
        *counts.entry(auth_id.to_string()).or_insert(0) += 1;
        InFlightGuard {
            owner: self.clone(),
            auth_id: auth_id.to_string(),
        }
    }

    /// Fills each candidate's load, runs `pick` (the selector) on them and takes a slot for the
    /// chosen one, all under the counts lock.
    pub(crate) fn pick_and_acquire<'a>(
        self: &Arc<Self>,
        cands: &mut [Cand<'a>],
        pick: impl FnOnce(&[Cand<'a>]) -> Option<usize>,
    ) -> (Option<usize>, Option<InFlightGuard>) {
        let mut counts = self.counts.lock();
        for c in cands.iter_mut() {
            let score = c.smart.unwrap_or(Score::UNKNOWN);
            c.smart = Some(Score {
                load: counts.get(c.id).copied().unwrap_or(0),
                ..score
            });
        }
        let idx = pick(cands);
        let guard = idx.map(|i| self.acquire_locked(&mut counts, cands[i].id));
        (idx, guard)
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

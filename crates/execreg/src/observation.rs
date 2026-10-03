//! In-flight execution snapshots (Go: sdk/cliproxy/executionregistry/observation.go).

use std::time::SystemTime;

use crate::registry::Registry;

/// An immutable in-flight execution snapshot entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub request_id: String,
    pub credential_id: String,
    pub model: String,
    pub request_kind: String,
    pub started_at: Option<SystemTime>,
    pub accounted: bool,
}

/// An immutable in-flight execution snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Freeze {
    pub revision: i64,
    pub barrier_revision: i64,
    pub executions: Vec<Observation>,
}

impl Registry {
    /// Records the latest Home observation barrier. Ignored unless it is positive and newer.
    pub fn observe_barrier(&self, revision: i64) {
        if revision <= 0 {
            return;
        }
        let mut core = self.core();
        if revision > core.observed_barrier {
            core.observed_barrier = revision;
            core.pending_barrier_sequence = core.next;
        }
    }

    /// Copies all active executions into an immutable snapshot. The observed barrier is published
    /// only once every dispatch that began before it was observed has been resolved.
    pub fn freeze_in_flight(&self, _now: SystemTime) -> Freeze {
        let mut core = self.core();
        if core.observed_barrier > core.published_barrier {
            let blocked = core.pending.iter().any(|sequence| *sequence <= core.pending_barrier_sequence);
            if !blocked {
                core.published_barrier = core.observed_barrier;
            }
        }
        core.snapshot_revision += 1;
        let mut scopes: Vec<_> = core.scopes.iter().collect();
        scopes.sort_by_key(|(id, _)| **id);
        Freeze {
            revision: core.snapshot_revision,
            barrier_revision: core.published_barrier,
            executions: scopes
                .into_iter()
                .map(|(_, scope)| {
                    let spec = scope.spec();
                    Observation {
                        request_id: spec.request_id.clone(),
                        credential_id: spec.credential_id.clone(),
                        model: spec.model.clone(),
                        request_kind: spec.kind.clone(),
                        started_at: spec.started_at,
                        accounted: spec.accounted,
                    }
                })
                .collect(),
        }
    }
}

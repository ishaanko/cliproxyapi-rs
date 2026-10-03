//! Execution registry for one Home subscriber lifetime (Go: sdk/cliproxy/executionregistry).
//!
//! A [`Registry`] hands out dispatch tokens ([`PendingDispatch`]) while it accepts traffic, turns a
//! token into an active [`Scope`] once the dispatch is resolved, and on [`Registry::drain`] or
//! [`Registry::close`] cancels every bound resource and waits for the owners to finish. Accounted
//! scopes report a cumulative release sequence per (credential, model) through a release sink, and
//! [`Registry::freeze_in_flight`] snapshots what is currently executing for Home observation.
//!
//! Go's `ctx` arguments become future cancellation: wrap `drain`, `wait_pending` and
//! [`ReleaseTicket::wait`] in `tokio::time::timeout` (or drop them) to bound the wait. State changes
//! made before the first await, such as entering the draining state, persist when a wait is dropped.

mod observation;
mod registry;
mod signal;

pub use observation::{Freeze, Observation};
pub use registry::{
    CloseFn, Error, PendingDispatch, Registry, ReleaseGroup, ReleaseSink, ReleaseTicket, Scope, ScopeSpec, State,
};
pub use signal::Signal;

#[cfg(test)]
mod tests;

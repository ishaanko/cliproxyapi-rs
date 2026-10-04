//! Usage of attempts whose client hung up (Go: a cancelled request context makes the executor's
//! reporter publish its failure itself; here the executor call is simply dropped).
//!
//! A [`DetachGuard`] watches the await points of an attempt. When the attempt's future is dropped
//! at one of them (the handler future dies with the client connection), the guard detaches the
//! attempt's [`UsageCollector`]: the failure the dropped reporter publishes, or one a still
//! running stream task publishes later, is recorded straight away instead of waiting for a
//! conductor that no longer exists. Auth state is not touched, only usage.

use std::future::Future;
use std::time::Instant;

use cpa_auth::Auth;

use super::Manager;
use super::cooldown::ExecResult;
use super::usage::UsageFacts;
use crate::usage_report::UsageCollector;

pub(crate) struct DetachGuard {
    armed: bool,
    manager: Manager,
    usage: UsageCollector,
    auth: Option<Auth>,
    /// Template of the attempt result the late records are built against.
    result: ExecResult,
    facts: UsageFacts,
    started: Instant,
}

impl DetachGuard {
    pub(crate) fn new(manager: Manager, usage: UsageCollector, auth: Option<Auth>, result: ExecResult, facts: UsageFacts, started: Instant) -> Self {
        Self { armed: false, manager, usage, auth, result, facts, started }
    }

    /// Awaits `fut` with the guard armed; dropping the future (and so this guard) while it is
    /// pending detaches the collector.
    pub(crate) async fn run<F: Future>(&mut self, fut: F) -> F::Output {
        self.armed = true;
        let out = fut.await;
        self.armed = false;
        out
    }
}

impl Drop for DetachGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (manager, auth, started) = (self.manager.clone(), self.auth.clone(), self.started);
        let (mut result, template) = (self.result.clone(), self.facts.clone());
        result.success = false;
        self.usage.detach(move |record| {
            let facts = UsageFacts { latency: started.elapsed(), reports: vec![record], ..template.clone() };
            manager.record_usage_only(&result, auth.as_ref(), facts);
        });
    }
}

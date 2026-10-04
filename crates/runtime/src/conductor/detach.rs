//! Usage of attempts whose client hung up (Go: a cancelled request context makes the executor's
//! reporter publish its failure itself; here the executor call is simply dropped).
//!
//! A [`DetachGuard`] watches the await points of an attempt. When the attempt's future is dropped
//! at one of them (the handler future dies with the client connection), the guard detaches the
//! attempt's [`UsageCollector`]: the failure the dropped reporter publishes, or one a still
//! running stream task publishes later, is recorded straight away instead of waiting for a
//! conductor that no longer exists. Local attempts touch only usage (Go returns the context
//! error before marking the result); a Home-dispatched attempt also reports its failed result.
//!
//! The guard owns the attempt's [`ExecResult`] and [`UsageFacts`] templates, so the conductor
//! builds them once before the call and takes them back after it; nothing is cloned for the
//! guard's sake.

use std::future::Future;
use std::time::Instant;

use cpa_auth::Auth;

use super::Manager;
use super::cooldown::ExecResult;
use super::errors::result_error_from_error;
use super::rules;
use super::usage::UsageFacts;
use crate::apilog::ApiLogHandle;
use crate::executor::ExecError;
use crate::usage_report::UsageCollector;

/// The result a detached collector's sink keeps. The sink lives inside the collector, so the
/// options it holds must not point back at the collector (a reference cycle that would leak the
/// attempt) nor keep a Home selection alive past the executor's call.
pub(crate) fn for_detached_sink(mut result: ExecResult) -> ExecResult {
    result.options.usage_collector = None;
    result.options.lifecycle = None;
    result
}

pub(crate) struct DetachGuard {
    armed: bool,
    manager: Manager,
    /// Collector of the attempt's reports; `None` when no usage is collected (token counting).
    usage: Option<UsageCollector>,
    /// Credential snapshot of a Home-dispatched attempt (local ones resolve the auth by id).
    home_auth: Option<Auth>,
    /// Report the failed result to Home when cancelled (unary Home attempts).
    report_home: bool,
    /// Source of the response headers the attempt received before it was cancelled.
    api_log: ApiLogHandle,
    result: Option<ExecResult>,
    facts: Option<UsageFacts>,
    started: Instant,
}

impl DetachGuard {
    pub(crate) fn new(
        manager: Manager,
        usage: Option<UsageCollector>,
        api_log: ApiLogHandle,
        result: ExecResult,
        facts: UsageFacts,
        started: Instant,
    ) -> Self {
        Self { armed: false, manager, usage, home_auth: None, report_home: false, api_log, result: Some(result), facts: Some(facts), started }
    }

    /// Marks a Home-dispatched attempt: records resolve the auth from `auth`, and with
    /// `report_result` a cancellation also reports the failed result to Home.
    pub(crate) fn home(mut self, auth: Auth, report_result: bool) -> Self {
        self.home_auth = Some(auth);
        self.report_home = report_result;
        self
    }

    /// The credential was refreshed mid-attempt: later records use the new options and id.
    pub(crate) fn refreshed(&mut self, auth: &Auth, opts: &crate::executor::Options) {
        if let Some(result) = self.result.as_mut() {
            result.auth_id.clone_from(&auth.id);
            result.options = opts.clone();
        }
    }

    /// Awaits `fut` with the guard armed; dropping the future (and so this guard) while it is
    /// pending detaches the collector.
    pub(crate) async fn run<F: Future>(&mut self, fut: F) -> F::Output {
        self.armed = true;
        let out = fut.await;
        self.armed = false;
        out
    }

    /// The attempt's result template, handed back for the conductor to complete.
    pub(crate) fn take_result(&mut self) -> ExecResult {
        self.result.take().expect("result template taken once")
    }

    /// The result template of a failed attempt.
    pub(crate) fn failure_result(&mut self, err: &ExecError, credential_scope: bool) -> ExecResult {
        let mut result = self.take_result();
        result.retry_after = err.retry_after;
        result.credential_scope = credential_scope;
        result.error = Some(result_error_from_error(err));
        result.response_headers = err.recorded_headers();
        result
    }

    /// Both templates (unary attempts), with the facts' latency and reports filled in.
    pub(crate) fn take_parts(&mut self) -> (ExecResult, UsageFacts) {
        let facts = self.facts();
        (self.take_result(), facts)
    }

    /// The usage facts of the attempt so far: latency since the call started and the reports
    /// published to the collector.
    pub(crate) fn facts(&mut self) -> UsageFacts {
        let mut facts = self.facts.clone().unwrap_or_default();
        facts.latency = self.started.elapsed();
        facts.reports = self.usage.as_ref().map(UsageCollector::take).unwrap_or_default();
        facts
    }
}

impl Drop for DetachGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(mut result) = self.result.take() else { return };
        let template = self.facts.take().unwrap_or_default();
        result.success = false;
        result.response_headers = self.api_log.response_headers();
        let auth = self.home_auth.take().or_else(|| self.manager.get(&result.auth_id));
        if self.report_home {
            // Go: the cancelled execution returns ctx.Err(), which the Home path reports as the
            // attempt's failed result.
            let err = ExecError::new(0, "context canceled");
            result.error = Some(result_error_from_error(&err));
            if let Some(auth) = &auth {
                let action = rules::match_action(auth, &err, &self.manager.cfg());
                rules::apply_action_to_result(action, &mut result);
            }
            self.manager.report_home_result(result.clone(), auth.as_ref(), None);
        }
        if let Some(usage) = self.usage.take() {
            let (manager, started) = (self.manager.clone(), self.started);
            let result = for_detached_sink(result);
            usage.detach(move |record| {
                let facts = UsageFacts { latency: started.elapsed(), reports: vec![record], ..template.clone() };
                manager.record_usage_only(&result, auth.as_ref(), facts);
            });
        }
    }
}

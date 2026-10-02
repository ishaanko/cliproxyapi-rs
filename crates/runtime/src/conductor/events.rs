//! Lifecycle hooks, result policy and failure events (Go: Hook/ResultPolicy in conductor.go,
//! error_events.go).

use chrono::{DateTime, Utc};
use cpa_auth::types::{Auth, QuotaState, Status};
use serde::Serialize;

use super::cooldown::ExecResult;
use super::errors::AuthErrorExt;

/// Observer of credential lifecycle and execution results. Implementations must be cheap and
/// non-blocking; they run inline on the request path.
pub trait Hook: Send + Sync {
    fn on_auth_registered(&self, _auth: &Auth) {}
    fn on_auth_updated(&self, _auth: &Auth) {}
    fn on_result(&self, _result: &ExecResult) {}
}

/// Inspects and may rewrite an execution result before cooldown state is mutated.
pub trait ResultPolicy: Send + Sync {
    fn apply_result_policy(&self, result: ExecResult) -> ExecResult;
}

impl<F: Fn(ExecResult) -> ExecResult + Send + Sync> ResultPolicy for F {
    fn apply_result_policy(&self, result: ExecResult) -> ExecResult {
        self(result)
    }
}

/// Receives the JSON payload of each failed attempt (the request-log/management error feed).
pub type ErrorEventSink = std::sync::Arc<dyn Fn(Vec<u8>) + Send + Sync>;

#[derive(Serialize)]
struct ErrorEvent {
    timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "String::is_empty")]
    provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    auth_id: String,
    auth_index: String,
    status_code: i32,
    body: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    code: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    retryable: bool,
    auth_status: AuthStatus,
}

#[derive(Serialize)]
struct AuthStatus {
    status: Status,
    #[serde(skip_serializing_if = "String::is_empty")]
    status_message: String,
    disabled: bool,
    unavailable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_retry_after: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota: Option<QuotaStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<ModelStatus>,
}

#[derive(Serialize)]
struct QuotaStatus {
    exceeded: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_recover_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "is_zero")]
    backoff_level: i32,
}

#[derive(Serialize)]
struct ModelStatus {
    name: String,
    status: Status,
    #[serde(skip_serializing_if = "String::is_empty")]
    status_message: String,
    unavailable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_retry_after: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota: Option<QuotaStatus>,
}

fn is_zero(v: &i32) -> bool {
    *v == 0
}

fn quota_status(q: &QuotaState) -> Option<QuotaStatus> {
    if !q.exceeded && q.reason.trim().is_empty() && q.next_recover_at.is_none() && q.backoff_level == 0 {
        return None;
    }
    Some(QuotaStatus {
        exceeded: q.exceeded,
        reason: q.reason.trim().to_string(),
        next_recover_at: q.next_recover_at,
        backoff_level: q.backoff_level,
    })
}

/// JSON event for a failed attempt, with the credential/model state after the failure was applied.
pub fn build_error_event_payload(result: &ExecResult, auth: &Auth, now: DateTime<Utc>) -> Option<Vec<u8>> {
    if result.success {
        return None;
    }
    let model = result.model.trim();
    let model_status = (!model.is_empty()).then(|| auth.model_states.get(model)).flatten().map(|s| ModelStatus {
        name: model.to_string(),
        status: s.status,
        status_message: s.status_message.trim().to_string(),
        unavailable: s.unavailable,
        next_retry_after: s.next_retry_after,
        quota: quota_status(&s.quota),
    });
    let (status_code, body, code, retryable) = match &result.error {
        Some(e) => {
            let body = if !e.message.trim().is_empty() {
                e.message.trim().to_string()
            } else if !e.go_string().trim().is_empty() {
                e.go_string().trim().to_string()
            } else {
                "request failed".to_string()
            };
            (if e.http_status > 0 { e.http_status } else { 500 }, body, e.code.trim().to_string(), e.retryable)
        }
        None => (500, "request failed".to_string(), String::new(), false),
    };
    let event = ErrorEvent {
        timestamp: now,
        provider: result.provider.trim().to_string(),
        model: model.to_string(),
        auth_id: result.auth_id.trim().to_string(),
        auth_index: auth.index.trim().to_string(),
        status_code,
        body,
        code,
        retryable,
        auth_status: AuthStatus {
            status: auth.status,
            status_message: auth.status_message.trim().to_string(),
            disabled: auth.disabled,
            unavailable: auth.unavailable,
            next_retry_after: auth.next_retry_after,
            quota: quota_status(&auth.quota),
            model: model_status,
        },
    };
    serde_json::to_vec(&event).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_auth::types::AuthError;

    #[test]
    fn error_event_carries_status_and_state() {
        let mut auth = Auth::new("a", "claude");
        auth.unavailable = true;
        let result = ExecResult {
            auth_id: "a".into(),
            provider: "claude".into(),
            model: "m".into(),
            route_model: "m".into(),
            success: false,
            retry_after: None,
            credential_scope: false,
            error: Some(AuthError { message: "boom".into(), http_status: 503, retryable: true, ..Default::default() }),
            options: crate::executor::Options::new(cpa_translator::Format::OpenAI),
            skip_quota_observation: false,
            response_headers: Default::default(),
        };
        let bytes = build_error_event_payload(&result, &auth, Utc::now()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status_code"], 503);
        assert_eq!(v["body"], "boom");
        assert_eq!(v["auth_status"]["unavailable"], true);
        let mut ok = result;
        ok.success = true;
        assert!(build_error_event_payload(&ok, &auth, Utc::now()).is_none());
    }
}

//! Auth manager / conductor (Go: sdk/cliproxy/auth). Selects a credential per request,
//! invokes the provider executor, classifies failures into cooldowns, and retries or fails
//! over across credentials.
//!
//! The public method signatures below are the contract the HTTP layer codes against; the
//! internals are filled in by the conductor port.

use std::sync::Arc;

use cpa_auth::Auth;

use crate::executor::{DynExecutor, ExecError, Options, Request, Response, StreamResult};

/// Shared handle used by HTTP handlers and the management API.
pub type SharedManager = Arc<Manager>;

#[derive(Default)]
pub struct Manager {}

impl Manager {
    /// Register (or replace) the executor for `executor.identifier()`.
    pub fn register_executor(&self, _executor: DynExecutor) {
        todo!("conductor port")
    }

    /// Non-streaming execution across the candidate `providers` (Go: Manager.Execute).
    pub async fn execute(&self, _providers: &[String], _req: Request, _opts: Options) -> Result<Response, ExecError> {
        todo!("conductor port")
    }

    /// Streaming execution; failover is only possible before the first chunk.
    pub async fn execute_stream(&self, _providers: &[String], _req: Request, _opts: Options) -> Result<StreamResult, ExecError> {
        todo!("conductor port")
    }

    /// Token counting (Go: Manager.ExecuteCount).
    pub async fn execute_count(&self, _providers: &[String], _req: Request, _opts: Options) -> Result<Response, ExecError> {
        todo!("conductor port")
    }

    /// Insert or update a credential (Go: Manager.Register/Update).
    pub async fn update(&self, _auth: Auth) -> Result<Auth, ExecError> {
        todo!("conductor port")
    }

    /// Remove a credential by id.
    pub async fn remove(&self, _id: &str) {
        todo!("conductor port")
    }

    /// Snapshot of all credentials with live status/cooldown state.
    pub fn list(&self) -> Vec<Auth> {
        todo!("conductor port")
    }

    pub fn get(&self, _id: &str) -> Option<Auth> {
        todo!("conductor port")
    }

    /// Go: Manager.SupportsApplyPatchForProviders.
    pub fn supports_apply_patch_for_providers(&self, _providers: &[String], _model: &str) -> bool {
        todo!("conductor port")
    }
}

use std::{future::Future, pin::Pin};

use super::profile::ProfileExecution;
use crate::session::model::ModelError;
use crate::session::model::{ModelOutcome, ModelRequest};

pub trait ModelPort: Send + Sync {
    fn complete<'a>(
        &'a self,
        request: &'a ModelRequest,
        execution: ProfileExecution,
    ) -> Pin<Box<dyn Future<Output = Result<ModelOutcome, ModelError>> + Send + 'a>>;

    /// Release connection-local provider state after the session runner leaves
    /// its current activation. Durable conversation state remains in events.
    fn release_session(&self, _session_id: &str) {}
}

pub use ModelPort as ModelExecutor;

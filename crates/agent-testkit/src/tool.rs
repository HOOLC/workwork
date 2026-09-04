use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;
use zork_agent::session::events::ToolOutcome;
use zork_agent::session::tools::{
    NoToolState, ToolCompatibility, ToolContext, ToolContract, ToolDefinitionError, ToolExecution,
    ToolImplementation, ToolInstance, ToolRegistry,
};

pub struct ControlledTool {
    requests: tokio::sync::mpsc::Receiver<CapturedToolRequest>,
}

struct ControlledToolImplementation {
    requests: tokio::sync::mpsc::Sender<CapturedToolRequest>,
}

struct CapturedToolRequest {
    request: PendingToolRequest,
}

pub struct PendingToolRequest {
    pub context: ToolContext,
    pub arguments: Value,
    response: tokio::sync::oneshot::Sender<ToolExecution>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the tool request is no longer waiting for a response")]
pub struct ToolRequestClosed;

impl ControlledTool {
    pub fn register(
        registry: &Arc<ToolRegistry>,
        contract: ToolContract,
        capacity: usize,
    ) -> Result<Self, ToolDefinitionError> {
        Self::register_with_compatibility(registry, contract, Arc::new(NoToolState), capacity)
    }

    pub fn register_with_compatibility(
        registry: &Arc<ToolRegistry>,
        contract: ToolContract,
        compatibility: Arc<dyn ToolCompatibility>,
        capacity: usize,
    ) -> Result<Self, ToolDefinitionError> {
        let (sender, requests) = tokio::sync::mpsc::channel(capacity.max(1));
        registry.register(Arc::new(ToolInstance::new(
            contract,
            Arc::new(ControlledToolImplementation { requests: sender }),
            compatibility,
        )?));
        Ok(Self { requests })
    }

    pub async fn request(&mut self) -> PendingToolRequest {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.requests.recv())
            .await
            .expect("timed out waiting for the expected tool request")
            .expect("zork-agent stopped before issuing the expected tool request")
            .request
    }
}

impl PendingToolRequest {
    pub fn respond(self, result: ToolExecution) -> Result<(), ToolRequestClosed> {
        self.response.send(result).map_err(|_| ToolRequestClosed)
    }

    pub fn succeed(self, data: Value) -> Result<(), ToolRequestClosed> {
        self.respond(ToolExecution {
            outcome: ToolOutcome::Succeeded,
            data,
            result_schema_version: 1,
            knowledge: None,
        })
    }

    pub fn fail(self, message: impl Into<String>) -> Result<(), ToolRequestClosed> {
        self.respond(ToolExecution {
            outcome: ToolOutcome::Failed,
            data: serde_json::json!({"error": message.into()}),
            result_schema_version: 1,
            knowledge: None,
        })
    }
}

impl ToolImplementation for ControlledToolImplementation {
    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: &'a Value,
    ) -> Pin<Box<dyn std::future::Future<Output = ToolExecution> + Send + 'a>> {
        let requests = self.requests.clone();
        let context = context.clone();
        let arguments = arguments.clone();
        Box::pin(async move {
            let (response, received) = tokio::sync::oneshot::channel();
            if requests
                .send(CapturedToolRequest {
                    request: PendingToolRequest {
                        context,
                        arguments,
                        response,
                    },
                })
                .await
                .is_err()
            {
                return failed_execution("the controlled tool has no test controller");
            }
            received
                .await
                .unwrap_or_else(|_| failed_execution("the controlled tool response was dropped"))
        })
    }
}

fn failed_execution(message: impl Into<String>) -> ToolExecution {
    ToolExecution {
        outcome: ToolOutcome::Failed,
        data: serde_json::json!({"error": message.into()}),
        result_schema_version: 1,
        knowledge: None,
    }
}

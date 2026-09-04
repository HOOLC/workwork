//! Supervised logical-tool execution.

use std::collections::{BTreeSet, HashMap};
use std::future::pending;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::events::{ToolInvocation, ToolOutcome};
use super::ports::{Clock, SystemClock};
use super::tools::{ToolContext, ToolExecution, ToolKnowledge, ToolRegistry, ToolResolution};

/// Resolves immutable tool instances, validates calls, contains panics and
/// exposes only normal [`ToolExecution`] outcomes to the runner.
pub struct ToolExecutor {
    registry: Arc<ToolRegistry>,
    clock: Arc<dyn Clock>,
    timeout: Duration,
    permits: Arc<tokio::sync::Semaphore>,
    live: ExecutorTable,
}

impl ToolExecutor {
    pub fn new(registry: Arc<ToolRegistry>, timeout: Duration) -> Self {
        Self::with_concurrency(registry, timeout, 64)
    }

    pub fn with_concurrency(
        registry: Arc<ToolRegistry>,
        timeout: Duration,
        max_concurrency: usize,
    ) -> Self {
        Self::with_clock(registry, Arc::new(SystemClock), timeout, max_concurrency)
    }

    pub fn with_clock(
        registry: Arc<ToolRegistry>,
        clock: Arc<dyn Clock>,
        timeout: Duration,
        max_concurrency: usize,
    ) -> Self {
        Self {
            registry,
            clock,
            timeout,
            permits: Arc::new(tokio::sync::Semaphore::new(max_concurrency.max(1))),
            live: ExecutorTable::default(),
        }
    }

    pub async fn execute(&self, context: ToolContext, invocation: ToolInvocation) -> ToolExecution {
        self.execute_supervised(context, invocation, None).await
    }

    async fn execute_supervised(
        &self,
        context: ToolContext,
        invocation: ToolInvocation,
        cancellation: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> ToolExecution {
        if let Some(reason) = &invocation.rejection {
            return rejected_execution(reason);
        }

        let mut cancelled = Box::pin(async move {
            match cancellation {
                Some(receiver) => {
                    let _ = receiver.await;
                }
                None => pending::<()>().await,
            }
        });

        let permit = tokio::select! {
            _ = &mut cancelled => return cancelled_execution(),
            permit = self.permits.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return failed_execution("Tool executor is shutting down.".into(), None),
            }
        };

        let instance = match self
            .registry
            .resolve(&invocation.tool, invocation.tool_version.as_ref())
        {
            ToolResolution::Ready(instance) => instance,
            ToolResolution::VersionChanged { current } => {
                return failed_execution(
                    format!(
                        "Tool {} has changed. Retry the call or use tool.help if its current usage is needed.",
                        invocation.tool
                    ),
                    Some(ToolKnowledge::Current {
                        name: invocation.tool,
                        version: current,
                    }),
                );
            }
            ToolResolution::Unavailable => {
                return failed_execution(
                    format!("Tool {} is unavailable.", invocation.tool),
                    Some(ToolKnowledge::Removed {
                        name: invocation.tool,
                    }),
                );
            }
        };

        let arguments = match instance.prepare_arguments(invocation.arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return failed_execution(format!("Invalid tool arguments: {error}"), None);
            }
        };
        let mut task = tokio::spawn(async move { instance.execute(&context, &arguments).await });
        let timeout = self.clock.sleep(self.timeout);
        tokio::pin!(timeout);

        let result = tokio::select! {
            _ = &mut cancelled => {
                task.abort();
                let _ = task.await;
                cancelled_execution()
            }
            _ = &mut timeout => {
                task.abort();
                let _ = task.await;
                ToolExecution {
                    outcome: ToolOutcome::TimedOut,
                    data: serde_json::json!({"error": "Tool execution timed out."}),
                    result_schema_version: 1,
                    knowledge: None,
                }
            }
            joined = &mut task => match joined {
                Ok(result) => result,
                Err(error) if error.is_cancelled() => cancelled_execution(),
                Err(error) => failed_execution(format!("Tool task panicked: {error}"), None),
            }
        };
        drop(permit);
        result
    }

    pub fn dispatch_batch(
        self: &Arc<Self>,
        session_id: &str,
        workspace: &str,
        invocations: Vec<ToolInvocation>,
        results: tokio::sync::mpsc::Sender<CompletedTool>,
    ) {
        for invocation in invocations {
            let executor = self.clone();
            let task_session_id = session_id.to_owned();
            let workspace = workspace.to_owned();
            let invocation_id = invocation.invocation_id.clone();
            let invocation_for_task = invocation.clone();
            let results = results.clone();
            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
            let (start_tx, start_rx) = tokio::sync::oneshot::channel();

            tokio::spawn(async move {
                if start_rx.await.is_err() {
                    return;
                }
                let context = ToolContext {
                    session_id: task_session_id.clone(),
                    invocation_id: invocation_id.clone(),
                    workspace,
                };
                let execution = executor
                    .execute_supervised(context, invocation_for_task.clone(), Some(cancel_rx))
                    .await;
                // Stay live until the result is queued, including under backpressure.
                let _ = results
                    .send(CompletedTool {
                        session_id: task_session_id.clone(),
                        invocation: invocation_for_task,
                        execution,
                    })
                    .await;
                executor.live.remove(&task_session_id, &invocation_id);
            });

            self.live
                .insert(session_id, &invocation.invocation_id, cancel_tx);
            let _ = start_tx.send(());
        }
    }

    pub fn cancel(&self, session_id: &str, invocation_id: &str) -> bool {
        self.live.cancel(session_id, invocation_id)
    }

    pub async fn cancel_session_and_wait(&self, session_id: &str) -> usize {
        let count = self.live.cancel_session(session_id);
        self.live.wait_session_idle(session_id).await;
        count
    }

    pub fn live(&self, session_id: &str) -> BTreeSet<String> {
        self.live.live(session_id)
    }
}

#[derive(Clone, Debug)]
pub struct CompletedTool {
    pub session_id: String,
    pub invocation: ToolInvocation,
    pub execution: ToolExecution,
}

fn cancelled_execution() -> ToolExecution {
    ToolExecution {
        outcome: ToolOutcome::Cancelled,
        data: serde_json::json!({"error": "Tool execution was cancelled."}),
        result_schema_version: 1,
        knowledge: None,
    }
}

fn failed_execution(message: String, knowledge: Option<ToolKnowledge>) -> ToolExecution {
    ToolExecution {
        outcome: ToolOutcome::Failed,
        data: serde_json::json!({"error": message}),
        result_schema_version: 1,
        knowledge,
    }
}

pub(super) fn rejected_execution(reason: &str) -> ToolExecution {
    failed_execution(format!("Invalid provider call: {reason}"), None)
}

type Cancellation = Option<tokio::sync::oneshot::Sender<()>>;

#[derive(Default)]
struct ExecutorTable {
    inner: Mutex<HashMap<String, HashMap<String, Cancellation>>>,
    changed: tokio::sync::Notify,
}

impl ExecutorTable {
    fn insert(
        &self,
        session_id: &str,
        invocation_id: &str,
        cancellation: tokio::sync::oneshot::Sender<()>,
    ) {
        self.inner
            .lock()
            .expect("executor table mutex poisoned")
            .entry(session_id.to_owned())
            .or_default()
            .insert(invocation_id.to_owned(), Some(cancellation));
    }

    fn remove(&self, session_id: &str, invocation_id: &str) {
        let mut table = self.inner.lock().expect("executor table mutex poisoned");
        if let Some(invocations) = table.get_mut(session_id) {
            invocations.remove(invocation_id);
            if invocations.is_empty() {
                table.remove(session_id);
            }
        }
        drop(table);
        self.changed.notify_waiters();
    }

    fn cancel(&self, session_id: &str, invocation_id: &str) -> bool {
        let cancellation = self
            .inner
            .lock()
            .expect("executor table mutex poisoned")
            .get_mut(session_id)
            .and_then(|invocations| invocations.get_mut(invocation_id))
            .and_then(Option::take);
        cancellation.is_some_and(|sender| sender.send(()).is_ok())
    }

    fn cancel_session(&self, session_id: &str) -> usize {
        let cancellations = {
            let mut table = self.inner.lock().expect("executor table mutex poisoned");
            table
                .get_mut(session_id)
                .into_iter()
                .flat_map(|invocations| invocations.values_mut())
                .filter_map(Option::take)
                .collect::<Vec<_>>()
        };
        let count = cancellations.len();
        for cancellation in cancellations {
            let _ = cancellation.send(());
        }
        count
    }

    fn live(&self, session_id: &str) -> BTreeSet<String> {
        self.inner
            .lock()
            .expect("executor table mutex poisoned")
            .get(session_id)
            .map(|invocations| invocations.keys().cloned().collect())
            .unwrap_or_default()
    }

    async fn wait_session_idle(&self, session_id: &str) {
        loop {
            let changed = self.changed.notified();
            if self.live(session_id).is_empty() {
                return;
            }
            changed.await;
        }
    }
}

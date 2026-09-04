use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use zork_agent::session::ports::{
    ProcessHandle, ProcessRequest, ProcessSpawner, ProcessStatus, SpawnedProcess,
};

pub struct ControlledProcesses {
    requests: tokio::sync::mpsc::UnboundedReceiver<PendingProcess>,
}

pub struct ControlledProcessSpawner {
    requests: tokio::sync::mpsc::UnboundedSender<PendingProcess>,
}

pub struct PendingProcess {
    pub request: ProcessRequest,
    state: Arc<ProcessState>,
}

struct ProcessState {
    value: Mutex<ProcessValue>,
    changed: tokio::sync::Notify,
}

struct ProcessValue {
    output: Option<tokio::sync::mpsc::UnboundedSender<std::io::Result<Vec<u8>>>>,
    exit: Option<VirtualExit>,
    killed: bool,
}

#[derive(Clone)]
enum VirtualExit {
    Status(ProcessStatus),
    Error(std::io::ErrorKind, String),
}

impl ControlledProcesses {
    pub fn pair() -> (Arc<ControlledProcessSpawner>, Self) {
        let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(ControlledProcessSpawner { requests }),
            Self { requests: receiver },
        )
    }

    pub async fn request(&mut self) -> PendingProcess {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.requests.recv())
            .await
            .expect("timed out waiting for the expected virtual process")
            .expect("zork-agent stopped before starting the expected virtual process")
    }
}

impl PendingProcess {
    pub fn output(&self, bytes: impl Into<Vec<u8>>) {
        let state = self
            .state
            .value
            .lock()
            .expect("virtual process lock poisoned");
        if let Some(output) = &state.output {
            let _ = output.send(Ok(bytes.into()));
        }
    }

    pub fn succeed(self) {
        self.finish(VirtualExit::Status(ProcessStatus { success: true }));
    }

    pub fn fail(self) {
        self.finish(VirtualExit::Status(ProcessStatus { success: false }));
    }

    pub fn error(self, kind: std::io::ErrorKind, message: impl Into<String>) {
        self.finish(VirtualExit::Error(kind, message.into()));
    }

    pub fn was_killed(&self) -> bool {
        self.state
            .value
            .lock()
            .expect("virtual process lock poisoned")
            .killed
    }

    fn finish(&self, exit: VirtualExit) {
        let mut state = self
            .state
            .value
            .lock()
            .expect("virtual process lock poisoned");
        if state.exit.is_none() {
            state.exit = Some(exit);
            state.output.take();
            self.state.changed.notify_waiters();
        }
    }
}

impl ProcessSpawner for ControlledProcessSpawner {
    fn spawn(&self, request: ProcessRequest) -> std::io::Result<SpawnedProcess> {
        let (output, receiver) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(ProcessState {
            value: Mutex::new(ProcessValue {
                output: Some(output),
                exit: None,
                killed: false,
            }),
            changed: tokio::sync::Notify::new(),
        });
        self.requests
            .send(PendingProcess {
                request,
                state: state.clone(),
            })
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "virtual process controller is unavailable",
                )
            })?;
        Ok(SpawnedProcess {
            output: receiver,
            handle: Arc::new(ControlledProcessHandle { state }),
        })
    }
}

struct ControlledProcessHandle {
    state: Arc<ProcessState>,
}

impl ProcessHandle for ControlledProcessHandle {
    fn wait<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<ProcessStatus>> + Send + 'a>> {
        Box::pin(async move {
            loop {
                let changed = self.state.changed.notified();
                if let Some(exit) = self
                    .state
                    .value
                    .lock()
                    .expect("virtual process lock poisoned")
                    .exit
                    .clone()
                {
                    return match exit {
                        VirtualExit::Status(status) => Ok(status),
                        VirtualExit::Error(kind, message) => {
                            Err(std::io::Error::new(kind, message))
                        }
                    };
                }
                changed.await;
            }
        })
    }

    fn kill<'a>(&'a self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self
                .state
                .value
                .lock()
                .expect("virtual process lock poisoned");
            state.killed = true;
            state.output.take();
            state
                .exit
                .get_or_insert(VirtualExit::Status(ProcessStatus { success: false }));
            self.state.changed.notify_waiters();
            Ok(())
        })
    }
}

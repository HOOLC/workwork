use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::AsyncReadExt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRequest {
    pub command: String,
    pub current_dir: std::path::PathBuf,
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessStatus {
    pub success: bool,
}

pub struct SpawnedProcess {
    pub output: tokio::sync::mpsc::UnboundedReceiver<std::io::Result<Vec<u8>>>,
    pub handle: Arc<dyn ProcessHandle>,
}

pub trait ProcessSpawner: Send + Sync {
    fn spawn(&self, request: ProcessRequest) -> std::io::Result<SpawnedProcess>;
}

pub trait ProcessHandle: Send + Sync {
    fn wait<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<ProcessStatus>> + Send + 'a>>;

    fn kill<'a>(&'a self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProcessSpawner;

impl ProcessSpawner for SystemProcessSpawner {
    fn spawn(&self, request: ProcessRequest) -> std::io::Result<SpawnedProcess> {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("exec 2>&1\n{}", request.command))
            .current_dir(request.current_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .envs(request.environment)
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        let child_id = child.id();
        let mut stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::other("spawned process did not expose captured stdout")
        })?;
        let (output, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut buffer = vec![0_u8; 8192];
            loop {
                match stdout.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(read) => {
                        if output.send(Ok(buffer[..read].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = output.send(Err(error));
                        break;
                    }
                }
            }
        });
        Ok(SpawnedProcess {
            output: receiver,
            handle: Arc::new(SystemProcess {
                child: tokio::sync::Mutex::new(child),
                process_group_armed: AtomicBool::new(true),
                #[cfg(unix)]
                process_group_id: child_id.and_then(|id| libc::pid_t::try_from(id).ok()),
            }),
        })
    }
}

struct SystemProcess {
    child: tokio::sync::Mutex<tokio::process::Child>,
    process_group_armed: AtomicBool,
    #[cfg(unix)]
    process_group_id: Option<libc::pid_t>,
}

impl ProcessHandle for SystemProcess {
    fn wait<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<ProcessStatus>> + Send + 'a>> {
        Box::pin(async move {
            let status = self.child.lock().await.wait().await?;
            self.process_group_armed.store(false, Ordering::Release);
            Ok(ProcessStatus {
                success: status.success(),
            })
        })
    }

    fn kill<'a>(&'a self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.kill_group();
            self.child.lock().await.kill().await
        })
    }
}

impl SystemProcess {
    #[cfg(unix)]
    fn kill_group(&self) {
        if self.process_group_armed.swap(false, Ordering::AcqRel) {
            if let Some(process_group_id) = self.process_group_id {
                // SAFETY: the child was created as leader of a new process
                // group, and a negative pid targets that complete group.
                unsafe {
                    libc::kill(-process_group_id, libc::SIGKILL);
                }
            }
        }
    }

    #[cfg(not(unix))]
    fn kill_group(&self) {
        self.process_group_armed.store(false, Ordering::Release);
    }
}

impl Drop for SystemProcess {
    fn drop(&mut self) {
        self.kill_group();
    }
}

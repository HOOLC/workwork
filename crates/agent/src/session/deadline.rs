//! Process-wide, rebuildable deadline index.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use super::events::DeadlineKind;
use super::ports::Clock;

#[derive(Clone, Debug)]
pub struct DeadlineWake {
    pub session_id: String,
    pub deadline: DeadlineKind,
}

#[derive(Clone)]
pub struct DeadlineScheduler {
    commands: tokio::sync::mpsc::Sender<Command>,
}

impl DeadlineScheduler {
    pub fn start(
        clock: Arc<dyn Clock>,
        command_capacity: usize,
        wake_capacity: usize,
    ) -> (Self, tokio::sync::mpsc::Receiver<DeadlineWake>) {
        let (commands, receiver) = tokio::sync::mpsc::channel(command_capacity.max(1));
        let (wakes, wake_receiver) = tokio::sync::mpsc::channel(wake_capacity.max(1));
        tokio::spawn(run(clock, receiver, wakes));
        (Self { commands }, wake_receiver)
    }

    pub async fn arm(
        &self,
        session_id: impl Into<String>,
        deadline: DeadlineKind,
        deadline_ms: i64,
    ) -> Result<(), DeadlineError> {
        self.commands
            .send(Command::Arm {
                session_id: session_id.into(),
                deadline,
                deadline_ms,
            })
            .await
            .map_err(|_| DeadlineError::Shutdown)
    }

    pub async fn cancel_session(&self, session_id: impl Into<String>) {
        let _ = self
            .commands
            .send(Command::CancelSession {
                session_id: session_id.into(),
            })
            .await;
    }

    pub async fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown).await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeadlineError {
    #[error("deadline scheduler is shut down")]
    Shutdown,
}

enum Command {
    Arm {
        session_id: String,
        deadline: DeadlineKind,
        deadline_ms: i64,
    },
    CancelSession {
        session_id: String,
    },
    Shutdown,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum DeadlineKey {
    AutoWait(String),
    Wait(String),
}

impl From<&DeadlineKind> for DeadlineKey {
    fn from(deadline: &DeadlineKind) -> Self {
        match deadline {
            DeadlineKind::AutoWait { step_id } => Self::AutoWait(step_id.clone()),
            DeadlineKind::Wait { invocation_id } => Self::Wait(invocation_id.clone()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Scheduled {
    deadline_ms: i64,
    serial: u64,
    session_id: String,
    deadline: DeadlineKind,
}

impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .deadline_ms
            .cmp(&self.deadline_ms)
            .then_with(|| other.serial.cmp(&self.serial))
    }
}

impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

async fn run(
    clock: Arc<dyn Clock>,
    mut commands: tokio::sync::mpsc::Receiver<Command>,
    wakes: tokio::sync::mpsc::Sender<DeadlineWake>,
) {
    let mut heap = BinaryHeap::<Scheduled>::new();
    let mut current = HashMap::<(String, DeadlineKey), (i64, u64)>::new();
    let mut next_serial = 0_u64;

    loop {
        let sleep = heap.peek().map_or_else(
            || Box::pin(std::future::pending()) as super::ports::Sleep,
            |entry| clock.sleep_until(entry.deadline_ms),
        );
        tokio::pin!(sleep);

        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(Command::Arm { session_id, deadline, deadline_ms }) => {
                    next_serial = next_serial.wrapping_add(1);
                    let key = (session_id.clone(), DeadlineKey::from(&deadline));
                    current.insert(key, (deadline_ms, next_serial));
                    heap.push(Scheduled {
                        deadline_ms,
                        serial: next_serial,
                        session_id,
                        deadline,
                    });
                }
                Some(Command::CancelSession { session_id }) => {
                    current.retain(|(candidate, _), _| candidate != &session_id);
                }
                Some(Command::Shutdown) | None => return,
            },
            _ = &mut sleep, if !heap.is_empty() => {
                let now = clock.now_ms();
                while heap.peek().is_some_and(|entry| entry.deadline_ms <= now) {
                    let entry = heap.pop().expect("heap was checked as non-empty");
                    let key = (entry.session_id.clone(), DeadlineKey::from(&entry.deadline));
                    if current.get(&key) != Some(&(entry.deadline_ms, entry.serial)) {
                        continue;
                    }
                    current.remove(&key);
                    if wakes.send(DeadlineWake {
                        session_id: entry.session_id,
                        deadline: entry.deadline,
                    }).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

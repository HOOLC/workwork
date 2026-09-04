//! Lightweight session slots, bounded admission and runner lifecycle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use futures_util::StreamExt;
use ulid::Ulid;

use super::deadline::DeadlineWake;
use super::events::{RuntimeFailure, Selection, TurnOutcome};
use super::query::{Commit, QueryError, ReadResult, SessionQuery, SessionReadHint};
use super::recovery::{recover, Recovery, RecoveryError};
use super::runner::{RunnerCommand, RunnerDependencies, RunnerExit, RunnerFailure, SessionRunner};
use super::state::SessionState;
use super::store::{SessionStore, StoreError};

const SLOT_UNCHECKED: u8 = 0;
const SLOT_RECOVERING: u8 = 1;
const SLOT_ACTIVE: u8 = 2;
const SLOT_IDLE: u8 = 3;
const SLOT_CIRCUIT_OPEN: u8 = 4;
const SLOT_UNAVAILABLE: u8 = 5;
const SLOT_DELETING: u8 = 6;

#[derive(Clone, Debug)]
pub struct SupervisorOptions {
    pub session_queue_capacity: usize,
    pub global_queue_capacity: usize,
    pub runner_fault_limit: u32,
    pub startup_recovery_concurrency: usize,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            session_queue_capacity: 64,
            global_queue_capacity: 4096,
            runner_fault_limit: 5,
            startup_recovery_concurrency: 16,
        }
    }
}

pub struct SessionSupervisor {
    dependencies: RunnerDependencies,
    store: Arc<dyn SessionStore>,
    query: Arc<dyn SessionQuery>,
    slots: RwLock<HashMap<String, Arc<SessionSlot>>>,
    global_capacity: Arc<tokio::sync::Semaphore>,
    next_runner_id: AtomicU64,
    stopping: AtomicBool,
    discovery_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    options: SupervisorOptions,
}

struct SessionSlot {
    session_id: String,
    read_hint: std::sync::Mutex<Option<SessionReadHint>>,
    status: tokio::sync::Mutex<SlotStatus>,
    changed: tokio::sync::Notify,
    completed_runner_id: AtomicU64,
    runner_completed: tokio::sync::Notify,
    public_status: AtomicU8,
    finished: AtomicBool,
}

enum SlotStatus {
    Unchecked,
    Recovering,
    Active {
        runner_id: u64,
        sender: tokio::sync::mpsc::Sender<RunnerCommand>,
    },
    Idle,
    CircuitOpen(String),
    Unavailable(String),
    Deleting,
}

enum DispatchAttempt<T> {
    Sent,
    Full(T),
    Closed(T),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicSlotStatus {
    Unchecked,
    Recovering,
    Active,
    Idle,
    CircuitOpen,
    Unavailable,
    Deleting,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSlotView {
    pub session_id: String,
    pub status: PublicSlotStatus,
    pub finished: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SupervisorError {
    #[error("session not found")]
    NotFound,
    #[error("session queue is full")]
    SessionOverloaded,
    #[error("global queue is full")]
    GlobalOverloaded,
    #[error("session runner circuit is open: {0}")]
    CircuitOpen(String),
    #[error("session is unavailable: {0}")]
    Unavailable(String),
    #[error("session is being deleted")]
    Deleting,
    #[error("runner request failed: {0}")]
    Runner(String),
    #[error("store error: {0}")]
    Store(String),
    #[error("query error: {0}")]
    Query(String),
}

impl SessionSupervisor {
    pub fn start(
        dependencies: RunnerDependencies,
        query: Arc<dyn SessionQuery>,
        deadline_wakes: tokio::sync::mpsc::Receiver<DeadlineWake>,
        options: SupervisorOptions,
    ) -> Arc<Self> {
        let supervisor = Arc::new(Self {
            store: dependencies.store.clone(),
            query,
            dependencies,
            slots: RwLock::new(HashMap::new()),
            global_capacity: Arc::new(tokio::sync::Semaphore::new(
                options.global_queue_capacity.max(1),
            )),
            next_runner_id: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
            discovery_task: std::sync::Mutex::new(None),
            options,
        });
        supervisor.start_deadline_wakes(deadline_wakes);
        supervisor.start_discovery();
        supervisor
    }

    pub async fn create_session(
        self: &Arc<Self>,
        selection: Selection,
        system_prompt: Option<String>,
        workspace: String,
        context: Option<zork_config::ContextConfig>,
    ) -> Result<String, SupervisorError> {
        let capacity = self.try_global_capacity()?;
        let session_id = loop {
            let candidate = self.dependencies.ids.next();
            if !self.query.exists(&candidate)
                && !self
                    .slots
                    .read()
                    .expect("session slots lock poisoned")
                    .contains_key(&candidate)
            {
                break candidate;
            }
        };
        let slot = Arc::new(SessionSlot::new(
            session_id.clone(),
            SlotStatus::Recovering,
            None,
        ));
        self.slots
            .write()
            .expect("session slots lock poisoned")
            .insert(session_id.clone(), slot.clone());
        self.activate(slot.clone(), SessionState::empty(&session_id), Vec::new())
            .await?;

        let (response, received) = tokio::sync::oneshot::channel();
        self.dispatch_external(
            &slot,
            RunnerCommand::Create {
                selection,
                system_prompt,
                workspace,
                context,
                response,
                capacity: Some(capacity),
            },
        )
        .await?;
        await_response(received).await?;
        Ok(session_id)
    }

    pub async fn submit_input(
        self: &Arc<Self>,
        session_id: &str,
        content: String,
    ) -> Result<(), SupervisorError> {
        let slot = self.lookup(session_id)?;
        let capacity = self.try_global_capacity()?;
        let (response, received) = tokio::sync::oneshot::channel();
        self.dispatch_external(
            &slot,
            RunnerCommand::Input {
                content,
                response,
                capacity: Some(capacity),
            },
        )
        .await?;
        await_response(received).await
    }

    pub async fn set_selection(
        self: &Arc<Self>,
        session_id: &str,
        selection: Selection,
    ) -> Result<(), SupervisorError> {
        let slot = self.lookup(session_id)?;
        let capacity = self.try_global_capacity()?;
        let (response, received) = tokio::sync::oneshot::channel();
        self.dispatch_external(
            &slot,
            RunnerCommand::SetSelection {
                selection,
                response,
                capacity: Some(capacity),
            },
        )
        .await?;
        await_response(received).await
    }

    pub async fn set_context(
        self: &Arc<Self>,
        session_id: &str,
        config: zork_config::ContextConfig,
    ) -> Result<(), SupervisorError> {
        let slot = self.lookup(session_id)?;
        let capacity = self.try_global_capacity()?;
        let (response, received) = tokio::sync::oneshot::channel();
        self.dispatch_external(
            &slot,
            RunnerCommand::SetContext {
                config,
                response,
                capacity: Some(capacity),
            },
        )
        .await?;
        await_response(received).await
    }

    pub async fn cancel_turn(self: &Arc<Self>, session_id: &str) -> Result<(), SupervisorError> {
        let slot = self.lookup(session_id)?;
        let capacity = self.try_global_capacity()?;
        let (response, received) = tokio::sync::oneshot::channel();
        self.dispatch_external(
            &slot,
            RunnerCommand::CancelTurn {
                response,
                capacity: Some(capacity),
            },
        )
        .await?;
        await_response(received).await
    }

    pub async fn inspect(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<SessionState, SupervisorError> {
        let slot = self.lookup(session_id)?;
        {
            let status = slot.status.lock().await;
            if matches!(&*status, SlotStatus::CircuitOpen(_)) {
                drop(status);
                return self
                    .recover_state(&slot)
                    .await
                    .map(|recovery| recovery.state);
            }
        }
        self.ensure_active(slot.clone()).await?;
        let (response, received) = tokio::sync::oneshot::channel();
        self.send_internal(&slot, RunnerCommand::Inspect(response))
            .await?;
        received
            .await
            .map_err(|_| SupervisorError::Runner("runner exited before returning its state".into()))
    }

    pub async fn delete(self: &Arc<Self>, session_id: &str) -> Result<(), SupervisorError> {
        let slot = self.lookup(session_id)?;
        let runner = loop {
            let notified = slot.changed.notified();
            let mut status = slot.status.lock().await;
            match &*status {
                SlotStatus::Recovering => {
                    drop(status);
                    notified.await;
                }
                SlotStatus::Active { runner_id, sender } => {
                    let runner = Some((*runner_id, sender.clone()));
                    *status = SlotStatus::Deleting;
                    slot.set_public(SLOT_DELETING, false);
                    slot.changed.notify_waiters();
                    break runner;
                }
                SlotStatus::Deleting => return Err(SupervisorError::Deleting),
                _ => {
                    *status = SlotStatus::Deleting;
                    slot.set_public(SLOT_DELETING, false);
                    slot.changed.notify_waiters();
                    break None;
                }
            }
        };

        if let Some((runner_id, sender)) = runner {
            let (response, received) = tokio::sync::oneshot::channel();
            let _ = sender.send(RunnerCommand::Stop(response)).await;
            let _ = received.await;
            slot.wait_for_runner(runner_id).await;
        }
        self.dependencies
            .executor
            .cancel_session_and_wait(session_id)
            .await;
        self.dependencies
            .deadlines
            .cancel_session(session_id.to_owned())
            .await;
        self.dependencies
            .model
            .release(super::model::ModelReleaseSuggestion::Session(session_id));

        let store = self.store.clone();
        let session = session_id.to_owned();
        let detached = tokio::task::spawn_blocking(move || store.detach_session(&session))
            .await
            .map_err(|error| SupervisorError::Store(error.to_string()))?
            .map_err(store_error)?;
        self.slots
            .write()
            .expect("session slots lock poisoned")
            .remove(session_id);
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || detached.cleanup()).await;
        });
        Ok(())
    }

    pub fn list(&self) -> Vec<SessionSlotView> {
        let slots = self.slots.read().expect("session slots lock poisoned");
        let mut views = slots
            .values()
            .map(|slot| SessionSlotView {
                session_id: slot.session_id.clone(),
                status: slot.public_status(),
                finished: slot.finished.load(Ordering::Acquire),
            })
            .collect::<Vec<_>>();
        views.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        views
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.lookup(session_id).is_ok()
    }

    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        let discovery = self
            .discovery_task
            .lock()
            .expect("startup discovery task mutex poisoned")
            .take();
        if let Some(discovery) = discovery {
            let _ = discovery.await;
        }
        let slots = self
            .slots
            .read()
            .expect("session slots lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for slot in slots {
            let runner = {
                let status = slot.status.lock().await;
                match &*status {
                    SlotStatus::Active { runner_id, sender } => Some((*runner_id, sender.clone())),
                    _ => None,
                }
            };
            if let Some((runner_id, sender)) = runner {
                let (response, received) = tokio::sync::oneshot::channel();
                if sender.send(RunnerCommand::Stop(response)).await.is_ok() {
                    let _ = received.await;
                }
                slot.wait_for_runner(runner_id).await;
            } else {
                self.dependencies
                    .executor
                    .cancel_session_and_wait(&slot.session_id)
                    .await;
                self.dependencies
                    .deadlines
                    .cancel_session(slot.session_id.clone())
                    .await;
                self.dependencies
                    .model
                    .release(super::model::ModelReleaseSuggestion::Session(
                        &slot.session_id,
                    ));
            }
        }
    }

    fn start_deadline_wakes(
        self: &Arc<Self>,
        mut wakes: tokio::sync::mpsc::Receiver<DeadlineWake>,
    ) {
        let supervisor = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(wake) = wakes.recv().await {
                let Some(supervisor) = supervisor.upgrade() else {
                    return;
                };
                let Ok(slot) = supervisor.lookup(&wake.session_id) else {
                    continue;
                };
                if supervisor.ensure_active(slot.clone()).await.is_err() {
                    continue;
                }
                let _ = supervisor
                    .send_internal(&slot, RunnerCommand::Deadline(wake.deadline))
                    .await;
            }
        });
    }

    fn start_discovery(self: &Arc<Self>) {
        let supervisor = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            let Some(owner) = supervisor.upgrade() else {
                return;
            };
            let query = owner.query.clone();
            let discovered =
                match tokio::task::spawn_blocking(move || query.discover_sessions()).await {
                    Ok(Ok(discovered)) => discovered,
                    _ => return,
                };

            let slots = {
                let mut map = owner.slots.write().expect("session slots lock poisoned");
                discovered
                    .into_iter()
                    .map(|session| {
                        map.entry(session.session_id.clone())
                            .or_insert_with(|| {
                                Arc::new(SessionSlot::new(
                                    session.session_id,
                                    SlotStatus::Unchecked,
                                    Some(session.read_hint),
                                ))
                            })
                            .clone()
                    })
                    .collect::<Vec<_>>()
            };
            let concurrency = owner.options.startup_recovery_concurrency.max(1);
            drop(owner);

            futures_util::stream::iter(slots)
                .for_each_concurrent(concurrency, |slot| {
                    let supervisor = supervisor.clone();
                    async move {
                        if let Some(owner) = supervisor.upgrade() {
                            owner.check_discovered(slot).await;
                        }
                    }
                })
                .await;
        });
        *self
            .discovery_task
            .lock()
            .expect("startup discovery task mutex poisoned") = Some(task);
    }

    async fn check_discovered(self: &Arc<Self>, slot: Arc<SessionSlot>) {
        if self.stopping.load(Ordering::Acquire) {
            slot.take_read_hint();
            return;
        }
        {
            let mut status = slot.status.lock().await;
            if !matches!(*status, SlotStatus::Unchecked) {
                slot.take_read_hint();
                return;
            }
            *status = SlotStatus::Recovering;
            slot.set_public(SLOT_RECOVERING, false);
        }
        if self.last_commit(&slot).await.is_ok_and(|read| {
            read.diagnostics.is_empty() && read.value.as_ref().is_some_and(commit_finishes_session)
        }) {
            slot.take_read_hint();
            let mut status = slot.status.lock().await;
            if matches!(*status, SlotStatus::Recovering) {
                *status = SlotStatus::Idle;
                slot.set_public(SLOT_IDLE, true);
                slot.changed.notify_waiters();
            }
            return;
        }
        match self.recover_state(&slot).await {
            Ok(recovery)
                if requires_runner(&recovery.state) || !recovery.diagnostics.is_empty() =>
            {
                let initial = recovery_commands(&recovery);
                let _ = self.activate(slot, recovery.state, initial).await;
            }
            Ok(recovery) => {
                let finished = recovery.state.last_turn_outcome == Some(TurnOutcome::Finished);
                let mut status = slot.status.lock().await;
                if matches!(*status, SlotStatus::Recovering) {
                    *status = SlotStatus::Idle;
                    slot.set_public(SLOT_IDLE, finished);
                    slot.changed.notify_waiters();
                }
            }
            Err(error) => self.mark_unavailable(&slot, error.to_string()).await,
        }
    }

    async fn ensure_active(
        self: &Arc<Self>,
        slot: Arc<SessionSlot>,
    ) -> Result<(), SupervisorError> {
        loop {
            let notified = slot.changed.notified();
            let recover = {
                let mut status = slot.status.lock().await;
                match &*status {
                    SlotStatus::Active { .. } => return Ok(()),
                    SlotStatus::Unchecked | SlotStatus::Idle => {
                        *status = SlotStatus::Recovering;
                        slot.set_public(SLOT_RECOVERING, false);
                        true
                    }
                    SlotStatus::Recovering => false,
                    SlotStatus::CircuitOpen(message) => {
                        return Err(SupervisorError::CircuitOpen(message.clone()))
                    }
                    SlotStatus::Unavailable(message) => {
                        return Err(SupervisorError::Unavailable(message.clone()))
                    }
                    SlotStatus::Deleting => return Err(SupervisorError::Deleting),
                }
            };
            if recover {
                match self.recover_state(&slot).await {
                    Ok(recovery) => {
                        let initial = recovery_commands(&recovery);
                        return self.activate(slot.clone(), recovery.state, initial).await;
                    }
                    Err(error) => {
                        self.mark_unavailable(&slot, error.to_string()).await;
                        return Err(error);
                    }
                }
            }
            notified.await;
        }
    }

    async fn recover_state(&self, slot: &Arc<SessionSlot>) -> Result<Recovery, SupervisorError> {
        let query = self.query.clone();
        let tools = self.dependencies.tools.clone();
        let session_id = slot.session_id.clone();
        let hint = slot.take_read_hint();
        tokio::task::spawn_blocking(move || {
            recover(query.as_ref(), &session_id, hint.as_ref(), &tools)
        })
        .await
        .map_err(|error| SupervisorError::Query(error.to_string()))?
        .map_err(recovery_error)
    }

    async fn last_commit(
        &self,
        slot: &Arc<SessionSlot>,
    ) -> Result<ReadResult<Option<Commit>>, SupervisorError> {
        let query = self.query.clone();
        let session_id = slot.session_id.clone();
        let hint = slot.read_hint();
        tokio::task::spawn_blocking(move || query.last_commit(&session_id, hint.as_ref()))
            .await
            .map_err(|error| SupervisorError::Query(error.to_string()))?
            .map_err(query_error)
    }

    async fn activate(
        self: &Arc<Self>,
        slot: Arc<SessionSlot>,
        state: SessionState,
        initial: Vec<RunnerCommand>,
    ) -> Result<(), SupervisorError> {
        let (sender, receiver) = tokio::sync::mpsc::channel(
            self.options
                .session_queue_capacity
                .max(initial.len())
                .max(1),
        );
        for command in initial {
            sender
                .try_send(command)
                .map_err(|_| SupervisorError::SessionOverloaded)?;
        }
        let runner_id = self.next_runner_id.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut status = slot.status.lock().await;
            if !matches!(*status, SlotStatus::Recovering) {
                return Err(SupervisorError::Runner(
                    "session changed state while recovery completed".into(),
                ));
            }
            *status = SlotStatus::Active { runner_id, sender };
            slot.set_public(SLOT_ACTIVE, false);
            slot.changed.notify_waiters();
        }
        self.spawn_runner(slot, runner_id, state, receiver);
        Ok(())
    }

    fn spawn_runner(
        self: &Arc<Self>,
        slot: Arc<SessionSlot>,
        runner_id: u64,
        state: SessionState,
        receiver: tokio::sync::mpsc::Receiver<RunnerCommand>,
    ) {
        let supervisor = Arc::downgrade(self);
        let dependencies = self.dependencies.clone();
        tokio::spawn(async move {
            let runner = SessionRunner::new(dependencies, state, receiver);
            let task = tokio::spawn(runner.run());
            let exit = match task.await {
                Ok(exit) => exit,
                Err(error) => RunnerExit::Failed(RunnerFailure {
                    failure_id: "panic".into(),
                    stage: "runner".into(),
                    message: panic_message(error),
                }),
            };
            {
                if let Some(supervisor) = supervisor.upgrade() {
                    supervisor
                        .runner_exited(slot.clone(), runner_id, exit)
                        .await;
                }
            }
            slot.complete_runner(runner_id);
        });
    }

    async fn runner_exited(
        self: &Arc<Self>,
        slot: Arc<SessionSlot>,
        runner_id: u64,
        exit: RunnerExit,
    ) {
        if self.stopping.load(Ordering::Acquire) {
            let mut status = slot.status.lock().await;
            if matches!(&*status, SlotStatus::Active { runner_id: current, .. } if *current == runner_id)
            {
                *status = SlotStatus::Idle;
                slot.set_public(SLOT_IDLE, false);
                slot.changed.notify_waiters();
            }
            return;
        }
        match exit {
            RunnerExit::Idle(idle) => {
                let mut restart = None;
                {
                    let mut status = slot.status.lock().await;
                    let sender = match &*status {
                        SlotStatus::Active {
                            runner_id: current,
                            sender,
                        } if *current == runner_id => sender.clone(),
                        _ => return,
                    };
                    if idle.commands.is_empty() {
                        let finished = idle.state.last_turn_outcome == Some(TurnOutcome::Finished);
                        *status = SlotStatus::Idle;
                        slot.set_public(SLOT_IDLE, finished);
                    } else {
                        let next = self.next_runner_id.fetch_add(1, Ordering::Relaxed) + 1;
                        *status = SlotStatus::Active {
                            runner_id: next,
                            sender,
                        };
                        slot.set_public(SLOT_ACTIVE, false);
                        restart = Some((next, idle.state, idle.commands));
                    }
                    slot.changed.notify_waiters();
                }
                if let Some((next, state, receiver)) = restart {
                    self.spawn_runner(slot, next, state, receiver);
                }
            }
            RunnerExit::CircuitOpen => {
                let mut status = slot.status.lock().await;
                if matches!(&*status, SlotStatus::Active { runner_id: current, .. } if *current == runner_id)
                {
                    *status = SlotStatus::CircuitOpen(
                        "the same runner failure reached the configured limit".into(),
                    );
                    slot.set_public(SLOT_CIRCUIT_OPEN, false);
                    slot.changed.notify_waiters();
                }
            }
            RunnerExit::Stopped => {
                let mut status = slot.status.lock().await;
                if matches!(&*status, SlotStatus::Active { runner_id: current, .. } if *current == runner_id)
                {
                    *status = SlotStatus::Idle;
                    slot.set_public(SLOT_IDLE, false);
                    slot.changed.notify_waiters();
                }
            }
            RunnerExit::Failed(failure) => {
                {
                    let mut status = slot.status.lock().await;
                    if !matches!(&*status, SlotStatus::Active { runner_id: current, .. } if *current == runner_id)
                    {
                        return;
                    }
                    *status = SlotStatus::Recovering;
                    slot.set_public(SLOT_RECOVERING, false);
                    slot.changed.notify_waiters();
                }
                let supervisor = self.clone();
                tokio::spawn(async move {
                    supervisor.rebuild_after_failure(slot, failure).await;
                });
            }
        }
    }

    async fn rebuild_after_failure(
        self: Arc<Self>,
        slot: Arc<SessionSlot>,
        failure: RunnerFailure,
    ) {
        let recovery = match self.recover_state(&slot).await {
            Ok(recovery) => recovery,
            Err(error) => {
                self.mark_unavailable(&slot, error.to_string()).await;
                return;
            }
        };
        let fingerprint = failure.fingerprint();
        let consecutive_count = recovery
            .state
            .fault_streak
            .as_ref()
            .filter(|streak| streak.fingerprint == fingerprint)
            .map_or(1, |streak| streak.count.saturating_add(1));
        let circuit_open = consecutive_count >= self.options.runner_fault_limit.max(1);
        let initial = vec![RunnerCommand::RecordFault {
            failure: RuntimeFailure {
                failure_id: failure.failure_id,
                stage: failure.stage,
                message: failure.message,
            },
            consecutive_count,
            circuit_open,
        }];
        if let Err(error) = self.activate(slot.clone(), recovery.state, initial).await {
            self.mark_unavailable(&slot, error.to_string()).await;
        }
    }

    async fn dispatch_external(
        self: &Arc<Self>,
        slot: &Arc<SessionSlot>,
        mut command: RunnerCommand,
    ) -> Result<(), SupervisorError> {
        loop {
            self.ensure_active(slot.clone()).await?;
            let notified = slot.changed.notified();
            let status = slot.status.lock().await;
            match &*status {
                SlotStatus::Active { sender, .. } => match try_dispatch(sender, command) {
                    DispatchAttempt::Sent => return Ok(()),
                    DispatchAttempt::Full(_) => return Err(SupervisorError::SessionOverloaded),
                    DispatchAttempt::Closed(returned) => {
                        command = returned;
                        drop(status);
                        notified.await;
                    }
                },
                SlotStatus::Recovering => {
                    drop(status);
                    notified.await;
                }
                SlotStatus::CircuitOpen(message) => {
                    return Err(SupervisorError::CircuitOpen(message.clone()))
                }
                SlotStatus::Unavailable(message) => {
                    return Err(SupervisorError::Unavailable(message.clone()))
                }
                SlotStatus::Deleting => return Err(SupervisorError::Deleting),
                SlotStatus::Unchecked | SlotStatus::Idle => {
                    drop(status);
                }
            }
        }
    }

    async fn send_internal(
        self: &Arc<Self>,
        slot: &Arc<SessionSlot>,
        command: RunnerCommand,
    ) -> Result<(), SupervisorError> {
        loop {
            self.ensure_active(slot.clone()).await?;
            let notified = slot.changed.notified();
            let (runner_id, sender) = {
                let status = slot.status.lock().await;
                match &*status {
                    SlotStatus::Active { runner_id, sender } => (*runner_id, sender.clone()),
                    SlotStatus::Recovering => {
                        drop(status);
                        notified.await;
                        continue;
                    }
                    SlotStatus::Unchecked | SlotStatus::Idle => {
                        // The runner can become idle after ensure_active returns,
                        // before this notification is registered. Nothing will wake
                        // an idle slot: retry activation instead of waiting for it.
                        drop(status);
                        continue;
                    }
                    SlotStatus::CircuitOpen(message) => {
                        return Err(SupervisorError::CircuitOpen(message.clone()))
                    }
                    SlotStatus::Unavailable(message) => {
                        return Err(SupervisorError::Unavailable(message.clone()))
                    }
                    SlotStatus::Deleting => return Err(SupervisorError::Deleting),
                }
            };
            let permit = match sender.reserve_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    notified.await;
                    continue;
                }
            };
            let status = slot.status.lock().await;
            match &*status {
                SlotStatus::Active {
                    runner_id: current, ..
                } if *current == runner_id => {
                    permit.send(command);
                    return Ok(());
                }
                _ => {
                    drop(permit);
                    drop(status);
                }
            }
        }
    }

    fn lookup(&self, session_id: &str) -> Result<Arc<SessionSlot>, SupervisorError> {
        if session_id.parse::<Ulid>().is_err() {
            return Err(SupervisorError::NotFound);
        }
        if let Some(slot) = self
            .slots
            .read()
            .expect("session slots lock poisoned")
            .get(session_id)
            .cloned()
        {
            return Ok(slot);
        }
        if !self.query.exists(session_id) {
            return Err(SupervisorError::NotFound);
        }
        let mut slots = self.slots.write().expect("session slots lock poisoned");
        Ok(slots
            .entry(session_id.to_owned())
            .or_insert_with(|| {
                Arc::new(SessionSlot::new(
                    session_id.to_owned(),
                    SlotStatus::Unchecked,
                    None,
                ))
            })
            .clone())
    }

    fn try_global_capacity(&self) -> Result<tokio::sync::OwnedSemaphorePermit, SupervisorError> {
        self.global_capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| SupervisorError::GlobalOverloaded)
    }

    async fn mark_unavailable(&self, slot: &Arc<SessionSlot>, message: String) {
        let mut status = slot.status.lock().await;
        if matches!(*status, SlotStatus::Recovering) {
            *status = SlotStatus::Unavailable(message);
            slot.set_public(SLOT_UNAVAILABLE, false);
            slot.changed.notify_waiters();
        }
    }
}

impl SessionSlot {
    fn new(session_id: String, status: SlotStatus, read_hint: Option<SessionReadHint>) -> Self {
        let public_status = match status {
            SlotStatus::Unchecked => SLOT_UNCHECKED,
            SlotStatus::Recovering => SLOT_RECOVERING,
            _ => unreachable!("new slots start unchecked or recovering"),
        };
        Self {
            session_id,
            read_hint: std::sync::Mutex::new(read_hint),
            status: tokio::sync::Mutex::new(status),
            changed: tokio::sync::Notify::new(),
            completed_runner_id: AtomicU64::new(0),
            runner_completed: tokio::sync::Notify::new(),
            public_status: AtomicU8::new(public_status),
            finished: AtomicBool::new(false),
        }
    }

    fn set_public(&self, status: u8, finished: bool) {
        self.finished.store(finished, Ordering::Release);
        self.public_status.store(status, Ordering::Release);
    }

    fn public_status(&self) -> PublicSlotStatus {
        match self.public_status.load(Ordering::Acquire) {
            SLOT_UNCHECKED => PublicSlotStatus::Unchecked,
            SLOT_RECOVERING => PublicSlotStatus::Recovering,
            SLOT_ACTIVE => PublicSlotStatus::Active,
            SLOT_IDLE => PublicSlotStatus::Idle,
            SLOT_CIRCUIT_OPEN => PublicSlotStatus::CircuitOpen,
            SLOT_UNAVAILABLE => PublicSlotStatus::Unavailable,
            SLOT_DELETING => PublicSlotStatus::Deleting,
            _ => PublicSlotStatus::Unavailable,
        }
    }

    fn read_hint(&self) -> Option<SessionReadHint> {
        self.read_hint
            .lock()
            .expect("session read hint mutex poisoned")
            .clone()
    }

    fn take_read_hint(&self) -> Option<SessionReadHint> {
        self.read_hint
            .lock()
            .expect("session read hint mutex poisoned")
            .take()
    }

    fn complete_runner(&self, runner_id: u64) {
        self.completed_runner_id
            .fetch_max(runner_id, Ordering::Release);
        self.runner_completed.notify_waiters();
    }

    async fn wait_for_runner(&self, runner_id: u64) {
        loop {
            let completed = self.runner_completed.notified();
            if self.completed_runner_id.load(Ordering::Acquire) >= runner_id {
                return;
            }
            completed.await;
        }
    }
}

fn requires_runner(state: &SessionState) -> bool {
    let pending_requires_runner = state.pending_tools.values().any(|pending| {
        pending.result.is_none() || state.last_turn_outcome != Some(TurnOutcome::Cancelled)
    });
    state.active_turn.is_some()
        || state.active_step.is_some()
        || pending_requires_runner
        || !state.unconsumed_inputs.is_empty()
        || !state.pending_notices.is_empty()
        || state.auto_wait.is_some()
        || state.wait_deadline.is_some()
}

fn commit_finishes_session(commit: &Commit) -> bool {
    matches!(
        commit.last().map(|envelope| &envelope.event),
        Some(super::events::SessionEvent::TurnFinished {
            outcome: TurnOutcome::Finished,
            ..
        })
    )
}

fn try_dispatch<T>(sender: &tokio::sync::mpsc::Sender<T>, message: T) -> DispatchAttempt<T> {
    match sender.try_send(message) {
        Ok(()) => DispatchAttempt::Sent,
        Err(tokio::sync::mpsc::error::TrySendError::Full(returned)) => {
            DispatchAttempt::Full(returned)
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(returned)) => {
            DispatchAttempt::Closed(returned)
        }
    }
}

fn recovery_commands(recovery: &Recovery) -> Vec<RunnerCommand> {
    if recovery.diagnostics.is_empty() {
        return Vec::new();
    }
    vec![RunnerCommand::RecordFault {
        failure: RuntimeFailure {
            failure_id: "recovery_diagnostic".into(),
            stage: "session.recover".into(),
            message: recovery.diagnostics.join("\n"),
        },
        consecutive_count: 1,
        circuit_open: false,
    }]
}

async fn await_response(
    response: tokio::sync::oneshot::Receiver<Result<(), super::runner::RunnerRequestError>>,
) -> Result<(), SupervisorError> {
    response
        .await
        .map_err(|_| SupervisorError::Runner("runner exited before durable confirmation".into()))?
        .map_err(|error| SupervisorError::Runner(error.message))
}

fn store_error(error: StoreError) -> SupervisorError {
    match error {
        StoreError::SessionNotFound(_) => SupervisorError::NotFound,
        other => SupervisorError::Store(other.to_string()),
    }
}

fn recovery_error(error: RecoveryError) -> SupervisorError {
    match error {
        RecoveryError::Query(error) => query_error(error),
        invalid @ RecoveryError::Invalid { .. } => {
            SupervisorError::Unavailable(invalid.to_string())
        }
    }
}

fn query_error(error: QueryError) -> SupervisorError {
    match error {
        QueryError::SessionNotFound(_) => SupervisorError::NotFound,
        other => SupervisorError::Query(other.to_string()),
    }
}

fn panic_message(error: tokio::task::JoinError) -> String {
    if !error.is_panic() {
        return error.to_string();
    }
    let payload = error.into_panic();
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Contract: docs/zork-agent-architecture.md [STARTUP-03]
    fn a_hundred_thousand_finished_sessions_remain_lightweight_slots() {
        const SESSION_COUNT: usize = 100_000;
        let mut slots = HashMap::with_capacity(SESSION_COUNT);
        for index in 0..SESSION_COUNT {
            let session_id = format!("{index:026}");
            let slot = Arc::new(SessionSlot::new(
                session_id.clone(),
                SlotStatus::Unchecked,
                None,
            ));
            slot.set_public(SLOT_IDLE, true);
            slots.insert(session_id, slot);
        }

        assert_eq!(slots.len(), SESSION_COUNT);
        assert!(slots.values().all(|slot| {
            slot.public_status() == PublicSlotStatus::Idle && slot.finished.load(Ordering::Acquire)
        }));
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [SUPERVISOR-03]
    fn a_closed_channel_returns_the_exact_unsent_command_for_retry() {
        let (closed, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);

        let returned = match try_dispatch(&closed, String::from("original command")) {
            DispatchAttempt::Closed(returned) => returned,
            DispatchAttempt::Sent | DispatchAttempt::Full(_) => {
                panic!("the closed channel returns its unsent command")
            }
        };

        let (replacement, mut receiver) = tokio::sync::mpsc::channel(1);
        assert!(matches!(
            try_dispatch(&replacement, returned),
            DispatchAttempt::Sent
        ));
        assert_eq!(receiver.try_recv().unwrap(), "original command");
    }
}

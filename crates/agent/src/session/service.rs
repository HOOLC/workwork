//! Thin application facade over supervisor, query and live observation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use super::compression::SegmentCompressor;
use super::deadline::DeadlineScheduler;
use super::events::Selection;
use super::executor::ToolExecutor;
use super::model::ModelGateway;
use super::ports::{Clock, IdGenerator};
use super::query::{QueryError, SessionQuery};
use super::runner::{RunnerDependencies, RunnerObserver, RunnerOptions};
use super::state::SessionState;
use super::store::{EventEnvelope, SessionStore};
use super::supervisor::{SessionSlotView, SessionSupervisor, SupervisorError, SupervisorOptions};
use super::tools::ToolRegistry;

#[derive(Clone)]
pub struct ServiceOptions {
    pub runner: RunnerOptions,
    pub supervisor: SupervisorOptions,
    pub tool_timeout: Duration,
    pub tool_concurrency: usize,
    pub deadline_command_capacity: usize,
    pub deadline_wake_capacity: usize,
    pub live_event_capacity: usize,
    pub compression_retry_interval: Duration,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            runner: RunnerOptions::default(),
            supervisor: SupervisorOptions::default(),
            tool_timeout: Duration::from_secs(60 * 60),
            tool_concurrency: 64,
            deadline_command_capacity: 4096,
            deadline_wake_capacity: 4096,
            live_event_capacity: 256,
            compression_retry_interval: Duration::from_secs(30),
        }
    }
}

pub struct SessionService {
    supervisor: Arc<SessionSupervisor>,
    query: Arc<dyn SessionQuery>,
    deadlines: DeadlineScheduler,
    live: Arc<LiveEventHub>,
    compressor: SegmentCompressor,
}

pub struct ServiceDependencies {
    pub store: Arc<dyn SessionStore>,
    pub query: Arc<dyn SessionQuery>,
    pub model: Arc<dyn ModelGateway>,
    pub tools: Arc<ToolRegistry>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGenerator>,
}

impl SessionService {
    pub fn start(dependencies: ServiceDependencies, options: ServiceOptions) -> Arc<Self> {
        let query = dependencies.query.clone();
        let compressor = SegmentCompressor::start(
            dependencies.store.clone(),
            options.compression_retry_interval,
        );
        let live = Arc::new(LiveEventHub::new(options.live_event_capacity));
        let executor = Arc::new(ToolExecutor::with_clock(
            dependencies.tools.clone(),
            dependencies.clock.clone(),
            options.tool_timeout,
            options.tool_concurrency,
        ));
        let (deadlines, deadline_wakes) = DeadlineScheduler::start(
            dependencies.clock.clone(),
            options.deadline_command_capacity,
            options.deadline_wake_capacity,
        );
        let supervisor = SessionSupervisor::start(
            RunnerDependencies {
                store: dependencies.store,
                model: dependencies.model,
                tools: dependencies.tools,
                executor,
                deadlines: deadlines.clone(),
                clock: dependencies.clock,
                ids: dependencies.ids,
                observer: live.clone(),
                compressor: compressor.clone(),
                options: options.runner,
            },
            query.clone(),
            deadline_wakes,
            options.supervisor,
        );
        Arc::new(Self {
            supervisor,
            query,
            deadlines,
            live,
            compressor,
        })
    }

    pub async fn create_session(
        &self,
        selection: Selection,
        system_prompt: Option<String>,
        workspace: String,
        context: Option<zork_config::ContextConfig>,
    ) -> Result<String, SupervisorError> {
        self.supervisor
            .create_session(selection, system_prompt, workspace, context)
            .await
    }

    pub async fn submit_input(
        &self,
        session_id: &str,
        content: String,
    ) -> Result<(), SupervisorError> {
        self.supervisor.submit_input(session_id, content).await
    }

    pub async fn set_selection(
        &self,
        session_id: &str,
        selection: Selection,
    ) -> Result<(), SupervisorError> {
        self.supervisor.set_selection(session_id, selection).await
    }

    pub async fn cancel(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.supervisor.cancel_turn(session_id).await
    }

    pub async fn set_context(
        &self,
        session_id: &str,
        config: zork_config::ContextConfig,
    ) -> Result<(), SupervisorError> {
        self.supervisor.set_context(session_id, config).await
    }

    pub async fn delete(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.supervisor.delete(session_id).await
    }

    pub async fn state(&self, session_id: &str) -> Result<SessionState, SupervisorError> {
        self.supervisor.inspect(session_id).await
    }

    pub fn sessions(&self) -> Vec<SessionSlotView> {
        self.supervisor.list()
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.supervisor.contains(session_id)
    }

    pub fn history_after(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, QueryError> {
        self.query.after(session_id, cursor, limit)
    }

    pub fn scan_history_after(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        visit: &mut dyn FnMut(EventEnvelope) -> bool,
    ) -> Result<(), QueryError> {
        self.query.scan_after(session_id, cursor, visit)
    }

    pub fn history_before(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, QueryError> {
        self.query.before(session_id, cursor, limit)
    }

    pub fn subscribe(&self, session_id: &str) -> Result<SessionSubscription, SupervisorError> {
        if !self.contains(session_id) {
            return Err(SupervisorError::NotFound);
        }
        Ok(self.live.subscribe(session_id))
    }

    pub async fn shutdown(&self) {
        self.supervisor.shutdown().await;
        self.deadlines.shutdown().await;
        self.compressor.shutdown().await;
    }
}

#[derive(Clone, Debug)]
pub enum LiveSessionEvent {
    Durable(Arc<EventEnvelope>),
    TextDelta {
        session_id: String,
        generation: u64,
        step_id: String,
        text: String,
    },
}

pub struct SessionSubscription {
    receiver: tokio::sync::broadcast::Receiver<LiveSessionEvent>,
    _stream: Arc<SessionStream>,
}

impl SessionSubscription {
    pub async fn recv(
        &mut self,
    ) -> Result<LiveSessionEvent, tokio::sync::broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}

struct LiveEventHub {
    capacity: usize,
    streams: Mutex<HashMap<String, Weak<SessionStream>>>,
}

struct SessionStream {
    sender: tokio::sync::broadcast::Sender<LiveSessionEvent>,
}

impl LiveEventHub {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            streams: Mutex::new(HashMap::new()),
        }
    }

    fn subscribe(&self, session_id: &str) -> SessionSubscription {
        let stream = {
            let mut streams = self.streams.lock().expect("live event hub mutex poisoned");
            match streams.get(session_id).and_then(Weak::upgrade) {
                Some(stream) => stream,
                None => {
                    let (sender, _) = tokio::sync::broadcast::channel(self.capacity);
                    let stream = Arc::new(SessionStream { sender });
                    streams.insert(session_id.to_owned(), Arc::downgrade(&stream));
                    stream
                }
            }
        };
        SessionSubscription {
            receiver: stream.sender.subscribe(),
            _stream: stream,
        }
    }

    fn stream(&self, session_id: &str) -> Option<Arc<SessionStream>> {
        let mut streams = self.streams.lock().expect("live event hub mutex poisoned");
        let stream = streams.get(session_id).and_then(Weak::upgrade);
        if stream.is_none() {
            streams.remove(session_id);
        }
        stream
    }
}

impl RunnerObserver for LiveEventHub {
    fn persisted(&self, session_id: &str, events: &[EventEnvelope]) {
        let Some(stream) = self.stream(session_id) else {
            return;
        };
        for event in events {
            if event.event.is_history_visible() {
                let _ = stream
                    .sender
                    .send(LiveSessionEvent::Durable(Arc::new(event.clone())));
            }
        }
    }

    fn text_delta(&self, session_id: &str, generation: u64, step_id: &str, text: &str) {
        let Some(stream) = self.stream(session_id) else {
            return;
        };
        let _ = stream.sender.send(LiveSessionEvent::TextDelta {
            session_id: session_id.to_owned(),
            generation,
            step_id: step_id.to_owned(),
            text: text.to_owned(),
        });
    }
}

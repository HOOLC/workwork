//! Real-store startup fixture and measurement used by the 100k-session benchmark.

use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use ulid::Ulid;
use zork_agent::session::event_id::{EventId, EventIdError};
use zork_agent::session::events::{SessionEvent, TurnOutcome, EVENT_SCHEMA_VERSION};
use zork_agent::session::model::{
    ModelError, ModelGateway, ModelOutcome, ModelReleaseSuggestion, ModelRequest,
};
use zork_agent::session::ports::{SystemClock, SystemIdGenerator};
use zork_agent::session::query::{
    Commit, FileSessionQuery, QueryError, ReadResult, ReadSummary, SessionDiscovery, SessionQuery,
    SessionReadHint, SnapshotWindow,
};
use zork_agent::session::service::{ServiceDependencies, ServiceOptions, SessionService};
use zork_agent::session::store::{EventEnvelope, SessionStore, StoreError, StreamStore};
use zork_agent::session::supervisor::{PublicSlotStatus, SupervisorError};
use zork_agent::session::tools::ToolRegistry;
use zork_agent::session::wire::SessionSelection;

#[derive(Clone, Copy, Debug)]
pub struct RealStartupLimits {
    pub exact_recovery: Duration,
    pub prioritized_recovery: Duration,
    pub complete_startup: Duration,
}

impl Default for RealStartupLimits {
    fn default() -> Self {
        Self {
            exact_recovery: Duration::from_millis(100),
            prioritized_recovery: Duration::from_secs(1),
            complete_startup: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RealStartupPerformance {
    pub session_count: usize,
    pub discovery_scan_ms: u64,
    pub exact_recovery_ms: u64,
    pub prioritized_recovery_ms: u64,
    pub background_recovery_ms: u64,
    pub total_startup_ms: u64,
    pub peak_rss_bytes: Option<u64>,
    pub probe_count: usize,
    pub finished_probe_count: usize,
    pub recovery_count: usize,
    pub finished_recovery_count: usize,
    pub target_recovery_count: usize,
    pub prioritized_recovery_count: usize,
    pub model_request_count: usize,
    pub target_was_oldest: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StartupPerformanceError {
    #[error("startup performance fixture requires at least three sessions")]
    FixtureTooSmall,
    #[error("startup performance fixture worker panicked")]
    FixtureWorkerPanicked,
    #[error("startup performance fixture I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("startup performance fixture JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("startup performance fixture event ID failed: {0}")]
    EventId(#[from] EventIdError),
    #[error("startup store failed: {0}")]
    Store(#[from] StoreError),
    #[error("startup supervisor failed: {0}")]
    Supervisor(#[from] SupervisorError),
    #[error("startup measurement timed out while {0}")]
    Timeout(&'static str),
    #[error("startup measurement violated its fixture contract: {0}")]
    Contract(String),
}

/// Creates current-schema, normally finished sessions without putting fixture
/// construction, serialization or file allocation into the measured process.
pub fn prepare_real_startup_fixture(
    data_root: &Path,
    session_count: usize,
) -> Result<String, StartupPerformanceError> {
    if session_count < 3 {
        return Err(StartupPerformanceError::FixtureTooSmall);
    }

    let sessions_root = data_root.join("sessions");
    std::fs::create_dir_all(&sessions_root)?;

    // The requested session is written first and has the greatest ULID. It is
    // therefore last in discovery order both when mtimes differ and when the
    // filesystem timestamp resolution makes them equal.
    let target_index = session_count - 1;
    let target_session_id = fixture_session_id(target_index);
    write_finished_session(&sessions_root, target_index)?;

    let next = AtomicUsize::new(0);
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .min(16)
        .min(target_index.max(1));
    let results = std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            workers.push(scope.spawn(|| -> Result<(), StartupPerformanceError> {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= target_index {
                        return Ok(());
                    }
                    write_finished_session(&sessions_root, index)?;
                }
            }));
        }
        workers
            .into_iter()
            .map(std::thread::ScopedJoinHandle::join)
            .collect::<Vec<_>>()
    });
    for result in results {
        result.map_err(|_| StartupPerformanceError::FixtureWorkerPanicked)??;
    }

    Ok(target_session_id)
}

pub async fn measure_real_startup(
    data_root: &Path,
    session_count: usize,
    limits: RealStartupLimits,
) -> Result<RealStartupPerformance, StartupPerformanceError> {
    if session_count < 3 {
        return Err(StartupPerformanceError::FixtureTooSmall);
    }

    let started = Instant::now();
    let target_session_id = fixture_session_id(session_count - 1);
    let instrumentation = Arc::new(StartupInstrumentation::new(target_session_id.clone()));
    let store = Arc::new(StreamStore::open(data_root)?);
    let query = Arc::new(MeasuredStartupQuery {
        inner: FileSessionQuery::open(data_root),
        instrumentation: instrumentation.clone(),
    });
    let model = Arc::new(CountingModelGateway::default());
    let tools = Arc::new(ToolRegistry::default());
    let service = SessionService::start(
        ServiceDependencies {
            store: store as Arc<dyn SessionStore>,
            query: query as Arc<dyn SessionQuery>,
            model: model.clone() as Arc<dyn ModelGateway>,
            tools,
            clock: Arc::new(SystemClock),
            ids: Arc::new(SystemIdGenerator),
        },
        ServiceOptions::default(),
    );

    wait_until(
        remaining(limits.complete_startup, started.elapsed())?,
        "scanning real session directories",
        || instrumentation.discovery.paused.load(Ordering::Acquire),
    )
    .await?;
    let discovery_scan_ms = millis(started.elapsed());

    let exact_started = Instant::now();
    let exact_result =
        tokio::time::timeout(limits.exact_recovery, service.state(&target_session_id)).await;
    let exact_recovery_ms = millis(exact_started.elapsed());
    let background_started = Instant::now();
    instrumentation.discovery.release();
    let target_state = exact_result.map_err(|_| {
        StartupPerformanceError::Timeout("performing exact recovery for the requested session")
    })??;

    if target_state.last_turn_outcome != Some(TurnOutcome::Finished) {
        return Err(StartupPerformanceError::Contract(
            "the requested fixture session was not recovered as normally finished".into(),
        ));
    }
    if instrumentation.recoveries.load(Ordering::Acquire) != 1
        || instrumentation.target_recoveries.load(Ordering::Acquire) != 1
    {
        return Err(StartupPerformanceError::Contract(
            "background discovery was paused, but the exact request did not perform exactly one recovery"
                .into(),
        ));
    }

    wait_until(
        remaining(limits.complete_startup, started.elapsed())?,
        "reaching the controlled background recovery boundary",
        || {
            instrumentation
                .background_recovery
                .paused
                .load(Ordering::Acquire)
        },
    )
    .await?;
    let prioritized_session_id = instrumentation.prioritized_session_id().ok_or_else(|| {
        StartupPerformanceError::Contract(
            "discovery did not select a distinct session for prioritized recovery".into(),
        )
    })?;
    let prioritized_started = Instant::now();
    let prioritized_result = tokio::time::timeout(
        limits.prioritized_recovery,
        service.state(&prioritized_session_id),
    )
    .await;
    let prioritized_recovery_ms = millis(prioritized_started.elapsed());
    instrumentation.background_recovery.release();
    let prioritized_state = prioritized_result.map_err(|_| {
        StartupPerformanceError::Timeout(
            "prioritizing a requested session while background recovery was active",
        )
    })??;
    if prioritized_state.last_turn_outcome != Some(TurnOutcome::Finished) {
        return Err(StartupPerformanceError::Contract(
            "the prioritized fixture session was not recovered as normally finished".into(),
        ));
    }

    wait_until(
        remaining(limits.complete_startup, started.elapsed())?,
        "checking all discovered sessions",
        || {
            instrumentation.probes.load(Ordering::Acquire)
                + instrumentation.recoveries.load(Ordering::Acquire)
                >= session_count
        },
    )
    .await?;

    // Capture startup RSS before the validation call allocates a 100k-entry
    // public view. The view is only used to prove every slot reached its final
    // lightweight state; it is not part of normal startup residency.
    let peak_rss_bytes = peak_rss_bytes();
    wait_until(
        remaining(limits.complete_startup, started.elapsed())?,
        "observing all lightweight finished slots",
        || {
            let sessions = service.sessions();
            sessions.len() == session_count
                && sessions
                    .iter()
                    .all(|slot| slot.status == PublicSlotStatus::Idle && slot.finished)
        },
    )
    .await?;

    let performance = RealStartupPerformance {
        session_count,
        discovery_scan_ms,
        exact_recovery_ms,
        prioritized_recovery_ms,
        background_recovery_ms: millis(background_started.elapsed()),
        total_startup_ms: millis(started.elapsed()),
        peak_rss_bytes,
        probe_count: instrumentation.probes.load(Ordering::Acquire),
        finished_probe_count: instrumentation.finished_probes.load(Ordering::Acquire),
        recovery_count: instrumentation.recoveries.load(Ordering::Acquire),
        finished_recovery_count: instrumentation.finished_recoveries.load(Ordering::Acquire),
        target_recovery_count: instrumentation.target_recoveries.load(Ordering::Acquire),
        prioritized_recovery_count: instrumentation
            .prioritized_recoveries
            .load(Ordering::Acquire),
        model_request_count: model.requests.load(Ordering::Acquire),
        target_was_oldest: instrumentation.target_was_oldest.load(Ordering::Acquire),
    };
    service.shutdown().await;

    if performance.probe_count + performance.recovery_count != session_count
        || performance.probe_count != session_count - 2
        || performance.finished_probe_count != session_count - 2
        || performance.recovery_count != 2
        || performance.finished_recovery_count != 2
        || performance.target_recovery_count != 1
        || performance.prioritized_recovery_count != 1
        || performance.model_request_count != 0
        || !performance.target_was_oldest
    {
        return Err(StartupPerformanceError::Contract(format!(
            "unexpected completed measurement: {performance:?}"
        )));
    }

    Ok(performance)
}

fn write_finished_session(
    sessions_root: &Path,
    index: usize,
) -> Result<(), StartupPerformanceError> {
    let session_ulid = fixture_session_ulid(index);
    let session_id = session_ulid.to_string();
    let turn_id = "turn".to_owned();
    let events = [
        SessionEvent::SessionCreated {
            session_id: session_id.clone(),
            created_at_ms: i64::try_from(index).unwrap_or(i64::MAX),
            selection: SessionSelection {
                profile_id: "benchmark".into(),
                model: "benchmark".into(),
                thinking: "medium".into(),
            },
            system_prompt: None,
            workspace: "/benchmark".into(),
            tools: Vec::new(),
        },
        SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            started_at_ms: 0,
        },
        SessionEvent::TurnFinished {
            turn_id,
            outcome: TurnOutcome::Finished,
            outstanding: Vec::new(),
            finished_at_ms: 1,
        },
    ];
    let batch_count = u32::try_from(events.len()).expect("fixture batch count fits u32");
    let mut bytes = Vec::with_capacity(1024);
    let mut first_event_id = None;
    for (batch_index, event) in events.into_iter().enumerate() {
        let event_id = EventId::from_sequence(session_ulid, batch_index as u64 + 1)?.to_string();
        first_event_id.get_or_insert_with(|| event_id.clone());
        serde_json::to_writer(
            &mut bytes,
            &EventEnvelope {
                event_id,
                schema_version: EVENT_SCHEMA_VERSION,
                batch_index: batch_index as u32,
                batch_count,
                event,
            },
        )?;
        bytes.write_all(b"\n")?;
    }

    let segments = sessions_root.join(session_id).join("segments");
    std::fs::create_dir_all(&segments)?;
    std::fs::write(
        segments.join(format!("{}.jsonl", first_event_id.unwrap())),
        bytes,
    )?;
    Ok(())
}

fn fixture_session_ulid(index: usize) -> Ulid {
    Ulid::from(index as u128 + 1)
}

fn fixture_session_id(index: usize) -> String {
    fixture_session_ulid(index).to_string()
}

struct MeasuredStartupQuery {
    inner: FileSessionQuery,
    instrumentation: Arc<StartupInstrumentation>,
}

impl SessionQuery for MeasuredStartupQuery {
    fn exists(&self, session_id: &str) -> bool {
        self.inner.exists(session_id)
    }

    fn discover_sessions(&self) -> Result<Vec<SessionDiscovery>, QueryError> {
        let discovered = self.inner.discover_sessions()?;
        self.instrumentation
            .discovered_sessions
            .store(discovered.len(), Ordering::Release);
        self.instrumentation.target_was_oldest.store(
            discovered.last().is_some_and(|session| {
                session.session_id == self.instrumentation.target_session_id
            }),
            Ordering::Release,
        );
        self.instrumentation.set_prioritized_session(
            discovered
                .iter()
                .rev()
                .find(|session| session.session_id != self.instrumentation.target_session_id)
                .map(|session| session.session_id.clone()),
        );
        self.instrumentation.discovery.hold();
        Ok(discovered)
    }

    fn last_commit(
        &self,
        session_id: &str,
        hint: Option<&SessionReadHint>,
    ) -> Result<ReadResult<Option<Commit>>, QueryError> {
        self.instrumentation.background_recovery.hold();
        let read = self.inner.last_commit(session_id, hint)?;
        self.instrumentation.probes.fetch_add(1, Ordering::AcqRel);
        if read.diagnostics.is_empty()
            && read
                .value
                .as_ref()
                .and_then(Commit::last)
                .is_some_and(|envelope| {
                    matches!(
                        envelope.event,
                        SessionEvent::TurnFinished {
                            outcome: TurnOutcome::Finished,
                            ..
                        }
                    )
                })
        {
            self.instrumentation
                .finished_probes
                .fetch_add(1, Ordering::AcqRel);
        }
        Ok(read)
    }

    fn snapshot_windows(
        &self,
        session_id: &str,
        hint: Option<&SessionReadHint>,
        visit: &mut dyn FnMut(SnapshotWindow) -> bool,
    ) -> Result<ReadSummary, QueryError> {
        let prioritized_session_id = self.instrumentation.prioritized_session_id();
        let mut finished = false;
        let result = self
            .inner
            .snapshot_windows(session_id, hint, &mut |window| {
                finished |= window.commits.iter().any(|commit| {
                    commit.last().is_some_and(|envelope| {
                        matches!(
                            envelope.event,
                            SessionEvent::TurnFinished {
                                outcome: TurnOutcome::Finished,
                                ..
                            }
                        )
                    })
                });
                visit(window)
            })?;
        self.instrumentation
            .recoveries
            .fetch_add(1, Ordering::AcqRel);
        if finished {
            self.instrumentation
                .finished_recoveries
                .fetch_add(1, Ordering::AcqRel);
        }
        if session_id == self.instrumentation.target_session_id {
            self.instrumentation
                .target_recoveries
                .fetch_add(1, Ordering::AcqRel);
        }
        if prioritized_session_id.as_deref() == Some(session_id) {
            self.instrumentation
                .prioritized_recoveries
                .fetch_add(1, Ordering::AcqRel);
        }
        Ok(result)
    }

    fn all_commits_forward(
        &self,
        session_id: &str,
        visit: &mut dyn FnMut(Commit) -> bool,
    ) -> Result<ReadSummary, QueryError> {
        self.inner.all_commits_forward(session_id, visit)
    }

    fn scan_after(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        visit: &mut dyn FnMut(EventEnvelope) -> bool,
    ) -> Result<(), QueryError> {
        self.inner.scan_after(session_id, cursor, visit)
    }

    fn before(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, QueryError> {
        self.inner.before(session_id, cursor, limit)
    }

    fn event(&self, session_id: &str, event_id: &str) -> Result<Option<EventEnvelope>, QueryError> {
        self.inner.event(session_id, event_id)
    }
}

struct StartupInstrumentation {
    target_session_id: String,
    discovery: PauseGate,
    background_recovery: PauseGate,
    prioritized_session_id: Mutex<Option<String>>,
    discovered_sessions: AtomicUsize,
    probes: AtomicUsize,
    finished_probes: AtomicUsize,
    recoveries: AtomicUsize,
    finished_recoveries: AtomicUsize,
    target_recoveries: AtomicUsize,
    prioritized_recoveries: AtomicUsize,
    target_was_oldest: AtomicBool,
}

impl StartupInstrumentation {
    fn new(target_session_id: String) -> Self {
        Self {
            target_session_id,
            discovery: PauseGate::default(),
            background_recovery: PauseGate::default(),
            prioritized_session_id: Mutex::new(None),
            discovered_sessions: AtomicUsize::new(0),
            probes: AtomicUsize::new(0),
            finished_probes: AtomicUsize::new(0),
            recoveries: AtomicUsize::new(0),
            finished_recoveries: AtomicUsize::new(0),
            target_recoveries: AtomicUsize::new(0),
            prioritized_recoveries: AtomicUsize::new(0),
            target_was_oldest: AtomicBool::new(false),
        }
    }

    fn set_prioritized_session(&self, prioritized_session_id: Option<String>) {
        *self
            .prioritized_session_id
            .lock()
            .expect("prioritized session mutex poisoned") = prioritized_session_id;
    }

    fn prioritized_session_id(&self) -> Option<String> {
        self.prioritized_session_id
            .lock()
            .expect("prioritized session mutex poisoned")
            .clone()
    }
}

#[derive(Default)]
struct PauseGate {
    paused: AtomicBool,
    state: Mutex<bool>,
    changed: Condvar,
}

impl PauseGate {
    fn hold(&self) {
        let mut released = self
            .state
            .lock()
            .expect("startup pause gate mutex poisoned");
        self.paused.store(true, Ordering::Release);
        self.changed.notify_all();
        while !*released {
            released = self
                .changed
                .wait(released)
                .expect("startup pause gate mutex poisoned while paused");
        }
    }

    fn release(&self) {
        let mut released = self
            .state
            .lock()
            .expect("startup pause gate mutex poisoned");
        *released = true;
        self.changed.notify_all();
    }
}

#[derive(Default)]
struct CountingModelGateway {
    requests: AtomicUsize,
}

impl ModelGateway for CountingModelGateway {
    fn complete<'a>(
        &'a self,
        _request: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelOutcome, ModelError>> + Send + 'a>> {
        self.requests.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Err(ModelError::Unavailable) })
    }

    fn release(&self, _suggestion: ModelReleaseSuggestion<'_>) {}
}

async fn wait_until(
    timeout: Duration,
    stage: &'static str,
    condition: impl Fn() -> bool,
) -> Result<(), StartupPerformanceError> {
    if condition() {
        return Ok(());
    }
    tokio::time::timeout(timeout, async {
        loop {
            tokio::time::sleep(Duration::from_millis(2)).await;
            if condition() {
                return;
            }
        }
    })
    .await
    .map_err(|_| StartupPerformanceError::Timeout(stage))
}

fn remaining(limit: Duration, elapsed: Duration) -> Result<Duration, StartupPerformanceError> {
    limit
        .checked_sub(elapsed)
        .ok_or(StartupPerformanceError::Timeout("completing startup"))
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(unix)]
fn peak_rss_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage initializes the supplied rusage for the current process
    // and does not retain the pointer.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: getrusage returned success, so the structure is initialized.
    let rss = unsafe { usage.assume_init() }.ru_maxrss.max(0) as u64;
    #[cfg(target_os = "macos")]
    {
        Some(rss)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(rss.saturating_mul(1024))
    }
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    // Contract: docs/zork-agent-architecture.md [STARTUP-02, STARTUP-03, PERF-03]
    async fn real_store_measurement_prioritizes_then_recovers_finished_sessions_once() {
        let root = tempfile::tempdir().unwrap();
        let data_root = root.path().join("data");
        let target = prepare_real_startup_fixture(&data_root, 128).unwrap();

        let measured = measure_real_startup(
            &data_root,
            128,
            RealStartupLimits {
                exact_recovery: Duration::from_secs(5),
                prioritized_recovery: Duration::from_secs(5),
                complete_startup: Duration::from_secs(30),
            },
        )
        .await
        .unwrap();

        assert_eq!(target, fixture_session_id(127));
        assert_eq!(measured.probe_count, 126);
        assert_eq!(measured.finished_probe_count, 126);
        assert_eq!(measured.recovery_count, 2);
        assert_eq!(measured.finished_recovery_count, 2);
        assert_eq!(measured.target_recovery_count, 1);
        assert_eq!(measured.prioritized_recovery_count, 1);
        assert_eq!(measured.model_request_count, 0);
        assert!(measured.target_was_oldest);
    }
}

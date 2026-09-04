//! Repeatable real-file measurements for every public session query shape.

use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use ulid::Ulid;
use zork_agent::session::event_id::{EventId, EventIdError};
use zork_agent::session::events::{Selection, SessionEvent, EVENT_SCHEMA_VERSION};
use zork_agent::session::query::{FileSessionQuery, QueryError, SessionQuery, WindowOrigin};
use zork_agent::session::store::EventEnvelope;

#[derive(Clone, Copy, Debug)]
pub struct QueryApiPerformance {
    pub segment_count: usize,
    pub event_count: usize,
    pub iterations: usize,
    pub last_commit: Duration,
    pub snapshot_window: Duration,
    pub history_after: Duration,
    pub history_before: Duration,
    pub event_lookup: Duration,
    pub discovery: Duration,
    pub exists: Duration,
    pub all_commits: Duration,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct HistoryFragmentPerformance {
    pub event_count: usize,
    pub fragment_len: usize,
    pub query_count: usize,
    pub worker_count: usize,
    pub serial: Duration,
    pub parallel: Duration,
    pub rss_before_queries_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
}

impl HistoryFragmentPerformance {
    pub fn serial_queries_per_second(self) -> f64 {
        self.query_count as f64 / self.serial.as_secs_f64()
    }

    pub fn parallel_queries_per_second(self) -> f64 {
        self.query_count as f64 / self.parallel.as_secs_f64()
    }
}

impl QueryApiPerformance {
    pub fn all_commits_per_second(self) -> f64 {
        self.event_count as f64 / self.all_commits.as_secs_f64()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QueryPerformanceError {
    #[error(
        "query benchmark requires at least two segments, 128 events per segment and one iteration"
    )]
    InvalidFixture,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    EventId(#[from] EventIdError),
    #[error(transparent)]
    Query(#[from] QueryError),
    #[error("query benchmark fixture violated its contract: {0}")]
    Contract(String),
}

pub fn measure_query_apis(
    segment_count: usize,
    events_per_segment: usize,
    iterations: usize,
) -> Result<QueryApiPerformance, QueryPerformanceError> {
    if segment_count < 2 || events_per_segment < 128 || iterations == 0 {
        return Err(QueryPerformanceError::InvalidFixture);
    }
    let fixture = QueryFixture::create(segment_count, events_per_segment)?;
    let query = FileSessionQuery::open(fixture.root.path());

    verify_query_results(&query, &fixture)?;

    let last_commit = measure(iterations, || {
        let read = query.last_commit(&fixture.session_id, None)?;
        black_box(read.value);
        Ok(())
    })?;
    let snapshot_window = measure(iterations, || {
        let mut windows = 0;
        query.snapshot_windows(&fixture.session_id, None, &mut |window| {
            windows += 1;
            black_box(window);
            true
        })?;
        black_box(windows);
        Ok(())
    })?;
    let history_after = measure(iterations, || {
        black_box(query.after(&fixture.session_id, Some(&fixture.cursor_event_id), 20)?);
        Ok(())
    })?;
    let history_before = measure(iterations, || {
        black_box(query.before(&fixture.session_id, Some(&fixture.cursor_event_id), 20)?);
        Ok(())
    })?;
    let event_lookup = measure(iterations, || {
        black_box(query.event(&fixture.session_id, &fixture.cursor_event_id)?);
        Ok(())
    })?;
    let discovery = measure(iterations, || {
        black_box(query.discover_sessions()?);
        Ok(())
    })?;
    let exists_started = Instant::now();
    for _ in 0..iterations {
        black_box(query.exists(black_box(&fixture.session_id)));
    }
    let exists = per_call(exists_started.elapsed(), iterations);

    let all_started = Instant::now();
    let mut commit_count = 0usize;
    query.all_commits_forward(&fixture.session_id, &mut |commit| {
        commit_count += 1;
        black_box(commit);
        false
    })?;
    let all_commits = all_started.elapsed();
    if commit_count != fixture.event_count {
        return Err(QueryPerformanceError::Contract(format!(
            "all_commits_forward returned {commit_count} commits for {} one-event commits",
            fixture.event_count
        )));
    }

    Ok(QueryApiPerformance {
        segment_count,
        event_count: fixture.event_count,
        iterations,
        last_commit,
        snapshot_window,
        history_after,
        history_before,
        event_lookup,
        discovery,
        exists,
        all_commits,
    })
}

pub fn measure_history_fragment_queries(
    segment_count: usize,
    events_per_segment: usize,
    fragment_len: usize,
    query_count: usize,
    worker_count: usize,
) -> Result<HistoryFragmentPerformance, QueryPerformanceError> {
    if fragment_len < 128 || query_count < 1_000 || worker_count < 2 {
        return Err(QueryPerformanceError::InvalidFixture);
    }
    let fixture = QueryFixture::create(segment_count, events_per_segment)?;
    if fixture.event_count <= fragment_len.saturating_mul(2).saturating_add(2) {
        return Err(QueryPerformanceError::InvalidFixture);
    }
    let query = FileSessionQuery::open(fixture.root.path());
    let rss_before_queries_bytes = peak_rss_bytes();

    let requests = history_requests(&fixture, fragment_len, query_count)?;
    let serial_started = Instant::now();
    run_history_requests(&query, &fixture.session_id, &requests, fragment_len)?;
    let serial = serial_started.elapsed();

    let requested_workers = worker_count.min(query_count);
    let chunk_len = query_count.div_ceil(requested_workers);
    let worker_count = requests.chunks(chunk_len).len();
    let barrier = Arc::new(Barrier::new(worker_count + 1));
    let parallel = std::thread::scope(|scope| -> Result<Duration, QueryPerformanceError> {
        let handles = requests
            .chunks(chunk_len)
            .map(|chunk| {
                let barrier = Arc::clone(&barrier);
                let query = &query;
                let session_id = fixture.session_id.as_str();
                scope.spawn(move || {
                    barrier.wait();
                    run_history_requests(query, session_id, chunk, fragment_len)
                })
            })
            .collect::<Vec<_>>();
        let started = Instant::now();
        barrier.wait();
        for handle in handles {
            handle.join().map_err(|_| {
                QueryPerformanceError::Contract("parallel history query worker panicked".into())
            })??;
        }
        Ok(started.elapsed())
    })?;

    Ok(HistoryFragmentPerformance {
        event_count: fixture.event_count,
        fragment_len,
        query_count,
        worker_count,
        serial,
        parallel,
        rss_before_queries_bytes,
        peak_rss_bytes: peak_rss_bytes(),
    })
}

#[derive(Clone)]
struct HistoryRequest {
    cursor: String,
    before: bool,
}

fn history_requests(
    fixture: &QueryFixture,
    fragment_len: usize,
    query_count: usize,
) -> Result<Vec<HistoryRequest>, QueryPerformanceError> {
    let session_ulid: Ulid = fixture
        .session_id
        .parse()
        .expect("fixture session ID remains a ULID");
    let first = fragment_len + 2;
    let span = fixture.event_count - fragment_len.saturating_mul(2) - 2;
    (0..query_count)
        .map(|index| {
            let mut sequence = first + index.saturating_mul(104_729) % span;
            if sequence == fixture.snapshot_sequence {
                sequence += 1;
            }
            Ok(HistoryRequest {
                cursor: EventId::from_sequence(session_ulid, sequence as u64)?.to_string(),
                before: index % 2 == 0,
            })
        })
        .collect()
}

fn run_history_requests(
    query: &FileSessionQuery,
    session_id: &str,
    requests: &[HistoryRequest],
    fragment_len: usize,
) -> Result<(), QueryPerformanceError> {
    for request in requests {
        let events = if request.before {
            query.before(session_id, Some(&request.cursor), fragment_len)?
        } else {
            query.after(session_id, Some(&request.cursor), fragment_len)?
        };
        if events.len() != fragment_len {
            return Err(QueryPerformanceError::Contract(format!(
                "history fragment returned {} events instead of {fragment_len}",
                events.len()
            )));
        }
        black_box(events);
    }
    Ok(())
}

fn measure(
    iterations: usize,
    mut operation: impl FnMut() -> Result<(), QueryPerformanceError>,
) -> Result<Duration, QueryPerformanceError> {
    let started = Instant::now();
    for _ in 0..iterations {
        operation()?;
    }
    Ok(per_call(started.elapsed(), iterations))
}

fn per_call(elapsed: Duration, iterations: usize) -> Duration {
    elapsed / u32::try_from(iterations).unwrap_or(u32::MAX)
}

fn verify_query_results(
    query: &FileSessionQuery,
    fixture: &QueryFixture,
) -> Result<(), QueryPerformanceError> {
    let last = query.last_commit(&fixture.session_id, None)?;
    if !last.diagnostics.is_empty()
        || last
            .value
            .as_ref()
            .and_then(|commit| commit.last())
            .map(|event| event.event_id.as_str())
            != Some(fixture.last_event_id.as_str())
    {
        return Err(QueryPerformanceError::Contract(
            "last_commit did not return the fixture tail".into(),
        ));
    }

    let mut origin = None;
    let mut suffix_commits = 0;
    query.snapshot_windows(&fixture.session_id, None, &mut |window| {
        origin = Some(window.origin);
        suffix_commits = window.commits.len();
        true
    })?;
    if !matches!(origin, Some(WindowOrigin::Snapshot(_)))
        || suffix_commits != fixture.events_after_snapshot
    {
        return Err(QueryPerformanceError::Contract(
            "snapshot_windows did not return the newest bounded suffix".into(),
        ));
    }

    if query
        .after(&fixture.session_id, Some(&fixture.cursor_event_id), 20)?
        .len()
        != 20
        || query
            .before(&fixture.session_id, Some(&fixture.cursor_event_id), 20)?
            .len()
            != 20
        || query
            .event(&fixture.session_id, &fixture.cursor_event_id)?
            .is_none()
        || !query.exists(&fixture.session_id)
        || query.discover_sessions()?.len() != 1
    {
        return Err(QueryPerformanceError::Contract(
            "one or more query APIs returned an unexpected fixture result".into(),
        ));
    }
    Ok(())
}

struct QueryFixture {
    root: TempDir,
    session_id: String,
    event_count: usize,
    cursor_event_id: String,
    last_event_id: String,
    events_after_snapshot: usize,
    snapshot_sequence: usize,
}

impl QueryFixture {
    fn create(
        segment_count: usize,
        events_per_segment: usize,
    ) -> Result<Self, QueryPerformanceError> {
        let root = tempfile::tempdir()?;
        let session_ulid: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
            .parse()
            .expect("benchmark session ULID is valid");
        let session_id = session_ulid.to_string();
        let segments = root
            .path()
            .join("sessions")
            .join(&session_id)
            .join("segments");
        std::fs::create_dir_all(&segments)?;

        let event_count = segment_count.saturating_mul(events_per_segment);
        let snapshot_sequence = event_count - 64;
        let cursor_sequence = event_count - 32;
        let cursor_event_id =
            EventId::from_sequence(session_ulid, cursor_sequence as u64)?.to_string();
        let last_event_id = EventId::from_sequence(session_ulid, event_count as u64)?.to_string();

        for segment_index in 0..segment_count {
            let first_sequence = segment_index * events_per_segment + 1;
            let first_event_id =
                EventId::from_sequence(session_ulid, first_sequence as u64)?.to_string();
            let file = std::fs::File::create(segments.join(format!("{first_event_id}.jsonl")))?;
            let mut writer = BufWriter::new(file);
            for sequence in first_sequence..first_sequence + events_per_segment {
                let event = if sequence == 1 {
                    SessionEvent::SessionCreated {
                        session_id: session_id.clone(),
                        created_at_ms: 0,
                        selection: selection(0),
                        system_prompt: None,
                        workspace: "/query-benchmark".into(),
                        tools: Vec::new(),
                    }
                } else if sequence == snapshot_sequence {
                    SessionEvent::Snapshot {
                        state_schema_version: 1,
                        state: serde_json::json!({"fixture": true}),
                    }
                } else {
                    SessionEvent::SelectionChanged {
                        selection: selection(sequence),
                    }
                };
                serde_json::to_writer(
                    &mut writer,
                    &EventEnvelope {
                        event_id: EventId::from_sequence(session_ulid, sequence as u64)?
                            .to_string(),
                        schema_version: EVENT_SCHEMA_VERSION,
                        batch_index: 0,
                        batch_count: 1,
                        event,
                    },
                )?;
                writer.write_all(b"\n")?;
            }
            writer.flush()?;
        }

        Ok(Self {
            root,
            session_id,
            event_count,
            cursor_event_id,
            last_event_id,
            events_after_snapshot: event_count - snapshot_sequence,
            snapshot_sequence,
        })
    }
}

fn selection(sequence: usize) -> Selection {
    Selection {
        profile_id: "benchmark".into(),
        model: format!("model-{sequence}"),
        thinking: "medium".into(),
    }
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

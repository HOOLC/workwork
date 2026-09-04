//! Large, repeatable random-access pressure workload for `SessionQuery`.

use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use ulid::Ulid;
use zork_agent::session::event_id::EventId;
use zork_agent::session::events::{Input, Selection, SessionEvent, EVENT_SCHEMA_VERSION};
use zork_agent::session::query::{FileSessionQuery, SessionQuery};
use zork_agent::session::store::EventEnvelope;

use crate::query_performance::QueryPerformanceError;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QueryPressureManifest {
    pub session_count: usize,
    pub fragments_per_session: usize,
    pub target_fragment_bytes: usize,
    pub logical_bytes: u64,
    pub physical_bytes: u64,
    pub event_count: usize,
    sessions: Vec<PressureSession>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PressureSession {
    session_id: String,
    event_count: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct QueryPressureMode {
    pub workers: usize,
    pub elapsed: Duration,
    pub queries_per_second: f64,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QueryPressurePerformance {
    pub query_count: usize,
    pub max_query_events: usize,
    pub warmup: Duration,
    pub modes: Vec<QueryPressureMode>,
    pub peak_rss_bytes: Option<u64>,
}

pub fn prepare_query_pressure_fixture(
    data_root: &Path,
    session_count: usize,
    fragments_per_session: usize,
    target_fragment_bytes: usize,
    event_payload_bytes: usize,
) -> Result<QueryPressureManifest, QueryPerformanceError> {
    if session_count == 0
        || fragments_per_session == 0
        || target_fragment_bytes < 1024
        || event_payload_bytes < 128
    {
        return Err(QueryPerformanceError::InvalidFixture);
    }
    let sessions_root = data_root.join("sessions");
    std::fs::create_dir_all(&sessions_root)?;
    let next = AtomicUsize::new(0);
    let finished = AtomicUsize::new(0);
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .min(session_count);
    let sessions = std::thread::scope(|scope| {
        let handles = (0..worker_count)
            .map(|_| {
                scope.spawn(
                    || -> Result<Vec<(usize, PressureSession, u64, u64)>, QueryPerformanceError> {
                        let mut completed = Vec::new();
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            if index >= session_count {
                                return Ok(completed);
                            }
                            let (session, logical, physical) = write_pressure_session(
                                &sessions_root,
                                index,
                                fragments_per_session,
                                target_fragment_bytes,
                                event_payload_bytes,
                            )?;
                            completed.push((index, session, logical, physical));
                            let finished = finished.fetch_add(1, Ordering::Relaxed) + 1;
                            if finished.is_multiple_of(10) || finished == session_count {
                                eprintln!("prepared {finished}/{session_count} pressure sessions");
                            }
                        }
                    },
                )
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(std::thread::ScopedJoinHandle::join)
            .collect::<Vec<_>>()
    });
    let mut ordered = Vec::with_capacity(session_count);
    let mut logical_bytes = 0u64;
    let mut physical_bytes = 0u64;
    for result in sessions {
        for (index, session, logical, physical) in result.map_err(|_| {
            QueryPerformanceError::Contract("pressure fixture worker panicked".into())
        })?? {
            logical_bytes = logical_bytes.saturating_add(logical);
            physical_bytes = physical_bytes.saturating_add(physical);
            ordered.push((index, session));
        }
    }
    ordered.sort_by_key(|(index, _)| *index);
    let sessions = ordered
        .into_iter()
        .map(|(_, session)| session)
        .collect::<Vec<_>>();
    let event_count = sessions.iter().map(|session| session.event_count).sum();
    Ok(QueryPressureManifest {
        session_count,
        fragments_per_session,
        target_fragment_bytes,
        logical_bytes,
        physical_bytes,
        event_count,
        sessions,
    })
}

pub fn measure_query_pressure(
    data_root: &Path,
    manifest: &QueryPressureManifest,
    query_count: usize,
    max_query_events: usize,
    worker_counts: &[usize],
) -> Result<QueryPressurePerformance, QueryPerformanceError> {
    if query_count == 0 || max_query_events == 0 || worker_counts.is_empty() {
        return Err(QueryPerformanceError::InvalidFixture);
    }
    let requests = pressure_requests(manifest, query_count, max_query_events)?;
    let query = FileSessionQuery::open(data_root);
    eprintln!("pressure initial peak RSS: {:?}", peak_rss_bytes());
    let warmup_started = Instant::now();
    run_pressure_requests(&query, &requests, 1)?;
    let warmup = warmup_started.elapsed();
    eprintln!(
        "pressure warmup completed in {:.2}s; peak RSS {:?}",
        warmup.as_secs_f64(),
        peak_rss_bytes()
    );

    let mut modes = Vec::with_capacity(worker_counts.len());
    for workers in worker_counts.iter().copied() {
        let (elapsed, latencies) = run_pressure_requests(&query, &requests, workers)?;
        eprintln!(
            "pressure: {workers} workers completed {query_count} queries in {:.2}s; peak RSS {:?}",
            elapsed.as_secs_f64(),
            peak_rss_bytes()
        );
        modes.push(QueryPressureMode {
            workers,
            elapsed,
            queries_per_second: query_count as f64 / elapsed.as_secs_f64(),
            p50: percentile(&latencies, 50),
            p95: percentile(&latencies, 95),
            p99: percentile(&latencies, 99),
        });
    }
    Ok(QueryPressurePerformance {
        query_count,
        max_query_events,
        warmup,
        modes,
        peak_rss_bytes: peak_rss_bytes(),
    })
}

fn write_pressure_session(
    sessions_root: &Path,
    session_index: usize,
    fragments_per_session: usize,
    target_fragment_bytes: usize,
    event_payload_bytes: usize,
) -> Result<(PressureSession, u64, u64), QueryPerformanceError> {
    let session_ulid = Ulid::from(session_index as u128 + 1);
    let session_id = session_ulid.to_string();
    let segments = sessions_root.join(&session_id).join("segments");
    std::fs::create_dir_all(&segments)?;
    let payload = "x".repeat(event_payload_bytes);
    let mut line = Vec::with_capacity(event_payload_bytes + 512);
    let mut sequence = 1usize;
    let mut logical_bytes = 0u64;
    let mut physical_bytes = 0u64;
    for fragment_index in 0..fragments_per_session {
        let first_event_id = EventId::from_sequence(session_ulid, sequence as u64)?.to_string();
        let path = segments.join(format!("{first_event_id}.jsonl.zst"));
        let file = std::fs::File::create(&path)?;
        let encoder = zstd::stream::write::Encoder::new(file, 1)?;
        let mut writer = BufWriter::new(encoder);
        let mut fragment_bytes = 0usize;
        while fragment_bytes < target_fragment_bytes {
            let event = if sequence == 1 {
                SessionEvent::SessionCreated {
                    session_id: session_id.clone(),
                    created_at_ms: 0,
                    selection: Selection {
                        profile_id: "pressure".into(),
                        model: "pressure".into(),
                        thinking: "medium".into(),
                    },
                    system_prompt: None,
                    workspace: "/query-pressure".into(),
                    tools: Vec::new(),
                }
            } else {
                SessionEvent::InputAppended {
                    input: Input {
                        input_id: format!("input-{sequence}"),
                        content: format!("{session_index}:{fragment_index}:{sequence}:{payload}"),
                        received_at_ms: sequence as i64,
                    },
                }
            };
            line.clear();
            serde_json::to_writer(
                &mut line,
                &EventEnvelope {
                    event_id: EventId::from_sequence(session_ulid, sequence as u64)?.to_string(),
                    schema_version: EVENT_SCHEMA_VERSION,
                    batch_index: 0,
                    batch_count: 1,
                    event,
                },
            )?;
            line.push(b'\n');
            writer.write_all(&line)?;
            fragment_bytes = fragment_bytes.saturating_add(line.len());
            sequence += 1;
        }
        writer.flush()?;
        writer
            .into_inner()
            .map_err(|error| error.into_error())?
            .finish()?
            .sync_data()?;
        logical_bytes = logical_bytes.saturating_add(fragment_bytes as u64);
        physical_bytes = physical_bytes.saturating_add(path.metadata()?.len());
    }
    Ok((
        PressureSession {
            session_id,
            event_count: sequence - 1,
        },
        logical_bytes,
        physical_bytes,
    ))
}

#[derive(Clone)]
struct PressureRequest {
    session_id: String,
    cursor: String,
    limit: usize,
    before: bool,
}

fn pressure_requests(
    manifest: &QueryPressureManifest,
    query_count: usize,
    max_query_events: usize,
) -> Result<Vec<PressureRequest>, QueryPerformanceError> {
    let mut random = SplitMix64(0x7a6f_726b_7175_6572);
    let mut requests = Vec::with_capacity(query_count);
    for _ in 0..query_count {
        let session = &manifest.sessions[random.index(manifest.sessions.len())];
        if session.event_count <= max_query_events.saturating_mul(2).saturating_add(1) {
            return Err(QueryPerformanceError::InvalidFixture);
        }
        let session_ulid = session
            .session_id
            .parse::<Ulid>()
            .expect("pressure fixture session ID remains valid");
        let span = session.event_count - max_query_events.saturating_mul(2);
        let sequence = max_query_events + 1 + random.index(span);
        requests.push(PressureRequest {
            session_id: session.session_id.clone(),
            cursor: EventId::from_sequence(session_ulid, sequence as u64)?.to_string(),
            limit: 1 + random.index(max_query_events),
            before: random.next() & 1 == 0,
        });
    }
    Ok(requests)
}

fn run_pressure_requests(
    query: &FileSessionQuery,
    requests: &[PressureRequest],
    workers: usize,
) -> Result<(Duration, Vec<Duration>), QueryPerformanceError> {
    let workers = workers.max(1).min(requests.len());
    let barrier = Arc::new(Barrier::new(workers + 1));
    std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|worker| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || -> Result<Vec<Duration>, QueryPerformanceError> {
                    let mut latencies = Vec::with_capacity(requests.len().div_ceil(workers));
                    barrier.wait();
                    for request in requests.iter().skip(worker).step_by(workers) {
                        let started = Instant::now();
                        let events = if request.before {
                            query.before(
                                &request.session_id,
                                Some(&request.cursor),
                                request.limit,
                            )?
                        } else {
                            query.after(
                                &request.session_id,
                                Some(&request.cursor),
                                request.limit,
                            )?
                        };
                        latencies.push(started.elapsed());
                        if events.len() != request.limit {
                            return Err(QueryPerformanceError::Contract(format!(
                                "pressure query returned {} events instead of {}",
                                events.len(),
                                request.limit
                            )));
                        }
                        black_box(events);
                    }
                    Ok(latencies)
                })
            })
            .collect::<Vec<_>>();
        let started = Instant::now();
        barrier.wait();
        let mut latencies = Vec::with_capacity(requests.len());
        for handle in handles {
            latencies.extend(handle.join().map_err(|_| {
                QueryPerformanceError::Contract("pressure query worker panicked".into())
            })??);
        }
        Ok((started.elapsed(), latencies))
    })
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    let mut values = values.to_vec();
    values.sort_unstable();
    let index = values
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1)
        .min(values.len().saturating_sub(1));
    values.get(index).copied().unwrap_or_default()
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, upper: usize) -> usize {
        (self.next() as usize) % upper
    }
}

#[cfg(unix)]
fn peak_rss_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage initializes the supplied rusage and keeps no pointer.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: getrusage returned success.
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

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use ulid::Ulid;
use zork_agent::session::event_id::EventId;
use zork_agent::session::events::{Input, Selection, SessionEvent, EVENT_SCHEMA_VERSION};
use zork_agent::session::store::{EventEnvelope, SessionStore, StreamStore};

const DEFAULT_SEGMENT_MIB: u64 = 32;
const MAX_SEGMENT_MIB: u64 = 32;
const MAX_PEAK_RSS_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
struct CompressionPerformance {
    raw_bytes: u64,
    compressed_bytes: u64,
    elapsed_ms: u64,
    peak_rss_bytes: Option<u64>,
}

impl CompressionPerformance {
    fn throughput_mib_per_second(&self) -> f64 {
        self.raw_bytes as f64 / (1024.0 * 1024.0) / (self.elapsed_ms.max(1) as f64 / 1_000.0)
    }

    fn ratio(&self) -> f64 {
        self.raw_bytes as f64 / self.compressed_bytes.max(1) as f64
    }
}

// Contract: docs/zork-agent-architecture.md [SEGMENT-02]
fn main() {
    let arguments = std::env::args()
        .skip(1)
        .filter(|value| value != "--bench")
        .collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("--measure-child") {
        measure_child(&arguments);
        return;
    }

    let segment_mib = arguments
        .first()
        .map(|value| {
            value
                .parse::<u64>()
                .expect("segment MiB must be an integer")
        })
        .unwrap_or(DEFAULT_SEGMENT_MIB);
    assert!(
        (1..=MAX_SEGMENT_MIB).contains(&segment_mib),
        "segment MiB must be between 1 and {MAX_SEGMENT_MIB}"
    );

    let root = tempfile::tempdir().expect("create compression benchmark root");
    let data_root = root.path().join("data");
    let (session_id, first_event_id) =
        prepare_fixture(&data_root, segment_mib * 1024 * 1024).expect("prepare segment fixture");
    let output = Command::new(std::env::current_exe().expect("resolve benchmark executable"))
        .arg("--measure-child")
        .arg(&data_root)
        .arg(&session_id)
        .arg(&first_event_id)
        .output()
        .expect("start compression measurement child");
    assert!(
        output.status.success(),
        "compression child failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let measured: CompressionPerformance =
        serde_json::from_slice(&output.stdout).expect("parse compression report");
    let peak_rss_bytes = measured
        .peak_rss_bytes
        .expect("the release compression benchmark requires Unix RSS measurement");

    assert!(
        peak_rss_bytes <= MAX_PEAK_RSS_BYTES,
        "compression peak RSS was {:.1} MiB",
        peak_rss_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "segment compression: {:.1} MiB raw -> {:.1} MiB zstd ({:.2}x), {} ms, {:.1} MiB/s, peak RSS {:.1} MiB",
        measured.raw_bytes as f64 / (1024.0 * 1024.0),
        measured.compressed_bytes as f64 / (1024.0 * 1024.0),
        measured.ratio(),
        measured.elapsed_ms,
        measured.throughput_mib_per_second(),
        peak_rss_bytes as f64 / (1024.0 * 1024.0),
    );
}

fn prepare_fixture(data_root: &Path, target_bytes: u64) -> std::io::Result<(String, String)> {
    let session_ulid: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session_id = session_ulid.to_string();
    let segments = data_root
        .join("sessions")
        .join(&session_id)
        .join("segments");
    std::fs::create_dir_all(&segments)?;
    let first_event_id = EventId::from_sequence(session_ulid, 1)
        .expect("fixture event ID")
        .to_string();
    let sealed_path = segments.join(format!("{first_event_id}.jsonl"));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&sealed_path)?;
    let mut writer = BufWriter::new(file);
    let mut written = write_event(
        &mut writer,
        EventEnvelope {
            event_id: first_event_id.clone(),
            schema_version: EVENT_SCHEMA_VERSION,
            batch_index: 0,
            batch_count: 1,
            event: SessionEvent::SessionCreated {
                session_id: session_id.clone(),
                created_at_ms: 1,
                selection: Selection {
                    profile_id: "benchmark".into(),
                    model: "benchmark".into(),
                    thinking: "medium".into(),
                },
                system_prompt: None,
                workspace: "/benchmark".into(),
                tools: Vec::new(),
            },
        },
    )?;
    let mut sequence = 2u64;
    while written < target_bytes {
        let event_id = EventId::from_sequence(session_ulid, sequence)
            .expect("fixture event ID")
            .to_string();
        written += write_event(
            &mut writer,
            EventEnvelope {
                event_id: event_id.clone(),
                schema_version: EVENT_SCHEMA_VERSION,
                batch_index: 0,
                batch_count: 1,
                event: SessionEvent::InputAppended {
                    input: Input {
                        input_id: event_id,
                        content: fixture_payload(sequence, 16 * 1024),
                        received_at_ms: sequence as i64,
                    },
                },
            },
        )?;
        sequence += 1;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;

    let active_event_id = EventId::from_sequence(session_ulid, sequence)
        .expect("fixture event ID")
        .to_string();
    let active_path = segments.join(format!("{active_event_id}.jsonl"));
    let mut active = BufWriter::new(File::create(active_path)?);
    write_event(
        &mut active,
        EventEnvelope {
            event_id: active_event_id,
            schema_version: EVENT_SCHEMA_VERSION,
            batch_index: 0,
            batch_count: 1,
            event: SessionEvent::Snapshot {
                state_schema_version: 1,
                state: serde_json::json!({}),
            },
        },
    )?;
    active.flush()?;
    active.get_ref().sync_all()?;
    Ok((session_id, first_event_id))
}

fn write_event(writer: &mut BufWriter<File>, event: EventEnvelope) -> std::io::Result<u64> {
    let mut line = serde_json::to_vec(&event).map_err(std::io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    Ok(line.len() as u64)
}

fn fixture_payload(sequence: u64, size: usize) -> String {
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let pattern = b" tool result event transcript workspace json rust ";
    let mut state = sequence ^ 0x9e37_79b9_7f4a_7c15;
    let mut bytes = Vec::with_capacity(size);
    for index in 0..size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.push(if index % 2 == 0 {
            alphabet[state as usize % alphabet.len()]
        } else {
            pattern[(index + sequence as usize) % pattern.len()]
        });
    }
    String::from_utf8(bytes).expect("fixture payload is ASCII")
}

fn measure_child(arguments: &[String]) {
    assert_eq!(arguments.len(), 4, "invalid compression child arguments");
    let data_root = Path::new(&arguments[1]);
    let session_id = &arguments[2];
    let first_event_id = &arguments[3];
    let source = data_root
        .join("sessions")
        .join(session_id)
        .join("segments")
        .join(format!("{first_event_id}.jsonl"));
    let destination = source.with_extension("jsonl.zst");
    let raw_bytes = source.metadata().expect("read raw segment metadata").len();
    let store = StreamStore::open(data_root).expect("open compression store");
    let started = Instant::now();
    store
        .compress_segment(session_id, first_event_id)
        .expect("compress sealed segment");
    let elapsed_ms = started.elapsed().as_millis().max(1) as u64;
    let measured = CompressionPerformance {
        raw_bytes,
        compressed_bytes: destination
            .metadata()
            .expect("read compressed segment metadata")
            .len(),
        elapsed_ms,
        peak_rss_bytes: peak_rss_bytes(),
    };
    println!(
        "{}",
        serde_json::to_string(&measured).expect("serialize compression report")
    );
}

#[cfg(unix)]
fn peak_rss_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage initializes the supplied structure and retains no pointer.
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

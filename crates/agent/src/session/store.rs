//! Single-writer persistence for per-session append-only event segments.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fs2::FileExt;
use serde_json::Value;
use ulid::Ulid;

use super::event_id::{EventId, EventIdError};
use super::events::{SessionEvent, EVENT_SCHEMA_VERSION};
use super::log::{
    active_segment, first_event_id, latest_segment, lines_reverse, segment_paths, StoredEnvelope,
};

pub use super::log::EventEnvelope;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid session id {0:?}")]
    InvalidSessionId(String),
    #[error("event id error: {0}")]
    EventId(#[from] EventIdError),
    #[error("event batch contains too many events")]
    BatchTooLarge,
    #[error("session {0} parse error: {1}")]
    Parse(String, String),
    #[error("segment {0:?} is still active")]
    ActiveSegment(String),
    #[error("invalid segment {0}")]
    InvalidSegment(PathBuf),
    #[error("session {0} does not exist")]
    SessionNotFound(String),
}

#[derive(Clone, Copy, Debug)]
pub struct StoreOptions {
    pub segment_target_bytes: u64,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            segment_target_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SnapshotAppend {
    pub envelope: EventEnvelope,
    pub sealed_segment: Option<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CompressionCandidate {
    pub session_id: String,
    pub first_event_id: String,
}

pub struct DetachedSession {
    cleanup: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl DetachedSession {
    pub fn new(cleanup: impl FnOnce() + Send + 'static) -> Self {
        Self {
            cleanup: Some(Box::new(cleanup)),
        }
    }

    pub fn empty() -> Self {
        Self { cleanup: None }
    }

    pub fn cleanup(mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

pub trait SessionStore: Send + Sync {
    fn append_batch(
        &self,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<Vec<EventEnvelope>, StoreError>;

    fn append_snapshot(
        &self,
        session_id: &str,
        state_schema_version: u32,
        state: Value,
    ) -> Result<SnapshotAppend, StoreError>;

    fn compress_segment(&self, session_id: &str, first_event_id: &str) -> Result<(), StoreError>;

    fn compression_candidates(&self) -> Result<Vec<CompressionCandidate>, StoreError> {
        Ok(Vec::new())
    }

    fn detach_session(&self, session_id: &str) -> Result<DetachedSession, StoreError>;
}

pub struct StreamStore {
    data_root: PathBuf,
    root: PathBuf,
    last_sequences: Mutex<HashMap<String, u64>>,
    _root_lock: File,
    options: StoreOptions,
}

impl StreamStore {
    pub fn open(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::open_with_options(root, StoreOptions::default())
    }

    pub fn open_with_options(
        root: impl AsRef<Path>,
        options: StoreOptions,
    ) -> std::io::Result<Self> {
        let data_root = root.as_ref();
        std::fs::create_dir_all(data_root)?;
        let root = data_root.join("sessions");
        std::fs::create_dir_all(&root)?;
        let root_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data_root.join(".zork-agent.lock"))?;
        root_lock.try_lock_exclusive()?;
        Ok(Self {
            data_root: data_root.to_owned(),
            root,
            last_sequences: Mutex::new(HashMap::new()),
            _root_lock: root_lock,
            options,
        })
    }

    fn append(
        &self,
        session_id: &str,
        events: &[SessionEvent],
        new_segment: bool,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let batch_count = u32::try_from(events.len()).map_err(|_| StoreError::BatchTooLarge)?;
        let session_ulid = parse_session_id(session_id)?;
        let segments = self.root.join(session_id).join("segments");
        std::fs::create_dir_all(&segments)?;
        let active = active_segment(&segments)?;
        if let Some(path) = active.as_deref() {
            truncate_torn_tail(path)?;
        }

        let base_sequence = self
            .last_sequences
            .lock()
            .expect("store sequence cache mutex poisoned")
            .get(session_id)
            .copied()
            .map(Ok)
            .unwrap_or_else(|| last_sequence(&segments, session_ulid))?;
        let envelopes = events
            .iter()
            .enumerate()
            .map(|(index, event)| {
                let sequence = base_sequence
                    .checked_add(index as u64 + 1)
                    .ok_or(EventIdError::SequenceOutOfRange(u64::MAX))?;
                Ok(EventEnvelope {
                    event_id: EventId::from_sequence(session_ulid, sequence)?.to_string(),
                    schema_version: EVENT_SCHEMA_VERSION,
                    batch_index: index as u32,
                    batch_count,
                    event: event.clone(),
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let path = match (new_segment, active) {
            (false, Some(path)) => path,
            _ => segments.join(format!("{}.jsonl", envelopes[0].event_id)),
        };
        let mut bytes = Vec::new();
        for envelope in &envelopes {
            serde_json::to_writer(&mut bytes, envelope)
                .map_err(|error| StoreError::Parse(session_id.into(), error.to_string()))?;
            bytes.push(b'\n');
        }
        if let Err(error) = append_and_sync(&path, &bytes, File::sync_data) {
            self.last_sequences
                .lock()
                .expect("store sequence cache mutex poisoned")
                .remove(session_id);
            return Err(error.into());
        }
        self.last_sequences
            .lock()
            .expect("store sequence cache mutex poisoned")
            .insert(session_id.to_owned(), base_sequence + events.len() as u64);
        Ok(envelopes)
    }
}

impl SessionStore for StreamStore {
    fn append_batch(
        &self,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        self.append(session_id, events, false)
    }

    fn append_snapshot(
        &self,
        session_id: &str,
        state_schema_version: u32,
        state: Value,
    ) -> Result<SnapshotAppend, StoreError> {
        parse_session_id(session_id)?;
        let segments = self.root.join(session_id).join("segments");
        let active = active_segment(&segments)?;
        let should_cut = active
            .as_ref()
            .map(|path| path.metadata())
            .transpose()?
            .is_some_and(|metadata| metadata.len() >= self.options.segment_target_bytes);
        let sealed_segment = should_cut
            .then(|| {
                active
                    .as_deref()
                    .and_then(first_event_id)
                    .map(str::to_owned)
            })
            .flatten();
        let mut envelopes = self.append(
            session_id,
            &[SessionEvent::Snapshot {
                state_schema_version,
                state,
            }],
            should_cut,
        )?;
        Ok(SnapshotAppend {
            envelope: envelopes.pop().expect("one snapshot event was appended"),
            sealed_segment,
        })
    }

    fn compress_segment(&self, session_id: &str, first_event_id: &str) -> Result<(), StoreError> {
        let session_ulid = parse_session_id(session_id)?;
        EventId::parse(session_ulid, first_event_id)?;
        let segments = self.root.join(session_id).join("segments");
        let source = segments.join(format!("{first_event_id}.jsonl"));
        let destination = segments.join(format!("{first_event_id}.jsonl.zst"));
        if !source.is_file() && destination.is_file() {
            return Ok(());
        }
        if active_segment(&segments)?.as_deref() == Some(source.as_path()) {
            return Err(StoreError::ActiveSegment(first_event_id.to_owned()));
        }

        if destination.is_file() {
            std::fs::remove_file(&source)?;
            File::open(&segments)?.sync_data()?;
            return Ok(());
        }
        let temporary = segments.join(format!(".{first_event_id}.jsonl.zst.tmp"));
        let result = (|| -> Result<(), StoreError> {
            match std::fs::remove_file(&temporary) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let prefix = complete_prefix_len(&source)?;
            let output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            let mut encoder = zstd::stream::write::Encoder::new(output, 12)?;
            encoder.set_pledged_src_size(Some(prefix))?;
            std::io::copy(&mut File::open(&source)?.take(prefix), &mut encoder)?;
            encoder.finish()?.sync_data()?;
            std::fs::rename(&temporary, &destination)?;
            File::open(&segments)?.sync_data()?;
            std::fs::remove_file(&source)?;
            File::open(&segments)?.sync_data()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    fn compression_candidates(&self) -> Result<Vec<CompressionCandidate>, StoreError> {
        let mut candidates = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !entry.path().is_dir() || session_id.parse::<Ulid>().is_err() {
                continue;
            }
            let segments = entry.path().join("segments");
            let active = active_segment(&segments)?;
            for path in segment_paths(&segments)? {
                if path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
                    || active.as_deref() == Some(path.as_path())
                {
                    continue;
                }
                if let Some(first_event_id) = first_event_id(&path) {
                    candidates.push(CompressionCandidate {
                        session_id: session_id.clone(),
                        first_event_id: first_event_id.to_owned(),
                    });
                }
            }
        }
        candidates.sort_unstable();
        Ok(candidates)
    }

    fn detach_session(&self, session_id: &str) -> Result<DetachedSession, StoreError> {
        parse_session_id(session_id)?;
        let path = self.root.join(session_id);
        if !path.is_dir() {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        let detached_root = self.data_root.join("detached-sessions");
        std::fs::create_dir_all(&detached_root)?;
        let detached = detached_root.join(format!("{session_id}-{}", crate::ids::new_ulid()));
        std::fs::rename(path, &detached)?;
        File::open(&self.root)?.sync_data()?;
        File::open(&detached_root)?.sync_data()?;
        self.last_sequences
            .lock()
            .expect("store sequence cache mutex poisoned")
            .remove(session_id);
        Ok(DetachedSession::new(move || {
            let _ = std::fs::remove_dir_all(detached);
        }))
    }
}

fn parse_session_id(session_id: &str) -> Result<Ulid, StoreError> {
    session_id
        .parse()
        .map_err(|_| StoreError::InvalidSessionId(session_id.to_owned()))
}

fn last_sequence(segments: &Path, session_ulid: Ulid) -> Result<u64, StoreError> {
    let Some(path) = latest_segment(segments)? else {
        return Ok(0);
    };
    if path.to_string_lossy().ends_with(".jsonl.zst") {
        return Err(StoreError::InvalidSegment(path));
    }
    let mut sequence = None;
    lines_reverse(&path, &mut |_, _, line| {
        sequence = serde_json::from_slice::<StoredEnvelope>(line)
            .ok()
            .and_then(|envelope| EventId::parse_sequence(session_ulid, &envelope.event_id).ok());
        true
    })?;
    sequence.ok_or(StoreError::InvalidSegment(path))
}

fn complete_prefix_len(path: &Path) -> Result<u64, StoreError> {
    let mut prefix = None;
    let mut expected = None;
    let mut invalid = false;
    lines_reverse(path, &mut |start, after, line| {
        let envelope = match serde_json::from_slice::<StoredEnvelope>(line) {
            Ok(envelope) if envelope.batch_count > 0 => envelope,
            _ => {
                invalid = true;
                return true;
            }
        };
        let Some((index, count)) = expected else {
            if envelope.batch_index.checked_add(1) == Some(envelope.batch_count) {
                prefix = Some(after);
                return true;
            }
            if envelope.batch_index >= envelope.batch_count {
                invalid = true;
                return true;
            }
            if envelope.batch_index == 0 {
                prefix = Some(start);
                return true;
            }
            expected = Some((envelope.batch_index - 1, envelope.batch_count));
            return false;
        };
        if envelope.batch_count != count || envelope.batch_index != index {
            invalid = true;
            return true;
        }
        if index == 0 {
            prefix = Some(start);
            return true;
        }
        expected = Some((index - 1, count));
        false
    })?;
    if invalid || (expected.is_some() && prefix.is_none()) {
        return Err(StoreError::InvalidSegment(path.to_owned()));
    }
    Ok(prefix.unwrap_or(0))
}

fn append_and_sync(
    path: &Path,
    bytes: &[u8],
    sync: impl FnOnce(&File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(bytes)?;
    sync(&file)
}

fn truncate_torn_tail(path: &Path) -> std::io::Result<()> {
    const BLOCK: usize = 8 * 1024;
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    let mut end = length;
    let mut buffer = [0; BLOCK];
    while end > 0 {
        let start = end.saturating_sub(BLOCK as u64);
        let size = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..size])?;
        if let Some(index) = buffer[..size].iter().rposition(|byte| *byte == b'\n') {
            return file.set_len(start + index as u64 + 1);
        }
        end = start;
    }
    file.set_len(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Contract: docs/zork-agent-architecture.md [PERSIST-03, EVENT-02]
    fn sync_failure_is_returned_after_the_complete_batch_was_written() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let error = append_and_sync(&path, b"complete-batch\n", |_| {
            Err(std::io::Error::other("injected sync failure"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(std::fs::read(&path).unwrap(), b"complete-batch\n");
    }
}

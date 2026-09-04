//! Durable envelope and composable line, commit and snapshot streams.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use ulid::Ulid;

use super::event_id::EventId;
use super::events::{migrate_event, validate_history_event, EventMigrationError, SessionEvent};

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    pub event_id: String,
    pub schema_version: u32,
    pub batch_index: u32,
    pub batch_count: u32,
    pub event: SessionEvent,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredEnvelope {
    pub(crate) event_id: String,
    pub(crate) schema_version: u32,
    pub(crate) batch_index: u32,
    pub(crate) batch_count: u32,
    event: Box<RawValue>,
}

impl StoredEnvelope {
    fn migrate(self) -> Result<EventEnvelope, EventMigrationError> {
        Ok(EventEnvelope {
            event_id: self.event_id,
            schema_version: self.schema_version,
            batch_index: self.batch_index,
            batch_count: self.batch_count,
            event: migrate_event(self.schema_version, &self.event)?,
        })
    }
}

impl<'de> Deserialize<'de> for EventEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        StoredEnvelope::deserialize(deserializer)?
            .migrate()
            .map_err(serde::de::Error::custom)
    }
}

pub(crate) enum StreamItem<T> {
    Value(T),
    Fault(String),
}

pub(crate) struct SnapshotWindow {
    pub(crate) snapshot: Option<EventEnvelope>,
    pub(crate) commits: Vec<Vec<EventEnvelope>>,
}

pub(crate) struct HistoryRecord<'a> {
    pub(crate) path: &'a Path,
    pub(crate) event_id: &'a str,
    pub(crate) visible: bool,
    pub(crate) line: &'a [u8],
}

/// Hold a segment open across scans even when rotation replaces its path.
pub(crate) struct HistoryReader {
    path: PathBuf,
    file: File,
}

impl HistoryReader {
    pub(crate) fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            path: path.to_owned(),
            file: File::open(path)?,
        })
    }

    pub(crate) fn scan(
        &mut self,
        session_ulid: Ulid,
        visit: &mut dyn FnMut(StreamItem<HistoryRecord<'_>>) -> bool,
    ) -> std::io::Result<()> {
        self.file.rewind()?;
        let mut stream = HistoryStream::new(session_ulid);
        let mut line_visit = |line: &[u8]| stream.push(&self.path, line, visit);
        if self.path.to_string_lossy().ends_with(".jsonl.zst") {
            let decoder = zstd::stream::read::Decoder::new(&mut self.file)?;
            reader_lines(BufReader::new(decoder), &mut line_visit)
        } else {
            reader_lines(BufReader::new(&mut self.file), &mut line_visit)
        }
    }
}

pub(crate) fn commits_forward(
    paths: &[PathBuf],
    session_ulid: Ulid,
    visit: &mut dyn FnMut(StreamItem<Vec<EventEnvelope>>) -> bool,
) -> std::io::Result<()> {
    let mut stream = CommitStream::new(Direction::Forward, session_ulid);
    for path in paths {
        let mut stop = false;
        lines_forward(path, &mut |line| {
            stop = stream.push(path, line, visit);
            stop
        })?;
        if stop {
            return Ok(());
        }
        stream.pending.clear();
    }
    Ok(())
}

pub(crate) fn history_records_forward(
    paths: &[PathBuf],
    session_ulid: Ulid,
    visit: &mut dyn FnMut(StreamItem<HistoryRecord<'_>>) -> bool,
) -> std::io::Result<()> {
    let mut stream = HistoryStream::new(session_ulid);
    for path in paths {
        let mut stop = false;
        lines_forward(path, &mut |line| {
            stop = stream.push(path, line, visit);
            stop
        })?;
        if stop {
            return Ok(());
        }
        stream.pending.clear();
    }
    Ok(())
}

pub(crate) fn commits_reverse(
    path: &Path,
    session_ulid: Ulid,
    visit: &mut dyn FnMut(StreamItem<Vec<EventEnvelope>>) -> bool,
) -> std::io::Result<()> {
    let mut stream = CommitStream::new(Direction::Reverse, session_ulid);
    lines_reverse(path, &mut |_, _, line| stream.push(path, line, visit))
}

pub(crate) fn snapshots_reverse(
    path: &Path,
    session_ulid: Ulid,
    visit: &mut dyn FnMut(StreamItem<SnapshotWindow>) -> bool,
) -> std::io::Result<()> {
    let mut commits = Vec::new();
    let mut stopped = false;
    commits_reverse(path, session_ulid, &mut |item| match item {
        StreamItem::Fault(error) => {
            stopped = visit(StreamItem::Fault(error));
            stopped
        }
        StreamItem::Value(mut commit)
            if commit.len() == 1 && matches!(commit[0].event, SessionEvent::Snapshot { .. }) =>
        {
            commits.reverse();
            stopped = visit(StreamItem::Value(SnapshotWindow {
                snapshot: commit.pop(),
                commits: std::mem::take(&mut commits),
            }));
            stopped
        }
        StreamItem::Value(commit) => {
            commits.push(commit);
            false
        }
    })?;
    if !stopped && !commits.is_empty() {
        commits.reverse();
        let _ = visit(StreamItem::Value(SnapshotWindow {
            snapshot: None,
            commits,
        }));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedEnvelope<'a> {
    #[serde(borrow)]
    event_id: &'a str,
    schema_version: u32,
    batch_index: u32,
    batch_count: u32,
    #[serde(borrow)]
    event: &'a RawValue,
}

/// Query input already lives in a byte buffer. Borrow its raw event while
/// migrating instead of allocating a second full JSON payload in StoredEnvelope.
pub(crate) fn decode_envelope(line: &[u8]) -> Result<EventEnvelope, serde_json::Error> {
    let raw: BorrowedEnvelope<'_> = serde_json::from_slice(line)?;
    Ok(EventEnvelope {
        event_id: raw.event_id.to_owned(),
        schema_version: raw.schema_version,
        batch_index: raw.batch_index,
        batch_count: raw.batch_count,
        event: migrate_event(raw.schema_version, raw.event).map_err(serde::de::Error::custom)?,
    })
}

struct OwnedHistoryRecord {
    event_id: String,
    batch_index: u32,
    batch_count: u32,
    sequence: u64,
    line: Vec<u8>,
}

struct HistoryStream {
    session_ulid: Ulid,
    previous_sequence: Option<u64>,
    pending: Vec<OwnedHistoryRecord>,
}

impl HistoryStream {
    fn new(session_ulid: Ulid) -> Self {
        Self {
            session_ulid,
            previous_sequence: None,
            pending: Vec::new(),
        }
    }

    fn push(
        &mut self,
        path: &Path,
        line: &[u8],
        visit: &mut dyn FnMut(StreamItem<HistoryRecord<'_>>) -> bool,
    ) -> bool {
        let raw = match serde_json::from_slice::<BorrowedEnvelope<'_>>(line) {
            Ok(raw) => raw,
            Err(error) => {
                self.pending.clear();
                return visit(StreamItem::Fault(format!(
                    "segment {} record is not a usable event envelope: {error}",
                    path.display()
                )));
            }
        };
        let sequence = match EventId::parse_sequence(self.session_ulid, raw.event_id) {
            Ok(sequence) => sequence,
            Err(error) => {
                self.pending.clear();
                return visit(StreamItem::Fault(format!(
                    "segment {} has invalid event ID {:?}: {error}",
                    path.display(),
                    raw.event_id
                )));
            }
        };
        if self
            .previous_sequence
            .is_some_and(|previous| sequence <= previous)
        {
            self.pending.clear();
            return visit(StreamItem::Fault(format!(
                "segment {} has out-of-order event ID {}",
                path.display(),
                raw.event_id
            )));
        }
        self.previous_sequence = Some(sequence);

        let follows = self.pending.last().is_some_and(|previous| {
            previous.batch_count == raw.batch_count
                && previous.batch_index.checked_add(1) == Some(raw.batch_index)
                && previous.sequence.checked_add(1) == Some(sequence)
        });
        if !follows {
            self.pending.clear();
            if raw.batch_index != 0 {
                return false;
            }
        }
        if raw.batch_count == 1 {
            let visible = match validate_history_event(raw.schema_version, raw.event) {
                Ok(visible) => visible,
                Err(error) => {
                    return visit(StreamItem::Fault(format!(
                        "segment {} has an unmigratable commit: {error}",
                        path.display()
                    )));
                }
            };
            return visit(StreamItem::Value(HistoryRecord {
                path,
                event_id: raw.event_id,
                visible,
                line,
            }));
        }

        let count = raw.batch_count as usize;
        self.pending.push(OwnedHistoryRecord {
            event_id: raw.event_id.to_owned(),
            batch_index: raw.batch_index,
            batch_count: raw.batch_count,
            sequence,
            line: line.to_vec(),
        });
        if self.pending.len() != count {
            return false;
        }

        let complete = std::mem::take(&mut self.pending);
        let mut visibility = Vec::with_capacity(complete.len());
        for record in &complete {
            let raw = match serde_json::from_slice::<BorrowedEnvelope<'_>>(&record.line) {
                Ok(raw) => raw,
                Err(error) => {
                    return visit(StreamItem::Fault(format!(
                        "segment {} record is not a usable event envelope: {error}",
                        path.display()
                    )));
                }
            };
            match validate_history_event(raw.schema_version, raw.event) {
                Ok(visible) => visibility.push(visible),
                Err(error) => {
                    return visit(StreamItem::Fault(format!(
                        "segment {} has an unmigratable commit: {error}",
                        path.display()
                    )));
                }
            }
        }
        for (record, visible) in complete.iter().zip(visibility) {
            if visit(StreamItem::Value(HistoryRecord {
                path,
                event_id: &record.event_id,
                visible,
                line: &record.line,
            })) {
                return true;
            }
        }
        false
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Reverse,
}

struct CommitStream {
    direction: Direction,
    session_ulid: Ulid,
    previous_sequence: Option<u64>,
    pending: Vec<(StoredEnvelope, u64)>,
}

impl CommitStream {
    fn new(direction: Direction, session_ulid: Ulid) -> Self {
        Self {
            direction,
            session_ulid,
            previous_sequence: None,
            pending: Vec::new(),
        }
    }

    fn push(
        &mut self,
        path: &Path,
        line: &[u8],
        visit: &mut dyn FnMut(StreamItem<Vec<EventEnvelope>>) -> bool,
    ) -> bool {
        let raw = match serde_json::from_slice::<StoredEnvelope>(line) {
            Ok(raw) => raw,
            Err(error) => {
                self.pending.clear();
                return visit(StreamItem::Fault(format!(
                    "segment {} record is not a usable event envelope: {error}",
                    path.display()
                )));
            }
        };
        let sequence = match EventId::parse_sequence(self.session_ulid, &raw.event_id) {
            Ok(sequence) => sequence,
            Err(error) => {
                self.pending.clear();
                return visit(StreamItem::Fault(format!(
                    "segment {} has invalid event ID {:?}: {error}",
                    path.display(),
                    raw.event_id
                )));
            }
        };
        if self
            .previous_sequence
            .is_some_and(|previous| match self.direction {
                Direction::Forward => sequence <= previous,
                Direction::Reverse => sequence >= previous,
            })
        {
            self.pending.clear();
            return visit(StreamItem::Fault(format!(
                "segment {} has out-of-order event ID {}",
                path.display(),
                raw.event_id
            )));
        }
        self.previous_sequence = Some(sequence);

        let starts = match self.direction {
            Direction::Forward => raw.batch_index == 0,
            Direction::Reverse => {
                raw.batch_count > 0 && raw.batch_index.checked_add(1) == Some(raw.batch_count)
            }
        };
        let follows = self
            .pending
            .last()
            .is_some_and(|(previous, previous_sequence)| {
                previous.batch_count == raw.batch_count
                    && match self.direction {
                        Direction::Forward => {
                            previous.batch_index.checked_add(1) == Some(raw.batch_index)
                                && previous_sequence.checked_add(1) == Some(sequence)
                        }
                        Direction::Reverse => {
                            previous.batch_index.checked_sub(1) == Some(raw.batch_index)
                                && sequence.checked_add(1) == Some(*previous_sequence)
                        }
                    }
            });
        if !follows {
            self.pending.clear();
            if !starts {
                return false;
            }
        }
        let count = raw.batch_count as usize;
        self.pending.push((raw, sequence));
        if self.pending.len() != count {
            return false;
        }

        let mut raw = std::mem::take(&mut self.pending);
        if matches!(self.direction, Direction::Reverse) {
            raw.reverse();
        }
        let mut commit = Vec::with_capacity(raw.len());
        for (envelope, _) in raw {
            match envelope.migrate() {
                Ok(envelope) => commit.push(envelope),
                Err(error) => {
                    return visit(StreamItem::Fault(format!(
                        "segment {} has an unmigratable commit: {error}",
                        path.display()
                    )));
                }
            }
        }
        visit(StreamItem::Value(commit))
    }
}

fn lines_forward(path: &Path, visit: &mut impl FnMut(&[u8]) -> bool) -> std::io::Result<()> {
    if path.to_string_lossy().ends_with(".jsonl.zst") {
        let decoder = zstd::stream::read::Decoder::new(File::open(path)?)?;
        reader_lines(BufReader::new(decoder), visit)
    } else {
        reader_lines(BufReader::new(File::open(path)?), visit)
    }
}

fn reader_lines(
    mut reader: impl BufRead,
    visit: &mut impl FnMut(&[u8]) -> bool,
) -> std::io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 || line.last() != Some(&b'\n') {
            return Ok(());
        }
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if visit(&line) {
            return Ok(());
        }
    }
}

pub(crate) fn lines_reverse(
    path: &Path,
    visit: &mut impl FnMut(u64, u64, &[u8]) -> bool,
) -> std::io::Result<()> {
    const BLOCK: usize = 8 * 1024;

    if path.to_string_lossy().ends_with(".jsonl.zst") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "compressed segments only support forward streaming",
        ));
    }

    let mut file = File::open(path)?;
    let mut cursor = file.metadata()?.len();
    let mut newer_newline = None::<u64>;
    let mut reversed_line = Vec::new();
    let mut block = [0; BLOCK];

    while cursor > 0 {
        let block_start = cursor.saturating_sub(BLOCK as u64);
        let size = usize::try_from(cursor - block_start)
            .map_err(|_| std::io::Error::other("reverse-read block is too large"))?;
        file.seek(SeekFrom::Start(block_start))?;
        file.read_exact(&mut block[..size])?;

        for index in (0..size).rev() {
            let offset = block_start + index as u64;
            let byte = block[index];
            if newer_newline.is_none() {
                if byte == b'\n' {
                    newer_newline = Some(offset);
                }
                continue;
            }

            if byte != b'\n' {
                reversed_line.push(byte);
                continue;
            }

            reversed_line.reverse();
            if reversed_line.last() == Some(&b'\r') {
                reversed_line.pop();
            }
            let end = newer_newline.expect("a newer newline was observed") + 1;
            if visit(offset + 1, end, &reversed_line) {
                return Ok(());
            }
            reversed_line.clear();
            newer_newline = Some(offset);
        }
        cursor = block_start;
    }

    if let Some(newline) = newer_newline {
        reversed_line.reverse();
        if reversed_line.last() == Some(&b'\r') {
            reversed_line.pop();
        }
        let _ = visit(0, newline + 1, &reversed_line);
    }
    Ok(())
}

pub(crate) fn segment_paths(segments: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !segments.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = BTreeMap::new();
    for entry in std::fs::read_dir(segments)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some((first, plain)) = segment_name(&name).filter(|_| entry.path().is_file()) else {
            continue;
        };
        let candidate = entry.path();
        paths
            .entry(first.to_owned())
            .and_modify(|current| {
                if plain {
                    *current = candidate.clone();
                }
            })
            .or_insert(candidate);
    }
    Ok(paths.into_values().collect())
}

pub(crate) fn latest_segment(segments: &Path) -> std::io::Result<Option<PathBuf>> {
    Ok(segment_paths(segments)?.into_iter().next_back())
}

pub(crate) fn active_segment(segments: &Path) -> std::io::Result<Option<PathBuf>> {
    Ok(segment_paths(segments)?.into_iter().rfind(|path| {
        path.extension()
            .is_some_and(|extension| extension == "jsonl")
    }))
}

pub(crate) fn first_event_id(path: &Path) -> Option<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(segment_name)
        .map(|(first, _)| first)
}

fn segment_name(name: &str) -> Option<(&str, bool)> {
    let (first, plain) = name
        .strip_suffix(".jsonl")
        .map(|first| (first, true))
        .or_else(|| name.strip_suffix(".jsonl.zst").map(|first| (first, false)))?;
    (!first.is_empty()).then_some((first, plain))
}

#[cfg(test)]
mod tests {
    use super::lines_reverse;

    #[cfg(unix)]
    #[test]
    // Contract: docs/zork-agent-architecture.md [QUERY-01, SEGMENT-01]
    fn repeated_history_scans_keep_the_open_segment_after_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let session = ulid::Ulid::new();
        let event_id = super::EventId::from_sequence(session, 1)
            .unwrap()
            .to_string();
        let mut data = serde_json::to_vec(&serde_json::json!({
            "event_id": event_id,
            "schema_version": crate::session::events::EVENT_SCHEMA_VERSION,
            "batch_index": 0, "batch_count": 1,
            "event": {"kind": "input_appended", "input": {
                "input_id": "input-1", "content": "original", "received_at_ms": 0
            }}
        }))
        .unwrap();
        data.push(b'\n');
        for compressed in [false, true] {
            let path = root.path().join(if compressed {
                "segment.jsonl.zst"
            } else {
                "segment.jsonl"
            });
            let bytes = if compressed {
                zstd::encode_all(data.as_slice(), 1).unwrap()
            } else {
                data.clone()
            };
            std::fs::write(&path, bytes).unwrap();
            let mut reader = super::HistoryReader::open(&path).unwrap();
            for pass in 0..2 {
                let mut ids = Vec::new();
                reader
                    .scan(session, &mut |item| {
                        match item {
                            super::StreamItem::Value(record) => {
                                ids.push(record.event_id.to_owned())
                            }
                            super::StreamItem::Fault(error) => panic!("{error}"),
                        }
                        false
                    })
                    .unwrap();
                assert_eq!(ids, vec![event_id.clone()]);
                if pass == 0 {
                    std::fs::remove_file(&path).unwrap();
                    std::fs::write(&path, b"replacement inode").unwrap();
                }
            }
        }
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [EVENT-02, QUERY-01]
    fn reverse_lines_cross_blocks_and_ignore_a_torn_tail() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("segment.jsonl");
        let long_line = vec![b'x'; 20 * 1024];
        let mut bytes = b"first\r\n".to_vec();
        bytes.extend_from_slice(&long_line);
        bytes.extend_from_slice(b"\nlast\n{\"torn\"");
        std::fs::write(&path, &bytes).unwrap();

        let mut lines = Vec::new();
        lines_reverse(&path, &mut |start, end, line| {
            lines.push((start, end, line.to_vec()));
            false
        })
        .unwrap();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].2, b"last");
        assert_eq!(lines[1].2, long_line);
        assert_eq!(lines[2].2, b"first");
        for (start, end, _) in lines {
            assert_eq!(bytes[usize::try_from(end).unwrap() - 1], b'\n');
            assert!(start < end);
        }
    }
}

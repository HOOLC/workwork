//! All read-only access to durable session segments.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use ulid::Ulid;

use super::event_id::EventId;
use super::log::{
    commits_forward, commits_reverse, first_event_id, history_records_forward, latest_segment,
    segment_paths, snapshots_reverse, EventEnvelope, HistoryReader, HistoryRecord,
    SnapshotWindow as LogSnapshotWindow, StreamItem,
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commit {
    pub events: Vec<EventEnvelope>,
}

impl Commit {
    pub fn new(events: Vec<EventEnvelope>) -> Self {
        Self { events }
    }

    pub fn last(&self) -> Option<&EventEnvelope> {
        self.events.last()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadSummary {
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadResult<T> {
    pub value: T,
    pub diagnostics: Vec<String>,
}

impl<T> ReadResult<T> {
    fn new(value: T, diagnostics: Vec<String>) -> Self {
        Self { value, diagnostics }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDiscovery {
    pub session_id: String,
    pub last_activity_ms: i64,
    pub read_hint: SessionReadHint,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionReadHint {
    session_id: String,
    latest_segment: Option<PathBuf>,
}

impl SessionReadHint {
    pub fn empty(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            latest_segment: None,
        }
    }

    fn path(&self, session_id: &str) -> Option<&Path> {
        (self.session_id == session_id)
            .then_some(self.latest_segment.as_deref())
            .flatten()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WindowOrigin {
    Snapshot(Box<EventEnvelope>),
    SessionStart,
    Unanchored,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotWindow {
    pub origin: WindowOrigin,
    pub commits: Vec<Commit>,
}

/// The single read boundary for session persistence. Methods above raw commit
/// streaming are shared here so callers do not reconstruct log semantics.
pub trait SessionQuery: Send + Sync {
    fn exists(&self, session_id: &str) -> bool;

    fn discover_sessions(&self) -> Result<Vec<SessionDiscovery>, QueryError>;

    fn last_commit(
        &self,
        session_id: &str,
        hint: Option<&SessionReadHint>,
    ) -> Result<ReadResult<Option<Commit>>, QueryError>;

    fn snapshot_windows(
        &self,
        session_id: &str,
        hint: Option<&SessionReadHint>,
        visit: &mut dyn FnMut(SnapshotWindow) -> bool,
    ) -> Result<ReadSummary, QueryError>;

    fn all_commits_forward(
        &self,
        session_id: &str,
        visit: &mut dyn FnMut(Commit) -> bool,
    ) -> Result<ReadSummary, QueryError>;

    fn scan_after(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        visit: &mut dyn FnMut(EventEnvelope) -> bool,
    ) -> Result<(), QueryError>;

    fn after(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, QueryError> {
        let mut events = Vec::with_capacity(limit.min(128));
        self.scan_after(session_id, cursor, &mut |event| {
            if events.len() < limit {
                events.push(event);
            }
            events.len() == limit
        })?;
        Ok(events)
    }

    fn before(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, QueryError>;

    fn event(&self, session_id: &str, event_id: &str) -> Result<Option<EventEnvelope>, QueryError>;
}

pub struct FileSessionQuery {
    root: PathBuf,
}

impl FileSessionQuery {
    pub fn open(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().join("sessions"),
        }
    }

    fn segments(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id).join("segments")
    }

    fn latest(
        &self,
        session_id: &str,
        hint: Option<&SessionReadHint>,
    ) -> Result<PathBuf, QueryError> {
        if let Some(path) = hint.and_then(|hint| hint.path(session_id)) {
            return Ok(path.to_owned());
        }
        latest_segment(&self.segments(session_id))?
            .ok_or_else(|| QueryError::SessionNotFound(session_id.to_owned()))
    }

    fn indexed_paths(&self, session_id: &str) -> Result<Vec<(String, PathBuf)>, QueryError> {
        let session_ulid = parse_session_id(session_id)?;
        segment_paths(&self.segments(session_id))?
            .into_iter()
            .map(|path| {
                let first = first_event_id(&path)
                    .ok_or_else(|| QueryError::InvalidSegment(path.clone()))?;
                EventId::parse_sequence(session_ulid, first)
                    .map_err(|_| QueryError::InvalidSegment(path.clone()))?;
                Ok((first.to_owned(), path))
            })
            .collect()
    }

    fn history_paths(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        through_cursor: bool,
    ) -> Result<Vec<PathBuf>, QueryError> {
        Ok(select_history_paths(
            self.indexed_paths(session_id)?,
            cursor,
            through_cursor,
        ))
    }

    fn before_from_files(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, QueryError> {
        let indexed = self.indexed_paths(session_id)?;
        if indexed.is_empty() {
            return Ok(Vec::new());
        }
        let boundary = match cursor {
            Some(cursor) => {
                let boundary = indexed.partition_point(|(first, _)| first.as_str() <= cursor);
                if boundary == 0 {
                    return Err(QueryError::UnknownCursor(cursor.to_owned()));
                }
                boundary
            }
            None => indexed.len(),
        };

        let end_segment = boundary - 1;
        let mut events = VecDeque::with_capacity(limit.min(128));
        for segment in (0..=end_segment).rev() {
            let needed = limit.saturating_sub(events.len());
            let mut visible_count = 0usize;
            let mut found = cursor.is_none() || segment != end_segment;
            let mut last_event_id = String::new();
            let path = indexed[segment].1.clone();
            let mut reader = HistoryReader::open(&path)?;
            self.stream_history_reader(session_id, &mut reader, &mut |record| {
                if segment < end_segment {
                    last_event_id.clear();
                    last_event_id.push_str(record.event_id);
                }
                if segment == end_segment && Some(record.event_id) == cursor {
                    found = true;
                    return Ok(true);
                }
                if record.visible && needed > 0 {
                    visible_count += 1;
                }
                Ok(false)
            })?;
            if !found {
                if let Some(cursor) = cursor {
                    return Err(QueryError::UnknownCursor(cursor.to_owned()));
                }
            }
            if segment < end_segment
                && !last_event_id.is_empty()
                && last_event_id.as_str() >= indexed[segment + 1].0.as_str()
            {
                return Err(QueryError::InvalidLog(format!(
                    "segment {} ends at event {} which overlaps next segment {}",
                    path.display(),
                    last_event_id,
                    indexed[segment + 1].1.display()
                )));
            }
            // Locate the tail first, then allocate only returned payloads.
            // This avoids allocator pressure from repeatedly discarding large
            // events while scanning compressed segments forwards.
            let count = visible_count.min(needed);
            let mut skip = visible_count - count;
            let mut local = Vec::with_capacity(count);
            if count > 0 {
                self.stream_history_reader(session_id, &mut reader, &mut |record| {
                    if record.visible {
                        if skip > 0 {
                            skip -= 1;
                        } else {
                            local.push(decode_history_record(&record)?);
                        }
                    }
                    Ok(local.len() == count)
                })?;
                if local.len() != count {
                    return Err(QueryError::InvalidLog(format!(
                        "segment {} changed while reading history",
                        path.display()
                    )));
                }
            }
            for event in local.into_iter().rev() {
                events.push_front(event);
            }
            if events.len() == limit {
                break;
            }
        }
        Ok(events.into())
    }

    fn scan_after_from_files(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        visit: &mut dyn FnMut(EventEnvelope) -> bool,
    ) -> Result<(), QueryError> {
        let paths = self.history_paths(session_id, cursor, false)?;
        let mut found = cursor.is_none();
        self.stream_history_records(session_id, &paths, &mut |record| {
            if !found {
                found = Some(record.event_id) == cursor;
                return Ok(false);
            }
            Ok(record.visible && visit(decode_history_record(&record)?))
        })?;
        if !found {
            if let Some(cursor) = cursor {
                return Err(QueryError::UnknownCursor(cursor.to_owned()));
            }
        }
        Ok(())
    }

    fn event_from_files(
        &self,
        session_id: &str,
        event_id: &str,
    ) -> Result<Option<EventEnvelope>, QueryError> {
        let paths = self.history_paths(session_id, Some(event_id), false)?;
        let mut found = None;
        self.stream_history_records(session_id, &paths, &mut |record| {
            if record.event_id == event_id {
                if record.visible {
                    found = Some(decode_history_record(&record)?);
                }
                Ok(true)
            } else {
                Ok(false)
            }
        })?;
        Ok(found)
    }

    fn stream_history_records(
        &self,
        session_id: &str,
        paths: &[PathBuf],
        visit: &mut dyn FnMut(HistoryRecord<'_>) -> Result<bool, QueryError>,
    ) -> Result<(), QueryError> {
        let session_ulid = parse_session_id(session_id)?;
        let mut fault = None;
        history_records_forward(paths, session_ulid, &mut |item| match item {
            StreamItem::Value(record) => match visit(record) {
                Ok(stop) => stop,
                Err(error) => {
                    fault = Some(error);
                    true
                }
            },
            StreamItem::Fault(error) => {
                fault = Some(QueryError::InvalidLog(error));
                true
            }
        })?;
        fault.map_or(Ok(()), Err)
    }
    fn stream_history_reader(
        &self,
        session_id: &str,
        reader: &mut HistoryReader,
        visit: &mut dyn FnMut(HistoryRecord<'_>) -> Result<bool, QueryError>,
    ) -> Result<(), QueryError> {
        let session_ulid = parse_session_id(session_id)?;
        let mut fault = None;
        reader.scan(session_ulid, &mut |item| match item {
            StreamItem::Value(record) => match visit(record) {
                Ok(stop) => stop,
                Err(error) => {
                    fault = Some(error);
                    true
                }
            },
            StreamItem::Fault(error) => {
                fault = Some(QueryError::InvalidLog(error));
                true
            }
        })?;
        fault.map_or(Ok(()), Err)
    }
}

fn discover_session(entry: std::fs::DirEntry) -> Result<Option<SessionDiscovery>, QueryError> {
    let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
        return Ok(None);
    };
    if session_id.parse::<Ulid>().is_err() || !entry.path().is_dir() {
        return Ok(None);
    }
    let mut latest = None;
    let mut last_activity_ms = 0;
    for path in segment_paths(&entry.path().join("segments"))? {
        let modified = path
            .metadata()?
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        last_activity_ms = last_activity_ms.max(modified);
        latest = Some(path);
    }
    Ok(Some(SessionDiscovery {
        read_hint: SessionReadHint {
            session_id: session_id.clone(),
            latest_segment: latest,
        },
        session_id,
        last_activity_ms,
    }))
}

impl SessionQuery for FileSessionQuery {
    fn exists(&self, session_id: &str) -> bool {
        session_id.parse::<Ulid>().is_ok() && self.root.join(session_id).is_dir()
    }

    fn discover_sessions(&self) -> Result<Vec<SessionDiscovery>, QueryError> {
        // Directory entries are consumed once; only independent metadata I/O
        // runs in parallel. Keep a small fixed upper bound even on big hosts.
        let entries = std::sync::Mutex::new(std::fs::read_dir(&self.root)?);
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(4);
        let mut sessions = std::thread::scope(|scope| {
            let handles = (0..workers)
                .map(|_| {
                    scope.spawn(|| -> Result<Vec<SessionDiscovery>, QueryError> {
                        let mut sessions = Vec::new();
                        loop {
                            let next = entries
                                .lock()
                                .expect("session directory iterator poisoned")
                                .next();
                            let Some(entry) = next else {
                                break;
                            };
                            if let Some(session) = discover_session(entry?)? {
                                sessions.push(session);
                            }
                        }
                        Ok(sessions)
                    })
                })
                .collect::<Vec<_>>();
            let mut sessions = Vec::new();
            for handle in handles {
                let batch = handle
                    .join()
                    .map_err(|_| std::io::Error::other("session discovery worker panicked"))??;
                sessions.extend(batch);
            }
            Ok::<_, QueryError>(sessions)
        })?;
        sessions.sort_by(|left, right| {
            right
                .last_activity_ms
                .cmp(&left.last_activity_ms)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        Ok(sessions)
    }

    fn last_commit(
        &self,
        session_id: &str,
        hint: Option<&SessionReadHint>,
    ) -> Result<ReadResult<Option<Commit>>, QueryError> {
        let session_ulid = parse_session_id(session_id)?;
        let path = self.latest(session_id, hint)?;
        reject_compressed(&path)?;

        let mut commit = None;
        let mut diagnostics = Vec::new();
        commits_reverse(&path, session_ulid, &mut |item| {
            match item {
                StreamItem::Value(events) => commit = Some(Commit::new(events)),
                StreamItem::Fault(error) => diagnostics.push(error),
            }
            true
        })?;
        Ok(ReadResult::new(commit, diagnostics))
    }

    fn snapshot_windows(
        &self,
        session_id: &str,
        hint: Option<&SessionReadHint>,
        visit: &mut dyn FnMut(SnapshotWindow) -> bool,
    ) -> Result<ReadSummary, QueryError> {
        let session_ulid = parse_session_id(session_id)?;
        let latest = self.latest(session_id, hint)?;
        reject_compressed(&latest)?;

        let at_session_start = is_session_start(&latest, session_ulid);
        let mut diagnostics = Vec::new();
        snapshots_reverse(&latest, session_ulid, &mut |item| match item {
            StreamItem::Fault(error) => {
                diagnostics.push(error);
                false
            }
            StreamItem::Value(LogSnapshotWindow { snapshot, commits }) => {
                let origin = match snapshot {
                    Some(snapshot) => WindowOrigin::Snapshot(Box::new(snapshot)),
                    None if at_session_start => WindowOrigin::SessionStart,
                    None => WindowOrigin::Unanchored,
                };
                visit(SnapshotWindow {
                    origin,
                    commits: commits.into_iter().map(Commit::new).collect(),
                })
            }
        })?;
        Ok(ReadSummary { diagnostics })
    }

    fn all_commits_forward(
        &self,
        session_id: &str,
        visit: &mut dyn FnMut(Commit) -> bool,
    ) -> Result<ReadSummary, QueryError> {
        let session_ulid = parse_session_id(session_id)?;
        let paths = self
            .indexed_paths(session_id)?
            .into_iter()
            .map(|(_, path)| path)
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return Err(QueryError::SessionNotFound(session_id.to_owned()));
        }

        let mut diagnostics = Vec::new();
        commits_forward(&paths, session_ulid, &mut |item| match item {
            StreamItem::Value(events) => visit(Commit::new(events)),
            StreamItem::Fault(error) => {
                diagnostics.push(error);
                false
            }
        })?;
        Ok(ReadSummary { diagnostics })
    }

    fn scan_after(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        visit: &mut dyn FnMut(EventEnvelope) -> bool,
    ) -> Result<(), QueryError> {
        validate_cursor(session_id, cursor)?;
        self.scan_after_from_files(session_id, cursor, visit)
    }

    fn before(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, QueryError> {
        validate_cursor(session_id, cursor)?;
        self.before_from_files(session_id, cursor, limit)
    }

    fn event(&self, session_id: &str, event_id: &str) -> Result<Option<EventEnvelope>, QueryError> {
        validate_cursor(session_id, Some(event_id))?;
        self.event_from_files(session_id, event_id)
    }
}

fn select_history_paths(
    paths: Vec<(String, PathBuf)>,
    cursor: Option<&str>,
    through_cursor: bool,
) -> Vec<PathBuf> {
    let boundary = cursor
        .map(|cursor| paths.partition_point(|(first, _)| first.as_str() <= cursor))
        .unwrap_or(if through_cursor { paths.len() } else { 1 });
    if through_cursor {
        paths
            .into_iter()
            .take(boundary)
            .map(|(_, path)| path)
            .collect()
    } else {
        paths
            .into_iter()
            .skip(boundary.saturating_sub(1))
            .map(|(_, path)| path)
            .collect()
    }
}

fn decode_history_record(record: &HistoryRecord<'_>) -> Result<EventEnvelope, QueryError> {
    decode_history_line(record.path, record.line)
}

fn decode_history_line(path: &Path, line: &[u8]) -> Result<EventEnvelope, QueryError> {
    super::log::decode_envelope(line).map_err(|error| {
        QueryError::InvalidLog(format!(
            "segment {} has an unmigratable commit: {error}",
            path.display()
        ))
    })
}

fn reject_compressed(path: &Path) -> Result<(), QueryError> {
    if is_compressed(path) {
        Err(QueryError::InvalidSegment(path.to_owned()))
    } else {
        Ok(())
    }
}

fn is_compressed(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".jsonl.zst")
}

fn is_session_start(path: &Path, session_ulid: Ulid) -> bool {
    first_event_id(path).and_then(|event_id| EventId::parse_sequence(session_ulid, event_id).ok())
        == Some(1)
}

fn validate_cursor(session_id: &str, cursor: Option<&str>) -> Result<(), QueryError> {
    let session_ulid = parse_session_id(session_id)?;
    if let Some(cursor) = cursor {
        EventId::parse_sequence(session_ulid, cursor)
            .map_err(|_| QueryError::InvalidCursor(cursor.to_owned()))?;
    }
    Ok(())
}

fn parse_session_id(session_id: &str) -> Result<Ulid, QueryError> {
    session_id
        .parse()
        .map_err(|_| QueryError::InvalidSessionId(session_id.to_owned()))
}

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid session id {0:?}")]
    InvalidSessionId(String),
    #[error("invalid event cursor {0:?}")]
    InvalidCursor(String),
    #[error("unknown event cursor {0:?}")]
    UnknownCursor(String),
    #[error("invalid segment {0}")]
    InvalidSegment(PathBuf),
    #[error("invalid durable log: {0}")]
    InvalidLog(String),
    #[error("session {0} does not exist")]
    SessionNotFound(String),
}

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};

use ulid::Ulid;
use zork_agent::session::event_id::EventId;
use zork_agent::session::events::SessionEvent;
use zork_agent::session::query::{
    Commit, QueryError, ReadResult, ReadSummary, SessionDiscovery, SessionQuery, SessionReadHint,
    SnapshotWindow, WindowOrigin,
};
use zork_agent::session::store::EventEnvelope;

use crate::store::MemoryData;

#[derive(Clone)]
pub struct MemorySessionQuery {
    inner: Arc<Mutex<MemoryData>>,
    recovery_gate: Arc<(Mutex<PauseGate>, Condvar)>,
    history_scan_gate: Arc<(Mutex<PauseGate>, Condvar)>,
}

#[derive(Default)]
struct PauseGate {
    paused_sessions: HashSet<String>,
    blocked: HashMap<String, usize>,
}

pub struct QueryPause {
    session_id: String,
    gate: Arc<(Mutex<PauseGate>, Condvar)>,
}

impl MemorySessionQuery {
    pub(crate) fn new(inner: Arc<Mutex<MemoryData>>) -> Self {
        Self {
            inner,
            recovery_gate: Arc::new((Mutex::new(PauseGate::default()), Condvar::new())),
            history_scan_gate: Arc::new((Mutex::new(PauseGate::default()), Condvar::new())),
        }
    }

    pub fn recovery_calls(&self, session_id: &str) -> usize {
        self.inner
            .lock()
            .expect("memory query lock poisoned")
            .recovery_calls
            .get(session_id)
            .copied()
            .unwrap_or_default()
    }

    pub fn history_scan_calls(&self, session_id: &str) -> usize {
        self.inner
            .lock()
            .expect("memory query lock poisoned")
            .history_scan_calls
            .get(session_id)
            .copied()
            .unwrap_or_default()
    }

    pub fn pause_recovery(&self, session_id: impl Into<String>) -> QueryPause {
        Self::pause(&self.recovery_gate, session_id.into(), "recovery")
    }

    pub fn pause_history_scan(&self, session_id: impl Into<String>) -> QueryPause {
        Self::pause(&self.history_scan_gate, session_id.into(), "history scan")
    }

    fn pause(
        gate_state: &Arc<(Mutex<PauseGate>, Condvar)>,
        session_id: String,
        operation: &str,
    ) -> QueryPause {
        let (lock, _) = gate_state.as_ref();
        let mut gate = lock.lock().expect("memory query gate poisoned");
        assert!(
            gate.paused_sessions.insert(session_id.clone()),
            "memory {operation} for {session_id} is already paused"
        );
        drop(gate);
        QueryPause {
            session_id,
            gate: gate_state.clone(),
        }
    }

    pub fn blocked_recoveries(&self, session_id: &str) -> usize {
        Self::blocked(&self.recovery_gate, session_id)
    }

    pub fn blocked_history_scans(&self, session_id: &str) -> usize {
        Self::blocked(&self.history_scan_gate, session_id)
    }

    fn blocked(gate: &Arc<(Mutex<PauseGate>, Condvar)>, session_id: &str) -> usize {
        gate.0
            .lock()
            .expect("memory query gate poisoned")
            .blocked
            .get(session_id)
            .copied()
            .unwrap_or_default()
    }

    fn wait_for_permission(gate: &Arc<(Mutex<PauseGate>, Condvar)>, session_id: &str) {
        let (lock, changed) = gate.as_ref();
        let mut gate = lock.lock().expect("memory query gate poisoned");
        if !gate.paused_sessions.contains(session_id) {
            return;
        }
        *gate.blocked.entry(session_id.to_owned()).or_default() += 1;
        changed.notify_all();
        while gate.paused_sessions.contains(session_id) {
            gate = changed
                .wait(gate)
                .expect("memory query gate poisoned while waiting");
        }
        if let Some(blocked) = gate.blocked.get_mut(session_id) {
            *blocked = blocked.saturating_sub(1);
            if *blocked == 0 {
                gate.blocked.remove(session_id);
            }
        }
    }

    fn events(&self, session_id: &str) -> Result<Vec<EventEnvelope>, QueryError> {
        self.inner
            .lock()
            .expect("memory query lock poisoned")
            .sessions
            .get(session_id)
            .map(|session| session.events.clone())
            .ok_or_else(|| QueryError::SessionNotFound(session_id.to_owned()))
    }
}

impl Drop for QueryPause {
    fn drop(&mut self) {
        let (lock, changed) = self.gate.as_ref();
        let mut gate = lock.lock().expect("memory query gate poisoned");
        gate.paused_sessions.remove(&self.session_id);
        changed.notify_all();
    }
}

impl SessionQuery for MemorySessionQuery {
    fn exists(&self, session_id: &str) -> bool {
        session_id.parse::<Ulid>().is_ok()
            && self
                .inner
                .lock()
                .expect("memory query lock poisoned")
                .sessions
                .contains_key(session_id)
    }

    fn discover_sessions(&self) -> Result<Vec<SessionDiscovery>, QueryError> {
        let data = self.inner.lock().expect("memory query lock poisoned");
        let mut sessions = data
            .sessions
            .iter()
            .map(|(session_id, session)| SessionDiscovery {
                session_id: session_id.clone(),
                last_activity_ms: session.last_activity,
                read_hint: SessionReadHint::empty(session_id.clone()),
            })
            .collect::<Vec<_>>();
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
        _hint: Option<&SessionReadHint>,
    ) -> Result<ReadResult<Option<Commit>>, QueryError> {
        parse_session_id(session_id)?;
        let events = self.events(session_id)?;
        Ok(ReadResult {
            value: complete_commits(&events).pop(),
            diagnostics: Vec::new(),
        })
    }

    fn snapshot_windows(
        &self,
        session_id: &str,
        _hint: Option<&SessionReadHint>,
        visit: &mut dyn FnMut(SnapshotWindow) -> bool,
    ) -> Result<ReadSummary, QueryError> {
        parse_session_id(session_id)?;
        Self::wait_for_permission(&self.recovery_gate, session_id);
        let events = {
            let mut data = self.inner.lock().expect("memory query lock poisoned");
            *data
                .recovery_calls
                .entry(session_id.to_owned())
                .or_default() += 1;
            data.sessions
                .get(session_id)
                .map(|session| session.events.clone())
                .ok_or_else(|| QueryError::SessionNotFound(session_id.to_owned()))?
        };

        let mut newer = Vec::new();
        let mut stopped = false;
        for mut commit in complete_commits(&events).into_iter().rev() {
            if commit.events.len() == 1
                && matches!(commit.events[0].event, SessionEvent::Snapshot { .. })
            {
                newer.reverse();
                stopped = visit(SnapshotWindow {
                    origin: WindowOrigin::Snapshot(Box::new(commit.events.pop().unwrap())),
                    commits: std::mem::take(&mut newer),
                });
                if stopped {
                    break;
                }
            } else {
                newer.push(commit);
            }
        }
        if !stopped && !newer.is_empty() {
            newer.reverse();
            let _ = visit(SnapshotWindow {
                origin: WindowOrigin::SessionStart,
                commits: newer,
            });
        }
        Ok(ReadSummary::default())
    }

    fn all_commits_forward(
        &self,
        session_id: &str,
        visit: &mut dyn FnMut(Commit) -> bool,
    ) -> Result<ReadSummary, QueryError> {
        parse_session_id(session_id)?;
        for commit in complete_commits(&self.events(session_id)?) {
            if visit(commit) {
                break;
            }
        }
        Ok(ReadSummary::default())
    }

    fn scan_after(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        visit: &mut dyn FnMut(EventEnvelope) -> bool,
    ) -> Result<(), QueryError> {
        let session_ulid = parse_session_id(session_id)?;
        validate_cursor(session_ulid, cursor)?;
        {
            let mut data = self.inner.lock().expect("memory query lock poisoned");
            *data
                .history_scan_calls
                .entry(session_id.to_owned())
                .or_default() += 1;
        }
        Self::wait_for_permission(&self.history_scan_gate, session_id);
        let events = self
            .inner
            .lock()
            .expect("memory query lock poisoned")
            .sessions
            .get(session_id)
            .map(|session| session.events.clone())
            .unwrap_or_default();
        let start = match cursor {
            Some(cursor) => events
                .iter()
                .position(|event| event.event_id == cursor)
                .map(|position| position + 1)
                .ok_or_else(|| QueryError::UnknownCursor(cursor.to_owned()))?,
            None => 0,
        };
        for event in events
            .into_iter()
            .skip(start)
            .filter(|event| event.event.is_history_visible())
        {
            if visit(event) {
                break;
            }
        }
        Ok(())
    }

    fn before(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, QueryError> {
        let session_ulid = parse_session_id(session_id)?;
        validate_cursor(session_ulid, cursor)?;
        let data = self.inner.lock().expect("memory query lock poisoned");
        let events = data
            .sessions
            .get(session_id)
            .map(|session| session.events.as_slice())
            .unwrap_or_default();
        let end = match cursor {
            Some(cursor) => events
                .iter()
                .position(|event| event.event_id == cursor)
                .ok_or_else(|| QueryError::UnknownCursor(cursor.to_owned()))?,
            None => events.len(),
        };
        let mut selected = events[..end]
            .iter()
            .rev()
            .filter(|event| event.event.is_history_visible())
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        selected.reverse();
        Ok(selected)
    }

    fn event(&self, session_id: &str, event_id: &str) -> Result<Option<EventEnvelope>, QueryError> {
        let session_ulid = parse_session_id(session_id)?;
        validate_cursor(session_ulid, Some(event_id))?;
        let data = self.inner.lock().expect("memory query lock poisoned");
        Ok(data
            .sessions
            .get(session_id)
            .into_iter()
            .flat_map(|session| session.events.iter())
            .find(|event| event.event_id == event_id && event.event.is_history_visible())
            .cloned())
    }
}

fn complete_commits(events: &[EventEnvelope]) -> Vec<Commit> {
    let mut commits = Vec::new();
    let mut index = 0;
    while index < events.len() {
        let count = events[index].batch_count as usize;
        let end = index.saturating_add(count);
        if count == 0 || end > events.len() {
            break;
        }
        let events = &events[index..end];
        if !events.iter().enumerate().all(|(batch_index, event)| {
            event.batch_count as usize == count && event.batch_index as usize == batch_index
        }) {
            break;
        }
        commits.push(Commit::new(events.to_vec()));
        index = end;
    }
    commits
}

fn parse_session_id(session_id: &str) -> Result<Ulid, QueryError> {
    session_id
        .parse()
        .map_err(|_| QueryError::InvalidSessionId(session_id.to_owned()))
}

fn validate_cursor(session_id: Ulid, cursor: Option<&str>) -> Result<(), QueryError> {
    if let Some(cursor) = cursor {
        EventId::parse(session_id, cursor)
            .map_err(|_| QueryError::InvalidCursor(cursor.to_owned()))?;
    }
    Ok(())
}

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use serde_json::Value;
use ulid::Ulid;
use zork_agent::session::event_id::EventId;
use zork_agent::session::events::{SessionEvent, EVENT_SCHEMA_VERSION};
use zork_agent::session::store::{
    DetachedSession, EventEnvelope, SessionStore, SnapshotAppend, StoreError,
};

use crate::query::MemorySessionQuery;

#[derive(Clone)]
pub struct MemorySessionStore {
    pub(crate) inner: Arc<Mutex<MemoryData>>,
    append_gate: Arc<(Mutex<AppendGate>, Condvar)>,
    query: MemorySessionQuery,
}

#[derive(Default)]
struct AppendGate {
    paused: bool,
    blocked: usize,
    release_error: Option<String>,
}

pub struct AppendPause {
    gate: Arc<(Mutex<AppendGate>, Condvar)>,
    released: bool,
}

#[derive(Default)]
pub(crate) struct MemoryData {
    pub(crate) sessions: HashMap<String, MemorySession>,
    pub(crate) next_activity: i64,
    pub(crate) recovery_calls: HashMap<String, usize>,
    pub(crate) history_scan_calls: HashMap<String, usize>,
}

#[derive(Default)]
pub(crate) struct MemorySession {
    pub(crate) events: Vec<EventEnvelope>,
    pub(crate) last_activity: i64,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn query(&self) -> MemorySessionQuery {
        self.query.clone()
    }

    pub fn events(&self, session_id: &str) -> Vec<EventEnvelope> {
        self.inner
            .lock()
            .expect("memory store lock poisoned")
            .sessions
            .get(session_id)
            .map(|session| session.events.clone())
            .unwrap_or_default()
    }

    pub fn pause_appends(&self) -> AppendPause {
        let (lock, _) = self.append_gate.as_ref();
        let mut gate = lock.lock().expect("memory append gate poisoned");
        assert!(!gate.paused, "memory appends are already paused");
        gate.paused = true;
        drop(gate);
        AppendPause {
            gate: self.append_gate.clone(),
            released: false,
        }
    }

    pub fn blocked_appends(&self) -> usize {
        self.append_gate
            .0
            .lock()
            .expect("memory append gate poisoned")
            .blocked
    }

    fn wait_for_append_permission(&self) -> Result<(), StoreError> {
        let (lock, changed) = self.append_gate.as_ref();
        let mut gate = lock.lock().expect("memory append gate poisoned");
        if !gate.paused {
            return take_append_release(&mut gate);
        }
        gate.blocked = gate.blocked.saturating_add(1);
        changed.notify_all();
        while gate.paused {
            gate = changed
                .wait(gate)
                .expect("memory append gate poisoned while waiting");
        }
        gate.blocked = gate.blocked.saturating_sub(1);
        take_append_release(&mut gate)
    }
}

impl Default for MemorySessionStore {
    fn default() -> Self {
        let inner = Arc::new(Mutex::new(MemoryData::default()));
        Self {
            query: MemorySessionQuery::new(inner.clone()),
            inner,
            append_gate: Arc::new((Mutex::new(AppendGate::default()), Condvar::new())),
        }
    }
}

impl Drop for AppendPause {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let (lock, changed) = self.gate.as_ref();
        let mut gate = lock.lock().expect("memory append gate poisoned");
        gate.paused = false;
        changed.notify_all();
    }
}

impl AppendPause {
    pub fn release_with_error(mut self, message: impl Into<String>) {
        let (lock, changed) = self.gate.as_ref();
        let mut gate = lock.lock().expect("memory append gate poisoned");
        gate.release_error = Some(message.into());
        gate.paused = false;
        changed.notify_all();
        drop(gate);
        self.released = true;
    }
}

impl SessionStore for MemorySessionStore {
    fn append_batch(
        &self,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        self.wait_for_append_permission()?;
        let session_ulid = parse_session_id(session_id)?;
        let batch_count = u32::try_from(events.len()).map_err(|_| StoreError::BatchTooLarge)?;
        let mut data = self.inner.lock().expect("memory store lock poisoned");
        let base_sequence = data
            .sessions
            .get(session_id)
            .map(|session| session.events.len())
            .unwrap_or_default();
        let mut appended = Vec::with_capacity(events.len());
        for (index, event) in events.iter().enumerate() {
            let sequence = u64::try_from(base_sequence.saturating_add(index).saturating_add(1))
                .map_err(|_| StoreError::BatchTooLarge)?;
            appended.push(EventEnvelope {
                event_id: EventId::from_sequence(session_ulid, sequence)?.to_string(),
                schema_version: EVENT_SCHEMA_VERSION,
                batch_index: index as u32,
                batch_count,
                event: event.clone(),
            });
        }
        data.next_activity = data.next_activity.saturating_add(1);
        let activity = data.next_activity;
        let session = data.sessions.entry(session_id.to_owned()).or_default();
        session.events.extend(appended.iter().cloned());
        session.last_activity = activity;
        Ok(appended)
    }

    fn append_snapshot(
        &self,
        session_id: &str,
        state_schema_version: u32,
        state: Value,
    ) -> Result<SnapshotAppend, StoreError> {
        let event = SessionEvent::Snapshot {
            state_schema_version,
            state,
        };
        let envelope = self
            .append_batch(session_id, &[event])?
            .into_iter()
            .next()
            .expect("one snapshot event was appended");
        Ok(SnapshotAppend {
            envelope,
            sealed_segment: None,
        })
    }

    fn compress_segment(&self, _: &str, _: &str) -> Result<(), StoreError> {
        Ok(())
    }

    fn detach_session(&self, session_id: &str) -> Result<DetachedSession, StoreError> {
        parse_session_id(session_id)?;
        let removed = self
            .inner
            .lock()
            .expect("memory store lock poisoned")
            .sessions
            .remove(session_id);
        removed
            .map(|_| DetachedSession::empty())
            .ok_or_else(|| StoreError::SessionNotFound(session_id.to_owned()))
    }
}

fn parse_session_id(session_id: &str) -> Result<Ulid, StoreError> {
    session_id
        .parse()
        .map_err(|_| StoreError::InvalidSessionId(session_id.to_owned()))
}

fn take_append_release(gate: &mut AppendGate) -> Result<(), StoreError> {
    match gate.release_error.take() {
        Some(message) => Err(StoreError::Io(std::io::Error::other(message))),
        None => Ok(()),
    }
}

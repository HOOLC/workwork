//! Composition of durable queries into a recovered [`SessionState`].

use std::collections::VecDeque;

use super::events::SessionEvent;
use super::log::EventEnvelope;
use super::query::{Commit, QueryError, SessionQuery, SessionReadHint, WindowOrigin};
use super::state::{migrate_snapshot, SessionState};
use super::tools::ToolRegistry;

#[derive(Clone, Debug, PartialEq)]
pub struct Recovery {
    pub state: SessionState,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Query(#[from] QueryError),
    #[error("session {session_id} could not be recovered: {message}")]
    Invalid { session_id: String, message: String },
}

/// Rebuilds state by composing bounded queries. The normal path only reads
/// the latest segment; all-segment traversal is a fallback when that segment
/// has no independent recovery origin.
pub fn recover(
    query: &dyn SessionQuery,
    session_id: &str,
    hint: Option<&SessionReadHint>,
    tools: &ToolRegistry,
) -> Result<Recovery, RecoveryError> {
    let mut newer_commits = VecDeque::<Vec<Commit>>::new();
    let mut snapshot_diagnostics = Vec::new();
    let mut recovered = None;
    let read = query.snapshot_windows(session_id, hint, &mut |window| match window.origin {
        WindowOrigin::Snapshot(snapshot) => {
            let snapshot = *snapshot;
            let SessionEvent::Snapshot {
                state_schema_version,
                state,
            } = snapshot.event
            else {
                unreachable!("snapshot query returned a non-snapshot origin")
            };
            let mut diagnostics = Vec::new();
            if let Some(mut state) = restore_snapshot(
                session_id,
                &snapshot.event_id,
                state_schema_version,
                state,
                tools,
                &mut diagnostics,
            ) {
                fold_commits(
                    &mut state,
                    window.commits,
                    session_id,
                    tools,
                    &mut diagnostics,
                );
                for commits in &newer_commits {
                    fold_commits_ref(&mut state, commits, session_id, tools, &mut diagnostics);
                }
                recovered = Some((state, diagnostics));
                true
            } else {
                snapshot_diagnostics.extend(diagnostics);
                newer_commits.push_front(window.commits);
                false
            }
        }
        WindowOrigin::SessionStart => {
            let mut state = SessionState::empty(session_id);
            let mut diagnostics = Vec::new();
            fold_commits(
                &mut state,
                window.commits,
                session_id,
                tools,
                &mut diagnostics,
            );
            for commits in &newer_commits {
                fold_commits_ref(&mut state, commits, session_id, tools, &mut diagnostics);
            }
            recovered = Some((state, diagnostics));
            true
        }
        WindowOrigin::Unanchored => true,
    })?;

    if let Some((state, fold_diagnostics)) = recovered {
        let mut diagnostics = read.diagnostics;
        diagnostics.extend(snapshot_diagnostics);
        diagnostics.extend(fold_diagnostics);
        return finish(session_id, state, diagnostics);
    }

    let mut state = SessionState::empty(session_id);
    let mut fold_diagnostics = Vec::new();
    let read = query.all_commits_forward(session_id, &mut |commit| {
        fold_commit(&mut state, commit, session_id, tools, &mut fold_diagnostics);
        false
    })?;
    let mut diagnostics = read.diagnostics;
    diagnostics.extend(fold_diagnostics);
    finish(session_id, state, diagnostics)
}

fn finish(
    session_id: &str,
    state: SessionState,
    diagnostics: Vec<String>,
) -> Result<Recovery, RecoveryError> {
    if !state.is_created() {
        return Err(RecoveryError::Invalid {
            session_id: session_id.to_owned(),
            message: "session has no complete SessionCreated event".into(),
        });
    }
    Ok(Recovery { state, diagnostics })
}

fn fold_commits(
    state: &mut SessionState,
    commits: Vec<Commit>,
    session_id: &str,
    tools: &ToolRegistry,
    diagnostics: &mut Vec<String>,
) {
    for commit in commits {
        fold_commit(state, commit, session_id, tools, diagnostics);
    }
}

fn fold_commit(
    state: &mut SessionState,
    commit: Commit,
    session_id: &str,
    tools: &ToolRegistry,
    diagnostics: &mut Vec<String>,
) {
    for envelope in commit.events {
        apply_envelope(state, envelope, session_id, tools, diagnostics);
    }
}

fn fold_commits_ref(
    state: &mut SessionState,
    commits: &[Commit],
    session_id: &str,
    tools: &ToolRegistry,
    diagnostics: &mut Vec<String>,
) {
    for commit in commits {
        for envelope in &commit.events {
            apply_envelope(state, envelope.clone(), session_id, tools, diagnostics);
        }
    }
}

fn apply_envelope(
    state: &mut SessionState,
    envelope: EventEnvelope,
    session_id: &str,
    tools: &ToolRegistry,
    diagnostics: &mut Vec<String>,
) {
    match envelope.event {
        SessionEvent::Snapshot {
            state_schema_version,
            state: snapshot,
        } => {
            if let Some(snapshot) = restore_snapshot(
                session_id,
                &envelope.event_id,
                state_schema_version,
                snapshot,
                tools,
                diagnostics,
            ) {
                *state = snapshot;
            }
        }
        event => {
            if let Err(error) = state.apply(&event, tools) {
                diagnostics.push(format!(
                    "event {} could not be folded: {error}",
                    envelope.event_id
                ));
            }
        }
    }
}

fn restore_snapshot(
    session_id: &str,
    event_id: &str,
    state_schema_version: u32,
    snapshot: serde_json::Value,
    tools: &ToolRegistry,
    diagnostics: &mut Vec<String>,
) -> Option<SessionState> {
    match migrate_snapshot(state_schema_version, snapshot, tools) {
        Ok(state) if state.session_id != session_id => {
            diagnostics.push(format!(
                "snapshot {event_id} belongs to session {} instead of {session_id}",
                state.session_id
            ));
            None
        }
        Ok(state) if !state.is_created() => {
            diagnostics.push(format!(
                "snapshot {event_id} does not contain a created session state"
            ));
            None
        }
        Ok(state) => Some(state),
        Err(error) => {
            diagnostics.push(format!("snapshot {event_id} was not usable: {error}"));
            None
        }
    }
}

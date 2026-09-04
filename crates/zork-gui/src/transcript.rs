//! Pure transcript projection and reconciliation helpers.
//!
//! The HTTP/SSE client intentionally exposes only public message content, not
//! internal event ids. These helpers keep the ordering and optimistic-send
//! rules explicit and testable without constructing a GPUI window.

use crate::api::{AgentStatus, Role, TranscriptMessage};

/// One renderable transcript row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptLine {
    Message { role: Role, content: String },
}

/// Convert a gateway-delivered message into a renderable line.
pub fn transcript_line_from(item: &TranscriptMessage) -> Option<TranscriptLine> {
    match item {
        TranscriptMessage::Message { role, content } => Some(TranscriptLine::Message {
            role: *role,
            content: content.clone(),
        }),
    }
}

pub fn project_page_with_pending(
    items: &[TranscriptMessage],
    pending_user: &mut Vec<String>,
) -> Vec<TranscriptLine> {
    let mut lines: Vec<_> = items.iter().filter_map(transcript_line_from).collect();
    for content in items.iter().rev().take(32).filter_map(|item| match item {
        TranscriptMessage::Message {
            role: Role::User,
            content,
        } => Some(content.as_str()),
        _ => None,
    }) {
        take_pending_user_echo(pending_user, content);
    }
    for content in pending_user.iter() {
        let is_already_projected = lines.iter().rev().take(32).any(|line| {
            matches!(
                line,
                TranscriptLine::Message {
                    role: Role::User,
                    content: projected,
                } if projected == content
            )
        });
        if !is_already_projected {
            lines.push(TranscriptLine::Message {
                role: Role::User,
                content: content.clone(),
            });
        }
    }
    lines
}

/// Insert one local user row before the network request completes.
pub fn begin_optimistic_user(
    lines: &mut Vec<TranscriptLine>,
    pending_user: &mut Vec<String>,
    content: String,
) {
    lines.push(TranscriptLine::Message {
        role: Role::User,
        content: content.clone(),
    });
    pending_user.push(content);
}

/// Consume the matching pending entry when the gateway user-message echo arrives.
/// `true` means the caller must suppress that echo because the local row is
/// already visible.
pub fn take_pending_user_echo(pending_user: &mut Vec<String>, content: &str) -> bool {
    let Some(position) = pending_user.iter().position(|item| item == content) else {
        return false;
    };
    pending_user.remove(position);
    true
}

/// Remove the pending optimistic row after a failed gateway send.
pub fn rollback_optimistic_user(
    lines: &mut Vec<TranscriptLine>,
    pending_user: &mut Vec<String>,
    content: &str,
) -> bool {
    let Some(position) = pending_user.iter().position(|item| item == content) else {
        return false;
    };
    pending_user.remove(position);

    let Some(line_position) = lines.iter().rposition(|line| {
        matches!(
            line,
            TranscriptLine::Message {
                role: Role::User,
                content: projected,
            } if projected == content
        )
    }) else {
        return false;
    };
    lines.remove(line_position);
    true
}

/// Prepend an oldest-first history page. The longest exact page-boundary
/// overlap is removed so a retried page cannot repeat already visible rows.
/// Returns the number of inserted renderable rows.
pub fn prepend_older_lines(lines: &mut Vec<TranscriptLine>, items: &[TranscriptMessage]) -> usize {
    let older: Vec<_> = items.iter().filter_map(transcript_line_from).collect();
    let max_overlap = older.len().min(lines.len());
    let overlap = (1..=max_overlap)
        .rev()
        .find(|count| older[older.len() - count..] == lines[..*count])
        .unwrap_or(0);
    let inserted = older.len() - overlap;
    if inserted > 0 {
        lines.splice(0..0, older[..inserted].iter().cloned());
    }
    inserted
}

fn truncate(text: &str, max_chars: usize) -> &str {
    let boundary = text
        .char_indices()
        .nth(max_chars)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(text.len());
    &text[..boundary]
}

fn short_id(id: &str) -> &str {
    truncate(id, 8)
}

/// Partial history is not the beginning of the task and therefore cannot
/// supply its title. Use the stable id until the true oldest page is present.
pub fn stable_task_title(id: &str, lines: &[TranscriptLine], has_older: bool) -> String {
    if has_older {
        return format!("Task {}", short_id(id));
    }
    lines
        .iter()
        .find_map(|line| match line {
            TranscriptLine::Message {
                role: Role::User,
                content,
            } => content
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| truncate(line, 72).to_owned()),
            _ => None,
        })
        .unwrap_or_else(|| format!("Task {}", short_id(id)))
}

/// Live activity supplements, but never becomes, a persisted message row.
pub fn should_render_live_activity(
    status: &AgentStatus,
    _last_line: Option<&TranscriptLine>,
) -> bool {
    !matches!(status, AgentStatus::Clear | AgentStatus::Finished)
}

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::config::RuntimeConfig;
use crate::im_entry::ImEntryGateway;
use anyhow::{Context, Result};
use reqwest::header::ACCEPT;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::warn;
use zork_agent_api::{ApiErrorBody, ApiErrorCode, DurableEvent, DURABLE_EVENT_NAME};

const WAIT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct AgentStatusProjector {
    config: RuntimeConfig,
    http: reqwest::Client,
    entries: ImEntryGateway,
    subscriptions: Arc<Mutex<HashMap<String, Subscription>>>,
}

struct Subscription {
    agent_session_id: String,
    connection_id: String,
    channel_id: String,
    root_message_id: String,
    task: JoinHandle<()>,
}

#[derive(Clone)]
struct ProjectionTarget {
    connection_id: String,
    channel_id: String,
    root_message_id: String,
    session_key: String,
}

impl AgentStatusProjector {
    pub fn new(config: RuntimeConfig, entries: ImEntryGateway) -> Result<Self> {
        Ok(Self {
            config,
            http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .context("Agent status HTTP client")?,
            entries,
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn ensure(
        &self,
        session_key: &str,
        agent_session_id: &str,
        connection_id: &str,
        channel_id: &str,
        root_message_id: &str,
    ) {
        let mut subscriptions = self.subscriptions.lock().await;
        if subscriptions.get(session_key).is_some_and(|subscription| {
            subscription.agent_session_id == agent_session_id
                && subscription.connection_id == connection_id
                && subscription.channel_id == channel_id
                && subscription.root_message_id == root_message_id
                && !subscription.task.is_finished()
        }) {
            return;
        }
        if let Some(previous) = subscriptions.remove(session_key) {
            previous.task.abort();
        }
        let config = self.config.clone();
        let http = self.http.clone();
        let entries = self.entries.clone();
        let session_key_owned = session_key.to_owned();
        let agent_session_id_owned = agent_session_id.to_owned();
        let connection_id_owned = connection_id.to_owned();
        let channel_id_owned = channel_id.to_owned();
        let root_message_id_owned = root_message_id.to_owned();
        let task_session_key = session_key_owned.clone();
        let task_agent_session_id = agent_session_id_owned.clone();
        let task_connection_id = connection_id_owned.clone();
        let task_channel_id = channel_id_owned.clone();
        let task_root_message_id = root_message_id_owned.clone();
        let task = tokio::spawn(async move {
            run_subscription(
                config,
                http,
                entries,
                ProjectionTarget {
                    connection_id: task_connection_id,
                    channel_id: task_channel_id,
                    root_message_id: task_root_message_id,
                    session_key: task_session_key,
                },
                task_agent_session_id,
            )
            .await;
        });
        subscriptions.insert(
            session_key_owned,
            Subscription {
                agent_session_id: agent_session_id_owned,
                connection_id: connection_id_owned,
                channel_id: channel_id_owned,
                root_message_id: root_message_id_owned,
                task,
            },
        );
    }

    pub async fn remove(&self, session_key: &str) {
        let removed = self.subscriptions.lock().await.remove(session_key);
        if let Some(subscription) = removed {
            subscription.task.abort();
            self.entries
                .clear_status(
                    &subscription.connection_id,
                    session_key,
                    &subscription.channel_id,
                    &subscription.root_message_id,
                )
                .await;
        }
    }
}

async fn run_subscription(
    config: RuntimeConfig,
    http: reqwest::Client,
    entries: ImEntryGateway,
    target: ProjectionTarget,
    agent_session_id: String,
) {
    let mut cursor = None;
    loop {
        match open_stream(&config, &http, &agent_session_id, cursor.as_deref()).await {
            Ok(Some(response)) => {
                if let Err(error) = consume_stream(
                    response,
                    &entries,
                    &target.connection_id,
                    &target.channel_id,
                    &target.root_message_id,
                    &target.session_key,
                    &mut cursor,
                )
                .await
                {
                    warn!(
                        session = %target.session_key,
                        agent_session_id,
                        error = %error,
                        "Agent status event stream disconnected"
                    );
                }
            }
            Ok(None) => return,
            Err(error) => {
                warn!(
                    session = %target.session_key,
                    agent_session_id,
                    error = %error,
                    "Agent status event stream connection failed"
                );
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn open_stream(
    config: &RuntimeConfig,
    http: &reqwest::Client,
    agent_session_id: &str,
    cursor: Option<&str>,
) -> Result<Option<reqwest::Response>> {
    let mut request = http
        .get(format!(
            "{}/sessions/{agent_session_id}/events",
            crate::agent::base_url(config)
        ))
        .header(ACCEPT, "text/event-stream");
    if let Some(cursor) = cursor {
        request = request.header("Last-Event-ID", cursor);
    }
    let response = crate::agent::authenticate(config, request)
        .send()
        .await
        .context("subscribe to Agent status events")?;
    let status = response.status();
    if !status.is_success() {
        let error = response.json::<ApiErrorBody>().await.with_context(|| {
            format!("invalid Agent event stream error response for HTTP {status}")
        })?;
        if status == reqwest::StatusCode::NOT_FOUND
            && error.error.code == ApiErrorCode::SessionNotFound
        {
            return Ok(None);
        }
        anyhow::bail!(
            "Agent event stream returned {status}: {}",
            error.error.message
        );
    }
    Ok(Some(response))
}

async fn consume_stream(
    mut response: reqwest::Response,
    entries: &ImEntryGateway,
    connection_id: &str,
    channel_id: &str,
    root_message_id: &str,
    session_key: &str,
    cursor: &mut Option<String>,
) -> Result<()> {
    let mut decoder = SseDecoder::default();
    let mut projection = ProjectionState::default();
    let mut wait_refresh = tokio::time::interval(WAIT_REFRESH_INTERVAL);
    wait_refresh.set_missed_tick_behavior(MissedTickBehavior::Delay);
    wait_refresh.tick().await;
    loop {
        tokio::select! {
            chunk = response.chunk() => {
                let Some(chunk) = chunk.context("read Agent status event stream")? else {
                    anyhow::bail!("Agent status event stream ended");
                };
                for frame in decoder.push(&chunk) {
                    if frame.event != DURABLE_EVENT_NAME {
                        continue;
                    }
                    let envelope = serde_json::from_str::<DurableEvent<AgentEvent>>(&frame.data)
                        .context("invalid Agent event envelope")?;
                    if frame.id.as_deref() != Some(envelope.event_id.as_str()) {
                        anyhow::bail!("Agent event ID differs between SSE and envelope");
                    }
                    *cursor = Some(envelope.event_id);
                    let Some(event) = envelope.event.status_event() else {
                        continue;
                    };
                    let starts_wait = matches!(
                        &event,
                        AgentStatusEvent::Waiting { .. }
                            | AgentStatusEvent::ToolsWaiting { .. }
                    );
                    let status_event = serde_json::to_value(&event)
                        .context("serialize projected Agent status event")?;
                    let status = projection.apply(event, now_ms());
                    if starts_wait {
                        wait_refresh.reset_after(WAIT_REFRESH_INTERVAL);
                    }
                    entries
                        .set_status(
                            connection_id,
                            session_key,
                            channel_id,
                            root_message_id,
                            status_event,
                            &status,
                        )
                        .await;
                }
            }
            _ = wait_refresh.tick(), if projection.waiting.is_some() => {
                let status = projection.refresh_wait(now_ms());
                entries
                    .refresh_status(
                        connection_id,
                        session_key,
                        channel_id,
                        root_message_id,
                        &status,
                    )
                    .await;
            }
        }
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AgentEvent {
    SessionCreated,
    InputAppended,
    SelectionChanged,
    ContextConfigured,
    TurnStarted,
    TurnCancelRequested,
    TurnFinished {
        outcome: AgentTurnOutcome,
    },
    StepStarted,
    StepCompleted {
        purpose: Option<String>,
        invocations: Vec<AgentInvocation>,
        auto_wait_deadline_ms: Option<i64>,
    },
    StepFailed {
        error: AgentFailure,
    },
    StepInterrupted,
    AutoWaitEnded,
    ToolCancelRequested,
    ToolResult {
        result: AgentToolResult,
    },
    #[serde(alias = "handoff_failed")]
    ContextFailed,
    #[serde(alias = "handoff_applied")]
    ContextApplied,
    DeadlineReached,
    RuntimeFault {
        failure: AgentFailure,
    },
    Snapshot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentTurnOutcome {
    Finished,
    Failed,
    Cancelled,
}

#[derive(Debug, Deserialize)]
struct AgentInvocation {
    invocation_id: String,
    tool: String,
}

#[derive(Debug, Deserialize)]
struct AgentToolResult {
    invocation_id: String,
    tool: String,
    outcome: String,
    data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AgentFailure {
    message: String,
}

impl AgentEvent {
    fn status_event(self) -> Option<AgentStatusEvent> {
        match self {
            Self::SessionCreated => Some(AgentStatusEvent::Clear),
            Self::TurnStarted | Self::StepStarted | Self::ContextApplied => {
                Some(AgentStatusEvent::Thinking)
            }
            Self::StepCompleted {
                purpose: Some(purpose),
                ..
            } if purpose != "conversation" => Some(AgentStatusEvent::Thinking),
            Self::StepCompleted {
                invocations,
                auto_wait_deadline_ms,
                ..
            } if !invocations.is_empty() => {
                let calls = invocations
                    .into_iter()
                    .map(|invocation| AgentToolCall {
                        tool_call_id: invocation.invocation_id,
                        tool_name: invocation.tool,
                    })
                    .collect();
                match auto_wait_deadline_ms {
                    Some(deadline_ms) => {
                        Some(AgentStatusEvent::ToolsWaiting { calls, deadline_ms })
                    }
                    None => Some(AgentStatusEvent::ToolsStarted { calls }),
                }
            }
            Self::StepCompleted { .. } => Some(AgentStatusEvent::Thinking),
            Self::StepFailed { error } | Self::RuntimeFault { failure: error } => {
                Some(AgentStatusEvent::Failed {
                    reason: error.message,
                })
            }
            Self::StepInterrupted | Self::TurnCancelRequested => {
                Some(AgentStatusEvent::Interrupted)
            }
            Self::ToolResult { result }
                if result.tool == "wait" && result.outcome == "succeeded" =>
            {
                result
                    .data
                    .get("until_ms")
                    .and_then(serde_json::Value::as_i64)
                    .map(|deadline_ms| AgentStatusEvent::Waiting {
                        reason: result
                            .data
                            .get("reason")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("the requested time")
                            .to_owned(),
                        deadline_ms,
                    })
            }
            Self::ToolResult { result } => Some(AgentStatusEvent::ToolFinished {
                tool_call_id: result.invocation_id,
            }),
            Self::TurnFinished { outcome } => Some(match outcome {
                AgentTurnOutcome::Finished => AgentStatusEvent::Finished,
                AgentTurnOutcome::Failed => AgentStatusEvent::Failed {
                    reason: "turn failed".to_owned(),
                },
                AgentTurnOutcome::Cancelled => AgentStatusEvent::Interrupted,
            }),
            Self::DeadlineReached => Some(AgentStatusEvent::Thinking),
            Self::ContextFailed
            | Self::AutoWaitEnded
            | Self::InputAppended
            | Self::SelectionChanged
            | Self::ContextConfigured
            | Self::ToolCancelRequested
            | Self::Snapshot => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, serde::Serialize, Eq, PartialEq)]
struct AgentToolCall {
    tool_call_id: String,
    tool_name: String,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize, Eq, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
enum AgentStatusEvent {
    Clear,
    Thinking,
    ToolsStarted {
        calls: Vec<AgentToolCall>,
    },
    ToolFinished {
        tool_call_id: String,
    },
    ToolsWaiting {
        calls: Vec<AgentToolCall>,
        deadline_ms: i64,
    },
    Waiting {
        reason: String,
        deadline_ms: i64,
    },
    Failed {
        reason: String,
    },
    Finished,
    Interrupted,
}

#[derive(Clone, Debug)]
struct WaitingStatus {
    reason: String,
    deadline_ms: i64,
}

#[derive(Default)]
struct ProjectionState {
    active_tools: Vec<AgentToolCall>,
    waiting: Option<WaitingStatus>,
}

impl ProjectionState {
    fn apply(&mut self, event: AgentStatusEvent, now_ms: i64) -> String {
        match event {
            AgentStatusEvent::Clear
            | AgentStatusEvent::Finished
            | AgentStatusEvent::Interrupted => {
                self.reset();
                String::new()
            }
            AgentStatusEvent::Thinking => {
                self.reset();
                "Thinking...".to_owned()
            }
            AgentStatusEvent::ToolsStarted { calls } => {
                self.waiting = None;
                self.active_tools = calls;
                self.current_tool_status()
            }
            AgentStatusEvent::ToolFinished { tool_call_id } => {
                self.waiting = None;
                self.active_tools
                    .retain(|call| call.tool_call_id != tool_call_id);
                if self.active_tools.is_empty() {
                    "Thinking...".to_owned()
                } else {
                    self.current_tool_status()
                }
            }
            AgentStatusEvent::ToolsWaiting { calls, deadline_ms } => {
                self.active_tools = calls;
                let reason = match self.active_tools.as_slice() {
                    [call] => format!("{} to finish", call.tool_name),
                    [] => "background tools to finish".to_owned(),
                    calls => format!("{} background tools to finish", calls.len()),
                };
                self.waiting = Some(WaitingStatus {
                    reason,
                    deadline_ms,
                });
                self.refresh_wait(now_ms)
            }
            AgentStatusEvent::Waiting {
                reason,
                deadline_ms,
            } => {
                self.active_tools.clear();
                self.waiting = Some(WaitingStatus {
                    reason,
                    deadline_ms,
                });
                self.refresh_wait(now_ms)
            }
            AgentStatusEvent::Failed { reason } => {
                self.reset();
                format!("Failed: {reason}")
            }
        }
    }

    fn refresh_wait(&self, now_ms: i64) -> String {
        let wait = self.waiting.as_ref().expect("wait refresh without wait");
        let remaining_ms = wait.deadline_ms.saturating_sub(now_ms).max(0);
        let remaining_seconds = remaining_ms.saturating_add(999) / 1_000;
        format!(
            "Waiting: {} · {:02}:{:02}",
            wait.reason,
            remaining_seconds / 60,
            remaining_seconds % 60
        )
    }

    fn current_tool_status(&self) -> String {
        self.active_tools
            .last()
            .map(|call| tool_status(&call.tool_name))
            .unwrap_or("Working...")
            .to_owned()
    }

    fn reset(&mut self) {
        self.active_tools.clear();
        self.waiting = None;
    }
}

fn tool_status(tool_name: &str) -> &'static str {
    match normalize_tool_name(tool_name).as_str() {
        "read" | "list" | "ls" | "glob" | "grep" => "Reading files...",
        "stat" => "Checking files...",
        "write" | "edit" | "applypatch" | "copy" | "delete" => "Updating files...",
        "exec" => "Running in environment...",
        "bash" | "execcommand" => "Running in workspace...",
        "python" => "Running Python...",
        "webbrowse" => "Browsing the web...",
        "searchquery" | "imagequery" => "Searching the web...",
        "readcontexthandoff" | "readsessionhistory" => "Reading session history...",
        "contexthandoff" => "Summarizing context...",
        "waitfor" => "Preparing to wait...",
        "end" => "Finishing...",
        _ => "Working...",
    }
}

fn normalize_tool_name(tool_name: &str) -> String {
    tool_name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    id: String,
    event: String,
    data: String,
}

struct SseFrame {
    id: Option<String>,
    event: String,
    data: String,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let Ok(line) = std::str::from_utf8(&line) else {
                self.event.clear();
                self.data.clear();
                continue;
            };
            if line.is_empty() {
                if !self.event.is_empty() || !self.data.is_empty() {
                    frames.push(SseFrame {
                        id: (!self.id.is_empty()).then(|| std::mem::take(&mut self.id)),
                        event: if self.event.is_empty() {
                            "message".to_owned()
                        } else {
                            std::mem::take(&mut self.event)
                        },
                        data: std::mem::take(&mut self.data),
                    });
                }
            } else if let Some(event) = line.strip_prefix("event:") {
                self.event = event.trim().to_owned();
            } else if let Some(id) = line.strip_prefix("id:") {
                self.id = id.trim().to_owned();
            } else if let Some(data) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(data.trim());
            }
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_uses_reason_and_refreshes_remaining_time() {
        let mut projection = ProjectionState::default();
        assert_eq!(
            projection.apply(
                AgentStatusEvent::Waiting {
                    reason: "the build".to_owned(),
                    deadline_ms: 65_000,
                },
                0,
            ),
            "Waiting: the build · 01:05"
        );
        assert_eq!(projection.refresh_wait(5_000), "Waiting: the build · 01:00");
    }

    #[test]
    fn explicit_wait_remains_projected_after_the_tool_batch_finishes() {
        let mut projection = ProjectionState::default();
        projection.apply(
            AgentStatusEvent::Waiting {
                reason: "the build".to_owned(),
                deadline_ms: 65_000,
            },
            0,
        );
        if let Some(event) = AgentEvent::AutoWaitEnded.status_event() {
            projection.apply(event, 0);
        }
        assert_eq!(projection.refresh_wait(5_000), "Waiting: the build · 01:00");
    }

    #[test]
    fn automatic_tool_wait_uses_the_same_countdown_projection() {
        let mut projection = ProjectionState::default();
        assert_eq!(
            projection.apply(
                AgentStatusEvent::ToolsWaiting {
                    calls: vec![AgentToolCall {
                        tool_call_id: "bash-1".to_owned(),
                        tool_name: "bash".to_owned(),
                    }],
                    deadline_ms: 60_000,
                },
                0,
            ),
            "Waiting: bash to finish · 01:00"
        );
        assert_eq!(
            projection.refresh_wait(5_000),
            "Waiting: bash to finish · 00:55"
        );
    }

    #[test]
    fn failure_is_visible_and_only_real_clear_events_remove_it() {
        let mut projection = ProjectionState::default();
        assert_eq!(
            projection.apply(
                AgentStatusEvent::Failed {
                    reason: "provider rejected the request".to_owned(),
                },
                0,
            ),
            "Failed: provider rejected the request"
        );
        assert_eq!(projection.apply(AgentStatusEvent::Interrupted, 0), "");
    }

    #[test]
    fn tool_results_reveal_the_next_active_tool_then_thinking() {
        let mut projection = ProjectionState::default();
        assert_eq!(
            projection.apply(
                AgentStatusEvent::ToolsStarted {
                    calls: vec![
                        AgentToolCall {
                            tool_call_id: "read-1".to_owned(),
                            tool_name: "read".to_owned(),
                        },
                        AgentToolCall {
                            tool_call_id: "bash-1".to_owned(),
                            tool_name: "bash".to_owned(),
                        },
                    ],
                },
                0,
            ),
            "Running in workspace..."
        );
        assert_eq!(
            projection.apply(
                AgentStatusEvent::ToolFinished {
                    tool_call_id: "bash-1".to_owned(),
                },
                0,
            ),
            "Reading files..."
        );
        assert_eq!(
            projection.apply(
                AgentStatusEvent::ToolFinished {
                    tool_call_id: "read-1".to_owned(),
                },
                0,
            ),
            "Thinking..."
        );
    }

    #[test]
    fn decoder_handles_split_utf8_status_frames() {
        let payload = "event: status\ndata: {\"state\":\"failed\",\"reason\":\"失败\"}\n\n";
        let bytes = payload.as_bytes();
        let split = payload.find('失').unwrap() + 1;
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(&bytes[..split]).is_empty());
        let frames = decoder.push(&bytes[split..]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "status");
        assert_eq!(
            serde_json::from_str::<AgentStatusEvent>(&frames[0].data).unwrap(),
            AgentStatusEvent::Failed {
                reason: "失败".to_owned()
            }
        );
    }
}

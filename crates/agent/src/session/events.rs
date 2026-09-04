//! Durable session facts and their in-memory schema migration.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue, Value};

use super::tools::{ToolChange, ToolIntroduction, ToolKnowledge, ToolVersion};
use super::wire::{ProviderContext, ProviderToolCall};

pub use super::wire::SessionSelection as Selection;

pub const EVENT_SCHEMA_VERSION: u32 = 1;

pub const END_TOOL_NAME: &str = "end";
pub const WAIT_TOOL_NAME: &str = "wait";
pub const HANDOFF_TOOL_NAME: &str = "handoff";
pub const TOOL_CANCEL_NAME: &str = "tool.cancel";
pub const TOOL_HELP_NAME: &str = "tool.help";
pub const HISTORY_LIST_NAME: &str = "history.list";
pub const FILE_READ_NAME: &str = "file.read";
pub const FILE_WRITE_NAME: &str = "file.write";
pub const FILE_EDIT_NAME: &str = "file.edit";
pub const SHELL_RUN_NAME: &str = "shell.run";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    #[default]
    Conversation,
    Compaction,
    Handoff,
}

impl Purpose {
    pub fn is_context(self) -> bool {
        self != Self::Conversation
    }
}

impl From<zork_config::ContextStrategy> for Purpose {
    fn from(strategy: zork_config::ContextStrategy) -> Self {
        match strategy {
            zork_config::ContextStrategy::Compaction => Self::Compaction,
            zork_config::ContextStrategy::Handoff => Self::Handoff,
        }
    }
}

pub(super) fn handoff_purpose() -> Purpose {
    Purpose::Handoff
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Finished,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepInterruptionReason {
    Recovery,
    #[serde(rename = "handoff_timeout")]
    LegacyHandoffTimeout,
    TurnCancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_text_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Input {
    pub input_id: String,
    pub content: String,
    pub received_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutstandingItem {
    pub kind: String,
    pub id: String,
    pub summary: String,
}

/// Durable zork-agent logical invocation. Rejected provider calls still have
/// an invocation so projection can return a direct error for the original ID.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolInvocation {
    pub invocation_id: String,
    pub provider_call_id: String,
    pub turn_id: String,
    pub started_at_ms: i64,
    pub tool: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_version: Option<ToolVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolResultData {
    pub invocation_id: String,
    pub tool: String,
    pub outcome: ToolOutcome,
    pub data: Value,
    pub result_schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<ToolKnowledge>,
    pub finished_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDeliveryMode {
    Direct,
    Notification,
}

/// A tool message frozen into one provider request by `StepStarted`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDelivery {
    Result {
        invocation: ToolInvocation,
        result: Box<ToolResultData>,
        mode: ToolDeliveryMode,
    },
    Pending {
        invocation: ToolInvocation,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoWaitEndReason {
    BatchCompleted,
    TimedOut,
    NewInput,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeadlineKind {
    AutoWait { step_id: String },
    Wait { invocation_id: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderErrorRecord {
    pub stage: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_input: Option<Box<super::wire::ProviderInputDiagnostics>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub message: String,
}

impl ProviderErrorRecord {
    /// A completed generation that could not form a document, not a transport,
    /// authentication or rate-limit failure. Those keep their own retry budget.
    pub fn is_invalid_document(&self) -> bool {
        matches!(
            self.provider_code.as_deref(),
            Some("max_output_tokens" | "content_filter")
        ) || matches!(
            self.stage.as_str(),
            "provider.outcome.tool_arguments_json" | "provider.outcome.tool_arguments_shape"
        )
    }

    pub fn is_context_overflow(&self) -> bool {
        provider_error_is_context_overflow(
            self.status_code,
            self.provider_code.as_deref(),
            &self.message,
        )
    }
}

pub fn provider_error_is_context_overflow(
    status_code: Option<u16>,
    provider_code: Option<&str>,
    message: &str,
) -> bool {
    const CODES: [&str; 6] = [
        "context_length_exceeded",
        "prompt_too_long",
        "input_too_long",
        "too_many_tokens",
        "context_window_exceeded",
        "maximum_context_length",
    ];
    if let Some(code) = provider_code {
        let code = code.to_ascii_lowercase();
        if CODES.iter().any(|candidate| code.contains(candidate)) {
            return true;
        }
    }
    if status_code == Some(400) {
        let message = message.to_ascii_lowercase();
        return message.contains("context length")
            || message.contains("context window")
            || message.contains("maximum context")
            || message.contains("prompt is too long")
            || message.contains("input is too long")
            || message.contains("too many tokens");
    }
    false
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeFailure {
    pub failure_id: String,
    pub stage: String,
    pub message: String,
}

impl RuntimeFailure {
    pub fn fingerprint(&self) -> String {
        if self.failure_id == "panic" {
            format!("panic:{}:{}", self.stage, self.message)
        } else {
            format!("{}:{}", self.failure_id, self.stage)
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEvent {
    SessionCreated {
        session_id: String,
        created_at_ms: i64,
        selection: Selection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt: Option<String>,
        workspace: String,
        tools: Vec<ToolIntroduction>,
    },
    InputAppended {
        input: Input,
    },
    SelectionChanged {
        selection: Selection,
    },
    ContextConfigured {
        config: zork_config::ContextConfig,
    },
    TurnStarted {
        turn_id: String,
        started_at_ms: i64,
    },
    TurnCancelRequested {
        turn_id: String,
        requested_at_ms: i64,
    },
    TurnFinished {
        turn_id: String,
        outcome: TurnOutcome,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        outstanding: Vec<OutstandingItem>,
        finished_at_ms: i64,
    },
    /// The exact append-only context additions are durable before the provider
    /// request starts. Replaying this event never needs to inspect live tools.
    StepStarted {
        step_id: String,
        turn_id: String,
        purpose: Purpose,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        consumed_inputs: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        deliveries: Vec<ToolDelivery>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_changes: Vec<ToolChange>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notices: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        outstanding: Vec<OutstandingItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_output_tokens: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_budget: Option<u64>,
        started_at_ms: i64,
    },
    StepCompleted {
        step_id: String,
        #[serde(default)]
        purpose: Purpose,
        assistant_text: String,
        provider_calls: Vec<ProviderToolCall>,
        invocations: Vec<ToolInvocation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_wait_deadline_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_context: Option<ProviderContext>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_input: Option<Box<super::wire::ProviderInputDiagnostics>>,
        completed_at_ms: i64,
    },
    StepFailed {
        step_id: String,
        error: ProviderErrorRecord,
        failed_at_ms: i64,
    },
    StepInterrupted {
        step_id: String,
        reason: StepInterruptionReason,
        interrupted_at_ms: i64,
    },
    AutoWaitEnded {
        step_id: String,
        reason: AutoWaitEndReason,
        ended_at_ms: i64,
    },
    ToolCancelRequested {
        invocation_id: String,
        requested_at_ms: i64,
    },
    ToolResult {
        result: ToolResultData,
    },
    #[serde(alias = "handoff_failed")]
    ContextFailed {
        generation: u64,
        #[serde(default = "handoff_purpose")]
        purpose: Purpose,
        message: String,
        failed_at_ms: i64,
    },
    #[serde(alias = "handoff_applied")]
    ContextApplied {
        generation: u64,
        #[serde(default = "handoff_purpose")]
        purpose: Purpose,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        document: Option<String>,
        /// Range in the old generation. Moving entries does not replay tool effects.
        #[serde(default)]
        retained: std::ops::Range<usize>,
        tools: Vec<ToolIntroduction>,
        carried_tools: Vec<ToolInvocation>,
        applied_at_ms: i64,
    },
    DeadlineReached {
        deadline: DeadlineKind,
        reached_at_ms: i64,
    },
    RuntimeFault {
        failure: RuntimeFailure,
        consecutive_count: u32,
        occurred_at_ms: i64,
    },
    /// Snapshot state remains raw JSON until its own schema version has been
    /// selected and migrated.
    Snapshot {
        state_schema_version: u32,
        state: Value,
    },
}

impl SessionEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "session_created",
            Self::InputAppended { .. } => "input_appended",
            Self::SelectionChanged { .. } => "selection_changed",
            Self::ContextConfigured { .. } => "context_configured",
            Self::TurnStarted { .. } => "turn_started",
            Self::TurnCancelRequested { .. } => "turn_cancel_requested",
            Self::TurnFinished { .. } => "turn_finished",
            Self::StepStarted { .. } => "step_started",
            Self::StepCompleted { .. } => "step_completed",
            Self::StepFailed { .. } => "step_failed",
            Self::StepInterrupted { .. } => "step_interrupted",
            Self::AutoWaitEnded { .. } => "auto_wait_ended",
            Self::ToolCancelRequested { .. } => "tool_cancel_requested",
            Self::ToolResult { .. } => "tool_result",
            Self::ContextFailed { .. } => "context_failed",
            Self::ContextApplied { .. } => "context_applied",
            Self::DeadlineReached { .. } => "deadline_reached",
            Self::RuntimeFault { .. } => "runtime_fault",
            Self::Snapshot { .. } => "snapshot",
        }
    }

    pub fn is_history_visible(&self) -> bool {
        !matches!(self, Self::Snapshot { .. })
    }
}

pub fn migrate_event(
    schema_version: u32,
    event: &RawValue,
) -> Result<SessionEvent, EventMigrationError> {
    match schema_version {
        EVENT_SCHEMA_VERSION => serde_json::from_str(event.get())
            .map_err(|error| EventMigrationError::InvalidCurrent(error.to_string())),
        other => Err(EventMigrationError::Unsupported(other)),
    }
}

/// Validates one durable event without materializing large history payloads
/// that the caller may not return. Less common variants still use the
/// canonical migration path so this stays aligned with `SessionEvent`.
pub(crate) fn validate_history_event(
    schema_version: u32,
    event: &RawValue,
) -> Result<bool, EventMigrationError> {
    if schema_version != EVENT_SCHEMA_VERSION {
        return migrate_event(schema_version, event).map(|event| event.is_history_visible());
    }

    // Serde writes the internal tag first. The prefix is only a fast-path
    // discriminator; any other valid JSON representation still goes through
    // the canonical migration below.
    let bytes = event.get().as_bytes();
    if bytes.starts_with(br#"{"kind":"input_appended","#) {
        let event = parse_history_event::<BorrowedInputAppended<'_>>(event)?;
        let _ = (
            event.kind,
            event.input.input_id,
            event.input.content,
            event.input.received_at_ms,
        );
        Ok(true)
    } else if bytes.starts_with(br#"{"kind":"selection_changed","#) {
        let event = parse_history_event::<BorrowedSelectionChanged<'_>>(event)?;
        let _ = (
            event.kind,
            event.selection.profile_id,
            event.selection.model,
            event.selection.thinking,
        );
        Ok(true)
    } else if bytes.starts_with(br#"{"kind":"snapshot","#) {
        let event = parse_history_event::<BorrowedSnapshot<'_>>(event)?;
        let _ = (event.kind, event.state_schema_version, event.state);
        Ok(false)
    } else {
        migrate_event(schema_version, event).map(|event| event.is_history_visible())
    }
}

fn parse_history_event<'a, T>(event: &'a RawValue) -> Result<T, EventMigrationError>
where
    T: Deserialize<'a>,
{
    serde_json::from_str(event.get())
        .map_err(|error| EventMigrationError::InvalidCurrent(error.to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedInputAppended<'a> {
    #[serde(borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    input: BorrowedInput<'a>,
}

#[derive(Deserialize)]
struct BorrowedInput<'a> {
    #[serde(borrow)]
    input_id: Cow<'a, str>,
    #[serde(borrow)]
    content: Cow<'a, str>,
    received_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedSelectionChanged<'a> {
    #[serde(borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    selection: BorrowedSelection<'a>,
}

#[derive(Deserialize)]
struct BorrowedSelection<'a> {
    #[serde(borrow)]
    profile_id: Cow<'a, str>,
    #[serde(borrow)]
    model: Cow<'a, str>,
    #[serde(borrow)]
    thinking: Cow<'a, str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedSnapshot<'a> {
    #[serde(borrow)]
    kind: Cow<'a, str>,
    state_schema_version: u32,
    #[serde(borrow)]
    state: &'a RawValue,
}

#[derive(Debug, thiserror::Error)]
pub enum EventMigrationError {
    #[error("unsupported event schema version {0}")]
    Unsupported(u32),
    #[error("invalid current event: {0}")]
    InvalidCurrent(String),
}

#[cfg(test)]
mod tests {
    use serde_json::value::RawValue;

    use super::{migrate_event, validate_history_event, EVENT_SCHEMA_VERSION};

    #[test]
    // Contract: docs/zork-agent-architecture.md [EVENT-04, QUERY-01]
    fn borrowed_history_validation_matches_the_current_event_schema() {
        for json in [
            r#"{"kind":"input_\u0061ppended","input":{"input_id":"input-\u0031","content":"line\nquoted \"value\"","received_at_ms":1,"future_nested":true}}"#,
            r#"{"kind":"selection_changed","selection":{"profile_id":"def\u0061ult","model":"model-a","thinking":"medium","future_nested":true}}"#,
            r#"{"kind":"snapshot","state_schema_version":1,"state":{"nested":["value"]}}"#,
        ] {
            let raw = RawValue::from_string(json.to_owned()).unwrap();
            let migrated = migrate_event(EVENT_SCHEMA_VERSION, &raw).unwrap();
            assert_eq!(
                validate_history_event(EVENT_SCHEMA_VERSION, &raw).unwrap(),
                migrated.is_history_visible()
            );
        }

        for json in [
            r#"{"kind":"input_appended","input":{"input_id":"broken","received_at_ms":1}}"#,
            r#"{"kind":"selection_changed","selection":{"profile_id":"default","model":"model-a","thinking":"medium"},"future_top_level":true}"#,
            r#"{"kind":"snapshot","state_schema_version":1}"#,
        ] {
            let raw = RawValue::from_string(json.to_owned()).unwrap();
            assert!(migrate_event(EVENT_SCHEMA_VERSION, &raw).is_err());
            assert!(validate_history_event(EVENT_SCHEMA_VERSION, &raw).is_err());
        }
    }
}

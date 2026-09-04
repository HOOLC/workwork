//! Shared wire types for the unversioned zork-agent HTTP API.

use serde::{Deserialize, Serialize};

pub use zork_config::{ContextConfig, ContextStrategy};
pub use zork_profile::{ProfileDocument, ProfileModel as AgentModel, ProfileView as AgentProfile};

pub const DURABLE_EVENT_NAME: &str = "event";
pub const TEXT_DELTA_EVENT_NAME: &str = "text_delta";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiErrorBody {
    pub error: ApiErrorDetail,
}

impl ApiErrorBody {
    pub fn new(code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ApiErrorDetail {
                code,
                message: message.into(),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiErrorDetail {
    pub code: ApiErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    InvalidRequest,
    InternalError,
    SessionNotFound,
    SessionOverloaded,
    GlobalOverloaded,
    RunnerCircuitOpen,
    SessionDeleting,
    SessionUnavailable,
    SelectionUnavailable,
    InvalidCursor,
    ProfileNotFound,
    Unauthorized,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ItemList<T> {
    pub items: Vec<T>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSelection {
    pub profile_id: String,
    pub model: String,
    pub thinking: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSessionRequest {
    pub profile_id: String,
    pub model: String,
    pub thinking: String,
    pub system_prompt: Option<String>,
    pub workspace: Option<String>,
    pub context: Option<ContextConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MailboxRequest {
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionView {
    pub session_id: String,
    pub workspace: String,
    pub profile_id: String,
    pub model: String,
    pub thinking: String,
    pub generation: u64,
    pub context: ContextConfig,
    pub status: SessionStatus,
}

impl SessionView {
    pub fn selection(&self) -> SessionSelection {
        SessionSelection {
            profile_id: self.profile_id.clone(),
            model: self.model.clone(),
            thinking: self.thinking.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub session_id: String,
    pub status: SessionStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Wait,
    Thinking,
    Waiting,
    Working,
    Finished,
    Failed,
    Cancelled,
    Unavailable,
    Deleting,
    Recovering,
}

impl SessionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Thinking => "thinking",
            Self::Waiting => "waiting",
            Self::Working => "working",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
            Self::Deleting => "deleting",
            Self::Recovering => "recovering",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessageQuery {
    pub limit: Option<usize>,
    pub before: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventQuery {
    #[serde(default)]
    pub transient: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessagePage {
    pub items: Vec<PublicMessage>,
    pub older_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublicMessage {
    #[serde(rename = "type")]
    pub kind: MessageKind,
    pub role: PublicRole,
    pub content: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Message,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicRole {
    Mailbox,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DurableEvent<T> {
    pub event_id: String,
    pub schema_version: u32,
    pub batch_index: u32,
    pub batch_count: u32,
    pub event: T,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextDeltaEvent {
    pub session_id: String,
    pub generation: u64,
    pub step_id: String,
    pub text: String,
}

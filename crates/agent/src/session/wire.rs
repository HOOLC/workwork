//! 共享词汇类型（v2 删除 v1 时从旧 session::state 晋升）：provider wire
//! 契约与选择身份。投影、适配器、模型端口共同消费；无运行时语义。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionSelection {
    pub profile_id: String,
    pub model: String,
    pub thinking: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRole {
    System,
    User,
    Assistant,
    Tool,
}

/// One call emitted by the provider. Its ID exists only to satisfy the
/// provider protocol; zork-agent assigns a separate ToolInvocation ULID.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProviderToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

/// provider 返回的加密 reasoning / 续链数据——agent 原样存档、原样回放
/// （opaque，不解释）；投影层直传给下一轮请求。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProviderContext {
    pub profile_id: String,
    pub provider: String,
    pub model: String,
    pub api: String,
    pub output_items: Arc<Vec<Value>>,
}

/// provider 可见对话消息（wire 契约；不进事件流）。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProviderMessage {
    pub role: TranscriptRole,
    pub content: Arc<str>,
    pub is_error: bool,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ProviderToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_context: Option<ProviderContext>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderInputMode {
    Full,
    Delta,
}

/// Non-sensitive facts about the input actually placed on the provider wire.
/// It deliberately contains neither the input items nor their serialized
/// bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderInputDiagnostics {
    pub mode: ProviderInputMode,
    pub logical_input_items: u64,
    pub sent_input_items: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

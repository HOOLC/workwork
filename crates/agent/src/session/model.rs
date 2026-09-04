//! 模型端口词汇（v2 删除 v1 时从旧 session::runtime 晋升）：请求/结果/
//! 错误、工具定义与流观察者。provider 适配器实现端口，v2 runner 消费。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::wire::{
    ProviderContext, ProviderInputDiagnostics, ProviderMessage, ProviderToolCall, SessionSelection,
};

/// Receives provider text deltas for transient observation only. The observer
/// is never consulted by the durable fold and its output is not persisted,
/// replayed, or included in session events.
pub trait ModelStreamObserver: Send + Sync + std::fmt::Debug {
    fn text_delta(&self, session_id: &str, generation: u64, step_id: &str, text: &str);
}

/// 无流观察者（静默；适配器默认值）。
#[derive(Clone, Debug)]
pub struct SilentStreamObserver;

impl ModelStreamObserver for SilentStreamObserver {
    fn text_delta(&self, _: &str, _: u64, _: &str, _: &str) {}
}

#[derive(Debug)]
pub struct ModelRequest {
    pub session_id: String,
    pub generation: u64,
    pub step_id: String,
    pub selection: SessionSelection,
    pub transcript: Arc<Vec<ProviderMessage>>,
    pub tools: Arc<Vec<ToolDefinition>>,
    pub max_output_tokens: Option<u32>,
    /// One-off context preparation must not reuse or alter conversation continuation.
    pub independent: bool,
    pub stream_observer: Arc<dyn ModelStreamObserver>,
}

/// The session runtime's only provider boundary. Implementations own profile
/// resolution, protocol retries, continuations, connections, and provider
/// resource management.
pub trait ModelGateway: Send + Sync {
    fn complete<'a>(
        &'a self,
        request: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelOutcome, ModelError>> + Send + 'a>>;

    /// A best-effort resource-management hint. Correctness must never depend on
    /// whether or when a provider acts on it.
    fn release(&self, _suggestion: ModelReleaseSuggestion<'_>) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelReleaseSuggestion<'a> {
    Session(&'a str),
    Generation {
        session_id: &'a str,
        generation: u64,
    },
}

#[derive(Clone, Debug)]
pub struct ModelOutcome {
    pub text: String,
    pub tool_calls: Vec<ProviderToolCall>,
    pub provider_context: Option<ProviderContext>,
    pub usage: Option<ModelTokenUsage>,
    pub provider_input: Option<Box<ProviderInputDiagnostics>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelTokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: u64,
    pub output_reasoning_tokens: Option<u64>,
    pub output_text_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderFailure {
    pub stage: &'static str,
    pub retryable: bool,
    pub status_code: Option<u16>,
    pub provider_code: Option<String>,
    pub request_id: Option<String>,
    pub message: String,
    pub provider_input: Option<Box<ProviderInputDiagnostics>>,
    pub usage: Option<Box<ModelTokenUsage>>,
}

impl ProviderFailure {
    pub fn new(stage: &'static str, retryable: bool, message: impl Into<String>) -> Self {
        Self {
            stage,
            retryable,
            status_code: None,
            provider_code: None,
            request_id: None,
            message: message.into(),
            provider_input: None,
            usage: None,
        }
    }

    /// "输入超出上下文窗口"类拒收（§2.6 规则 4'：API 是窗口的唯一裁判）。
    /// 按错误码精确匹配 + 400 状态下的消息措辞兜底——溢出错误是恢复动作
    ///（handoff）的触发器，宁缺毋滥：认错的错误只会像今天一样失败回合，
    /// 可观察、可补词表；把无关 400 认成溢出才会静默丢上下文。
    pub fn is_context_overflow(&self) -> bool {
        super::events::provider_error_is_context_overflow(
            self.status_code,
            self.provider_code.as_deref(),
            &self.message,
        )
    }
}

#[derive(Debug)]
pub enum ModelError {
    Unavailable,
    InvalidSelection,
    ProfileUnavailable,
    ProviderFailed(ProviderFailure),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub const TOOL_INTERRUPTED_MESSAGE: &str = "Agent runtime was interrupted before a durable result was recorded. This tool call may have completed, partially completed, or not started. Inspect the current state before deciding whether or how to recover.";

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(code: Option<&str>, status: Option<u16>, message: &str) -> ProviderFailure {
        ProviderFailure {
            stage: "test",
            retryable: false,
            status_code: status,
            provider_code: code.map(str::to_owned),
            request_id: None,
            message: message.to_owned(),
            provider_input: None,
            usage: None,
        }
    }

    /// §2.6 规则 4'：只有上下文超限类拒收才允许触发硬 reset——认错方向的
    /// 错误代价不对称（漏认 = 像今天一样失败回合，可观察；错认 = 静默丢
    /// 上下文）。
    #[test]
    // Contract: docs/zork-agent-architecture.md [RETRY-02, HANDOFF-01]
    fn context_overflow_classification() {
        assert!(failure(Some("context_length_exceeded"), None, "").is_context_overflow());
        assert!(failure(Some("Prompt_Too_Long"), None, "").is_context_overflow());
        assert!(failure(
            None,
            Some(400),
            "This model's maximum context length is 256000 tokens"
        )
        .is_context_overflow());
        assert!(
            failure(None, Some(400), "the input is too long for this model").is_context_overflow()
        );
        // 无关错误不得误判。
        assert!(
            !failure(Some("rate_limit_exceeded"), Some(429), "slow down").is_context_overflow()
        );
        assert!(!failure(
            Some("websocket_connection_limit_reached"),
            None,
            "60 minutes"
        )
        .is_context_overflow());
        assert!(!failure(None, Some(400), "invalid request body").is_context_overflow());
        assert!(!failure(None, Some(401), "bad token").is_context_overflow());
        // 状态码缺失、消息不含措辞 → 不判（宁缺毋滥）。
        assert!(
            !failure(Some("unknown_code"), None, "maximum context length").is_context_overflow()
        );
    }
}

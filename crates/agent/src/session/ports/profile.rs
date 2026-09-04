use std::collections::HashMap;

use crate::session::wire::SessionSelection;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelLimits {
    pub context_window_tokens: u64,
    pub max_output_tokens: u32,
    /// 输入预算预留百分比（profile 逐模型可配，缺省 10）。
    pub reserve_percent: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileResolveError {
    InvalidSelection,
    NotFound,
    AuthUnavailable,
    Backend,
}

/// Connection material for one provider invocation. It is neither cloned nor
/// serialized, so profile secrets cannot enter session state or events.
pub struct ProfileExecution {
    profile_id: String,
    provider: String,
    model: String,
    api: String,
    streaming: bool,
    parallel_tool_calls: bool,
    service_tier: Option<String>,
    base_url: String,
    headers: HashMap<String, String>,
    thinking: String,
    limits: ModelLimits,
    secret: String,
}

impl ProfileExecution {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        profile_id: String,
        provider: String,
        model: String,
        api: String,
        streaming: bool,
        parallel_tool_calls: bool,
        service_tier: Option<String>,
        base_url: String,
        headers: HashMap<String, String>,
        thinking: String,
        limits: ModelLimits,
        secret: String,
    ) -> Self {
        Self {
            profile_id,
            provider,
            model,
            api,
            streaming,
            parallel_tool_calls,
            service_tier,
            base_url,
            headers,
            thinking,
            limits,
            secret,
        }
    }

    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn api(&self) -> &str {
        &self.api
    }

    pub fn streaming(&self) -> bool {
        self.streaming
    }

    pub fn parallel_tool_calls(&self) -> bool {
        self.parallel_tool_calls
    }

    pub fn service_tier(&self) -> Option<&str> {
        self.service_tier.as_deref()
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn headers(&self) -> &HashMap<String, String> {
        &self.headers
    }

    pub fn thinking(&self) -> &str {
        &self.thinking
    }

    pub fn limits(&self) -> &ModelLimits {
        &self.limits
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl std::fmt::Debug for ProfileExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfileExecution")
            .field("profile_id", &self.profile_id)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api", &self.api)
            .field("streaming", &self.streaming)
            .field("parallel_tool_calls", &self.parallel_tool_calls)
            .field("service_tier", &self.service_tier)
            .field("base_url", &self.base_url)
            .field(
                "headers",
                &format_args!("<{} redacted headers>", self.headers.len()),
            )
            .field("thinking", &self.thinking)
            .field("limits", &self.limits)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[async_trait::async_trait]
pub trait ProfileResolver: Send + Sync {
    fn model_limits(
        &self,
        selection: &SessionSelection,
    ) -> Result<ModelLimits, ProfileResolveError>;

    async fn resolve(
        &self,
        selection: &SessionSelection,
    ) -> Result<ProfileExecution, ProfileResolveError>;
}

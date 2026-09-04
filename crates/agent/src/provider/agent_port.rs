//! ModelGateway composition: profile resolution plus the selected provider.

use std::sync::Arc;

use zork_agent::session::model::{
    ModelError, ModelGateway, ModelOutcome, ModelReleaseSuggestion, ModelRequest,
};
use zork_agent::session::ports::{ModelExecutor, ProfileResolveError, ProfileResolver};
use zork_agent::ProfileStore;

pub struct AgentModelPort {
    router: Arc<dyn ModelExecutor>,
    profiles: Arc<ProfileStore>,
}

impl AgentModelPort {
    pub fn new(router: Arc<dyn ModelExecutor>, profiles: Arc<ProfileStore>) -> Self {
        Self { router, profiles }
    }

    async fn resolve(
        &self,
        request: &ModelRequest,
    ) -> Result<zork_agent::session::ports::ProfileExecution, ModelError> {
        self.profiles
            .resolve(&request.selection)
            .await
            .map_err(|error| match error {
                ProfileResolveError::InvalidSelection => ModelError::InvalidSelection,
                ProfileResolveError::NotFound | ProfileResolveError::AuthUnavailable => {
                    ModelError::ProfileUnavailable
                }
                ProfileResolveError::Backend => ModelError::Unavailable,
            })
    }
}

impl ModelGateway for AgentModelPort {
    fn complete<'a>(
        &'a self,
        request: &'a ModelRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ModelOutcome, ModelError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let execution = self.resolve(request).await?;
            if request.max_output_tokens != Some(execution.limits().max_output_tokens) {
                return Err(ModelError::InvalidSelection);
            }
            self.router.complete(request, execution).await
        })
    }

    fn release(&self, suggestion: ModelReleaseSuggestion<'_>) {
        let session_id = match suggestion {
            ModelReleaseSuggestion::Session(session_id)
            | ModelReleaseSuggestion::Generation { session_id, .. } => session_id,
        };
        self.router.release_session(session_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use zork_agent::session::model::{SilentStreamObserver, ToolDefinition};
    use zork_agent::session::ports::{ModelExecutor, ProfileExecution};
    use zork_agent::session::wire::SessionSelection;

    use super::*;

    struct CountingExecutor(AtomicUsize);

    impl ModelExecutor for CountingExecutor {
        fn complete<'a>(
            &'a self,
            _: &'a ModelRequest,
            _: ProfileExecution,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<ModelOutcome, ModelError>> + Send + 'a>,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(ModelError::Unavailable) })
        }
    }

    #[tokio::test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-03]
    async fn changed_limits_reject_the_request_before_the_provider_adapter() {
        let root = tempfile::tempdir().unwrap();
        let profiles = Arc::new(ProfileStore::open(root.path().to_owned(), true, false));
        profiles
            .put(
                "fixture",
                json!({
                    "provider": "openai-compatible",
                    "billing": "usage",
                    "base_url": "https://example.invalid/v1",
                    "models": [{
                        "id": "model",
                        "api": "openai-completions",
                        "thinking": ["high"],
                        "default_thinking": "high",
                        "capabilities": {"input": ["text"]},
                        "limits": {"context_window_tokens": 100_000, "max_output_tokens": 10_000},
                        "default": true
                    }]
                }),
            )
            .unwrap();
        let executor = Arc::new(CountingExecutor(AtomicUsize::new(0)));
        let port = AgentModelPort::new(executor.clone(), profiles);
        let request = ModelRequest {
            session_id: "session".into(),
            generation: 1,
            step_id: "step".into(),
            selection: SessionSelection {
                profile_id: "fixture".into(),
                model: "model".into(),
                thinking: "high".into(),
            },
            transcript: Arc::new(Vec::new()),
            tools: Arc::new(Vec::<ToolDefinition>::new()),
            max_output_tokens: Some(9_999),
            independent: false,
            stream_observer: Arc::new(SilentStreamObserver),
        };

        assert!(matches!(
            port.complete(&request).await,
            Err(ModelError::InvalidSelection)
        ));
        assert_eq!(executor.0.load(Ordering::SeqCst), 0);
    }
}

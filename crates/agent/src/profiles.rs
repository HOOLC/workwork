use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::session::ports::{ModelLimits, ProfileExecution, ProfileResolveError, ProfileResolver};
use crate::session::runner::RunnerOptions;
use crate::session::state::SessionState;
use crate::session::wire::SessionSelection;

#[derive(Clone)]
pub struct ProfileStore {
    data_root: PathBuf,
    fake: bool,
    no_streaming: bool,
    http: reqwest::Client,
    statuses: zork_profile::MemoryStore,
}

impl ProfileStore {
    pub fn open(data_root: PathBuf, fake: bool, no_streaming: bool) -> Self {
        Self {
            data_root,
            fake,
            no_streaming,
            http: reqwest::Client::new(),
            statuses: zork_profile::MemoryStore::default(),
        }
    }

    fn paths(&self) -> zork_profile::DataRootPaths {
        zork_profile::DataRootPaths {
            data_root: self.data_root.clone(),
        }
    }

    pub fn list(&self) -> anyhow::Result<Vec<zork_profile::ProfileView>> {
        zork_profile::list_profiles_with_status(&self.paths(), &self.statuses)
    }

    pub fn get(&self, profile_id: &str) -> anyhow::Result<Option<zork_profile::ProfileView>> {
        zork_profile::get_profile_with_status(&self.paths(), &self.statuses, profile_id)
    }

    pub fn put(&self, profile_id: &str, body: Value) -> anyhow::Result<zork_profile::ProfileView> {
        let profile = zork_profile::put_profile(&self.paths(), profile_id, body)?;
        zork_profile::ProfileStore::remove_probe(&self.statuses, profile_id)?;
        Ok(profile)
    }

    pub fn delete(&self, profile_id: &str) -> anyhow::Result<()> {
        zork_profile::delete_profile(&self.paths(), profile_id)?;
        zork_profile::ProfileStore::remove_probe(&self.statuses, profile_id)
    }

    pub async fn refresh_all_statuses(&self) -> anyhow::Result<()> {
        if !self.fake {
            zork_profile::refresh_all(&self.paths(), &self.statuses, &self.http).await?;
        }
        Ok(())
    }

    pub async fn refresh_status(&self, profile_id: &str) -> anyhow::Result<()> {
        if !self.fake {
            zork_profile::refresh_profile(&self.paths(), &self.statuses, &self.http, profile_id)
                .await?;
        }
        Ok(())
    }

    pub fn runner_options(profiles: &Arc<Self>) -> RunnerOptions {
        let budget_profiles = profiles.clone();
        let input_budget = Arc::new(move |state: &SessionState| {
            let selection = state.selection.as_ref()?;
            let limits = budget_profiles.model_limits(selection).ok()?;
            Some(handoff_input_trigger(
                limits.context_window_tokens,
                u64::from(limits.max_output_tokens),
                limits.reserve_percent,
            ))
        });
        let output_profiles = profiles.clone();
        let max_output_tokens = Arc::new(move |state: &SessionState| {
            let selection = state.selection.as_ref()?;
            output_profiles
                .model_limits(selection)
                .ok()
                .map(|limits| limits.max_output_tokens)
        });
        RunnerOptions {
            input_budget,
            max_output_tokens,
            ..RunnerOptions::default()
        }
    }
}

fn handoff_input_trigger(
    context_window_tokens: u64,
    max_output_tokens: u64,
    reserve_percent: u64,
) -> u64 {
    const HANDOFF_STEP_OVERHEAD_TOKENS: u64 = 4_096;
    let output_ceiling = context_window_tokens.saturating_sub(max_output_tokens);
    let percent_reserve = context_window_tokens.saturating_mul(reserve_percent) / 100;
    context_window_tokens
        .saturating_sub(max_output_tokens.max(percent_reserve))
        .min(output_ceiling.saturating_sub(HANDOFF_STEP_OVERHEAD_TOKENS))
}

#[async_trait::async_trait]
impl ProfileResolver for ProfileStore {
    fn model_limits(
        &self,
        selection: &SessionSelection,
    ) -> Result<ModelLimits, ProfileResolveError> {
        let paths = self.paths();
        let document = zork_profile::read_profile(&paths, &selection.profile_id)
            .map_err(|_| ProfileResolveError::NotFound)?;
        let model = zork_profile::select_model(&document, &selection.model, &selection.thinking)
            .map_err(|_| ProfileResolveError::InvalidSelection)?;
        let limits = model
            .limits
            .as_ref()
            .ok_or(ProfileResolveError::InvalidSelection)?;
        Ok(ModelLimits {
            context_window_tokens: limits.context_window_tokens,
            max_output_tokens: limits.max_output_tokens,
            reserve_percent: limits.reserve_percent,
        })
    }

    async fn resolve(
        &self,
        selection: &SessionSelection,
    ) -> Result<ProfileExecution, ProfileResolveError> {
        if self.fake {
            let limits = self.model_limits(selection)?;
            return Ok(ProfileExecution::new(
                selection.profile_id.clone(),
                "openai".to_owned(),
                selection.model.clone(),
                "openai-completions".to_owned(),
                !self.no_streaming,
                false,
                None,
                "http://127.0.0.1:9/v1".to_owned(),
                Default::default(),
                selection.thinking.clone(),
                limits,
                "fake".to_owned(),
            ));
        }
        let execution = zork_profile::load_selected(
            &self.paths(),
            &self.http,
            &selection.profile_id,
            &selection.model,
            &selection.thinking,
        )
        .await
        .map_err(|_| ProfileResolveError::AuthUnavailable)?;
        Ok(ProfileExecution::new(
            execution.profile_id,
            execution.provider.clone(),
            execution.model,
            execution.api,
            execution.streaming && !self.no_streaming,
            execution.parallel_tool_calls,
            execution.service_tier,
            execution.base_url,
            execution.headers,
            execution.thinking,
            ModelLimits {
                context_window_tokens: execution.limits.context_window_tokens,
                max_output_tokens: execution.limits.max_output_tokens,
                reserve_percent: execution.limits.reserve_percent,
            },
            execution.bearer,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-01, PROVIDER-03]
    async fn process_override_disables_streaming_for_every_resolved_profile() {
        let root = tempfile::tempdir().unwrap();
        let profile = ProfileStore::open(root.path().to_owned(), true, false);
        profile
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
        let selection = SessionSelection {
            profile_id: "fixture".to_owned(),
            model: "model".to_owned(),
            thinking: "high".to_owned(),
        };

        let enabled = profile.resolve(&selection).await.unwrap();
        assert!(enabled.streaming());

        let disabled_profile = ProfileStore::open(root.path().to_owned(), true, true);
        let disabled = disabled_profile.resolve(&selection).await.unwrap();
        assert!(!disabled.streaming());
    }

    #[tokio::test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-03]
    async fn execution_rejects_a_model_without_limits_before_provider_resolution() {
        let root = tempfile::tempdir().unwrap();
        let profiles = ProfileStore::open(root.path().to_owned(), true, false);
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
                        "default": true
                    }]
                }),
            )
            .unwrap();

        assert!(matches!(
            profiles
                .resolve(&SessionSelection {
                    profile_id: "fixture".into(),
                    model: "model".into(),
                    thinking: "high".into(),
                })
                .await,
            Err(ProfileResolveError::InvalidSelection)
        ));
    }
}

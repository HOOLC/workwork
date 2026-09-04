use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::providers;
use crate::storage::{atomic_write, profile_path, validate_profile_id, ProfilePaths};

const INPUT_CAPABILITIES: &[&str] = &["text", "image"];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDocument {
    pub provider: String,
    #[serde(default = "default_billing")]
    pub billing: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "empty_object")]
    pub auth: Value,
    pub models: Vec<ProfileModel>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileModel {
    pub id: String,
    pub api: ModelApi,
    #[serde(default = "default_streaming")]
    pub streaming: bool,
    #[serde(default)]
    pub parallel_tool_calls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub thinking: Vec<String>,
    pub default_thinking: String,
    pub capabilities: ModelCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ModelLimits>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelApi {
    OpenaiCompletions,
    OpenaiResponses,
    OpenaiCodexResponses,
    AnthropicMessages,
}

impl ModelApi {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiCompletions => "openai-completions",
            Self::OpenaiResponses => "openai-responses",
            Self::OpenaiCodexResponses => "openai-codex-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelLimits {
    pub context_window_tokens: u64,
    pub max_output_tokens: u32,
    /// 输入预算预留百分比（0–100，千分之一精度可选）。预留 =
    /// max(max_output_tokens, window × pct%)。缺省 10。
    #[serde(
        default = "default_reserve_percent",
        skip_serializing_if = "is_default_reserve_percent"
    )]
    pub reserve_percent: u64,
}

fn default_reserve_percent() -> u64 {
    10
}

fn is_default_reserve_percent(value: &u64) -> bool {
    *value == 10
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    pub input: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileView {
    pub profile_id: String,
    pub provider: String,
    pub billing: String,
    pub auth_configured: bool,
    pub account: Value,
    #[serde(rename = "rateLimits")]
    pub rate_limits: Value,
    #[serde(rename = "checkedAt", skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    pub models: Vec<ProfileModel>,
}

pub fn list(paths: &impl ProfilePaths) -> Result<Vec<ProfileView>> {
    fs::create_dir_all(paths.profiles_root())?;
    let mut profiles = Vec::new();
    for entry in fs::read_dir(paths.profiles_root())? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let profile_id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .context("profile file name is not UTF-8")?;
        let document = read(paths, profile_id)?;
        profiles.push(view(profile_id, &document));
    }
    profiles.sort_by(|left, right| left.profile_id.cmp(&right.profile_id));
    Ok(profiles)
}

pub fn get(paths: &impl ProfilePaths, profile_id: &str) -> Result<Option<ProfileView>> {
    let path = profile_path(paths, profile_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let document = read(paths, profile_id)?;
    Ok(Some(view(profile_id, &document)))
}

pub fn list_with_status(
    paths: &impl ProfilePaths,
    statuses: &impl crate::ProfileStore,
) -> Result<Vec<ProfileView>> {
    let status_by_id: BTreeMap<_, _> = statuses
        .list_probes()?
        .into_iter()
        .map(|(profile_id, checked_at, account, rate_limits)| {
            (profile_id, (checked_at, account, rate_limits))
        })
        .collect();
    let mut profiles = list(paths)?;
    for profile in &mut profiles {
        if let Some(status) = status_by_id.get(&profile.profile_id) {
            apply_status(profile, status);
        }
    }
    Ok(profiles)
}

pub fn get_with_status(
    paths: &impl ProfilePaths,
    statuses: &impl crate::ProfileStore,
    profile_id: &str,
) -> Result<Option<ProfileView>> {
    let Some(mut profile) = get(paths, profile_id)? else {
        return Ok(None);
    };
    if let Some(status) = statuses
        .list_probes()?
        .into_iter()
        .find(|(id, _, _, _)| id == profile_id)
    {
        apply_status(&mut profile, &(status.1, status.2, status.3));
    }
    Ok(Some(profile))
}

pub fn put(paths: &impl ProfilePaths, profile_id: &str, body: Value) -> Result<ProfileView> {
    validate_profile_id(profile_id)?;
    let document: ProfileDocument =
        serde_json::from_value(body).context("invalid profile document")?;
    validate(&document)?;
    write(paths, profile_id, &document)?;
    Ok(view(profile_id, &document))
}

pub fn delete(paths: &impl ProfilePaths, profile_id: &str) -> Result<()> {
    let path = profile_path(paths, profile_id)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn read(paths: &impl ProfilePaths, profile_id: &str) -> Result<ProfileDocument> {
    let path = profile_path(paths, profile_id)?;
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let document: ProfileDocument =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    validate(&document)?;
    Ok(document)
}

pub(crate) fn write(
    paths: &impl ProfilePaths,
    profile_id: &str,
    document: &ProfileDocument,
) -> Result<()> {
    validate(document)?;
    atomic_write(
        &profile_path(paths, profile_id)?,
        &serde_json::to_vec_pretty(document)?,
    )
}

pub(crate) fn commit_probe_auth(
    paths: &impl ProfilePaths,
    profile_id: &str,
    expected_auth: &Value,
    refreshed_auth: Option<Value>,
) -> Result<bool> {
    let path = profile_path(paths, profile_id)?;
    if !path.exists() {
        return Ok(false);
    }
    let mut current = read(paths, profile_id)?;
    if &current.auth != expected_auth {
        return Ok(false);
    }
    if let Some(auth) = refreshed_auth {
        current.auth = auth;
        write(paths, profile_id, &current)?;
    }
    Ok(true)
}

pub fn select_model<'a>(
    document: &'a ProfileDocument,
    model_id: &str,
    thinking: &str,
) -> Result<&'a ProfileModel> {
    let model = document
        .models
        .iter()
        .find(|model| model.id == model_id)
        .with_context(|| format!("model {model_id} is not declared by profile"))?;
    if !model.thinking.iter().any(|level| level == thinking) {
        anyhow::bail!("model {model_id} does not support thinking {thinking}");
    }
    Ok(model)
}

pub fn validate(document: &ProfileDocument) -> Result<()> {
    let provider = providers::get(document.provider.trim())?;
    provider.template(document.billing.trim())?;
    if document.models.is_empty() {
        anyhow::bail!("profile must declare at least one model");
    }
    if document
        .base_url
        .as_deref()
        .is_some_and(|base_url| base_url.trim().is_empty())
    {
        anyhow::bail!("base_url must not be empty");
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut defaults = 0usize;
    for model in &document.models {
        if model.id.trim().is_empty() || !ids.insert(model.id.as_str()) {
            anyhow::bail!("model ids must be non-empty and unique");
        }
        if model.thinking.is_empty() {
            anyhow::bail!("model {} must declare thinking levels", model.id);
        }
        if let Some(service_tier) = &model.service_tier {
            if service_tier.trim().is_empty() {
                anyhow::bail!("model {} service_tier must not be empty", model.id);
            }
            if model.api != ModelApi::OpenaiCodexResponses {
                anyhow::bail!(
                    "model {} api {} does not support service_tier",
                    model.id,
                    model.api.as_str()
                );
            }
        }
        if model.parallel_tool_calls && model.api != ModelApi::OpenaiResponses {
            anyhow::bail!(
                "model {} api {} does not support parallel_tool_calls",
                model.id,
                model.api.as_str()
            );
        }
        let mut thinking = std::collections::BTreeSet::new();
        for level in &model.thinking {
            if level.trim().is_empty() || !thinking.insert(level.as_str()) {
                anyhow::bail!("model {} has invalid thinking level {level}", model.id);
            }
        }
        if !thinking.contains(model.default_thinking.as_str()) {
            anyhow::bail!(
                "model {} default_thinking must be in its thinking list",
                model.id
            );
        }
        if model.capabilities.input.is_empty() {
            anyhow::bail!("model {} must declare input capabilities", model.id);
        }
        if let Some(limits) = &model.limits {
            if limits.context_window_tokens == 0
                || limits.max_output_tokens == 0
                || u64::from(limits.max_output_tokens) >= limits.context_window_tokens
            {
                anyhow::bail!("model {} has invalid limits", model.id);
            }
        }
        let mut inputs = std::collections::BTreeSet::new();
        for capability in &model.capabilities.input {
            if !INPUT_CAPABILITIES.contains(&capability.as_str())
                || !inputs.insert(capability.as_str())
            {
                anyhow::bail!(
                    "model {} has invalid input capability {capability}",
                    model.id
                );
            }
        }
        defaults += usize::from(model.default);
    }
    if defaults > 1 {
        anyhow::bail!("profile may declare at most one default model");
    }
    Ok(())
}

pub fn view(profile_id: &str, document: &ProfileDocument) -> ProfileView {
    let auth_configured = providers::get(&document.provider)
        .and_then(|provider| provider.bearer(&document.auth))
        .is_ok();
    ProfileView {
        profile_id: profile_id.to_owned(),
        provider: document.provider.clone(),
        billing: document.billing.clone(),
        auth_configured,
        account: json!({ "ok": false, "error": "not_probed" }),
        rate_limits: json!({ "ok": false, "error": "not_probed" }),
        checked_at: None,
        models: document.models.clone(),
    }
}

fn apply_status(profile: &mut ProfileView, status: &(String, Value, Value)) {
    profile.checked_at = Some(status.0.clone());
    profile.account = status.1.clone();
    profile.rate_limits = status.2.clone();
}

fn default_billing() -> String {
    "subscription".to_owned()
}

fn default_streaming() -> bool {
    true
}

fn empty_object() -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DataRootPaths, MemoryStore, ProfileStore};

    fn fixture() -> Value {
        json!({
            "provider": "openai",
            "billing": "usage",
            "auth": { "type": "api_key", "key": "secret" },
            "models": [{
                "id": "gpt",
                "api": "openai-completions",
                "streaming": true,
                "thinking": ["low", "high"],
                "default_thinking": "high",
                "capabilities": { "input": ["text", "image"] },
                "default": true
            }]
        })
    }

    #[test]
    fn replacement_is_complete_and_public_view_has_no_secret() {
        let root = tempfile::tempdir().unwrap();
        let paths = DataRootPaths {
            data_root: root.path().to_owned(),
        };
        let public = put(&paths, "fixture", fixture()).unwrap();
        assert!(public.auth_configured);
        assert!(!serde_json::to_string(&public).unwrap().contains("secret"));

        let replacement = json!({
            "provider": "openai",
            "billing": "usage",
            "models": [{
                "id": "next",
                "api": "openai-completions",
                "streaming": true,
                "thinking": ["off"],
                "default_thinking": "off",
                "capabilities": { "input": ["text"] },
                "default": true
            }]
        });
        put(&paths, "fixture", replacement).unwrap();
        let stored = read(&paths, "fixture").unwrap();
        assert_eq!(stored.models[0].id, "next");
        assert_eq!(stored.auth, json!({}));
    }

    #[test]
    fn thinking_default_must_be_supported() {
        let mut invalid = fixture();
        invalid["models"][0]["default_thinking"] = json!("xhigh");
        let parsed: ProfileDocument = serde_json::from_value(invalid).unwrap();
        assert!(validate(&parsed).is_err());
    }

    #[test]
    fn thinking_levels_are_defined_by_each_model_profile() {
        let mut document = fixture();
        document["models"][0]["thinking"] = json!(["low", "future-depth"]);
        document["models"][0]["default_thinking"] = json!("future-depth");
        let parsed: ProfileDocument = serde_json::from_value(document).unwrap();

        validate(&parsed).unwrap();
        assert_eq!(
            select_model(&parsed, "gpt", "future-depth").unwrap().id,
            "gpt"
        );
        assert!(select_model(&parsed, "gpt", "xhigh").is_err());
    }

    #[test]
    fn openai_compatible_profile_owns_custom_endpoint_and_model_capabilities() {
        let document: ProfileDocument = serde_json::from_value(json!({
            "provider": "openai-compatible",
            "billing": "usage",
            "base_url": "https://models.example.test/v1",
            "auth": { "type": "api_key", "key": "secret" },
            "models": [{
                "id": "/models/GT-NVFP4-5090",
                "api": "openai-responses",
                "streaming": true,
                "parallel_tool_calls": false,
                "thinking": ["low", "medium", "xhigh"],
                "default_thinking": "xhigh",
                "capabilities": { "input": ["text"] },
                "limits": {
                    "context_window_tokens": 256_000,
                    "max_output_tokens": 56_000
                },
                "default": true
            }]
        }))
        .unwrap();

        validate(&document).unwrap();
        let provider = providers::get("openai-compatible").unwrap();
        assert_eq!(provider.info().label, "OpenAI Compatible");
        assert_eq!(provider.bearer(&document.auth).unwrap(), "secret");
    }

    #[test]
    fn model_streaming_defaults_to_true_and_can_be_disabled() {
        let mut document = fixture();
        document["models"][0]
            .as_object_mut()
            .unwrap()
            .remove("streaming");
        let parsed: ProfileDocument = serde_json::from_value(document).unwrap();

        assert!(parsed.models[0].streaming);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap()["models"][0]["streaming"],
            true
        );

        let mut document = fixture();
        document["models"][0]["streaming"] = json!(false);
        let parsed: ProfileDocument = serde_json::from_value(document).unwrap();

        assert!(!parsed.models[0].streaming);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap()["models"][0]["streaming"],
            false
        );
    }

    #[test]
    fn model_parallel_tool_calls_defaults_to_false_and_is_supported_only_by_standard_responses() {
        let parsed: ProfileDocument = serde_json::from_value(fixture()).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed).unwrap()["models"][0]["parallel_tool_calls"],
            false
        );

        let mut document = fixture();
        document["models"][0]["api"] = json!("openai-responses");
        document["models"][0]["parallel_tool_calls"] = json!(true);
        let parsed: ProfileDocument = serde_json::from_value(document).unwrap();
        validate(&parsed).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed).unwrap()["models"][0]["parallel_tool_calls"],
            true
        );

        let mut unsupported = fixture();
        unsupported["models"][0]["api"] = json!("openai-codex-responses");
        unsupported["models"][0]["parallel_tool_calls"] = json!(true);
        let parsed: ProfileDocument = serde_json::from_value(unsupported).unwrap();
        assert!(validate(&parsed).is_err());
    }

    #[test]
    fn codex_service_tier_is_optional_and_provider_defined() {
        let mut document = fixture();
        document["models"][0]["api"] = json!("openai-codex-responses");
        document["models"][0]["service_tier"] = json!("future-tier");
        let parsed: ProfileDocument = serde_json::from_value(document).unwrap();

        validate(&parsed).unwrap();
        assert_eq!(
            parsed.models[0].service_tier.as_deref(),
            Some("future-tier")
        );

        let mut invalid = fixture();
        invalid["models"][0]["api"] = json!("openai-codex-responses");
        invalid["models"][0]["service_tier"] = json!(" ");
        let parsed: ProfileDocument = serde_json::from_value(invalid).unwrap();
        assert!(validate(&parsed).is_err());
    }

    #[test]
    fn unsupported_api_does_not_silently_ignore_service_tier() {
        let mut document = fixture();
        document["models"][0]["service_tier"] = json!("priority");
        let parsed: ProfileDocument = serde_json::from_value(document).unwrap();

        assert!(validate(&parsed).is_err());
    }

    #[test]
    fn thinking_levels_must_be_nonempty_and_unique() {
        for levels in [json!([""]), json!(["high", "high"])] {
            let mut document = fixture();
            document["models"][0]["thinking"] = levels;
            document["models"][0]["default_thinking"] = json!("high");
            let parsed: ProfileDocument = serde_json::from_value(document).unwrap();

            assert!(validate(&parsed).is_err());
        }
    }

    #[test]
    fn every_provider_template_is_a_valid_profile_document() {
        for provider in providers::all() {
            for billing in provider.info().billing {
                let template = provider.template(billing.id).unwrap();
                let document: ProfileDocument =
                    serde_json::from_value(template.document(json!({}))).unwrap();
                validate(&document).unwrap();
            }
        }
    }

    #[test]
    // Contract: docs/zork-agent-architecture.md [PROVIDER-03]
    fn every_provider_default_limit_policy_is_table_driven() {
        let mut actual = Vec::new();
        for provider in providers::all() {
            for billing in provider.info().billing {
                let template = provider.template(billing.id).unwrap();
                let document: ProfileDocument =
                    serde_json::from_value(template.document(json!({}))).unwrap();
                let model = document
                    .models
                    .iter()
                    .find(|model| model.default)
                    .expect("every built-in template declares one default model");
                actual.push((
                    provider.info().id,
                    billing.id,
                    model
                        .limits
                        .as_ref()
                        .map(|limits| (limits.context_window_tokens, limits.max_output_tokens)),
                ));
            }
        }
        actual.sort_unstable();

        // None means the provider must be configured explicitly before use.
        assert_eq!(
            actual,
            vec![
                ("anthropic", "subscription", Some((1_000_000, 128_000))),
                ("anthropic", "usage", Some((1_000_000, 128_000))),
                ("github-copilot", "subscription", Some((1_047_576, 32_768))),
                ("github-copilot", "usage", Some((1_047_576, 32_768))),
                ("kimi-coding", "subscription", None),
                ("kimi-coding", "usage", None),
                ("openai", "subscription", Some((872_000, 128_000))),
                ("openai", "usage", Some((1_047_576, 32_768))),
                ("openai-compatible", "usage", None),
                ("opencode-go", "subscription", Some((1_048_576, 131_072))),
                ("openrouter", "usage", None),
                ("xai", "subscription", Some((500_000, 8_192))),
                ("xai", "usage", Some((500_000, 8_192))),
            ]
        );
    }

    #[test]
    fn public_view_includes_profile_account_and_quota_status_without_secret() {
        let root = tempfile::tempdir().unwrap();
        let paths = DataRootPaths {
            data_root: root.path().to_owned(),
        };
        put(&paths, "fixture", fixture()).unwrap();
        let statuses = MemoryStore::default();
        statuses
            .upsert_probe(
                "fixture",
                &json!({
                    "ok": true,
                    "account": {
                        "email": "person@example.com",
                        "planType": "Pro"
                    }
                }),
                &json!({
                    "ok": true,
                    "rateLimits": {
                        "primary": { "usedPercent": 25.0 }
                    }
                }),
            )
            .unwrap();

        let public = list_with_status(&paths, &statuses).unwrap();
        let value = serde_json::to_value(&public[0]).unwrap();
        assert_eq!(value["profile_id"], "fixture");
        assert_eq!(value["billing"], "usage");
        assert_eq!(
            value.pointer("/account/account/planType"),
            Some(&json!("Pro"))
        );
        assert_eq!(
            value.pointer("/rateLimits/rateLimits/primary/usedPercent"),
            Some(&json!(25.0))
        );
        assert!(value["checkedAt"].as_str().is_some());
        assert!(!serde_json::to_string(&public).unwrap().contains("secret"));
    }

    #[test]
    fn refreshed_auth_updates_only_the_current_profiles_unchanged_auth() {
        let root = tempfile::tempdir().unwrap();
        let paths = DataRootPaths {
            data_root: root.path().to_owned(),
        };
        put(&paths, "fixture", fixture()).unwrap();
        let expected_auth = read(&paths, "fixture").unwrap().auth;

        let mut edited = fixture();
        edited["models"][0]["id"] = json!("edited-model");
        put(&paths, "fixture", edited).unwrap();
        assert!(commit_probe_auth(
            &paths,
            "fixture",
            &expected_auth,
            Some(json!({ "type": "api_key", "key": "refreshed" })),
        )
        .unwrap());
        let stored = read(&paths, "fixture").unwrap();
        assert_eq!(stored.models[0].id, "edited-model");
        assert_eq!(stored.auth["key"], "refreshed");

        let mut newer = fixture();
        newer["auth"] = json!({ "type": "api_key", "key": "newer" });
        put(&paths, "fixture", newer).unwrap();
        assert!(!commit_probe_auth(
            &paths,
            "fixture",
            &expected_auth,
            Some(json!({ "type": "api_key", "key": "stale-refresh" })),
        )
        .unwrap());
        assert_eq!(read(&paths, "fixture").unwrap().auth["key"], "newer");

        delete(&paths, "fixture").unwrap();
        assert!(!commit_probe_auth(
            &paths,
            "fixture",
            &expected_auth,
            Some(json!({ "type": "api_key", "key": "must-not-recreate" })),
        )
        .unwrap());
        assert!(get(&paths, "fixture").unwrap().is_none());
    }
}

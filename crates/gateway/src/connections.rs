use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::sync::{watch, Mutex, RwLock};

use crate::config::now_rfc3339;
use crate::slack::{BotSelf, SlackGateway};

#[derive(Clone)]
pub struct ConnectionRuntime {
    pub config: zork_config::ImConnectionConfig,
    pub slack: SlackGateway,
    pub status: zork_slack::AssistantStatusHub,
    pub bot: Arc<Mutex<Option<BotSelf>>>,
}

#[derive(Clone, Debug)]
struct ConnectionHealth {
    state: &'static str,
    identity: Option<Value>,
    error: Option<String>,
    connected_at: Option<String>,
    updated_at: String,
}

impl ConnectionHealth {
    fn new(state: &'static str) -> Self {
        Self {
            state,
            identity: None,
            error: None,
            connected_at: None,
            updated_at: now_rfc3339(),
        }
    }

    fn value(&self) -> Value {
        json!({
            "state": self.state,
            "identity": self.identity,
            "error": self.error,
            "connectedAt": self.connected_at,
            "updatedAt": self.updated_at,
        })
    }
}

#[derive(Clone)]
pub struct ConnectionManager {
    data_root: PathBuf,
    http: reqwest::Client,
    runtimes: Arc<RwLock<HashMap<String, Arc<ConnectionRuntime>>>>,
    health: Arc<RwLock<HashMap<String, ConnectionHealth>>>,
    config_write: Arc<Mutex<()>>,
    revision: watch::Sender<u64>,
}

impl ConnectionManager {
    pub async fn load(data_root: PathBuf, http: reqwest::Client) -> Result<Self> {
        let file = zork_config::load_config(&data_root)?;
        validate_connections(&file.im_connections)?;
        let (revision, _) = watch::channel(0);
        let manager = Self {
            data_root,
            http,
            runtimes: Arc::new(RwLock::new(HashMap::new())),
            health: Arc::new(RwLock::new(HashMap::new())),
            config_write: Arc::new(Mutex::new(())),
            revision,
        };
        manager.replace_runtime_snapshot(&file.im_connections).await;
        Ok(manager)
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    pub async fn runtime(&self, connection_id: &str) -> Option<Arc<ConnectionRuntime>> {
        self.runtimes.read().await.get(connection_id).cloned()
    }

    pub async fn configs(&self) -> Vec<zork_config::ImConnectionConfig> {
        self.runtimes
            .read()
            .await
            .values()
            .map(|runtime| runtime.config.clone())
            .collect()
    }

    pub async fn views(&self) -> Vec<Value> {
        let runtimes = self.runtimes.read().await;
        let health = self.health.read().await;
        let mut rows = runtimes
            .values()
            .map(|runtime| {
                let config = &runtime.config;
                let (field_values, secret_fields_set) = provider_field_view(config);
                json!({
                    "id": config.id,
                    "name": config.name,
                    "provider": config.provider_name(),
                    "mode": config.mode,
                    "enabled": config.enabled,
                    "configured": config.configured(),
                    "fieldValues": field_values,
                    "secretFieldsSet": secret_fields_set,
                    "runtime": health.get(&config.id).map(ConnectionHealth::value)
                        .unwrap_or_else(|| ConnectionHealth::new(if config.enabled { "connecting" } else { "disabled" }).value()),
                })
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            left.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(right.get("name").and_then(Value::as_str).unwrap_or(""))
                .then_with(|| {
                    left.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .cmp(right.get("id").and_then(Value::as_str).unwrap_or(""))
                })
        });
        rows
    }

    pub async fn create(&self, connection: zork_config::ImConnectionConfig) -> Result<Value> {
        let _guard = self.config_write.lock().await;
        let mut file = zork_config::load_config(&self.data_root)?;
        if file
            .im_connections
            .iter()
            .any(|existing| existing.id == connection.id)
        {
            anyhow::bail!("connection_id_exists");
        }
        file.im_connections.push(connection.clone());
        validate_connections(&file.im_connections)?;
        zork_config::save_config(&self.data_root, &file)?;
        self.replace_runtime_snapshot(&file.im_connections).await;
        self.bump_revision();
        self.view(&connection.id)
            .await
            .context("connection missing after create")
    }

    pub async fn update(
        &self,
        connection: zork_config::ImConnectionConfig,
    ) -> Result<Option<Value>> {
        let _guard = self.config_write.lock().await;
        let mut file = zork_config::load_config(&self.data_root)?;
        let Some(existing) = file
            .im_connections
            .iter_mut()
            .find(|existing| existing.id == connection.id)
        else {
            return Ok(None);
        };
        *existing = connection.clone();
        validate_connections(&file.im_connections)?;
        zork_config::save_config(&self.data_root, &file)?;
        self.replace_runtime_snapshot(&file.im_connections).await;
        self.bump_revision();
        Ok(self.view(&connection.id).await)
    }

    pub async fn delete(&self, connection_id: &str) -> Result<bool> {
        let _guard = self.config_write.lock().await;
        let mut file = zork_config::load_config(&self.data_root)?;
        let before = file.im_connections.len();
        file.im_connections
            .retain(|connection| connection.id != connection_id);
        if file.im_connections.len() == before {
            return Ok(false);
        }
        zork_config::save_config(&self.data_root, &file)?;
        self.replace_runtime_snapshot(&file.im_connections).await;
        self.bump_revision();
        Ok(true)
    }

    pub async fn reload_from_disk(&self) -> Result<bool> {
        let _guard = self.config_write.lock().await;
        let file = zork_config::load_config(&self.data_root)?;
        validate_connections(&file.im_connections)?;
        let current = self.configs().await;
        if same_connections(&current, &file.im_connections) {
            return Ok(false);
        }
        self.replace_runtime_snapshot(&file.im_connections).await;
        self.bump_revision();
        Ok(true)
    }

    pub async fn set_connecting(&self, connection_id: &str) {
        self.set_health(connection_id, "connecting", None, None)
            .await;
    }

    pub async fn set_connected(&self, connection_id: &str, identity: Value) {
        let now = now_rfc3339();
        self.health.write().await.insert(
            connection_id.to_owned(),
            ConnectionHealth {
                state: "connected",
                identity: Some(identity),
                error: None,
                connected_at: Some(now.clone()),
                updated_at: now,
            },
        );
    }

    pub async fn set_error(&self, connection_id: &str, error: impl Into<String>) {
        self.set_health(connection_id, "error", None, Some(error.into()))
            .await;
    }

    async fn set_health(
        &self,
        connection_id: &str,
        state: &'static str,
        identity: Option<Value>,
        error: Option<String>,
    ) {
        let previous = self.health.read().await.get(connection_id).cloned();
        self.health.write().await.insert(
            connection_id.to_owned(),
            ConnectionHealth {
                state,
                identity: identity
                    .or_else(|| previous.as_ref().and_then(|value| value.identity.clone())),
                error,
                connected_at: previous.and_then(|value| value.connected_at),
                updated_at: now_rfc3339(),
            },
        );
    }

    pub async fn view(&self, connection_id: &str) -> Option<Value> {
        self.views()
            .await
            .into_iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(connection_id))
    }

    async fn replace_runtime_snapshot(&self, connections: &[zork_config::ImConnectionConfig]) {
        let current = self.runtimes.read().await.clone();
        let changed = connections
            .iter()
            .filter(|connection| {
                current
                    .get(&connection.id)
                    .is_none_or(|runtime| runtime.config != **connection)
            })
            .map(|connection| connection.id.as_str())
            .collect::<HashSet<_>>();
        let mut next = HashMap::new();
        for connection in connections {
            if let Some(existing) = current
                .get(&connection.id)
                .filter(|runtime| runtime.config == *connection)
            {
                next.insert(connection.id.clone(), existing.clone());
                continue;
            }
            let slack = connection
                .slack()
                .expect("Slack is the registered provider");
            next.insert(
                connection.id.clone(),
                Arc::new(ConnectionRuntime {
                    config: connection.clone(),
                    slack: SlackGateway::new(slack, self.http.clone()),
                    status: zork_slack::AssistantStatusHub::new(
                        self.http.clone(),
                        slack.bot_token.trim(),
                        slack.api_base_url(),
                    ),
                    bot: Arc::new(Mutex::new(None)),
                }),
            );
        }
        *self.runtimes.write().await = next;

        let ids = connections
            .iter()
            .map(|connection| connection.id.as_str())
            .collect::<HashSet<_>>();
        let mut health = self.health.write().await;
        health.retain(|id, _| ids.contains(id.as_str()));
        for connection in connections {
            if !connection.enabled {
                health.insert(connection.id.clone(), ConnectionHealth::new("disabled"));
            } else if !connection.configured() {
                health.insert(
                    connection.id.clone(),
                    ConnectionHealth {
                        state: "error",
                        identity: None,
                        error: Some("credentials_missing".into()),
                        connected_at: None,
                        updated_at: now_rfc3339(),
                    },
                );
            } else {
                if changed.contains(connection.id.as_str()) {
                    health.insert(connection.id.clone(), ConnectionHealth::new("connecting"));
                } else {
                    health
                        .entry(connection.id.clone())
                        .or_insert_with(|| ConnectionHealth::new("connecting"));
                }
            }
        }
    }

    fn bump_revision(&self) {
        self.revision.send_modify(|revision| *revision += 1);
    }
}

fn provider_field_view(config: &zork_config::ImConnectionConfig) -> (Value, Value) {
    match &config.provider {
        zork_config::ImProviderConfig::Slack(slack) => (
            json!({ "apiBaseUrl": slack.api_base_url() }),
            json!({
                "appToken": !slack.app_token.trim().is_empty(),
                "botToken": !slack.bot_token.trim().is_empty(),
            }),
        ),
    }
}

fn same_connections(
    left: &[zork_config::ImConnectionConfig],
    right: &[zork_config::ImConnectionConfig],
) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort_by(|a, b| a.id.cmp(&b.id));
    right.sort_by(|a, b| a.id.cmp(&b.id));
    left == right
}

fn validate_connections(connections: &[zork_config::ImConnectionConfig]) -> Result<()> {
    let mut ids = HashSet::new();
    for connection in connections {
        if connection.id.trim().is_empty() {
            anyhow::bail!("connection_id_required");
        }
        if connection.name.trim().is_empty() {
            anyhow::bail!("connection_name_required");
        }
        if !ids.insert(connection.id.as_str()) {
            anyhow::bail!("duplicate_connection_id: {}", connection.id);
        }
    }
    Ok(())
}

pub fn provider_catalog() -> Value {
    json!([{
        "id": "slack",
        "name": "Slack",
        "modes": ["normal", "proactive"],
        "fields": [
            { "id": "appToken", "label": "App Token", "secret": true, "placeholder": "xapp-..." },
            { "id": "botToken", "label": "Bot Token", "secret": true, "placeholder": "xoxb-..." },
            { "id": "apiBaseUrl", "label": "API Base URL", "secret": false, "optional": true, "placeholder": "https://slack.com/api" }
        ],
        "capabilities": ["messages", "files", "history", "status", "raw_api"]
    }])
}

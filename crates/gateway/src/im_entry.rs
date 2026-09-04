use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::connections::ConnectionManager;
use crate::db::{GatewayDb, SessionBindingRow, SessionRow, VisibleMessageRow};

pub const LOCAL_GUI_ENTRY_ID: &str = "local_gui";
pub const LOCAL_GUI_PLATFORM: &str = "local_gui";
const LOCAL_EVENT_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
pub struct EntryEvent {
    pub name: &'static str,
    pub data: Value,
}

#[derive(Clone, Default)]
struct LocalGuiEntry {
    senders: Arc<Mutex<HashMap<String, broadcast::Sender<EntryEvent>>>>,
}

impl LocalGuiEntry {
    fn sender(&self, session_key: &str) -> broadcast::Sender<EntryEvent> {
        let mut senders = self.senders.lock().expect("local GUI event hub mutex");
        senders
            .entry(session_key.to_owned())
            .or_insert_with(|| broadcast::channel(LOCAL_EVENT_CAPACITY).0)
            .clone()
    }

    fn subscribe(&self, session_key: &str) -> broadcast::Receiver<EntryEvent> {
        self.sender(session_key).subscribe()
    }

    fn publish(&self, session_key: &str, event: EntryEvent) {
        let _ = self.sender(session_key).send(event);
    }

    fn publish_message(&self, message: &VisibleMessageRow) {
        self.publish(
            &message.session_key,
            EntryEvent {
                name: "message",
                data: visible_message_json(message),
            },
        );
    }

    fn publish_status(&self, session_key: &str, status: Value) {
        self.publish(
            session_key,
            EntryEvent {
                name: "status",
                data: status,
            },
        );
    }
}

/// Provider-neutral gateway boundary for deliberate messages and activity.
///
/// Configured external connections and the built-in desktop entry both pass
/// through this type. Adding an external provider extends this dispatch point;
/// it does not change the Agent transcript or GUI message contract.
#[derive(Clone)]
pub struct ImEntryGateway {
    db: Arc<GatewayDb>,
    connections: Arc<ConnectionManager>,
    local_gui: LocalGuiEntry,
}

impl ImEntryGateway {
    pub fn new(db: Arc<GatewayDb>, connections: Arc<ConnectionManager>) -> Self {
        Self {
            db,
            connections,
            local_gui: LocalGuiEntry::default(),
        }
    }

    pub fn subscribe_local(&self, session_key: &str) -> broadcast::Receiver<EntryEvent> {
        self.local_gui.subscribe(session_key)
    }

    pub fn accept_local_user_message(
        &self,
        session: &SessionRow,
        message_id: &str,
        text: &str,
    ) -> Result<VisibleMessageRow> {
        ensure_local_session(session)?;
        let message = self.db.record_visible_message(
            message_id,
            &session.key,
            &session.connection_id,
            &session.channel_id,
            &session.root_thread_ts,
            "user",
            text,
            None,
        )?;
        self.local_gui.publish_message(&message);
        Ok(message)
    }

    pub async fn post_message(
        &self,
        session_key: &str,
        conversation_id: &str,
        root_message_id: &str,
        text: &str,
        kind: Option<&str>,
    ) -> Result<()> {
        let binding = self
            .db
            .get_binding(session_key)?
            .context("session_not_found")?;
        validate_destination(&binding, conversation_id, root_message_id)?;
        match binding.platform() {
            LOCAL_GUI_PLATFORM => {
                let message = self.db.record_visible_message(
                    &ulid::Ulid::new().to_string(),
                    binding.key(),
                    binding.connection_id(),
                    conversation_id,
                    root_message_id,
                    "assistant",
                    text,
                    kind,
                )?;
                self.local_gui.publish_message(&message);
            }
            "slack" => {
                let connection = self
                    .connections
                    .runtime(binding.connection_id())
                    .await
                    .context("IM connection is not configured")?;
                connection
                    .slack
                    .post_thread_message(conversation_id, root_message_id, text)
                    .await?;
                if matches!(binding, SessionBindingRow::Normal(_)) {
                    self.db.touch_reply(binding.key())?;
                }
            }
            platform => anyhow::bail!("unsupported_im_entry: {platform}"),
        }
        Ok(())
    }

    pub async fn set_status(
        &self,
        connection_id: &str,
        session_key: &str,
        conversation_id: &str,
        root_message_id: &str,
        status_event: Value,
        rendered_status: &str,
    ) {
        let platform = self
            .db
            .get_binding(session_key)
            .ok()
            .flatten()
            .map(|binding| binding.platform().to_owned());
        match platform.as_deref() {
            Some(LOCAL_GUI_PLATFORM) => self.local_gui.publish_status(session_key, status_event),
            Some("slack") => {
                if let Some(runtime) = self.connections.runtime(connection_id).await {
                    runtime
                        .status
                        .set_thread(conversation_id, root_message_id, rendered_status)
                        .await;
                }
            }
            _ => {}
        }
    }

    pub async fn refresh_status(
        &self,
        connection_id: &str,
        session_key: &str,
        conversation_id: &str,
        root_message_id: &str,
        rendered_status: &str,
    ) {
        let is_slack = self
            .db
            .get_binding(session_key)
            .ok()
            .flatten()
            .is_some_and(|binding| binding.platform() == "slack");
        if is_slack {
            if let Some(runtime) = self.connections.runtime(connection_id).await {
                runtime
                    .status
                    .set_thread(conversation_id, root_message_id, rendered_status)
                    .await;
            }
        }
    }

    pub async fn clear_status(
        &self,
        connection_id: &str,
        session_key: &str,
        conversation_id: &str,
        root_message_id: &str,
    ) {
        let platform = self
            .db
            .get_binding(session_key)
            .ok()
            .flatten()
            .map(|binding| binding.platform().to_owned());
        match platform.as_deref() {
            Some(LOCAL_GUI_PLATFORM) => self
                .local_gui
                .publish_status(session_key, json!({ "state": "clear" })),
            Some("slack") => {
                if let Some(runtime) = self.connections.runtime(connection_id).await {
                    runtime
                        .status
                        .clear_thread(conversation_id, root_message_id)
                        .await;
                }
            }
            _ => {}
        }
    }
}

pub fn visible_message_json(message: &VisibleMessageRow) -> Value {
    json!({
        "type": "message",
        "role": message.role,
        "content": message.text,
    })
}

fn ensure_local_session(session: &SessionRow) -> Result<()> {
    if session.platform != LOCAL_GUI_PLATFORM || session.connection_id != LOCAL_GUI_ENTRY_ID {
        anyhow::bail!("session_is_not_local_gui");
    }
    Ok(())
}

fn validate_destination(
    binding: &SessionBindingRow,
    conversation_id: &str,
    root_message_id: &str,
) -> Result<()> {
    if binding.mode() == zork_config::ImMode::Normal
        && (binding.conversation_id() != Some(conversation_id)
            || binding.root_message_id() != Some(root_message_id))
    {
        anyhow::bail!("session_destination_mismatch");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::EnsureSession;

    #[tokio::test]
    async fn local_gui_and_unknown_provider_use_the_same_explicit_dispatch_boundary() {
        let dir = tempfile::tempdir().unwrap();
        zork_config::ensure_layout(dir.path()).unwrap();
        let db = Arc::new(
            GatewayDb::open(&dir.path().join("state"), &dir.path().join("workspaces")).unwrap(),
        );
        let connections = Arc::new(
            ConnectionManager::load(
                dir.path().to_path_buf(),
                reqwest::Client::builder().no_proxy().build().unwrap(),
            )
            .await
            .unwrap(),
        );
        let entries = ImEntryGateway::new(db.clone(), connections);
        let local = db
            .create_session_at_workspace(
                EnsureSession {
                    connection_id: LOCAL_GUI_ENTRY_ID,
                    platform: LOCAL_GUI_PLATFORM,
                    channel_id: "local-1",
                    root_thread_ts: "local-1",
                    channel_type: Some("desktop"),
                    initiator_user_id: None,
                    initiator_message_ts: None,
                },
                &dir.path().join("local-project"),
            )
            .unwrap();
        let mut events = entries.subscribe_local(&local.key);

        entries
            .post_message(
                &local.key,
                "local-1",
                "local-1",
                "only an explicit send is visible",
                Some("final"),
            )
            .await
            .unwrap();

        let event = events.recv().await.unwrap();
        assert_eq!(event.name, "message");
        assert_eq!(event.data["role"], "assistant");
        assert_eq!(
            db.list_visible_messages(&local.key, None, 10)
                .unwrap()
                .len(),
            1
        );

        let unsupported = db
            .create_session_at_workspace(
                EnsureSession {
                    connection_id: "matrix-1",
                    platform: "matrix",
                    channel_id: "room-1",
                    root_thread_ts: "room-1",
                    channel_type: Some("room"),
                    initiator_user_id: None,
                    initiator_message_ts: None,
                },
                &dir.path().join("matrix-project"),
            )
            .unwrap();
        let error = entries
            .post_message(
                &unsupported.key,
                "room-1",
                "room-1",
                "must not silently fall back",
                Some("final"),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unsupported_im_entry: matrix"));
    }
}

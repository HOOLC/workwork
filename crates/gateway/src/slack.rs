use anyhow::{Context, Result};
use serde_json::{json, Value};

use zork_slack::SlackApi;
use zork_slack::{chunk_slack_message, markdownish_to_mrkdwn};

#[derive(Clone)]
pub struct SlackGateway {
    api: SlackApi,
}

#[derive(Clone, Debug, Default)]
pub struct BotSelf {
    pub user_id: String,
    #[allow(dead_code)]
    pub mention: String,
    pub raw: Value,
}

impl SlackGateway {
    pub fn new(config: &zork_config::SlackProviderConfig, http: reqwest::Client) -> Self {
        Self {
            api: SlackApi::new(
                config.bot_token.trim().to_string(),
                config.api_base_url(),
                http,
            ),
        }
    }

    /// Auth-test based bot identity; called by the Socket Mode loop on
    /// connect and cached in AppState.
    pub fn api(&self) -> &SlackApi {
        &self.api
    }

    pub async fn fetch_bot_self(&self) -> Result<BotSelf> {
        let payload = self.api.call("auth.test", &[]).await?;
        let user_id = payload
            .get("user_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .context("auth.test missing user_id")?
            .to_string();
        let mention = format!("<@{user_id}>");
        Ok(BotSelf {
            mention: mention.clone(),
            user_id,
            raw: json!({
                "surface": "Slack",
                "userId": payload.get("user_id"),
                "mention": mention,
                "botId": payload.get("bot_id"),
                "appId": payload.get("app_id"),
                "username": payload.get("user"),
            }),
        })
    }

    pub async fn conversation_info(
        &self,
        channel_id: &str,
    ) -> Option<(Option<String>, Option<String>)> {
        let payload = self
            .api
            .call("conversations.info", &[("channel", channel_id)])
            .await
            .ok()?;
        let channel = payload.get("channel")?;
        let name = channel
            .get("name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let channel_type = if channel.get("is_im").and_then(Value::as_bool) == Some(true) {
            Some("im".into())
        } else if channel.get("is_mpim").and_then(Value::as_bool) == Some(true) {
            Some("mpim".into())
        } else if channel.get("is_group").and_then(Value::as_bool) == Some(true) {
            Some("group".into())
        } else {
            Some("channel".into())
        };
        Some((name, channel_type))
    }

    pub async fn user_identity(&self, user_id: &str) -> Option<Value> {
        let payload = self
            .api
            .call("users.info", &[("user", user_id)])
            .await
            .ok()?;
        let user = payload.get("user")?;
        let profile = user.get("profile").cloned().unwrap_or(json!({}));
        Some(json!({
            "userId": user.get("id").and_then(Value::as_str).unwrap_or(user_id),
            "username": user.get("name"),
            "displayName": profile.get("display_name"),
            "realName": user.get("real_name").cloned().or_else(|| profile.get("real_name").cloned()),
            "mention": format!("<@{user_id}>"),
        }))
    }

    pub async fn post_thread_message(
        &self,
        channel_id: &str,
        thread_ts: &str,
        text: &str,
    ) -> Result<Option<String>> {
        let formatted = markdownish_to_mrkdwn(text);
        let mut last_ts = None;
        for chunk in chunk_slack_message(&formatted, 3_500) {
            let payload = self
                .api
                .call(
                    "chat.postMessage",
                    &[
                        ("channel", channel_id),
                        ("thread_ts", thread_ts),
                        ("text", &chunk),
                    ],
                )
                .await?;
            last_ts = payload
                .get("ts")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
        Ok(last_ts)
    }

    pub async fn thread_history(
        &self,
        conversation_id: &str,
        root_message_id: &str,
        before: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Value> {
        let mut messages = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut fields = vec![
                ("channel", conversation_id),
                ("ts", root_message_id),
                ("limit", "200"),
            ];
            if let Some(cursor) = cursor.as_deref() {
                fields.push(("cursor", cursor));
            }
            let payload = self.api.call("conversations.replies", &fields).await?;
            if let Some(entries) = payload.get("messages").and_then(Value::as_array) {
                messages.extend(entries.iter().cloned());
            }
            let has_more = payload.get("has_more").and_then(Value::as_bool) == Some(true);
            let next_cursor = payload
                .pointer("/response_metadata/next_cursor")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned);
            if !has_more || next_cursor.is_none() || messages.len() >= 1_000 {
                break;
            }
            cursor = next_cursor;
        }
        if before.is_some_and(|cursor| !cursor.trim().is_empty()) {
            messages.retain(|message| {
                let ts = message.get("ts").and_then(Value::as_str).unwrap_or("");
                ts < before.unwrap_or("")
            });
        }
        if let Some(limit) = limit.filter(|limit| *limit > 0) {
            let limit = limit.min(1_000) as usize;
            if messages.len() > limit {
                let split = messages.len() - limit;
                messages = messages.split_off(split);
            }
        }
        Ok(json!({ "ok": true, "messages": messages }))
    }

    pub async fn upload_file(
        &self,
        channel_id: &str,
        thread_ts: &str,
        filename: &str,
        bytes: &[u8],
        title: Option<&str>,
        initial_comment: Option<&str>,
    ) -> Result<Value> {
        let start = self
            .api
            .call(
                "files.getUploadURLExternal",
                &[("filename", filename), ("length", &bytes.len().to_string())],
            )
            .await?;
        let upload_url = start
            .get("upload_url")
            .and_then(Value::as_str)
            .context("missing upload_url")?
            .to_string();
        let file_id = start
            .get("file_id")
            .and_then(Value::as_str)
            .context("missing file_id")?
            .to_string();
        self.api.post_binary(&upload_url, bytes.to_vec()).await?;
        let files = if let Some(title) = title {
            json!([{ "id": file_id, "title": title }]).to_string()
        } else {
            json!([{ "id": file_id }]).to_string()
        };
        let mut fields = vec![
            ("files", files.as_str()),
            ("channel_id", channel_id),
            ("thread_ts", thread_ts),
        ];
        if let Some(comment) = initial_comment {
            fields.push(("initial_comment", comment));
        }
        self.api.call("files.completeUploadExternal", &fields).await
    }

    pub async fn download(&self, url: &str) -> Result<(Vec<u8>, String)> {
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()?
            .get(url)
            .header("authorization", format!("Bearer {}", self.api.bot_token()))
            .send()
            .await
            .context("slack download")?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = response.bytes().await.context("slack download body")?;
        Ok((bytes.to_vec(), content_type))
    }
}

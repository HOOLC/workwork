use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundEvent {
    pub source: String,
    pub conversation_id: String,
    pub root_message_id: String,
    pub message_id: Option<String>,
    pub channel_type: Option<String>,
    pub sender_user_id: String,
    pub sender_kind: String,
    pub text: String,
    pub control_text: String,
    pub mentioned_user_ids: Vec<String>,
    pub attachments: Vec<Value>,
    pub self_json: Option<Value>,
    pub slack_message: Option<Value>,
}

impl InboundEvent {
    pub fn is_stop(&self) -> bool {
        self.control_text.trim() == "-stop" && self.attachments.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.attachments.is_empty() && self.slack_message.is_none()
    }
}

#[cfg(test)]
pub fn parse_inbound_payload(raw: &str) -> Option<InboundEvent> {
    let value: Value = serde_json::from_str(raw).ok()?;
    parse_inbound_value(&value)
}

pub fn parse_inbound_value(value: &Value) -> Option<InboundEvent> {
    if value.get("v").and_then(Value::as_i64) != Some(1) {
        return None;
    }
    if value.get("kind").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let source = nonempty(value.get("source").and_then(Value::as_str))?;
    if !matches!(
        source.as_str(),
        "app_mention" | "thread_reply" | "direct_message" | "channel_message"
    ) {
        return None;
    }
    let conversation_id = nonempty(value.get("conversationId").and_then(Value::as_str))?;
    let root_message_id = nonempty(value.get("rootMessageId").and_then(Value::as_str))?;
    let sender = value.get("sender")?;
    let sender_user_id = nonempty(sender.get("userId").and_then(Value::as_str))?;
    let sender_kind = nonempty(sender.get("kind").and_then(Value::as_str))?;
    if !matches!(sender_kind.as_str(), "user" | "bot" | "app" | "unknown") {
        return None;
    }
    Some(InboundEvent {
        source,
        conversation_id,
        root_message_id,
        message_id: nonempty(value.get("messageId").and_then(Value::as_str)),
        channel_type: nonempty(value.get("channelType").and_then(Value::as_str)),
        sender_user_id,
        sender_kind,
        text: value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        control_text: value
            .get("controlText")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        mentioned_user_ids: value
            .get("mentionedUserIds")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| nonempty(entry.as_str()))
                    .collect()
            })
            .unwrap_or_default(),
        attachments: value
            .get("attachments")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| {
                entry.get("fileId").and_then(Value::as_str).is_some()
                    && entry.get("url").and_then(Value::as_str).is_some()
            })
            .collect(),
        self_json: value.get("self").cloned(),
        slack_message: value.get("slackMessage").cloned(),
    })
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_app_mention() {
        let raw = json!({
            "v": 1,
            "kind": "message",
            "source": "app_mention",
            "conversationId": "C123",
            "rootMessageId": "100.200",
            "messageId": "100.201",
            "sender": { "userId": "U123", "kind": "user" },
            "text": "<@UBOT> hello",
            "controlText": "hello",
            "mentionedUserIds": ["UBOT"],
            "attachments": []
        })
        .to_string();
        let event = parse_inbound_payload(&raw).unwrap();
        assert_eq!(event.conversation_id, "C123");
        assert_eq!(event.root_message_id, "100.200");
        assert_eq!(event.control_text, "hello");
        assert!(!event.is_stop());
    }

    #[test]
    fn detects_stop() {
        let raw = json!({
            "v": 1,
            "kind": "message",
            "source": "thread_reply",
            "conversationId": "C123",
            "rootMessageId": "1.0",
            "messageId": "1.1",
            "sender": { "userId": "U123", "kind": "user" },
            "text": "-stop",
            "controlText": "-stop",
            "mentionedUserIds": [],
            "attachments": []
        })
        .to_string();
        assert!(parse_inbound_payload(&raw).unwrap().is_stop());
    }

    #[test]
    fn drops_wrong_version() {
        let raw = json!({
            "v": 2,
            "kind": "message",
            "source": "app_mention",
            "conversationId": "C123",
            "rootMessageId": "1.0",
            "sender": { "userId": "U123", "kind": "user" },
            "text": "hi",
            "controlText": "hi"
        })
        .to_string();
        assert!(parse_inbound_payload(&raw).is_none());
    }
}

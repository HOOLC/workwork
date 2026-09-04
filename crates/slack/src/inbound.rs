use serde_json::{json, Value};

const IGNORED_SUBTYPES: &[&str] = &[
    "message_changed",
    "message_deleted",
    "channel_join",
    "channel_leave",
    "channel_topic",
    "channel_purpose",
    "channel_name",
    "channel_archive",
    "channel_unarchive",
    "thread_broadcast",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotIdentity {
    pub user_id: String,
    pub bot_id: Option<String>,
    pub app_id: Option<String>,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub real_name: Option<String>,
    pub surface: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SlackMessageMode {
    #[default]
    Thread,
    Proactive,
}

impl BotIdentity {
    pub fn mention(&self) -> String {
        format!("<@{}>", self.user_id)
    }

    pub fn to_self_json(&self) -> Value {
        json!({
            "surface": self.surface,
            "userId": self.user_id,
            "mention": self.mention(),
            "botId": self.bot_id,
            "appId": self.app_id,
            "username": self.username,
            "displayName": self.display_name,
            "realName": self.real_name,
        })
    }
}

pub fn parse_socket_payload(
    kind: &str,
    payload: &Value,
    bot: &BotIdentity,
) -> Option<(String, Value)> {
    parse_socket_payload_for_mode(kind, payload, bot, SlackMessageMode::Thread)
}

pub fn parse_socket_payload_for_mode(
    kind: &str,
    payload: &Value,
    bot: &BotIdentity,
    mode: SlackMessageMode,
) -> Option<(String, Value)> {
    match kind {
        "events_api" => parse_events_api(payload, bot, mode),
        _ => None,
    }
}

pub fn parse_history_message(
    conversation_id: &str,
    root_message_id: &str,
    message: &Value,
    bot: &BotIdentity,
) -> Option<Value> {
    let mut event = message.clone();
    let object = event.as_object_mut()?;
    object
        .entry("channel")
        .or_insert_with(|| Value::String(conversation_id.to_string()));
    if object.get("type").and_then(Value::as_str).is_none() {
        object.insert("type".into(), Value::String("message".into()));
    }
    if object.get("thread_ts").and_then(Value::as_str).is_none() {
        let thread_ts = nonempty(object.get("threadTs").and_then(Value::as_str))
            .unwrap_or_else(|| root_message_id.to_string());
        object.insert("thread_ts".into(), Value::String(thread_ts));
    }
    if object.get("channel_type").and_then(Value::as_str).is_none()
        && conversation_id.starts_with('D')
    {
        object.insert("channel_type".into(), Value::String("im".into()));
    }
    if should_ignore_event(&event, bot) {
        return None;
    }
    let parsed = parse_message_event(&event, bot, SlackMessageMode::Thread)?;
    let parsed_root = parsed.get("rootMessageId").and_then(Value::as_str)?;
    if parsed_root != root_message_id {
        return None;
    }
    Some(parsed)
}

fn parse_events_api(
    payload: &Value,
    bot: &BotIdentity,
    mode: SlackMessageMode,
) -> Option<(String, Value)> {
    let event_id = payload.get("event_id")?.as_str()?.trim();
    if event_id.is_empty() {
        return None;
    }
    let event = payload.get("event")?;
    if !event.is_object() {
        return None;
    }
    if should_ignore_event(event, bot) {
        return None;
    }
    let parsed = parse_message_event(event, bot, mode)?;
    Some((event_id.to_string(), parsed))
}

fn should_ignore_event(event: &Value, bot: &BotIdentity) -> bool {
    let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("");
    if IGNORED_SUBTYPES.contains(&subtype) {
        return true;
    }
    is_self_authored(event, bot)
}

fn is_self_authored(event: &Value, bot: &BotIdentity) -> bool {
    if let Some(user) = event.get("user").and_then(Value::as_str) {
        if user == bot.user_id {
            return true;
        }
    }
    if let (Some(bot_id), Some(own_bot_id)) = (
        event.get("bot_id").and_then(Value::as_str),
        bot.bot_id.as_deref(),
    ) {
        if bot_id == own_bot_id {
            return true;
        }
    }
    if let (Some(app_id), Some(own_app_id)) = (
        event.get("app_id").and_then(Value::as_str),
        bot.app_id.as_deref(),
    ) {
        if app_id == own_app_id {
            return true;
        }
    }
    false
}

fn parse_message_event(event: &Value, bot: &BotIdentity, mode: SlackMessageMode) -> Option<Value> {
    let event_type = event.get("type")?.as_str()?;
    let conversation_id = nonempty(event.get("channel").and_then(Value::as_str))?;
    let message_id = nonempty(event.get("ts").and_then(Value::as_str))?;
    let channel_type = nonempty(event.get("channel_type").and_then(Value::as_str));
    let text = event
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mentioned_user_ids = extract_mentioned_user_ids(&text);
    let control_text = normalize_control_text(&text, &bot.user_id);
    let attachments = normalize_attachments(event.get("files"));
    let slack_message = extract_slack_message(event);
    let sender = resolve_sender(event);

    let (source, root_message_id) = match event_type {
        "app_mention" => {
            let root = nonempty(event.get("thread_ts").and_then(Value::as_str))
                .unwrap_or_else(|| message_id.clone());
            ("app_mention", root)
        }
        "message" if channel_type.as_deref() == Some("im") => {
            let root = nonempty(event.get("thread_ts").and_then(Value::as_str))
                .unwrap_or_else(|| message_id.clone());
            ("direct_message", root)
        }
        "message" if mode == SlackMessageMode::Proactive => {
            match nonempty(event.get("thread_ts").and_then(Value::as_str)) {
                Some(root) => ("thread_reply", root),
                None => ("channel_message", message_id.clone()),
            }
        }
        "message" => {
            let root = nonempty(event.get("thread_ts").and_then(Value::as_str))?;
            ("thread_reply", root)
        }
        _ => return None,
    };

    Some(json!({
        "v": 1,
        "kind": "message",
        "source": source,
        "conversationId": conversation_id,
        "rootMessageId": root_message_id,
        "messageId": message_id,
        "channelType": channel_type,
        "sender": sender,
        "text": text,
        "controlText": control_text,
        "mentionedUserIds": mentioned_user_ids,
        "attachments": attachments,
        "self": bot.to_self_json(),
        "slackMessage": slack_message,
    }))
}

fn resolve_sender(event: &Value) -> Value {
    if let Some(user_id) = nonempty(event.get("user").and_then(Value::as_str)) {
        return json!({
            "userId": user_id,
            "kind": "user",
        });
    }
    let bot_id = nonempty(event.get("bot_id").and_then(Value::as_str));
    let app_id = nonempty(event.get("app_id").and_then(Value::as_str));
    let username = nonempty(event.get("username").and_then(Value::as_str));
    if let Some(bot_id) = bot_id.clone() {
        return json!({
            "userId": format!("bot:{bot_id}"),
            "kind": "bot",
            "botId": bot_id,
            "appId": app_id,
            "username": username,
        });
    }
    if let Some(app_id) = app_id.clone() {
        return json!({
            "userId": format!("app:{app_id}"),
            "kind": "app",
            "appId": app_id,
            "username": username,
        });
    }
    if let Some(username) = username.clone() {
        return json!({
            "userId": format!("username:{username}"),
            "kind": "unknown",
            "username": username,
        });
    }
    json!({
        "userId": "unknown:slack-message",
        "kind": "unknown",
    })
}

fn normalize_control_text(text: &str, bot_user_id: &str) -> String {
    text.replace(&format!("<@{bot_user_id}>"), "")
        .trim()
        .to_string()
}

fn extract_mentioned_user_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index + 3 < bytes.len() {
        if bytes[index] == b'<' && bytes[index + 1] == b'@' {
            let start = index + 2;
            if let Some(rel) = text[start..].find('>') {
                let id = &text[start..start + rel];
                if !id.is_empty()
                    && id
                        .chars()
                        .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
                    && !ids.iter().any(|existing| existing == id)
                {
                    ids.push(id.to_string());
                }
                index = start + rel + 1;
                continue;
            }
        }
        index += 1;
    }
    ids
}

fn normalize_attachments(files: Option<&Value>) -> Vec<Value> {
    let Some(Value::Array(entries)) = files else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let file = entry.as_object()?;
            let file_id = nonempty(file.get("id").and_then(Value::as_str))?;
            let url = pick_file_url(file)?;
            Some(json!({
                "fileId": file_id,
                "name": nonempty(file.get("name").and_then(Value::as_str)),
                "title": nonempty(file.get("title").and_then(Value::as_str)),
                "mimetype": nonempty(file.get("mimetype").and_then(Value::as_str)),
                "filetype": nonempty(file.get("filetype").and_then(Value::as_str)),
                "size": file.get("size").and_then(Value::as_i64),
                "url": url,
            }))
        })
        .collect()
}

fn extract_slack_message(event: &Value) -> Option<Value> {
    let mut selected = serde_json::Map::new();
    for key in ["subtype", "bot_id", "app_id", "username"] {
        if let Some(value) = nonempty(event.get(key).and_then(Value::as_str)) {
            selected.insert(key.to_string(), Value::String(value));
        }
    }
    for key in ["attachments", "blocks", "files"] {
        if let Some(value) = event.get(key) {
            if value.as_array().is_some_and(|entries| !entries.is_empty()) {
                selected.insert(key.to_string(), value.clone());
            }
        }
    }
    if selected.is_empty() {
        return None;
    }
    if let Some(text) = event.get("text") {
        selected.insert("text".to_string(), text.clone());
    }
    Some(Value::Object(selected))
}

fn pick_file_url(file: &serde_json::Map<String, Value>) -> Option<String> {
    for key in [
        "thumb_1024",
        "thumb_960",
        "thumb_720",
        "thumb_480",
        "thumb_360",
        "url_private_download",
        "url_private",
    ] {
        if let Some(url) = nonempty(file.get(key).and_then(Value::as_str)) {
            return Some(url);
        }
    }
    None
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

    fn bot() -> BotIdentity {
        BotIdentity {
            user_id: "UBOT".into(),
            bot_id: Some("BBOT".into()),
            app_id: Some("AAPP".into()),
            username: Some("zork".into()),
            display_name: Some("Zork".into()),
            real_name: None,
            surface: "Slack".into(),
        }
    }

    #[test]
    fn parses_app_mention_and_strips_bot() {
        let payload = json!({
            "event_id": "evt-1",
            "event": {
                "type": "app_mention",
                "user": "U123",
                "channel": "C123",
                "thread_ts": "100.200",
                "ts": "100.201",
                "text": "<@UBOT> hello gateway"
            }
        });
        let (id, parsed) = parse_socket_payload("events_api", &payload, &bot()).unwrap();
        assert_eq!(id, "evt-1");
        assert_eq!(parsed["kind"], "message");
        assert_eq!(parsed["source"], "app_mention");
        assert_eq!(parsed["conversationId"], "C123");
        assert_eq!(parsed["rootMessageId"], "100.200");
        assert_eq!(parsed["messageId"], "100.201");
        assert_eq!(parsed["controlText"], "hello gateway");
        assert_eq!(parsed["mentionedUserIds"], json!(["UBOT"]));
        assert_eq!(parsed["sender"]["userId"], "U123");
        assert_eq!(parsed["self"]["userId"], "UBOT");
        assert_eq!(parsed["self"]["mention"], "<@UBOT>");
        assert_eq!(parsed["self"]["surface"], "Slack");
        assert_eq!(parsed["self"]["username"], "zork");
        assert_eq!(parsed["self"]["displayName"], "Zork");
    }

    #[test]
    fn proactive_mode_accepts_an_unmentioned_channel_root_message() {
        let payload = json!({
            "event_id": "evt-proactive-root",
            "event": {
                "type": "message",
                "user": "U123",
                "channel": "C123",
                "channel_type": "channel",
                "ts": "100.200",
                "text": "an ordinary channel message"
            }
        });

        assert!(parse_socket_payload("events_api", &payload, &bot()).is_none());
        let (_, parsed) = parse_socket_payload_for_mode(
            "events_api",
            &payload,
            &bot(),
            SlackMessageMode::Proactive,
        )
        .expect("proactive channel message");
        assert_eq!(parsed["source"], "channel_message");
        assert_eq!(parsed["conversationId"], "C123");
        assert_eq!(parsed["rootMessageId"], "100.200");
        assert_eq!(parsed["messageId"], "100.200");
    }

    #[test]
    fn mention_without_thread_uses_message_id_as_root() {
        let payload = json!({
            "event_id": "evt-root",
            "event": {
                "type": "app_mention",
                "user": "U123",
                "channel": "C123",
                "ts": "100.201",
                "text": "hi"
            }
        });
        let (_, parsed) = parse_socket_payload("events_api", &payload, &bot()).unwrap();
        assert_eq!(parsed["rootMessageId"], "100.201");
    }

    #[test]
    fn parses_thread_reply() {
        let payload = json!({
            "event_id": "evt-2",
            "event": {
                "type": "message",
                "user": "U123",
                "channel": "C123",
                "thread_ts": "100.200",
                "ts": "100.202",
                "text": "follow up"
            }
        });
        let (_, parsed) = parse_socket_payload("events_api", &payload, &bot()).unwrap();
        assert_eq!(parsed["source"], "thread_reply");
        assert_eq!(parsed["controlText"], "follow up");
    }

    #[test]
    fn drops_channel_message_without_thread() {
        let payload = json!({
            "event_id": "evt-drop",
            "event": {
                "type": "message",
                "user": "U123",
                "channel": "C123",
                "ts": "100.202",
                "text": "noise"
            }
        });
        assert!(parse_socket_payload("events_api", &payload, &bot()).is_none());
    }

    #[test]
    fn drops_self_and_ignored_subtypes() {
        let self_event = json!({
            "event_id": "evt-self",
            "event": {
                "type": "app_mention",
                "user": "UBOT",
                "channel": "C123",
                "ts": "1.1",
                "text": "me"
            }
        });
        assert!(parse_socket_payload("events_api", &self_event, &bot()).is_none());

        let changed = json!({
            "event_id": "evt-changed",
            "event": {
                "type": "message",
                "subtype": "message_changed",
                "user": "U123",
                "channel": "C123",
                "thread_ts": "1.0",
                "ts": "1.1",
                "text": "edited"
            }
        });
        assert!(parse_socket_payload("events_api", &changed, &bot()).is_none());
    }

    #[test]
    fn drops_interactive_envelopes() {
        let payload = json!({ "type": "block_actions" });
        assert!(parse_socket_payload("interactive", &payload, &bot()).is_none());
    }

    #[test]
    fn parses_bot_card_with_blocks() {
        let payload = json!({
            "event_id": "evt-card",
            "event": {
                "type": "message",
                "subtype": "bot_message",
                "bot_id": "BLINEAR",
                "app_id": "ALINEAR",
                "username": "Linear",
                "channel": "C123",
                "thread_ts": "111.220",
                "ts": "111.223",
                "text": "",
                "blocks": [{ "type": "section" }],
                "attachments": [{ "title": "CUE-1180" }]
            }
        });
        let (_, parsed) = parse_socket_payload("events_api", &payload, &bot()).unwrap();
        assert_eq!(parsed["source"], "thread_reply");
        assert_eq!(parsed["sender"]["kind"], "bot");
        assert_eq!(parsed["slackMessage"]["bot_id"], "BLINEAR");
        assert_eq!(parsed["slackMessage"]["username"], "Linear");
        assert_eq!(
            parsed["slackMessage"]["attachments"][0]["title"],
            "CUE-1180"
        );
    }

    #[test]
    fn parses_history_reply_without_type() {
        let message = json!({
            "user": "U123",
            "channel": "C123",
            "threadTs": "100.200",
            "ts": "100.202",
            "text": "follow up"
        });
        let parsed = parse_history_message("C123", "100.200", &message, &bot()).unwrap();
        assert_eq!(parsed["source"], "thread_reply");
        assert_eq!(parsed["rootMessageId"], "100.200");
        assert_eq!(parsed["messageId"], "100.202");
        assert_eq!(parsed["controlText"], "follow up");
        assert_eq!(parsed["sender"]["userId"], "U123");
    }

    #[test]
    fn history_drops_self() {
        let message = json!({
            "user": "UBOT",
            "ts": "100.203",
            "text": "bot reply"
        });
        assert!(parse_history_message("C123", "100.200", &message, &bot()).is_none());
    }

    #[test]
    fn parses_direct_message() {
        let payload = json!({
            "event_id": "evt-dm",
            "event": {
                "type": "message",
                "channel_type": "im",
                "user": "U123",
                "channel": "D123",
                "ts": "100.201",
                "text": "hello dm"
            }
        });
        let (_, parsed) = parse_socket_payload("events_api", &payload, &bot()).unwrap();
        assert_eq!(parsed["source"], "direct_message");
        assert_eq!(parsed["rootMessageId"], "100.201");
        assert_eq!(parsed["controlText"], "hello dm");
    }
}

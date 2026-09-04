use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

struct Captured {
    method: String,
    target: String,
    path: String,
    body: String,
}

fn start_mock(
    handler: impl Fn(&Captured) -> (u16, String) + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stored = requests.clone();
    let handler = Arc::new(handler);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let captured = match read_http(&mut stream) {
                Some(captured) => captured,
                None => continue,
            };
            let (status, body) = handler(&captured);
            stored.lock().unwrap().push(captured);
            let reason = if (200..300).contains(&status) {
                "OK"
            } else {
                "ERR"
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}"), requests)
}

fn read_http(stream: &mut std::net::TcpStream) -> Option<Captured> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let mut header_end = None;
    let mut content_length = 0usize;
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if header_end.is_none() {
                    if let Some(pos) = find_headers_end(&buf) {
                        header_end = Some(pos);
                        content_length = content_length_of(&buf[..pos]);
                    }
                }
                if let Some(pos) = header_end {
                    if buf.len() >= pos + content_length {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let header_end = header_end?;
    let headers = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = headers.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let path = target
        .split_once('?')
        .map_or(target.as_str(), |(path, _)| path)
        .to_owned();
    let body = buf
        .get(header_end + 4..header_end + 4 + content_length)
        .or_else(|| buf.get(header_end + 4..))
        .map(|slice| String::from_utf8_lossy(slice).into_owned())
        .unwrap_or_default();
    Some(Captured {
        method,
        target,
        path,
        body,
    })
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length_of(headers: &[u8]) -> usize {
    let text = String::from_utf8_lossy(headers);
    for line in text.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

fn zork_call() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zork-call"));
    cmd.env_remove("SESSION_KEY")
        .env_remove("CHAT_PLATFORM")
        .env_remove("CHAT_CONNECTION_ID")
        .env_remove("CHAT_CONVERSATION_ID")
        .env_remove("CHAT_ROOT_MESSAGE_ID")
        .env_remove("ZORK_AGENT_SESSION_ID")
        .env_remove("BROKER_JOB_ID")
        .stdin(Stdio::null());
    cmd
}

fn zork_gh() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zork-gh"))
}

#[test]
fn help_prints_usage() {
    let output = zork_call().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage: zork-call"));
    assert!(stdout.contains("post-message") || stdout.contains("chat"));
}

#[test]
fn post_message_hits_broker() {
    let (url, requests) = start_mock(|_| (200, json!({ "ok": true }).to_string()));
    let output = zork_call()
        .args([
            "chat",
            "post-message",
            "--text",
            "hello",
            "--kind",
            "progress",
        ])
        .env("BROKER_API_BASE", &url)
        .env("SESSION_KEY", "connection-1:C123:1.2")
        .env("CHAT_PLATFORM", "slack")
        .env("CHAT_CONNECTION_ID", "connection-1")
        .env("CHAT_CONVERSATION_ID", "C123")
        .env("CHAT_ROOT_MESSAGE_ID", "1.2")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].method, "POST");
    assert_eq!(captured[0].path, "/chat/post-message");
    let body: Value = serde_json::from_str(&captured[0].body).unwrap();
    assert_eq!(body["conversationId"], "C123");
    assert_eq!(body["rootMessageId"], "1.2");
    assert_eq!(body["sessionKey"], "connection-1:C123:1.2");
    assert_eq!(body["text"], "hello");
    assert_eq!(body["kind"], "progress");
}

#[test]
fn agent_session_id_resolves_the_exact_gateway_binding_before_cwd() {
    let (url, requests) = start_mock(|request| {
        if request.path == "/cli/context" {
            return (
                200,
                json!({
                    "ok": true,
                    "platform": "local_gui",
                    "connectionId": "local_gui",
                    "sessionKey": "local_gui:conversation-2:conversation-2",
                    "conversationId": "conversation-2",
                    "rootMessageId": "conversation-2"
                })
                .to_string(),
            );
        }
        (200, json!({ "ok": true }).to_string())
    });
    let output = zork_call()
        .args([
            "chat",
            "post-message",
            "--text",
            "reply from task two",
            "--kind",
            "final",
        ])
        .env("BROKER_API_BASE", &url)
        .env("ZORK_AGENT_SESSION_ID", "agent-session-2")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].path, "/cli/context");
    assert!(
        captured[0].target.contains("threadId=agent-session-2"),
        "Agent tools must resolve by exact Agent session id, got {}",
        captured[0].target
    );
    assert_eq!(captured[1].path, "/chat/post-message");
    let body: Value = serde_json::from_str(&captured[1].body).unwrap();
    assert_eq!(
        body["sessionKey"],
        "local_gui:conversation-2:conversation-2"
    );
}

#[test]
fn slack_post_message_uses_only_its_explicit_destination() {
    let (url, requests) = start_mock(|_| (200, json!({ "ok": true }).to_string()));
    let output = zork_call()
        .args([
            "slack",
            "post-message",
            "--channel-id",
            "C-PROACTIVE",
            "--thread-ts",
            "9.8",
            "--text",
            "useful answer",
        ])
        .env("BROKER_API_BASE", &url)
        .env("SESSION_KEY", "connection-2:proactive")
        .env("CHAT_PLATFORM", "slack")
        .env("CHAT_CONNECTION_ID", "connection-2")
        .env("CHAT_CONVERSATION_ID", "C-WRONG")
        .env("CHAT_ROOT_MESSAGE_ID", "1.2")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/chat/post-message");
    let body: Value = serde_json::from_str(&captured[0].body).unwrap();
    assert_eq!(body["platform"], "slack");
    assert_eq!(body["sessionKey"], "connection-2:proactive");
    assert_eq!(body["conversationId"], "C-PROACTIVE");
    assert_eq!(body["rootMessageId"], "9.8");
    assert_eq!(body["text"], "useful answer");
    assert!(body.get("kind").is_none());
}

#[test]
fn slack_commands_never_fall_back_to_session_coordinates() {
    let output = zork_call()
        .args([
            "slack",
            "post-message",
            "--channel-id",
            "C-PROACTIVE",
            "--text",
            "must not be sent",
        ])
        .env("BROKER_API_BASE", "http://127.0.0.1:9")
        .env("CHAT_CONVERSATION_ID", "C-WRONG")
        .env("CHAT_ROOT_MESSAGE_ID", "1.2")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--thread-ts"));
}

#[test]
fn slack_thread_history_uses_its_explicit_destination() {
    let (url, requests) = start_mock(|request| {
        if request.path == "/cli/context" {
            return (
                200,
                json!({
                    "ok": true,
                    "platform": "slack",
                    "connectionId": "connection-2",
                    "sessionKey": "connection-2:proactive",
                    "mode": "proactive"
                })
                .to_string(),
            );
        }
        (200, "history".to_owned())
    });
    let output = zork_call()
        .args([
            "slack",
            "thread-history",
            "--channel-id",
            "C-PROACTIVE",
            "--thread-ts",
            "9.8",
            "--format",
            "text",
        ])
        .env("BROKER_API_BASE", &url)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].path, "/cli/context");
    assert_eq!(captured[1].method, "GET");
    assert_eq!(captured[1].path, "/chat/thread-history");
    assert!(captured[1]
        .target
        .contains("session_key=connection-2%3Aproactive"));
    assert!(captured[1].target.contains("conversation_id=C-PROACTIVE"));
    assert!(captured[1].target.contains("root_message_id=9.8"));
    assert!(captured[1].target.contains("format=text"));
}

#[test]
fn slack_post_file_uses_its_explicit_destination() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path().to_string_lossy().into_owned();
    let (url, requests) = start_mock(|_| (200, json!({ "ok": true }).to_string()));
    let output = zork_call()
        .args([
            "slack",
            "post-file",
            "--channel-id",
            "C-PROACTIVE",
            "--thread-ts",
            "9.8",
            "--file-path",
            &path,
        ])
        .env("BROKER_API_BASE", &url)
        .env("SESSION_KEY", "connection-2:proactive")
        .env("CHAT_PLATFORM", "slack")
        .env("CHAT_CONNECTION_ID", "connection-2")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/chat/post-file");
    let body: Value = serde_json::from_str(&captured[0].body).unwrap();
    assert_eq!(body["conversationId"], "C-PROACTIVE");
    assert_eq!(body["rootMessageId"], "9.8");
    assert_eq!(body["sessionKey"], "connection-2:proactive");
    assert_eq!(body["filePath"], path);
}

#[test]
fn notify_sends_content_without_a_mailbox_identity() {
    let (url, requests) = start_mock(|_| (200, json!({ "ok": true }).to_string()));
    let output = zork_call()
        .args(["notify", "--text", "job finished"])
        .env("BROKER_API_BASE", &url)
        .env("BROKER_JOB_ID", "job-1")
        .env("SESSION_KEY", "connection-1:C123:1.2")
        .env("CHAT_PLATFORM", "slack")
        .env("CHAT_CONNECTION_ID", "connection-1")
        .env("CHAT_CONVERSATION_ID", "C123")
        .env("CHAT_ROOT_MESSAGE_ID", "1.2")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/notify");
    let body: Value = serde_json::from_str(&captured[0].body).unwrap();
    assert_eq!(body["jobId"], "job-1");
    assert_eq!(body["sessionKey"], "connection-1:C123:1.2");
    assert_eq!(body["text"], "job finished");
    assert!(body.get("messageId").is_none());
    assert!(body.get("message_id").is_none());
}

#[test]
fn notify_rejects_the_removed_message_id_option() {
    let output = zork_call()
        .args([
            "notify",
            "--message-id",
            "obsolete-id",
            "--text",
            "job finished",
        ])
        .env("BROKER_API_BASE", "http://127.0.0.1:9")
        .env("CHAT_PLATFORM", "slack")
        .env("CHAT_CONVERSATION_ID", "C123")
        .env("CHAT_ROOT_MESSAGE_ID", "1.2")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--message-id'"));
}

#[test]
fn block_without_reason_fails_locally() {
    let output = zork_call()
        .args(["chat", "post-message", "--text", "x", "--kind", "block"])
        .env("BROKER_API_BASE", "http://127.0.0.1:9")
        .env("CHAT_CONVERSATION_ID", "C123")
        .env("CHAT_ROOT_MESSAGE_ID", "1.2")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing required argument --reason"));
}

#[test]
fn gh_wrapper_sets_token_and_strips_inherited_github_token() {
    let dir = tempfile::tempdir().unwrap();
    let capture = dir.path().join("capture");
    std::fs::create_dir_all(&capture).unwrap();
    let fake_gh = dir.path().join("real-gh");
    std::fs::write(
        &fake_gh,
        "#!/bin/sh\nprintf '%s' \"$GH_TOKEN\" > \"$CAPTURE_DIR/gh_token\"\nif [ -n \"${GITHUB_TOKEN+x}\" ]; then printf '%s' \"$GITHUB_TOKEN\" > \"$CAPTURE_DIR/github_token\"; fi\nprintf '%s\\n' \"$@\" > \"$CAPTURE_DIR/argv\"\npwd > \"$CAPTURE_DIR/cwd\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (url, _) = start_mock(|_| {
        (
            200,
            json!({ "ok": true, "token": "starter-token" }).to_string(),
        )
    });
    let output = zork_gh()
        .args(["pr", "create", "--fill"])
        .current_dir(dir.path())
        .env("BROKER_API_BASE", &url)
        .env("BROKER_REAL_GH_PATH", &fake_gh)
        .env("CAPTURE_DIR", &capture)
        .env("GH_TOKEN", "inherited-gh-token")
        .env("GITHUB_TOKEN", "inherited-github-token")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(capture.join("gh_token")).unwrap(),
        "starter-token"
    );
    assert!(!capture.join("github_token").exists());
    assert_eq!(
        std::fs::read_to_string(capture.join("argv"))
            .unwrap()
            .trim(),
        "pr\ncreate\n--fill"
    );
}

#[test]
fn gh_wrapper_does_not_exec_real_gh_when_broker_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let fake_gh = dir.path().join("real-gh");
    std::fs::write(&fake_gh, format!("#!/bin/sh\ntouch {}\n", ran.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (url, _) = start_mock(|_| {
        (
            409,
            json!({
                "ok": false,
                "message": "GitHub token for alice is invalid."
            })
            .to_string(),
        )
    });
    let output = zork_gh()
        .args(["pr", "create"])
        .env("BROKER_API_BASE", &url)
        .env("BROKER_REAL_GH_PATH", &fake_gh)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("GitHub token for alice is invalid."));
    assert!(!ran.exists());
}

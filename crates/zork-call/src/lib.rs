use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use reqwest::Url;
use serde_json::{json, Map, Value};

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser, Debug)]
#[command(
    name = "zork-call",
    about = "Session-bound CLI for chat, jobs, notify, and integrations.\nThread coordinates come from CHAT_* or the current workspace. BROKER_API_BASE is required."
)]
pub enum Cli {
    #[command(subcommand)]
    Chat(ChatCmd),
    #[command(subcommand)]
    Slack(SlackCmd),
    Notify(NotifyCmd),
    #[command(subcommand)]
    Job(JobCmd),
    #[command(subcommand)]
    Integration(IntegrationCmd),
}

#[derive(Subcommand, Debug)]
pub enum ChatCmd {
    #[command(name = "post-message")]
    PostMessage {
        #[arg(long)]
        text: String,
        #[arg(long, value_enum)]
        kind: MessageKind,
        #[arg(long)]
        reason: Option<String>,
    },
    #[command(name = "post-file")]
    PostFile {
        #[arg(long = "file-path")]
        file_path: String,
        #[arg(long = "initial-comment")]
        initial_comment: Option<String>,
    },
    #[command(name = "thread-history")]
    ThreadHistory {
        #[arg(long = "before-message-id", conflicts_with = "before_cursor")]
        before_message_id: Option<String>,
        #[arg(long = "before-cursor")]
        before_cursor: Option<String>,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        limit: Option<u64>,
        #[arg(long, value_parser = ["json", "text"])]
        format: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SlackCmd {
    #[command(name = "post-message")]
    PostMessage {
        #[arg(long = "channel-id")]
        channel_id: String,
        #[arg(long = "thread-ts")]
        thread_ts: String,
        #[arg(long)]
        text: String,
    },
    #[command(name = "post-file")]
    PostFile {
        #[arg(long = "channel-id")]
        channel_id: String,
        #[arg(long = "thread-ts")]
        thread_ts: String,
        #[arg(long = "file-path")]
        file_path: String,
        #[arg(long = "initial-comment")]
        initial_comment: Option<String>,
    },
    #[command(name = "thread-history")]
    ThreadHistory {
        #[arg(long = "channel-id")]
        channel_id: String,
        #[arg(long = "thread-ts")]
        thread_ts: String,
        #[arg(long = "before-message-id", conflicts_with = "before_cursor")]
        before_message_id: Option<String>,
        #[arg(long = "before-cursor")]
        before_cursor: Option<String>,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        limit: Option<u64>,
        #[arg(long, value_parser = ["json", "text"])]
        format: Option<String>,
    },
}

#[derive(clap::Args, Debug)]
pub struct NotifyCmd {
    #[arg(long)]
    text: String,
}

#[derive(Subcommand, Debug)]
pub enum JobCmd {
    Register {
        #[arg(long)]
        kind: String,
        #[arg(long)]
        script: String,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long = "restart-on-boot", value_parser = ["true", "false"])]
        restart_on_boot: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum IntegrationCmd {
    #[command(name = "list-tools")]
    ListTools {
        #[arg(long, value_parser = ["linear", "notion"])]
        server: String,
    },
    Call {
        #[arg(long, value_parser = ["linear", "notion"])]
        server: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        json: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum MessageKind {
    Progress,
    Final,
    Block,
    Wait,
}

impl MessageKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Final => "final",
            Self::Block => "block",
            Self::Wait => "wait",
        }
    }
}

pub fn run(cli: Cli) -> Result<()> {
    match cli {
        Cli::Chat(cmd) => run_chat(cmd),
        Cli::Slack(cmd) => run_slack(cmd),
        Cli::Notify(cmd) => run_notify(cmd),
        Cli::Job(cmd) => run_job(cmd),
        Cli::Integration(cmd) => run_integration(cmd),
    }
}

fn run_slack(cmd: SlackCmd) -> Result<()> {
    let context = resolve_session_context()?;
    if context.platform != "slack" {
        bail!("the current session is not bound to Slack");
    }
    match cmd {
        SlackCmd::PostMessage {
            channel_id,
            thread_ts,
            text,
        } => {
            let (conversation_id, root_message_id) =
                explicit_slack_coordinates(channel_id, thread_ts)?;
            request_broker(
                "POST",
                "/chat/post-message",
                None,
                Some(json!({
                    "platform": "slack",
                    "sessionKey": context.session_key,
                    "conversationId": conversation_id,
                    "rootMessageId": root_message_id,
                    "text": text,
                })),
            )
        }
        SlackCmd::PostFile {
            channel_id,
            thread_ts,
            file_path,
            initial_comment,
        } => {
            if !Path::new(&file_path).is_absolute() {
                bail!("--file-path must be an absolute path");
            }
            let (conversation_id, root_message_id) =
                explicit_slack_coordinates(channel_id, thread_ts)?;
            let mut body = json!({
                "platform": "slack",
                "sessionKey": context.session_key,
                "conversationId": conversation_id,
                "rootMessageId": root_message_id,
                "filePath": file_path,
            });
            if let Some(comment) = initial_comment {
                body["initialComment"] = json!(comment);
            }
            request_broker("POST", "/chat/post-file", None, Some(body))
        }
        SlackCmd::ThreadHistory {
            channel_id,
            thread_ts,
            before_message_id,
            before_cursor,
            limit,
            format,
        } => {
            let (conversation_id, root_message_id) =
                explicit_slack_coordinates(channel_id, thread_ts)?;
            let mut query = vec![
                ("platform", context.platform),
                ("session_key", context.session_key),
                ("conversation_id", conversation_id),
                ("root_message_id", root_message_id),
            ];
            if let Some(value) = before_message_id {
                query.push(("before_message_id", value));
            }
            if let Some(value) = before_cursor {
                query.push(("before_cursor", value));
            }
            if let Some(value) = limit {
                query.push(("limit", value.to_string()));
            }
            if let Some(value) = format {
                query.push(("format", value));
            }
            request_broker("GET", "/chat/thread-history", Some(query), None)
        }
    }
}

pub fn zork_gh_main(argv: &[String]) -> Result<i32> {
    let broker_api_base = read_env("BROKER_API_BASE");
    let real_gh_path = read_env("BROKER_REAL_GH_PATH")
        .map(PathBuf::from)
        .or_else(find_real_gh);
    if broker_api_base.is_none() || real_gh_path.is_none() {
        bail!("BROKER_API_BASE and BROKER_REAL_GH_PATH are required for broker gh wrapper.");
    }
    let broker_api_base = broker_api_base.unwrap();
    let real_gh_path = real_gh_path.unwrap();
    let cwd = env::current_dir().context("cwd")?;
    match resolve_github_token(&broker_api_base, &cwd, argv)? {
        Ok(token) => run_real_gh(&real_gh_path, &cwd, argv, &token),
        Err(message) => {
            eprint!("{message}");
            if !message.ends_with('\n') {
                eprintln!();
            }
            Ok(1)
        }
    }
}

fn run_chat(cmd: ChatCmd) -> Result<()> {
    match cmd {
        ChatCmd::PostMessage { text, kind, reason } => {
            require_reason_for_stop(kind.as_str(), reason.as_deref())?;
            let coords = resolve_chat_coordinates()?;
            let mut body = json!({
                "platform": coords.platform,
                "sessionKey": coords.session_key,
                "conversationId": coords.conversation_id,
                "rootMessageId": coords.root_message_id,
                "text": text,
                "kind": kind.as_str(),
            });
            if let Some(reason) = reason {
                body["reason"] = json!(reason);
            }
            request_broker("POST", "/chat/post-message", None, Some(body))
        }
        ChatCmd::PostFile {
            file_path,
            initial_comment,
        } => {
            if !Path::new(&file_path).is_absolute() {
                bail!("--file-path must be an absolute path");
            }
            let coords = resolve_chat_coordinates()?;
            let mut body = json!({
                "platform": coords.platform,
                "sessionKey": coords.session_key,
                "conversationId": coords.conversation_id,
                "rootMessageId": coords.root_message_id,
                "filePath": file_path,
            });
            if let Some(comment) = initial_comment {
                body["initialComment"] = json!(comment);
            }
            request_broker("POST", "/chat/post-file", None, Some(body))
        }
        ChatCmd::ThreadHistory {
            before_message_id,
            before_cursor,
            limit,
            format,
        } => {
            let coords = resolve_chat_coordinates()?;
            let mut query = vec![
                ("platform", coords.platform),
                ("session_key", coords.session_key),
                ("conversation_id", coords.conversation_id),
                ("root_message_id", coords.root_message_id),
            ];
            if let Some(value) = before_message_id {
                query.push(("before_message_id", value));
            }
            if let Some(value) = before_cursor {
                query.push(("before_cursor", value));
            }
            if let Some(value) = limit {
                query.push(("limit", value.to_string()));
            }
            if let Some(value) = format {
                query.push(("format", value));
            }
            request_broker("GET", "/chat/thread-history", Some(query), None)
        }
    }
}

fn run_notify(cmd: NotifyCmd) -> Result<()> {
    let coords = resolve_chat_coordinates()?;
    let mut body = json!({
        "sessionKey": coords.session_key,
        "text": cmd.text,
    });
    if let Some(job_id) = read_env("BROKER_JOB_ID") {
        body["jobId"] = json!(job_id);
    }
    request_broker("POST", "/notify", None, Some(body))
}

fn run_job(cmd: JobCmd) -> Result<()> {
    let JobCmd::Register {
        kind,
        script,
        cwd,
        restart_on_boot,
    } = cmd;
    let coords = resolve_chat_coordinates()?;
    let mut body = json!({
        "sessionKey": coords.session_key,
        "kind": kind,
        "script": script,
    });
    if let Some(cwd) = cwd {
        body["cwd"] = json!(cwd);
    }
    if let Some(restart) = restart_on_boot {
        body["restart_on_boot"] = json!(restart == "true");
    }
    request_broker("POST", "/jobs/register", None, Some(body))
}

fn run_integration(cmd: IntegrationCmd) -> Result<()> {
    match cmd {
        IntegrationCmd::ListTools { server } => request_broker(
            "GET",
            "/integrations/mcp-tools",
            Some(vec![("server", server)]),
            None,
        ),
        IntegrationCmd::Call { server, name, json } => request_broker(
            "POST",
            "/integrations/mcp-call",
            None,
            Some(json!({
                "server": server,
                "name": name,
                "arguments": parse_arguments_object(json.as_deref())?,
            })),
        ),
    }
}

fn require_reason_for_stop(kind: &str, reason: Option<&str>) -> Result<()> {
    if matches!(kind, "block" | "wait") && reason.is_none() {
        bail!("missing required argument --reason");
    }
    Ok(())
}

fn parse_arguments_object(value: Option<&str>) -> Result<Map<String, Value>> {
    let Some(value) = value else {
        return Ok(Map::new());
    };
    let parsed: Value = serde_json::from_str(value).context("invalid --json")?;
    match parsed {
        Value::Object(map) => Ok(map),
        _ => bail!("--json must be a JSON object"),
    }
}

fn resolve_chat_coordinates() -> Result<ChatCoordinates> {
    let context = resolve_session_context()?;
    let (Some(conversation_id), Some(root_message_id)) =
        (context.conversation_id, context.root_message_id)
    else {
        bail!("the current proactive session requires explicit IM destination coordinates");
    };
    Ok(ChatCoordinates {
        platform: context.platform,
        session_key: context.session_key,
        conversation_id,
        root_message_id,
    })
}

struct ChatCoordinates {
    platform: String,
    session_key: String,
    conversation_id: String,
    root_message_id: String,
}

fn explicit_slack_coordinates(
    conversation_id: String,
    root_message_id: String,
) -> Result<(String, String)> {
    if conversation_id.trim().is_empty() {
        bail!("--channel-id must not be empty");
    }
    if root_message_id.trim().is_empty() {
        bail!("--thread-ts must not be empty");
    }
    Ok((conversation_id, root_message_id))
}

struct SessionContext {
    platform: String,
    #[allow(dead_code)]
    connection_id: String,
    session_key: String,
    conversation_id: Option<String>,
    root_message_id: Option<String>,
}

fn resolve_session_context() -> Result<SessionContext> {
    if let (Some(session_key), Some(platform)) =
        (read_env("SESSION_KEY"), read_env("CHAT_PLATFORM"))
    {
        return Ok(SessionContext {
            platform,
            connection_id: read_env("CHAT_CONNECTION_ID").unwrap_or_default(),
            session_key,
            conversation_id: read_env("CHAT_CONVERSATION_ID"),
            root_message_id: read_env("CHAT_ROOT_MESSAGE_ID"),
        });
    }
    if let Some(agent_session_id) = read_env("ZORK_AGENT_SESSION_ID") {
        return lookup_session_context_by_agent_session(&agent_session_id);
    }
    let cwd = env::current_dir().context("cwd")?;
    lookup_session_context(&cwd.to_string_lossy())
}

fn lookup_session_context_by_agent_session(agent_session_id: &str) -> Result<SessionContext> {
    let query = vec![("threadId", agent_session_id.to_owned())];
    fetch_session_context(query)
}

fn lookup_session_context(cwd: &str) -> Result<SessionContext> {
    let query = vec![("cwd", cwd.to_string())];
    fetch_session_context(query)
}

fn fetch_session_context(query: Vec<(&str, String)>) -> Result<SessionContext> {
    let result = fetch_broker("GET", "/cli/context", Some(query), None)?;
    if !result.ok {
        bail!(
            "cli context lookup failed ({}): {}",
            result.status,
            result.text
        );
    }
    let payload: Value = serde_json::from_str(&result.text)
        .map_err(|_| anyhow::anyhow!("cli context response is not JSON"))?;
    if !payload.is_object() {
        bail!("cli context response is not an object");
    }
    let conversation_id = read_json_string(
        &payload,
        &["conversationId", "conversation_id", "channelId"],
    );
    let root_message_id = read_json_string(
        &payload,
        &["rootMessageId", "root_message_id", "rootThreadTs"],
    );
    let platform = read_json_string(&payload, &["platform"])
        .ok_or_else(|| anyhow::anyhow!("cli context is missing platform"))?;
    let connection_id = read_json_string(&payload, &["connectionId", "connection_id"])
        .ok_or_else(|| anyhow::anyhow!("cli context is missing connection ID"))?;
    let session_key = read_json_string(&payload, &["sessionKey", "session_key"])
        .ok_or_else(|| anyhow::anyhow!("cli context is missing session key"))?;
    Ok(SessionContext {
        platform,
        connection_id,
        session_key,
        conversation_id,
        root_message_id,
    })
}

fn request_broker(
    method: &str,
    path: &str,
    query: Option<Vec<(&str, String)>>,
    body: Option<Value>,
) -> Result<()> {
    let result = fetch_broker(method, path, query, body)?;
    if !result.ok {
        bail!("broker request failed ({}): {}", result.status, result.text);
    }
    write_stdout(&result.text)?;
    Ok(())
}

struct BrokerFetch {
    status: u16,
    ok: bool,
    text: String,
}

fn fetch_broker(
    method: &str,
    path: &str,
    query: Option<Vec<(&str, String)>>,
    body: Option<Value>,
) -> Result<BrokerFetch> {
    let base = require_env("BROKER_API_BASE")?;
    let mut url = Url::parse(&format!("{}{}", base.trim_end_matches('/'), path))
        .context("BROKER_API_BASE")?;
    if let Some(query) = query {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            if !value.is_empty() {
                pairs.append_pair(key, &value);
            }
        }
    }
    let url_display = url.to_string();
    let client = http_client()?;
    let mut request = match method {
        "GET" => client.get(url),
        "POST" => client.post(url),
        other => bail!("unsupported method {other}"),
    };
    if let Some(body) = body {
        request = request
            .header(CONTENT_TYPE, "application/json")
            .header("connection", "close")
            .json(&body);
    } else {
        request = request.header("connection", "close");
    }
    let response = request
        .send()
        .with_context(|| format!("broker request {method} {url_display}"))?;
    let status = response.status().as_u16();
    Ok(BrokerFetch {
        status,
        ok: response.status().is_success(),
        text: response.text().unwrap_or_default(),
    })
}

fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(FETCH_TIMEOUT)
        .http1_only()
        .no_proxy()
        .build()
        .context("http client")
}

fn resolve_github_token(
    broker_api_base: &str,
    cwd: &Path,
    argv: &[String],
) -> Result<std::result::Result<String, String>> {
    let url = Url::parse(&format!(
        "{}/github-token/resolve",
        broker_api_base.trim_end_matches('/')
    ))
    .context("BROKER_API_BASE")?;
    let response = http_client()?
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "cwd": cwd,
            "command": argv,
        }))
        .send()
        .context("github token resolve")?;
    let status = response.status().as_u16();
    let text = response.text().unwrap_or_default();
    let body: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
    };
    if !(200..300).contains(&status) || body.get("ok") != Some(&Value::Bool(true)) {
        return Ok(Err(token_error_message(status, &text)));
    }
    let token = body
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match token {
        Some(token) => Ok(Ok(token.to_string())),
        None => Ok(Err(
            "GitHub identity resolution did not return a token.".into()
        )),
    }
}

fn token_error_message(status: u16, text: &str) -> String {
    let body: Value = serde_json::from_str(text).unwrap_or_else(|_| json!({}));
    if let Some(message) = body.get("message").and_then(Value::as_str) {
        return format!("{message}\n");
    }
    if let Some(error) = body.get("error").and_then(Value::as_str) {
        return format!("{error}\n");
    }
    format!("GitHub identity resolution failed ({status}).\n")
}

fn run_real_gh(real_gh_path: &Path, cwd: &Path, argv: &[String], token: &str) -> Result<i32> {
    let mut command = Command::new(real_gh_path);
    command
        .args(argv)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for key in ["GH_TOKEN", "GITHUB_TOKEN", "BROKER_DEFAULT_GITHUB_TOKEN"] {
        command.env_remove(key);
    }
    command.env("GH_TOKEN", token);
    let status = command
        .status()
        .with_context(|| format!("exec {}", real_gh_path.display()))?;
    Ok(status.code().unwrap_or(1))
}

fn find_real_gh() -> Option<PathBuf> {
    let skip = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let skip_canon = skip.as_ref().and_then(|path| path.canonicalize().ok());
    let path_value = env::var_os("PATH")?;
    for dir in env::split_paths(&path_value) {
        if skip_canon
            .as_ref()
            .zip(dir.canonicalize().ok().as_ref())
            .is_some_and(|(skip, current)| skip == current)
        {
            continue;
        }
        let candidate = dir.join("gh");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn write_stdout(text: &str) -> io::Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

fn read_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn require_env(key: &str) -> Result<String> {
    read_env(key).ok_or_else(|| anyhow::anyhow!("missing environment variable {key}"))
}

fn read_json_string(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(entry) = value.get(*key).and_then(Value::as_str) {
            let trimmed = entry.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

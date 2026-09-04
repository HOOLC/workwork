use std::fs;
use std::path::{Path, PathBuf};

/// `sockaddr_un.sun_path` is 104 bytes on macOS (including NUL) and 108 on Linux.
const UNIX_SOCK_MAX_BYTES: usize = 103;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_GATEWAY_BIND: &str = "127.0.0.1:18790";
const DEFAULT_RUNTIME_BIND: &str = "127.0.0.1:3000";
const DEFAULT_CONTROL_BIND: &str = "127.0.0.1:3001";
const DEFAULT_AGENT_BIND: &str = "127.0.0.1:3010";
const DEFAULT_SLACK_API: &str = "https://slack.com/api";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileConfig {
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub im_connections: Vec<ImConnectionConfig>,
    #[serde(default)]
    pub bind: BindConfig,
    #[serde(default)]
    pub urls: UrlConfig,
    #[serde(default)]
    pub admin: AdminConfig,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    #[default]
    Compaction,
    Handoff,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub strategy: ContextStrategy,
    /// Approximate token target; call/result groups are never split.
    pub keep_recent_tokens: u32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            strategy: ContextStrategy::Compaction,
            keep_recent_tokens: 20_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImConnectionConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: ImMode,
    #[serde(flatten)]
    pub provider: ImProviderConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ImProviderConfig {
    Slack(SlackProviderConfig),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlackProviderConfig {
    #[serde(default)]
    pub app_token: String,
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub api_base_url: String,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImMode {
    #[default]
    Normal,
    Proactive,
}

fn default_enabled() -> bool {
    true
}

impl ImConnectionConfig {
    pub fn provider_name(&self) -> &'static str {
        match &self.provider {
            ImProviderConfig::Slack(_) => "slack",
        }
    }

    pub fn slack(&self) -> Option<&SlackProviderConfig> {
        match &self.provider {
            ImProviderConfig::Slack(config) => Some(config),
        }
    }

    pub fn configured(&self) -> bool {
        match &self.provider {
            ImProviderConfig::Slack(config) => {
                !config.app_token.trim().is_empty() && !config.bot_token.trim().is_empty()
            }
        }
    }
}

impl SlackProviderConfig {
    pub fn api_base_url(&self) -> String {
        let value = self.api_base_url.trim();
        if value.is_empty() {
            DEFAULT_SLACK_API.into()
        } else {
            value.trim_end_matches('/').to_string()
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BindConfig {
    #[serde(default = "default_gateway_bind")]
    pub gateway: String,
    #[serde(default = "default_runtime_bind")]
    pub runtime: String,
    #[serde(default = "default_control_bind")]
    pub control: String,
    #[serde(default = "default_agent_bind")]
    pub agent: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UrlConfig {
    #[serde(default)]
    pub gateway: String,
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub admin: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminConfig {
    #[serde(default)]
    pub token: String,
}

impl Default for BindConfig {
    fn default() -> Self {
        Self {
            gateway: default_gateway_bind(),
            runtime: default_runtime_bind(),
            control: default_control_bind(),
            agent: default_agent_bind(),
        }
    }
}

fn default_gateway_bind() -> String {
    DEFAULT_GATEWAY_BIND.into()
}
fn default_runtime_bind() -> String {
    DEFAULT_RUNTIME_BIND.into()
}
fn default_control_bind() -> String {
    DEFAULT_CONTROL_BIND.into()
}

fn default_agent_bind() -> String {
    DEFAULT_AGENT_BIND.into()
}

#[derive(Clone, Debug)]
pub struct ProcessArgs {
    pub data_root: PathBuf,
    pub listen_host: Option<String>,
    pub agent_token: Option<String>,
    pub fake_agent: bool,
    pub no_streaming: bool,
    pub ui_dir: Option<PathBuf>,
}

pub fn default_data_root() -> PathBuf {
    home_dir().join(".zork")
}

pub fn config_path(data_root: &Path) -> PathBuf {
    data_root.join("config.json")
}

pub fn parse_process_args() -> Result<ProcessArgs> {
    parse_process_args_from(std::env::args().skip(1))
}

pub fn parse_process_args_from<I, S>(args: I) -> Result<ProcessArgs>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut data_root = None;
    let mut listen_host = None;
    let mut agent_token = None;
    let mut fake_agent = false;
    let mut no_streaming = false;
    let mut ui_dir = None;
    let mut args = args.into_iter().peekable();
    while let Some(raw) = args.next() {
        let arg = raw.as_ref();
        match arg {
            "--data" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing value for --data"))?;
                data_root = Some(PathBuf::from(value.as_ref()));
            }
            "--listen" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing value for --listen"))?;
                listen_host = Some(value.as_ref().to_string());
            }
            "--agent-token" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing value for --agent-token"))?;
                if value.as_ref().is_empty() {
                    anyhow::bail!("--agent-token must not be empty");
                }
                agent_token = Some(value.as_ref().to_owned());
            }
            "--ui-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing value for --ui-dir"))?;
                ui_dir = Some(PathBuf::from(value.as_ref()));
            }
            "--fake-agent" => fake_agent = true,
            "--no-streaming" => no_streaming = true,
            "--help" | "-h" => {}
            other if other.starts_with('-') => {
                anyhow::bail!("unknown argument: {other}");
            }
            _ => {}
        }
    }
    Ok(ProcessArgs {
        data_root: data_root.unwrap_or_else(default_data_root),
        listen_host,
        agent_token,
        fake_agent,
        no_streaming,
        ui_dir,
    })
}

pub fn zork_sock_path(data_root: &Path) -> PathBuf {
    unix_socket_path(data_root, "sup")
}

pub fn zork_pid_path(data_root: &Path) -> PathBuf {
    data_root.join("zork.pid")
}

pub fn ready_pid_path(data_root: &Path, name: &str) -> PathBuf {
    data_root.join("run").join(format!("{name}.pid"))
}

pub fn write_ready_pid(data_root: &Path, name: &str) -> Result<()> {
    let dir = data_root.join("run");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(
        ready_pid_path(data_root, name),
        format!("{}\n", std::process::id()),
    )
    .with_context(|| format!("write {} ready pid", name))?;
    Ok(())
}

pub fn read_ready_pid(data_root: &Path, name: &str) -> Result<Option<u32>> {
    let path = ready_pid_path(data_root, name);
    match fs::read_to_string(&path) {
        Ok(body) => Ok(body.trim().parse().ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub fn clear_ready_pid(data_root: &Path, name: &str) {
    let path = ready_pid_path(data_root, name);
    let current = std::process::id();
    if fs::read_to_string(&path)
        .ok()
        .and_then(|body| body.trim().parse::<u32>().ok())
        == Some(current)
    {
        let _ = fs::remove_file(path);
    }
}

pub fn unix_socket_path(data_root: &Path, kind: &str) -> PathBuf {
    let run_dir = data_root.join("run");
    let _ = fs::create_dir_all(&run_dir);
    let preferred = run_dir.join(format!("{kind}.sock"));
    if unix_path_bytes(&preferred) <= UNIX_SOCK_MAX_BYTES {
        return preferred;
    }
    let root_token = fnv_hex(&path_bytes(data_root));
    let fallback = PathBuf::from("/tmp").join(format!("zork-{kind}-{root_token}.sock"));
    if unix_path_bytes(&fallback) <= UNIX_SOCK_MAX_BYTES {
        return fallback;
    }
    PathBuf::from("/tmp").join(format!("z{root_token}.sock"))
}

fn fnv_hex(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn unix_path_bytes(path: &Path) -> usize {
    path_bytes(path).len()
}

fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

pub fn ensure_layout(data_root: &Path) -> Result<FileConfig> {
    fs::create_dir_all(data_root).with_context(|| format!("create {}", data_root.display()))?;
    fs::create_dir_all(data_root.join("state"))?;
    fs::create_dir_all(data_root.join("sessions"))?;
    fs::create_dir_all(data_root.join("repos"))?;
    fs::create_dir_all(data_root.join("jobs"))?;
    fs::create_dir_all(data_root.join("logs"))?;
    fs::create_dir_all(data_root.join("bin"))?;
    fs::create_dir_all(data_root.join("run"))?;
    let path = config_path(data_root);
    if !path.exists() {
        save_config(data_root, &FileConfig::default())?;
    }
    load_config(data_root)
}

pub fn load_config(data_root: &Path) -> Result<FileConfig> {
    let path = config_path(data_root);
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let parsed: FileConfig =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(parsed)
}

pub fn save_config(data_root: &Path, config: &FileConfig) -> Result<()> {
    fs::create_dir_all(data_root)?;
    let path = config_path(data_root);
    let temporary_path = data_root.join("config.json.tmp");
    let body = serde_json::to_string_pretty(config)? + "\n";
    fs::write(&temporary_path, body)
        .with_context(|| format!("write {}", temporary_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary_path, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&temporary_path, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

pub fn apply_listen(config: &mut FileConfig, host: &str) {
    config.bind.gateway = replace_host(&config.bind.gateway, host);
    config.bind.runtime = replace_host(&config.bind.runtime, host);
    config.bind.control = replace_host(&config.bind.control, host);
}

pub fn has_configured_im_connection(config: &FileConfig) -> bool {
    config
        .im_connections
        .iter()
        .any(ImConnectionConfig::configured)
}

pub fn loopback_base_url(bind: &str) -> String {
    let port = bind.rsplit(':').next().unwrap_or("0");
    format!("http://127.0.0.1:{port}")
}

pub fn gateway_base_url(config: &FileConfig) -> String {
    if !config.urls.gateway.trim().is_empty() {
        return config.urls.gateway.trim_end_matches('/').to_string();
    }
    loopback_base_url(&config.bind.gateway)
}

pub fn runtime_base_url(config: &FileConfig) -> String {
    if !config.urls.runtime.trim().is_empty() {
        return config.urls.runtime.trim_end_matches('/').to_string();
    }
    loopback_base_url(&config.bind.runtime)
}

pub fn admin_base_url(config: &FileConfig) -> String {
    let value = config.urls.admin.trim();
    if !value.is_empty() {
        return value.trim_end_matches('/').to_string();
    }
    loopback_base_url(&config.bind.control)
}

pub fn admin_session_url(admin_base: &str, session_key: &str) -> String {
    let base = admin_base.trim().trim_end_matches('/');
    format!("{base}/admin/sessions/{}", encode_session_key(session_key))
}

fn encode_session_key(session_key: &str) -> String {
    session_key.replace(':', "%3A")
}

pub fn parse_bind(bind: &str) -> Result<std::net::SocketAddr> {
    bind.parse()
        .with_context(|| format!("invalid bind address {bind}"))
}

pub fn missing_commands(names: &[&str]) -> Vec<String> {
    names
        .iter()
        .copied()
        .filter(|name| find_on_path(name).is_none())
        .map(ToOwned::to_owned)
        .collect()
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_value = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_value) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn resolve_ui_dir(data_root: &Path, explicit: Option<&Path>) -> PathBuf {
    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(path.to_path_buf());
    }
    candidates.push(data_root.join("ui"));
    candidates.push(PathBuf::from("/ui"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("ui"));
            let mut current = dir.to_path_buf();
            for _ in 0..3 {
                if !current.pop() {
                    break;
                }
                candidates.push(current.join("apps/admin-ui/dist"));
            }
        }
    }
    candidates.push(PathBuf::from("apps/admin-ui/dist"));
    for candidate in &candidates {
        if candidate.join("index.html").is_file() {
            return candidate.clone();
        }
    }
    data_root.join("ui")
}

pub fn find_bin(name: &str, data_root: &Path) -> Option<PathBuf> {
    let local = data_root.join("bin").join(name);
    if local.is_file() {
        return Some(local);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(name);
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    find_on_path(name)
}

fn replace_host(bind: &str, host: &str) -> String {
    match bind.rsplit_once(':') {
        Some((_, port)) => format!("{host}:{port}"),
        None => format!("{host}:{bind}"),
    }
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = FileConfig {
            im_connections: vec![
                ImConnectionConfig {
                    id: "01J00000000000000000000001".into(),
                    name: "work".into(),
                    enabled: true,
                    mode: ImMode::Normal,
                    provider: ImProviderConfig::Slack(SlackProviderConfig {
                        app_token: "xapp-1".into(),
                        bot_token: "xoxb-1".into(),
                        api_base_url: String::new(),
                    }),
                },
                ImConnectionConfig {
                    id: "01J00000000000000000000002".into(),
                    name: "community".into(),
                    enabled: true,
                    mode: ImMode::Proactive,
                    provider: ImProviderConfig::Slack(SlackProviderConfig {
                        app_token: "xapp-2".into(),
                        bot_token: "xoxb-2".into(),
                        api_base_url: "https://slack.example/api/".into(),
                    }),
                },
            ],
            ..FileConfig::default()
        };
        save_config(dir.path(), &config).unwrap();
        let loaded = load_config(dir.path()).unwrap();
        assert_eq!(loaded.im_connections, config.im_connections);
        assert_eq!(loaded.im_connections[0].provider_name(), "slack");
        assert_eq!(
            loaded.im_connections[1].slack().unwrap().api_base_url(),
            "https://slack.example/api"
        );
        assert!(has_configured_im_connection(&loaded));
    }

    #[test]
    fn listen_rewrites_hosts() {
        let mut config = FileConfig::default();
        apply_listen(&mut config, "0.0.0.0");
        assert_eq!(config.bind.gateway, "0.0.0.0:18790");
        assert_eq!(
            loopback_base_url(&config.bind.control),
            "http://127.0.0.1:3001"
        );
    }

    #[test]
    fn admin_session_url_encodes_key() {
        let url = admin_session_url("http://127.0.0.1:3001/", "C123:100.200");
        assert_eq!(url, "http://127.0.0.1:3001/admin/sessions/C123%3A100.200");
    }

    #[test]
    fn unix_sockets_fit_under_sun_len() {
        let dir = tempfile::tempdir().unwrap();
        let sock = zork_sock_path(dir.path());
        assert!(sock.starts_with(dir.path().join("run")));
        assert!(unix_path_bytes(&sock) <= UNIX_SOCK_MAX_BYTES);
    }

    #[test]
    fn long_data_root_falls_back_to_tmp() {
        let long = PathBuf::from(format!("/{}", "x".repeat(180)));
        let sock = unix_socket_path(&long, "sup");
        assert!(sock.starts_with("/tmp/"));
        assert!(unix_path_bytes(&sock) <= UNIX_SOCK_MAX_BYTES);
    }

    #[test]
    fn ready_pid_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        write_ready_pid(dir.path(), "zork-agent").unwrap();
        assert_eq!(
            read_ready_pid(dir.path(), "zork-agent").unwrap(),
            Some(std::process::id())
        );
        clear_ready_pid(dir.path(), "zork-agent");
        assert_eq!(read_ready_pid(dir.path(), "zork-agent").unwrap(), None);
    }

    #[test]
    fn agent_token_is_an_optional_startup_argument() {
        let absent = parse_process_args_from(Vec::<String>::new()).unwrap();
        assert_eq!(absent.agent_token, None);

        let supplied = parse_process_args_from(["--agent-token", "exact secret"]).unwrap();
        assert_eq!(supplied.agent_token.as_deref(), Some("exact secret"));

        assert!(parse_process_args_from(["--agent-token"]).is_err());
        assert!(parse_process_args_from(["--agent-token", ""]).is_err());
    }

    #[test]
    fn no_streaming_is_an_optional_process_wide_flag() {
        let absent = parse_process_args_from(Vec::<String>::new()).unwrap();
        assert!(!absent.no_streaming);

        let supplied = parse_process_args_from(["--no-streaming"]).unwrap();
        assert!(supplied.no_streaming);
    }
}

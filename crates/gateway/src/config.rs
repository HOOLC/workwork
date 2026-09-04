use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub bind_addr: SocketAddr,
    pub service_name: String,
    pub data_root: PathBuf,
    pub state_dir: PathBuf,
    pub workspaces_root: PathBuf,
    pub repos_root: PathBuf,
    pub jobs_root: PathBuf,
    pub log_dir: PathBuf,
    pub broker_http_base_url: String,
    pub agent_bind: String,
    pub agent_token: Option<String>,
    pub slack_initial_thread_history_count: i64,
    pub slack_history_api_max_limit: i64,
    pub zork_bin_dir: PathBuf,
    pub zork_call_path: Option<PathBuf>,
    pub zork_gh_path: Option<PathBuf>,
    pub real_gh_path: Option<PathBuf>,

    #[allow(dead_code)]
    pub isolated_mcp_servers: Vec<String>,
    #[allow(dead_code)]
    pub github_api_base_url: String,
    pub default_github_login: Option<String>,
    pub default_github_token: Option<String>,
    #[allow(dead_code)]
    pub started_at: String,
}

impl RuntimeConfig {
    pub fn load() -> Result<Self> {
        let args = zork_config::parse_process_args()?;
        let mut file = zork_config::ensure_layout(&args.data_root)?;
        if let Some(host) = &args.listen_host {
            zork_config::apply_listen(&mut file, host);
        }
        let data_root = args.data_root;
        Ok(Self {
            bind_addr: zork_config::parse_bind(&file.bind.runtime)?,
            service_name: "zork-gateway".into(),
            state_dir: data_root.join("state"),
            workspaces_root: data_root.join("workspaces"),
            repos_root: data_root.join("repos"),
            jobs_root: data_root.join("jobs"),
            log_dir: data_root.join("logs"),
            broker_http_base_url: zork_config::loopback_base_url(&file.bind.runtime),
            agent_bind: file.bind.agent.clone(),
            agent_token: args.agent_token,
            slack_initial_thread_history_count: 8,
            slack_history_api_max_limit: 50,
            zork_bin_dir: data_root.join("bin"),
            zork_call_path: None,
            zork_gh_path: None,
            real_gh_path: None,

            isolated_mcp_servers: vec!["linear".into(), "notion".into()],
            github_api_base_url: "https://api.github.com".into(),
            default_github_login: None,
            default_github_token: None,
            started_at: now_rfc3339(),
            data_root,
        })
    }

    pub fn github_mappings_dir(&self) -> PathBuf {
        self.state_dir.join("github-author-mappings")
    }
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

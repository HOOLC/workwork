use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use zork_agent::provider::{AgentModelPort, ProviderRouter};
use zork_agent::session::events::Selection;
use zork_agent::session::ports::{
    ModelExecutor, SystemClock, SystemFileSystem, SystemIdGenerator, SystemProcessSpawner,
};
use zork_agent::session::query::{FileSessionQuery, QueryError, SessionQuery};
use zork_agent::session::service::{
    ServiceDependencies, ServiceOptions, SessionService, SessionSubscription,
};
use zork_agent::session::state::SessionState;
use zork_agent::session::store::{EventEnvelope, SessionStore, StreamStore};
use zork_agent::session::supervisor::SupervisorError;
use zork_agent::session::tools::{
    register_builtin_tools, BuiltinToolDependencies, ToolDefinitionError, ToolRegistry,
};
use zork_agent::ProfileStore;

use crate::provider::{ControlledHttpProvider, PendingHttpRequest};
use crate::server::AgentHttpServer;

pub struct RealAgent {
    root: TempDir,
    data_root: PathBuf,
    workspace_root: PathBuf,
    workspaces: HashMap<String, PathBuf>,
    service: Option<Arc<SessionService>>,
    profiles: Arc<ProfileStore>,
    server: Option<AgentHttpServer>,
    client: reqwest::Client,
    provider: Option<ControlledHttpProvider>,
    provider_base_url: String,
    agent_token: Option<String>,
    fake_agent: bool,
    sessions: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RealAgentError {
    #[error("real test environment io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("real test tool definition error: {0}")]
    Tool(#[from] ToolDefinitionError),
    #[error("restarted agent did not discover all existing sessions")]
    SessionsNotDiscovered,
    #[error("real test HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("real agent returned HTTP {status}: {body}")]
    Api { status: u16, body: String },
    #[error("real test profile could not be written: {0}")]
    Profile(String),
    #[error("real agent returned an invalid response: {0}")]
    InvalidResponse(String),
}

impl RealAgent {
    pub fn new() -> Result<Self, RealAgentError> {
        Self::start(None, false)
    }

    pub fn with_token(token: Option<String>) -> Result<Self, RealAgentError> {
        Self::start(token, false)
    }

    pub fn fake_with_token(token: Option<String>) -> Result<Self, RealAgentError> {
        Self::start(token, true)
    }

    fn start(token: Option<String>, fake_agent: bool) -> Result<Self, RealAgentError> {
        let root = tempfile::tempdir()?;
        let data_root = root.path().join("data");
        let workspace_root = root.path().join("workspaces");
        std::fs::create_dir_all(&workspace_root)?;
        let provider = ControlledHttpProvider::start(64)?;
        let provider_base_url = provider.base_url().to_owned();
        let (service, profiles) = start_service(root.path(), fake_agent)?;
        let server = AgentHttpServer::start(service.clone(), profiles.clone(), token.clone())?;
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(token) = &token {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .expect("test agent token is a valid header value"),
            );
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;
        Ok(Self {
            root,
            data_root,
            workspace_root,
            workspaces: HashMap::new(),
            service: Some(service),
            profiles,
            server: Some(server),
            client,
            provider: Some(provider),
            provider_base_url,
            agent_token: token,
            fake_agent,
            sessions: Vec::new(),
        })
    }

    pub fn workspace(&self, session_id: &str) -> Option<&Path> {
        self.workspaces.get(session_id).map(PathBuf::as_path)
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn base_url(&self) -> &str {
        self.server().base_url()
    }

    pub fn provider_base_url(&self) -> &str {
        &self.provider_base_url
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub async fn create_session(
        &mut self,
        selection: Selection,
        system_prompt: Option<String>,
    ) -> Result<String, RealAgentError> {
        self.write_profile(&selection)?;
        self.create_configured_session(selection, system_prompt)
            .await
    }

    pub async fn create_configured_session(
        &mut self,
        selection: Selection,
        system_prompt: Option<String>,
    ) -> Result<String, RealAgentError> {
        let workspace = self
            .workspace_root
            .join(format!("session-{:04}", self.sessions.len() + 1));
        std::fs::create_dir_all(&workspace)?;
        let response = self
            .client
            .post(format!("{}/sessions", self.base_url()))
            .json(&serde_json::json!({
                "profile_id": selection.profile_id,
                "model": selection.model,
                "thinking": selection.thinking,
                "system_prompt": system_prompt,
                "workspace": workspace,
            }))
            .send()
            .await?;
        let body = success_json(response).await?;
        let session_id = body
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| RealAgentError::InvalidResponse("missing session_id".into()))?
            .to_owned();
        self.sessions.push(session_id.clone());
        self.workspaces.insert(session_id.clone(), workspace);
        Ok(session_id)
    }

    pub fn install_profile(
        &self,
        profile_id: &str,
        document: serde_json::Value,
    ) -> Result<(), RealAgentError> {
        self.profiles
            .put(profile_id, document)
            .map(|_| ())
            .map_err(|error| RealAgentError::Profile(error.to_string()))
    }

    pub async fn send_mail(
        &self,
        session_id: &str,
        content: impl Into<String>,
    ) -> Result<(), RealAgentError> {
        let response = self
            .client
            .post(format!("{}/sessions/{session_id}/mailbox", self.base_url()))
            .json(&serde_json::json!({"content": content.into()}))
            .send()
            .await?;
        success_empty(response).await
    }

    pub async fn cancel(&self, session_id: &str) -> Result<(), RealAgentError> {
        let response = self
            .client
            .post(format!("{}/sessions/{session_id}/cancel", self.base_url()))
            .send()
            .await?;
        success_empty(response).await
    }

    pub async fn delete(&mut self, session_id: &str) -> Result<(), RealAgentError> {
        let response = self
            .client
            .delete(format!("{}/sessions/{session_id}", self.base_url()))
            .send()
            .await?;
        success_empty(response).await?;
        self.sessions.retain(|existing| existing != session_id);
        Ok(())
    }

    pub async fn request(&mut self) -> PendingHttpRequest {
        self.provider
            .as_mut()
            .expect("the real test provider has been shut down")
            .request()
            .await
    }

    pub async fn state(&self, session_id: &str) -> Result<SessionState, SupervisorError> {
        self.service().state(session_id).await
    }

    pub async fn wait_for_state(
        &self,
        session_id: &str,
        mut condition: impl FnMut(&SessionState) -> bool,
    ) -> SessionState {
        let mut subscription = self
            .subscribe(session_id)
            .expect("the real test session remains available");
        loop {
            let state = self
                .state(session_id)
                .await
                .expect("the real test session remains available");
            if condition(&state) {
                return state;
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("real state condition did not become true; latest state: {state:#?}")
                })
                .expect("the real session event stream remains open");
        }
    }

    pub fn subscribe(&self, session_id: &str) -> Result<SessionSubscription, SupervisorError> {
        self.service().subscribe(session_id)
    }

    pub fn history(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, QueryError> {
        self.service().history_after(session_id, cursor, limit)
    }

    pub async fn messages(&self, session_id: &str) -> Result<serde_json::Value, RealAgentError> {
        let response = self
            .client
            .get(format!(
                "{}/sessions/{session_id}/messages?limit=200",
                self.base_url()
            ))
            .send()
            .await?;
        success_json(response).await
    }

    pub async fn restart(&mut self) -> Result<(), RealAgentError> {
        self.stop_agent().await;
        self.start_agent().await
    }

    pub async fn stop_agent(&mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown().await;
        }
        if let Some(service) = self.service.take() {
            service.shutdown().await;
            drop(service);
        }
    }

    pub async fn start_agent(&mut self) -> Result<(), RealAgentError> {
        let (service, profiles) = start_service(self.root.path(), self.fake_agent)?;
        let server =
            AgentHttpServer::start(service.clone(), profiles.clone(), self.agent_token.clone())?;
        self.service = Some(service);
        self.profiles = profiles;
        self.server = Some(server);
        for _ in 0..4096 {
            if self
                .sessions
                .iter()
                .all(|session_id| self.service().contains(session_id))
            {
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
        Err(RealAgentError::SessionsNotDiscovered)
    }

    pub async fn shutdown(&mut self) {
        self.stop_agent().await;
        if let Some(provider) = self.provider.take() {
            provider.shutdown().await;
        }
    }

    fn service(&self) -> &Arc<SessionService> {
        self.service
            .as_ref()
            .expect("the real test agent has been shut down")
    }

    fn server(&self) -> &AgentHttpServer {
        self.server
            .as_ref()
            .expect("the real test agent HTTP server has been shut down")
    }

    fn write_profile(&self, selection: &Selection) -> Result<(), RealAgentError> {
        self.profiles
            .put(
                &selection.profile_id,
                serde_json::json!({
                    "provider": "openai",
                    "billing": "usage",
                    "base_url": format!("{}/v1", self.provider_base_url),
                    "auth": {"type": "api_key", "key": "test"},
                    "models": [{
                        "id": selection.model,
                        "api": "openai-completions",
                        "streaming": true,
                        "parallel_tool_calls": false,
                        "thinking": [selection.thinking],
                        "default_thinking": selection.thinking,
                        "capabilities": {"input": ["text"]},
                        "limits": {
                            "context_window_tokens": 256000,
                            "max_output_tokens": 32000
                        },
                        "default": true
                    }]
                }),
            )
            .map(|_| ())
            .map_err(|error| RealAgentError::Profile(error.to_string()))
    }
}

fn start_service(
    root: &Path,
    fake_agent: bool,
) -> Result<(Arc<SessionService>, Arc<ProfileStore>), RealAgentError> {
    let data = root.join("data");
    let store = Arc::new(StreamStore::open(&data)?);
    let query = Arc::new(FileSessionQuery::open(&data));
    let clock = Arc::new(SystemClock);
    let tools = Arc::new(ToolRegistry::default());
    register_builtin_tools(
        &tools,
        BuiltinToolDependencies {
            environment: BTreeMap::new(),
            query: query.clone() as Arc<dyn SessionQuery>,
            clock: clock.clone(),
            files: Arc::new(SystemFileSystem),
            processes: Arc::new(SystemProcessSpawner),
        },
    )?;
    let profiles = Arc::new(ProfileStore::open(data.clone(), fake_agent, false));
    let runner = ProfileStore::runner_options(&profiles);
    let provider: Arc<dyn ModelExecutor> = Arc::new(ProviderRouter::new());
    let model = Arc::new(AgentModelPort::new(provider, profiles.clone()));
    let service = SessionService::start(
        ServiceDependencies {
            store: store as Arc<dyn SessionStore>,
            query: query as Arc<dyn SessionQuery>,
            model,
            tools,
            clock,
            ids: Arc::new(SystemIdGenerator),
        },
        ServiceOptions {
            runner,
            ..ServiceOptions::default()
        },
    );
    Ok((service, profiles))
}

async fn success_json(response: reqwest::Response) -> Result<serde_json::Value, RealAgentError> {
    let status = response.status();
    if !status.is_success() {
        return Err(RealAgentError::Api {
            status: status.as_u16(),
            body: response.text().await?,
        });
    }
    response.json().await.map_err(RealAgentError::from)
}

async fn success_empty(response: reqwest::Response) -> Result<(), RealAgentError> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    Err(RealAgentError::Api {
        status: status.as_u16(),
        body: response.text().await?,
    })
}

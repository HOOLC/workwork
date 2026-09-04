use std::collections::BTreeMap;
use std::sync::Arc;

use zork_agent::session::events::Selection;
use zork_agent::session::query::SessionQuery;
use zork_agent::session::service::{
    ServiceDependencies, ServiceOptions, SessionService, SessionSubscription,
};
use zork_agent::session::state::SessionState;
use zork_agent::session::store::{EventEnvelope, SessionStore};
use zork_agent::session::supervisor::SupervisorError;
use zork_agent::session::supervisor::{PublicSlotStatus, SessionSlotView};
use zork_agent::session::tools::{ToolCompatibility, ToolContract, ToolRegistry};

use crate::clock::ManualClock;
use crate::filesystem::MemoryFileSystem;
use crate::identity::DeterministicIds;
use crate::model::{ControlledModel, ControlledModelGateway, PendingModelRequest};
use crate::process::{ControlledProcesses, PendingProcess};
use crate::query::MemorySessionQuery;
use crate::store::MemorySessionStore;
use crate::tool::ControlledTool;

pub struct TestWorld {
    service: Option<Arc<SessionService>>,
    model: ControlledModel,
    model_gateway: Arc<ControlledModelGateway>,
    options: ServiceOptions,
    pub clock: ManualClock,
    pub ids: DeterministicIds,
    pub store: MemorySessionStore,
    pub query: MemorySessionQuery,
    pub tools: Arc<ToolRegistry>,
    pub files: MemoryFileSystem,
    processes: ControlledProcesses,
}

#[derive(Debug, thiserror::Error)]
pub enum TestWorldError {
    #[error("memory store error: {0}")]
    Store(#[from] zork_agent::session::store::StoreError),
    #[error("memory query error: {0}")]
    Query(#[from] zork_agent::session::query::QueryError),
    #[error("restarted TestWorld did not discover all existing sessions")]
    SessionsNotDiscovered,
}

impl TestWorld {
    pub fn new() -> Self {
        Self::with_options(ServiceOptions::default())
    }

    pub fn with_options(options: ServiceOptions) -> Self {
        let clock = ManualClock::default();
        let ids = DeterministicIds::default();
        let store = MemorySessionStore::new();
        let query = store.query();
        let tools = Arc::new(ToolRegistry::default());
        let files = MemoryFileSystem::new();
        let (process_gateway, processes) = ControlledProcesses::pair();
        zork_agent::session::tools::register_builtin_tools(
            &tools,
            zork_agent::session::tools::BuiltinToolDependencies {
                environment: BTreeMap::new(),
                query: Arc::new(query.clone()),
                clock: Arc::new(clock.clone()),
                files: Arc::new(files.clone()),
                processes: process_gateway,
            },
        )
        .expect("the production built-in tool contracts are valid");
        let (model_gateway, model) = ControlledModel::pair(64);
        let service = start_service(
            &store,
            &query,
            &tools,
            &clock,
            &ids,
            model_gateway.clone(),
            options.clone(),
        );
        Self {
            service: Some(service),
            model,
            model_gateway,
            options,
            clock,
            ids,
            store,
            query,
            tools,
            files,
            processes,
        }
    }

    pub async fn create_session(
        &self,
        selection: Selection,
        system_prompt: Option<String>,
        workspace: impl Into<String>,
    ) -> Result<String, SupervisorError> {
        self.service()
            .create_session(selection, system_prompt, workspace.into(), None)
            .await
    }

    pub async fn send_mail(
        &self,
        session_id: &str,
        content: impl Into<String>,
    ) -> Result<(), SupervisorError> {
        self.service()
            .submit_input(session_id, content.into())
            .await
    }

    pub async fn cancel(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.service().cancel(session_id).await
    }

    pub async fn request(&mut self) -> PendingModelRequest {
        self.model.request().await
    }

    pub async fn process_request(&mut self) -> PendingProcess {
        self.processes.request().await
    }

    pub async fn state(&self, session_id: &str) -> Result<SessionState, SupervisorError> {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.service().state(session_id),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out inspecting session {session_id}; slots: {:?}",
                self.sessions()
            )
        })
    }

    pub async fn wait_for_state(
        &self,
        session_id: &str,
        mut condition: impl FnMut(&SessionState) -> bool,
    ) -> SessionState {
        let mut subscription = self
            .subscribe(session_id)
            .expect("the test session remains available");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let state = self
                .state(session_id)
                .await
                .expect("the test session remains available");
            if condition(&state) {
                return state;
            }
            tokio::time::timeout_at(deadline, subscription.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("virtual state condition did not become true; latest state: {state:#?}")
                })
                .expect("the virtual session event stream remains open");
        }
    }

    pub fn subscribe(&self, session_id: &str) -> Result<SessionSubscription, SupervisorError> {
        self.service().subscribe(session_id)
    }

    pub fn events(&self, session_id: &str) -> Vec<EventEnvelope> {
        self.store.events(session_id)
    }

    pub fn recovery_calls(&self, session_id: &str) -> usize {
        self.query.recovery_calls(session_id)
    }

    pub async fn wait_for_slot(&self, session_id: &str, expected: PublicSlotStatus) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if self
                    .sessions()
                    .iter()
                    .any(|slot| slot.session_id == session_id && slot.status == expected)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "session {session_id} did not reach {expected:?}; slots: {:?}",
                self.sessions()
            )
        });
    }

    pub fn sessions(&self) -> Vec<SessionSlotView> {
        self.service().sessions()
    }

    pub fn service_handle(&self) -> Arc<SessionService> {
        self.service().clone()
    }

    pub fn history(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, zork_agent::session::query::QueryError> {
        self.query.after(session_id, cursor, limit)
    }

    pub fn install_tool(
        &self,
        contract: ToolContract,
    ) -> Result<ControlledTool, zork_agent::session::tools::ToolDefinitionError> {
        ControlledTool::register(&self.tools, contract, 64)
    }

    pub fn install_stateful_tool(
        &self,
        contract: ToolContract,
        compatibility: Arc<dyn ToolCompatibility>,
    ) -> Result<ControlledTool, zork_agent::session::tools::ToolDefinitionError> {
        ControlledTool::register_with_compatibility(&self.tools, contract, compatibility, 64)
    }

    pub fn model_releases(&self) -> Vec<crate::model::ModelRelease> {
        self.model.releases()
    }

    pub async fn restart(&mut self) -> Result<(), TestWorldError> {
        let sessions = self
            .query
            .discover_sessions()?
            .into_iter()
            .map(|session| session.session_id)
            .collect::<Vec<_>>();
        if let Some(service) = self.service.take() {
            tokio::time::timeout(std::time::Duration::from_secs(10), service.shutdown())
                .await
                .expect("timed out shutting down the virtual test world");
        }
        self.service = Some(start_service(
            &self.store,
            &self.query,
            &self.tools,
            &self.clock,
            &self.ids,
            self.model_gateway.clone(),
            self.options.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if sessions
                    .iter()
                    .all(|session_id| self.service().contains(session_id))
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| TestWorldError::SessionsNotDiscovered)
    }

    pub async fn shutdown(&mut self) {
        if let Some(service) = self.service.take() {
            tokio::time::timeout(std::time::Duration::from_secs(10), service.shutdown())
                .await
                .expect("timed out shutting down the virtual test world");
        }
    }

    fn service(&self) -> &Arc<SessionService> {
        self.service
            .as_ref()
            .expect("the virtual test world has been shut down")
    }
}

impl Default for TestWorld {
    fn default() -> Self {
        Self::new()
    }
}

fn start_service(
    store: &MemorySessionStore,
    query: &MemorySessionQuery,
    tools: &Arc<ToolRegistry>,
    clock: &ManualClock,
    ids: &DeterministicIds,
    model: Arc<ControlledModelGateway>,
    options: ServiceOptions,
) -> Arc<SessionService> {
    SessionService::start(
        ServiceDependencies {
            store: Arc::new(store.clone()) as Arc<dyn SessionStore>,
            query: Arc::new(query.clone()) as Arc<dyn SessionQuery>,
            model,
            tools: tools.clone(),
            clock: Arc::new(clock.clone()),
            ids: Arc::new(ids.clone()),
        },
        options,
    )
}

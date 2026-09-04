use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router};
use zork_agent::http::{self, AppState};
use zork_agent::session::service::SessionService;
use zork_agent::ProfileStore;

pub struct AgentHttpServer {
    base_url: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
    _root: Option<tempfile::TempDir>,
}

impl AgentHttpServer {
    pub(crate) fn start(
        service: Arc<SessionService>,
        profiles: Arc<ProfileStore>,
        token: Option<String>,
    ) -> std::io::Result<Self> {
        Self::start_inner(service, profiles, token, None)
    }

    pub fn for_service(service: Arc<SessionService>) -> std::io::Result<Self> {
        let root = tempfile::tempdir()?;
        let profiles = Arc::new(ProfileStore::open(root.path().join("data"), true, true));
        Self::start_inner(service, profiles, None, Some(root))
    }

    fn start_inner(
        service: Arc<SessionService>,
        profiles: Arc<ProfileStore>,
        token: Option<String>,
        root: Option<tempfile::TempDir>,
    ) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let router = Router::new()
            .route("/readyz", get(readyz))
            .merge(http::router(AppState {
                service,
                profiles,
                token,
            }));
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await;
        });
        Ok(Self {
            base_url: format!("http://{address}"),
            shutdown: Some(shutdown),
            task,
            _root: root,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

async fn readyz() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "pid": std::process::id(),
        "service": "zork-agent"
    }))
}

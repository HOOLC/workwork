use std::sync::Arc;

use crate::config::RuntimeConfig;
use crate::connections::ConnectionManager;
use crate::db::GatewayDb;
use crate::im_entry::ImEntryGateway;
use crate::jobs::JobSupervisor;
use crate::status_projection::AgentStatusProjector;

/// Admin-plane attachments; None only during early construction.
#[derive(Clone)]
pub struct AdminPlane {
    pub db: Arc<crate::control_db::ControlDb>,
    pub admin_token: Option<String>,
    pub started_at: String,
    pub ui_dir: std::path::PathBuf,
    pub reload_sock: std::path::PathBuf,
}

#[derive(Clone)]
pub struct AppState {
    pub config: RuntimeConfig,
    pub db: Arc<GatewayDb>,
    pub connections: Arc<ConnectionManager>,
    pub entries: ImEntryGateway,
    pub status_projection: AgentStatusProjector,

    pub jobs: Arc<JobSupervisor>,
    pub admin: AdminPlane,
}

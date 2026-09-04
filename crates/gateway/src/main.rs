mod admin;
mod agent;
mod binshim;
mod config;
mod connections;
mod control_db;
mod control_github;
mod db;
mod delivery;
mod http;
mod im_entry;
mod inbound;
mod jobs;
mod slack;
mod socket;
mod state;
mod status_projection;
mod timeline;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{error, info};

use crate::config::RuntimeConfig;
use crate::db::GatewayDb;
use crate::jobs::JobSupervisor;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .init();

    let args = zork_config::parse_process_args()?;
    let mut config = RuntimeConfig::load()?;
    std::fs::create_dir_all(&config.state_dir)?;
    std::fs::create_dir_all(&config.workspaces_root)?;
    std::fs::create_dir_all(&config.repos_root)?;
    std::fs::create_dir_all(&config.jobs_root)?;
    std::fs::create_dir_all(&config.log_dir)?;
    let zork_call = binshim::install(&mut config)?;
    let zork_call_display = zork_call.display().to_string();
    info!(
        addr = %config.bind_addr,
        state = %config.state_dir.display().to_string(),
        zork_call = %zork_call_display,
        "gateway starting"
    );

    let db = Arc::new(GatewayDb::open(&config.state_dir, &config.workspaces_root)?);
    let http_client = reqwest::Client::builder().no_proxy().build()?;
    let connections = Arc::new(
        connections::ConnectionManager::load(config.data_root.clone(), http_client.clone()).await?,
    );
    let entries = im_entry::ImEntryGateway::new(db.clone(), connections.clone());
    let status_projection =
        status_projection::AgentStatusProjector::new(config.clone(), entries.clone())?;
    let jobs = Arc::new(JobSupervisor::new(db.clone(), config.clone()));
    jobs.restore().await?;

    let state = AppState {
        config: config.clone(),
        db: db.clone(),
        connections,
        entries,
        status_projection,
        jobs,
        admin: state::AdminPlane {
            db: Arc::new(control_db::ControlDb::open(&config.state_dir)?),
            admin_token: zork_config::load_config(&config.data_root)
                .ok()
                .and_then(|file| {
                    let token = file.admin.token.trim().to_string();
                    if token.is_empty() {
                        None
                    } else {
                        Some(token)
                    }
                }),
            started_at: config.started_at.clone(),
            ui_dir: zork_config::resolve_ui_dir(&config.data_root, args.ui_dir.as_deref()),
            reload_sock: zork_config::zork_sock_path(&config.data_root),
        },
    };

    for session in db.list_sessions()? {
        if let Some(agent_session_id) = session.id.as_deref() {
            state
                .status_projection
                .ensure(
                    &session.key,
                    agent_session_id,
                    &session.connection_id,
                    &session.channel_id,
                    &session.root_thread_ts,
                )
                .await;
        }
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let http_state = state.clone();
    let runtime_listener = http::bind_listener(config.bind_addr).await?;
    let gateway_bind = match gateway_bind(&config) {
        Some(bind) if bind != config.bind_addr.to_string() => Some(bind),
        _ => None,
    };
    let gateway_listener = match gateway_bind {
        Some(bind) => Some((
            http::bind_listener(zork_config::parse_bind(&bind)?).await?,
            bind,
        )),
        None => None,
    };
    let admin_listener =
        http::bind_listener(zork_config::parse_bind(&admin_bind(&config)?)?).await?;
    zork_config::write_ready_pid(&config.data_root, "zork-gateway")?;
    info!(
        runtime = %config.bind_addr,
        gateway = gateway_listener.as_ref().map(|(_, bind)| bind.clone()).unwrap_or_default(),
        admin = %admin_listener.local_addr()?,
        "gateway listening"
    );

    tokio::select! {
        result = http::serve_listener(runtime_listener, http::router(http_state.clone())) => {
            if let Err(error) = result {
                error!(error = %error, "runtime http exited");
            }
        }
        result = async {
            match gateway_listener {
                Some((listener, _)) => {
                    http::serve_listener(listener, http::gateway_router(http_state.clone())).await
                }
                None => std::future::pending().await,
            }
        } => {
            if let Err(error) = result {
                error!(error = %error, "gateway http exited");
            }
        }
        result = http::serve_listener(admin_listener, admin::router(state.clone())) => {
            if let Err(error) = result {
                error!(error = %error, "admin http exited");
            }
        }
        _ = socket::run_connections(state.clone(), shutdown_rx) => {}
        _ = shutdown_signal() => {
            info!("gateway shutting down");
            let _ = shutdown_tx.send(true);
        }
    }
    zork_config::clear_ready_pid(&config.data_root, "zork-gateway");
    Ok(())
}

/// Optional Slack-facing listener. Test roots can omit it and use only the
/// broker API listener.
fn gateway_bind(config: &RuntimeConfig) -> Option<String> {
    let file = zork_config::load_config(&config.data_root).ok()?;
    let bind = file.bind.gateway.trim().to_string();
    if bind.is_empty() {
        None
    } else {
        Some(bind)
    }
}

/// Admin UI listener, separate from the broker API listener.
fn admin_bind(config: &RuntimeConfig) -> Result<String> {
    let bind = zork_config::load_config(&config.data_root)
        .ok()
        .map(|file| file.bind.control.trim().to_string())
        .unwrap_or_default();
    if bind.is_empty() || bind == config.bind_addr.to_string() {
        Ok("127.0.0.1:3001".to_string())
    } else {
        Ok(bind)
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("listen for SIGTERM");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

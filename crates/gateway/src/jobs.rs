use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{now_rfc3339, RuntimeConfig};
use crate::db::{GatewayDb, JobRow};

const MAX_RUNTIME_MS: u64 = 12 * 60 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct JobEvent {
    pub session_key: String,
    pub job_id: String,
    pub kind: String,
    pub event_kind: String,
    pub summary: String,
}

pub struct JobSupervisor {
    db: Arc<GatewayDb>,
    config: RuntimeConfig,
    running: Mutex<HashMap<String, Child>>,
}

impl JobSupervisor {
    pub fn new(db: Arc<GatewayDb>, config: RuntimeConfig) -> Self {
        Self {
            db,
            config,
            running: Mutex::new(HashMap::new()),
        }
    }

    pub async fn restore(self: &Arc<Self>) -> Result<()> {
        for job in self.db.list_jobs()? {
            if job.restart_on_boot && matches!(job.status.as_str(), "registered" | "running") {
                if let Err(error) = self.spawn_job(&job).await {
                    warn!(job_id = %job.id, error = %error, "restore job failed");
                    self.db
                        .update_job_status(&job.id, "failed", Some(&error.to_string()), None)?;
                } else {
                    tokio::spawn(watch_job_exit(self.clone(), job.id.clone()));
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn register(
        self: &Arc<Self>,
        session_key: &str,
        kind: &str,
        script: &str,
        cwd: Option<&str>,
        shell: Option<&str>,
        restart_on_boot: bool,
    ) -> Result<JobRow> {
        let binding = self
            .db
            .get_binding(session_key)?
            .with_context(|| format!("Unknown session: {session_key}"))?;
        let id = Uuid::new_v4().to_string();
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let now = now_rfc3339();
        let job_dir = self.config.jobs_root.join(&id);
        std::fs::create_dir_all(&job_dir)?;
        let script_path = job_dir.join("run.sh");
        let shell = shell
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("sh");
        std::fs::write(&script_path, normalize_script(script, shell))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
        }
        let cwd = resolve_cwd(binding.workspace_path(), cwd);
        let job = JobRow {
            id: id.clone(),
            token,
            session_key: binding.key().to_owned(),
            kind: kind.trim().to_string(),
            shell: shell.to_string(),
            cwd: cwd.display().to_string(),
            script_path: script_path.display().to_string(),
            restart_on_boot,
            status: "registered".into(),
            created_at: now.clone(),
            updated_at: now,
        };
        self.db.insert_job(&job)?;
        self.spawn_job(&job).await?;
        tokio::spawn(watch_job_exit(self.clone(), id.clone()));
        self.db.get_job(&id)?.context("job missing after register")
    }

    pub async fn cancel(&self, job_id: &str, session_key: Option<&str>) -> Result<JobRow> {
        let job = self.db.get_job(job_id)?.context("job_not_found")?;
        if let Some(session_key) = session_key {
            if job.session_key != session_key {
                anyhow::bail!("job_session_mismatch");
            }
        }
        if !matches!(job.status.as_str(), "registered" | "running") {
            anyhow::bail!("job_not_cancellable:{}", job.status);
        }
        self.stop_child(&job.id).await;
        self.db
            .update_job_status(&job.id, "cancelled", None, None)?;
        self.db
            .get_job(&job.id)?
            .context("job missing after cancel")
    }

    pub async fn notify(
        &self,
        job_id: Option<&str>,
        summary: &str,
        session_key: &str,
    ) -> Result<&'static str> {
        let job = match job_id {
            Some(job_id) => {
                let job = self.db.get_job(job_id)?.context("job_not_found")?;
                if job.session_key != session_key {
                    anyhow::bail!("job_session_mismatch");
                }
                Some(job)
            }
            None => None,
        };
        let binding = self
            .db
            .get_binding(session_key)?
            .context("session_not_found")?;
        crate::delivery::handle_job_event(
            &self.config,
            &self.db,
            JobEvent {
                session_key: binding.key().to_owned(),
                job_id: job_id.unwrap_or("notify").to_string(),
                kind: "notify".into(),
                event_kind: "notify".into(),
                summary: summary.to_string(),
            },
        )
        .await?;
        if let Some(job) = job {
            self.db
                .update_job_status(&job.id, &job.status, None, Some(("notify", summary)))?;
        }
        Ok("delivered")
    }

    pub fn job_json(job: &JobRow) -> Value {
        json!({
            "id": job.id,
            "token": job.token,
            "status": job.status,
            "kind": job.kind,
            "cwd": job.cwd,
            "shell": job.shell,
            "scriptPath": job.script_path,
            "restartOnBoot": job.restart_on_boot,
            "sessionKey": job.session_key,
            "createdAt": job.created_at,
        })
    }

    async fn spawn_job(&self, job: &JobRow) -> Result<()> {
        let mut running = self.running.lock().await;
        if running.contains_key(&job.id) {
            return Ok(());
        }
        let binding = self
            .db
            .get_binding(&job.session_key)?
            .context("job session missing")?;
        let path_value = prepend_path(&self.config.zork_bin_dir);
        let mut command = Command::new(&job.script_path);
        command
            .current_dir(&job.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("PATH", path_value)
            .env("BROKER_JOB_ID", &job.id)
            .env("BROKER_API_BASE", &self.config.broker_http_base_url)
            .env("CHAT_PLATFORM", binding.platform())
            .env("CHAT_CONNECTION_ID", binding.connection_id())
            .env("SESSION_KEY", &job.session_key)
            .env("SESSION_WORKSPACE", &job.cwd)
            .env("REPOS_ROOT", &self.config.repos_root)
            .env("WORKTREE_PATH", &job.cwd)
            .env("BACKGROUND_JOB_KIND", &job.kind);
        if let Some(conversation_id) = binding.conversation_id() {
            command.env("CHAT_CONVERSATION_ID", conversation_id);
        }
        if let Some(root_message_id) = binding.root_message_id() {
            command.env("CHAT_ROOT_MESSAGE_ID", root_message_id);
        }
        if let Some(path) = &self.config.real_gh_path {
            command.env("BROKER_REAL_GH_PATH", path);
        }
        let mut child = command.spawn().context("spawn job")?;
        if let Some(stdout) = child.stdout.take() {
            let job_id = job.id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(job_id = %job_id, "{line}");
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let job_id = job.id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!(job_id = %job_id, "{line}");
                }
            });
        }
        self.db.update_job_status(&job.id, "running", None, None)?;
        let job_id = job.id.clone();
        let session_key = job.session_key.clone();
        let kind = job.kind.clone();
        running.insert(job.id.clone(), child);
        drop(running);
        let timeout_db = self.db.clone();
        let timeout_config = self.config.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(MAX_RUNTIME_MS)).await;
            if let Ok(Some(job)) = timeout_db.get_job(&job_id) {
                if matches!(job.status.as_str(), "running" | "registered") {
                    timeout_db
                        .update_job_status(&job_id, "failed", Some("timeout"), None)
                        .ok();
                    if let Err(error) = crate::delivery::handle_job_event(
                        &timeout_config,
                        &timeout_db,
                        JobEvent {
                            session_key,
                            job_id: job_id.clone(),
                            kind,
                            event_kind: "job_timeout".into(),
                            summary: "Background job timed out after 12h.".into(),
                        },
                    )
                    .await
                    {
                        warn!(job_id = %job_id, error = %error, "job timeout mailbox delivery failed");
                    }
                }
            }
        });
        info!(job_id = %job.id, "background job started");
        Ok(())
    }

    async fn stop_child(&self, job_id: &str) {
        if let Some(mut child) = self.running.lock().await.remove(job_id) {
            let _ = child.kill().await;
        }
    }
}

pub async fn watch_job_exit(supervisor: Arc<JobSupervisor>, job_id: String) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let mut running = supervisor.running.lock().await;
        let Some(child) = running.get_mut(&job_id) else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                running.remove(&job_id);
                drop(running);
                let ok = status.success();
                let summary = if ok {
                    format!("Background job {job_id} finished.")
                } else {
                    format!("Background job {job_id} failed ({status}).")
                };
                let job = supervisor.db.get_job(&job_id).ok().flatten();
                supervisor
                    .db
                    .update_job_status(
                        &job_id,
                        if ok { "succeeded" } else { "failed" },
                        if ok { None } else { Some(&summary) },
                        None,
                    )
                    .ok();
                if let Some(job) = job {
                    if let Err(error) = crate::delivery::handle_job_event(
                        &supervisor.config,
                        &supervisor.db,
                        JobEvent {
                            session_key: job.session_key,
                            job_id: job.id,
                            kind: job.kind,
                            event_kind: if ok {
                                "job_exit".into()
                            } else {
                                "job_failed".into()
                            },
                            summary,
                        },
                    )
                    .await
                    {
                        warn!(job_id = %job_id, error = %error, "job completion mailbox delivery failed");
                    }
                }
                return;
            }
            Ok(None) => {}
            Err(error) => {
                warn!(job_id = %job_id, error = %error, "job wait failed");
                return;
            }
        }
    }
}

fn normalize_script(script: &str, shell: &str) -> String {
    if script.starts_with("#!") {
        return if script.ends_with('\n') {
            script.to_string()
        } else {
            format!("{script}\n")
        };
    }
    format!("#!/usr/bin/env {shell}\nset -euo pipefail\n{script}\n")
}

fn resolve_cwd(workspace: &str, cwd: Option<&str>) -> PathBuf {
    match cwd.map(str::trim).filter(|value| !value.is_empty()) {
        Some(path) if Path::new(path).is_absolute() => PathBuf::from(path),
        Some(path) => Path::new(workspace).join(path),
        None => PathBuf::from(workspace),
    }
}

fn prepend_path(bin_dir: &Path) -> std::ffi::OsString {
    let mut entries = vec![bin_dir.to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(entries).unwrap_or_else(|_| bin_dir.as_os_str().to_os_string())
}

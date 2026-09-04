use std::fs;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

fn usage() -> &'static str {
    "\
Usage:
  zork start [--data DIR] [--listen HOST] [--agent-token TOKEN]
  zork update [--data DIR]

start   runs zork-gateway (Slack + mailbox delivery + admin) and zork-agent
update  drains and restarts zork-gateway and zork-agent
"
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .init();

    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let command = if argv.first().is_some_and(|arg| !arg.starts_with('-')) {
        argv.remove(0)
    } else {
        "start".into()
    };
    if argv.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{}", usage());
        return Ok(());
    }
    match command.as_str() {
        "update" => send_reload(argv).await,
        "start" => run_supervisor(argv).await,
        other => anyhow::bail!("unknown command {other}\n{}", usage().trim()),
    }
}

async fn send_reload(argv: Vec<String>) -> Result<()> {
    let args = zork_config::parse_process_args_from(argv)?;
    let sock = zork_config::zork_sock_path(&args.data_root);
    let mut stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("zork is not running ({})", sock.display()))?;
    stream.write_all(b"reload\n").await?;
    stream.flush().await?;
    let mut buf = String::new();
    BufReader::new(stream).read_line(&mut buf).await?;
    let reply = buf.trim();
    if reply != "ok" {
        anyhow::bail!("reload failed: {reply}");
    }
    println!("updated");
    Ok(())
}

async fn run_supervisor(argv: Vec<String>) -> Result<()> {
    let args = zork_config::parse_process_args_from(argv)?;
    let mut file = zork_config::ensure_layout(&args.data_root)?;
    if let Some(host) = &args.listen_host {
        zork_config::apply_listen(&mut file, host);
    }
    let missing = zork_config::missing_commands(&["git", "gh", "rg"]);
    if !missing.is_empty() {
        eprintln!(
            "missing on PATH (needed for agent turns): {}",
            missing.join(", ")
        );
    }

    let ui_dir = zork_config::resolve_ui_dir(&args.data_root, args.ui_dir.as_deref());
    let admin = zork_config::loopback_base_url(&file.bind.control);
    println!("admin  {admin}");
    println!("data   {}", args.data_root.display());
    if !zork_config::has_configured_im_connection(&file) {
        println!("setup  open Control and add an IM connection");
    }

    let pid_path = zork_config::zork_pid_path(&args.data_root);
    fs::write(&pid_path, format!("{}\n", std::process::id()))?;
    let sock_path = zork_config::zork_sock_path(&args.data_root);
    let _ = fs::remove_file(&sock_path);

    let mut gateway = spawn_named("zork-gateway", &args, Some(&ui_dir))?;
    let mut agent = spawn_named("zork-agent", &args, None)?;

    let (reload_tx, mut reload_rx) = mpsc::channel::<OneshotAck>(1);
    let sock_for_listen = sock_path.clone();
    let tx = reload_tx.clone();
    tokio::spawn(async move {
        if let Err(error) = listen_reload(&sock_for_listen, tx).await {
            warn!(error = %error, "reload socket ended");
        }
    });

    info!("zork started");
    let mut shutting_down = false;
    loop {
        tokio::select! {
            status = gateway.wait() => {
                log_exit("gateway", status);
                if shutting_down {
                    break;
                }
                gateway = spawn_named("zork-gateway", &args, Some(&ui_dir))?;
            }
            status = agent.wait() => {
                log_exit("agent", status);
                if shutting_down {
                    break;
                }
                agent = spawn_named("zork-agent", &args, None)?;
            }
            req = reload_rx.recv() => {
                let Some(ack) = req else { break };
                info!("zork update starting");
                let result = controlled_restart(
                    &args,
                    &ui_dir,
                    &mut gateway,
                    &mut agent,
                )
                .await;
                match result {
                    Ok(()) => {
                        info!("zork update complete");
                        let _ = ack.send("ok".into());
                    }
                    Err(error) => {
                        error!(error = %error, "zork update failed");
                        let _ = ack.send(format!("error: {error}"));
                    }
                }
            }
            _ = shutdown_signal() => {
                info!("zork shutting down");
                shutting_down = true;
                terminate_child(&mut gateway);
                terminate_child(&mut agent);
                let _ = tokio::time::timeout(Duration::from_secs(8), async {
                    tokio::join!(gateway.wait(), agent.wait())
                })
                .await;
                let _ = shutting_down;
                break;
            }
            _ = sighup() => {
                info!("zork update starting");
                if let Err(error) = controlled_restart(
                    &args,
                    &ui_dir,
                    &mut gateway,
                    &mut agent,
                )
                .await
                {
                    error!(error = %error, "zork update failed");
                } else {
                    info!("zork update complete");
                }
            }
        }
    }
    let _ = fs::remove_file(&sock_path);
    let _ = fs::remove_file(&pid_path);
    Ok(())
}

type OneshotAck = tokio::sync::oneshot::Sender<String>;

async fn listen_reload(sock: &std::path::Path, tx: mpsc::Sender<OneshotAck>) -> Result<()> {
    let listener = UnixListener::bind(sock).with_context(|| format!("bind {}", sock.display()))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut line = String::new();
        // A client that disconnects mid-request must not end the listener.
        if BufReader::new(reader).read_line(&mut line).await.is_err() {
            continue;
        }
        if line.trim() != "reload" {
            let _ = writer.write_all(b"error: unknown command\n").await;
            continue;
        }
        let (ack, rx) = tokio::sync::oneshot::channel();
        if tx.send(ack).await.is_err() {
            break;
        }
        let reply = rx.await.unwrap_or_else(|_| "error: cancelled".into());
        let _ = writer.write_all(format!("{reply}\n").as_bytes()).await;
    }
    Ok(())
}

async fn controlled_restart(
    args: &zork_config::ProcessArgs,
    ui_dir: &std::path::Path,
    gateway: &mut Child,
    agent: &mut Child,
) -> Result<()> {
    restart_named("zork-gateway", gateway, args, Some(ui_dir)).await?;
    restart_named("zork-agent", agent, args, None).await?;
    Ok(())
}

async fn restart_named(
    name: &str,
    current: &mut Child,
    args: &zork_config::ProcessArgs,
    ui_dir: Option<&std::path::Path>,
) -> Result<()> {
    info!(name, "draining current process");
    terminate_child(current);
    if tokio::time::timeout(Duration::from_secs(8), current.wait())
        .await
        .is_err()
    {
        let _ = current.start_kill();
        let _ = current.wait().await;
    }
    info!(name, "starting updated process");
    let mut next = spawn_named(name, args, ui_dir)?;
    let pid = next.id().context("updated process pid")?;
    if let Err(error) = wait_ready(name, &args.data_root, pid).await {
        terminate_child(&mut next);
        let _ = next.wait().await;
        return Err(error).with_context(|| format!("{name} readyz"));
    }
    *current = next;
    info!(name, pid, "restarted");
    Ok(())
}

async fn wait_ready(name: &str, data_root: &std::path::Path, pid: u32) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut last = "not ready".to_string();
    while std::time::Instant::now() < deadline {
        match zork_config::read_ready_pid(data_root, name) {
            Ok(Some(found)) if found == pid => return Ok(()),
            Ok(Some(found)) => last = format!("pid file {found}, want {pid}"),
            Ok(None) => last = "pid file missing".into(),
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("{last}")
}

fn log_exit(name: &str, status: std::result::Result<std::process::ExitStatus, std::io::Error>) {
    match status {
        Ok(status) if status.success() => info!(name, "process exited"),
        Ok(status) => error!(name, %status, "process exited"),
        Err(error) => error!(name, error = %error, "wait failed"),
    }
}

fn spawn_named(
    name: &str,
    args: &zork_config::ProcessArgs,
    ui_dir: Option<&std::path::Path>,
) -> Result<Child> {
    let bin = zork_config::find_bin(name, &args.data_root)
        .with_context(|| format!("{name} not found next to zork or on PATH"))?;
    let mut command = Command::new(&bin);
    command
        .arg("--data")
        .arg(&args.data_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some(host) = &args.listen_host {
        command.arg("--listen").arg(host);
    }
    if let Some(token) = &args.agent_token {
        command.arg("--agent-token").arg(token);
    }
    if args.fake_agent {
        command.arg("--fake-agent");
    }
    if let Some(ui_dir) = ui_dir {
        command.arg("--ui-dir").arg(ui_dir);
    }
    command
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))
}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
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

async fn sighup() {
    #[cfg(unix)]
    {
        let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("listen for SIGHUP");
        hangup.recv().await;
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<()>().await;
    }
}

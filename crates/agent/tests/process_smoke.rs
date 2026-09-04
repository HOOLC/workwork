#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
// Contract: docs/zork-agent-architecture.md [HTTP-01]
async fn binary_serves_readyz_and_shuts_down_cleanly() {
    let root = tempfile::tempdir().unwrap();
    let data_root = root.path().join("data");
    std::fs::create_dir_all(&data_root).unwrap();
    let port = free_port();
    std::fs::write(
        data_root.join("config.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "bind": {"agent": format!("127.0.0.1:{port}")}
        }))
        .unwrap(),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_zork-agent"))
        .arg("--data")
        .arg(&data_root)
        .arg("--fake-agent")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let child_id = child.id().unwrap();
    let base_url = format!("http://127.0.0.1:{port}");
    let ready = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(response) = reqwest::get(format!("{base_url}/readyz")).await {
                if response.status().is_success() {
                    break response;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("zork-agent became ready");
    assert_eq!(
        ready.json::<serde_json::Value>().await.unwrap()["service"],
        "zork-agent"
    );
    assert_eq!(
        zork_config::read_ready_pid(&data_root, "zork-agent").unwrap(),
        Some(child_id)
    );

    let signal_result = unsafe { libc::kill(child_id as i32, libc::SIGTERM) };
    assert_eq!(signal_result, 0);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("zork-agent handled SIGTERM")
        .unwrap();
    assert!(status.success());
    assert_eq!(
        zork_config::read_ready_pid(&data_root, "zork-agent").unwrap(),
        None
    );
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

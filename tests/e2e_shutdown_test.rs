//! The one real signal: a child process of the built binary gets SIGTERM
//! and must close its WebSocket clients, drain and exit 0. Out of process,
//! because a signal raised inside the test binary would stop every test.

mod common;

use std::process::Stdio;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::protocol::Message;

const GOING_AWAY: u16 = 1001;
/// preStop + endpoint_drain + WS_CLOSE_GRACE + shutdown_grace + 10 at the
/// defaults: what Kubernetes allows before SIGKILL.
const TERMINATION_GRACE: Duration = Duration::from_secs(47);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test]
async fn sigterm_drains_a_child_process() {
    let Some(kill) = common::client_bin("KILL_BIN") else { return };
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let tmp = tempfile::TempDir::new().unwrap();
    let port = free_port();
    let config = tmp.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nbind = \"127.0.0.1:{port}\"\nbase_url = \"http://127.0.0.1:{port}\"\nstorage_path = \"{}\"\n\
             [database]\nurl = \"sqlite:{}?mode=rwc\"\n",
            tmp.path().join("storage").display(),
            tmp.path().join("db/opencargo.db").display()
        ),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_opencargo"))
        .args(["--config", config.to_str().unwrap(), "serve"])
        .current_dir(tmp.path())
        .env_remove("OPENCARGO_CONFIG")
        .env("RUST_LOG", "off")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let base_url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !reqwest::get(format!("{base_url}/health/live")).await.is_ok_and(|r| r.status().is_success()) {
        assert!(Instant::now() < deadline, "the child never became live");
        tokio::task::yield_now().await;
    }

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/v1/events/ws"))
        .await
        .unwrap();
    ws.send(Message::text(json!({"type": "auth"}).to_string())).await.unwrap();
    ws.send(Message::text(json!({"type": "ping"}).to_string())).await.unwrap();

    let pid = child.id().unwrap().to_string();
    let signalled = std::process::Command::new(kill).args(["-TERM", &pid]).status().unwrap();
    assert!(signalled.success());

    let mut closed = None;
    while let Ok(Some(frame)) = tokio::time::timeout(TERMINATION_GRACE, ws.next()).await {
        if let Ok(Message::Close(frame)) = frame {
            closed = frame.map(|f| u16::from(f.code));
            break;
        }
    }
    assert_eq!(closed, Some(GOING_AWAY), "a close frame, not a reset");
    let status = tokio::time::timeout(TERMINATION_GRACE, child.wait())
        .await
        .expect("the child exits inside the termination grace")
        .unwrap();
    assert!(status.success(), "{status}");
}

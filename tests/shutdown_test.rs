//! The drain, driven through `TestServer::drain`: the same library call the
//! signal handler makes, never a signal raised inside the test binary.

mod common;

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use sha2::Digest;
use tokio::net::TcpStream;
use tokio_tungstenite::{tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream};

use common::{
    build_npm_publish_body, build_tarball, hosted, push_blob, spawn_server, SpawnOpts, TestServer,
    STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};
use opencargo::server::shutdown::WS_CLOSE_GRACE;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const DEADLINE: Duration = Duration::from_secs(10);
const GOING_AWAY: u16 = 1001;

fn opts(endpoint_drain: &str) -> SpawnOpts {
    SpawnOpts {
        repositories: vec![
            hosted("npm-pub", RepositoryFormat::Npm, Visibility::Public),
            hosted("oci-pub", RepositoryFormat::Oci, Visibility::Public),
        ],
        endpoint_drain: endpoint_drain.to_string(),
        ..Default::default()
    }
}

async fn ready_status(base_url: &str) -> Option<StatusCode> {
    reqwest::get(format!("{base_url}/health/ready")).await.ok().map(|r| r.status())
}

async fn ws_hello(base_url: &str) -> Ws {
    let url = format!("{}/api/v1/events/ws", base_url.replacen("http", "ws", 1));
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("ws connect");
    ws.send(Message::text(json!({"type": "auth"}).to_string())).await.unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "hello");
    ws
}

/// A pong proves the connection reached its event loop, where it is
/// subscribed to the bus.
async fn ping(ws: &mut Ws) {
    ws.send(Message::text(json!({"type": "ping"}).to_string())).await.unwrap();
    assert_eq!(next_json(ws).await["type"], "pong");
}

async fn next_frame(ws: &mut Ws) -> Message {
    loop {
        let frame = tokio::time::timeout(DEADLINE, ws.next())
            .await
            .expect("a frame before the deadline")
            .expect("a frame, not the end of the stream")
            .expect("no protocol error");
        if !matches!(frame, Message::Ping(_) | Message::Pong(_)) {
            return frame;
        }
    }
}

async fn next_json(ws: &mut Ws) -> Value {
    match next_frame(ws).await {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

async fn expect_going_away(ws: &mut Ws) {
    match next_frame(ws).await {
        Message::Close(Some(frame)) => assert_eq!(u16::from(frame.code), GOING_AWAY),
        other => panic!("expected a close frame, got {other:?}"),
    }
}

async fn publish(base_url: &str, name: &str) {
    let tarball = build_tarball(&format!(r#"{{"name":"{name}","version":"1.0.0"}}"#));
    let body = build_npm_publish_body(name, "1.0.0", "d", &tarball);
    let resp = reqwest::Client::new()
        .put(format!("{base_url}/npm-pub/{name}"))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Waits, bounded by `DEADLINE`, until `/health/ready` answers `want`.
async fn ready_becomes(base_url: &str, want: StatusCode) {
    let deadline = Instant::now() + DEADLINE;
    while ready_status(base_url).await != Some(want) {
        assert!(Instant::now() < deadline, "/health/ready never answered {want}");
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn ready_is_503_draining_during_the_endpoint_window() {
    let mut server = spawn_server(opts("2s")).await;
    let base_url = server.base_url.clone();
    let observe = async {
        ready_becomes(&base_url, StatusCode::SERVICE_UNAVAILABLE).await;
        let body: Value = reqwest::get(format!("{base_url}/health/ready")).await.unwrap().json().await.unwrap();
        assert_eq!(body["status"], "draining");
        let live = reqwest::get(format!("{base_url}/health/live")).await.unwrap();
        assert_eq!(live.status(), StatusCode::OK, "still serving inside the window");
    };
    tokio::join!(server.drain(), observe);
    assert!(ready_status(&base_url).await.is_none(), "after the drain the listener is gone");
}

#[tokio::test]
async fn the_default_drain_is_zero_and_shutdown_starts_at_once() {
    let mut server = spawn_server(opts("0s")).await;
    let started = Instant::now();
    server.drain().await;
    assert!(started.elapsed() < WS_CLOSE_GRACE, "nothing slept before the HTTP drain");
}

#[tokio::test]
async fn in_flight_streamed_download_completes_after_drain() {
    let mut server = spawn_server(opts("0s")).await;
    let client = reqwest::Client::new();
    let blob: Vec<u8> = (0..20 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let digest = push_blob(&client, &server.base_url, "oci-pub/big", &blob).await;
    let resp = client
        .get(format!("{}/v2/oci-pub/big/blobs/{digest}", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let read = async {
        let mut resp = resp;
        let mut hasher = sha2::Sha256::new();
        while let Some(chunk) = resp.chunk().await.expect("the body completes") {
            hasher.update(&chunk);
        }
        hasher.finalize()
    };
    let ((), got) = tokio::join!(server.drain(), read);
    assert_eq!(got.as_slice(), sha2::Sha256::digest(&blob).as_slice());
}

#[tokio::test]
async fn request_arriving_after_the_grace_starts_is_refused_not_dropped() {
    let mut server = spawn_server(opts("0s")).await;
    let base_url = server.base_url.clone();
    server.drain().await;
    let err = reqwest::get(format!("{base_url}/health/live")).await.unwrap_err();
    assert!(err.is_connect(), "{err}");
}

#[tokio::test]
async fn open_websocket_receives_a_close_frame_before_graceful_shutdown() {
    let mut server = spawn_server(opts("0s")).await;
    let mut ws = ws_hello(&server.base_url).await;
    ping(&mut ws).await;
    server.drain().await;
    let already = tokio::time::timeout(Duration::ZERO, ws.next()).await;
    match already {
        Ok(Some(Ok(Message::Close(Some(frame))))) => assert_eq!(u16::from(frame.code), GOING_AWAY),
        other => panic!("the close frame was on the wire when the drain returned: {other:?}"),
    }
}

#[tokio::test]
async fn the_live_feed_survives_the_endpoint_window() {
    let mut server = spawn_server(opts("2s")).await;
    let mut ws = ws_hello(&server.base_url).await;
    ping(&mut ws).await;
    let base_url = server.base_url.clone();
    let inside = async {
        ready_becomes(&base_url, StatusCode::SERVICE_UNAVAILABLE).await;
        publish(&base_url, "during-drain").await;
        let event = next_json(&mut ws).await;
        assert_eq!(event["type"], "package.published", "{event}");
        expect_going_away(&mut ws).await;
    };
    tokio::join!(server.drain(), inside);
}

/// An upgraded socket leaves the connection count at its upgrade, which is
/// why the drain tracks it itself.
#[tokio::test]
async fn ws_does_not_appear_in_connection_count() {
    let server = spawn_server(opts("0s")).await;
    let deadline = Instant::now() + DEADLINE;
    while server.srv.connection_count() != 0 {
        assert!(Instant::now() < deadline, "the harness's own connections never closed");
        tokio::task::yield_now().await;
    }
    let mut ws = ws_hello(&server.base_url).await;
    ping(&mut ws).await;
    assert_eq!(server.srv.connection_count(), 0);
}

#[tokio::test]
async fn drain_releases_the_lease() {
    let mut server: TestServer = spawn_server(SpawnOpts { lease: true, ..opts("0s") }).await;
    server.drain().await;
    let mut config = common::config_of(&server);
    config.server.lease = true;
    let started = Instant::now();
    let next = common::start(&mut config).await.expect("the lease was given back");
    assert!(next.lease.is_some());
    assert!(started.elapsed() < Duration::from_secs(3), "taken at once, not after going stale");
}

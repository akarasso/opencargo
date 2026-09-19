//! The one drain sequence, called by the signal handler and by the test
//! harness alike, so no caller can reorder it.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};
use tokio_util::task::task_tracker::TrackedFuture;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};

/// How long WebSocket clients get to receive their close frame: a flush,
/// not a request drain. Mirrored by `shutdown.wsCloseGraceSeconds` in the
/// chart so the termination grace period covers it.
pub const WS_CLOSE_GRACE: Duration = Duration::from_secs(5);

pub type ServerHandle = axum_server::Handle<SocketAddr>;

/// Two bits, not one: readiness flips at the head of the drain, while the
/// WebSocket cancellation comes at its end, once the endpoint is out of
/// rotation.
#[derive(Clone, Default)]
pub struct Shutdown {
    draining: Arc<AtomicBool>,
    token: CancellationToken,
    ws: TaskTracker,
}

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    /// `/health/ready` answers 503 from here on; everything else still serves.
    pub fn start_draining(&self) {
        self.draining.store(true, Ordering::Release);
    }

    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.token.cancelled()
    }

    /// An upgraded socket leaves hyper's connection count at the upgrade, so
    /// the drain waits for it here instead.
    pub fn track_ws<F: Future<Output = ()>>(&self, f: F) -> TrackedFuture<F> {
        self.ws.track_future(f)
    }

    /// Cancel every WebSocket client and wait, bounded, for their close
    /// frames to reach the wire.
    pub async fn close_websockets(&self) {
        self.token.cancel();
        self.ws.close();
        if tokio::time::timeout(WS_CLOSE_GRACE, self.ws.wait()).await.is_err() {
            warn!(open = self.ws.len(), "websocket clients still open after the close grace");
        }
    }

    /// Readiness 503, `endpoint_drain` still serving, the WebSocket close
    /// frames, then the HTTP drain bounded by `grace`. The caller then awaits
    /// its serve task and releases the lease.
    pub async fn drain(&self, srv: &ServerHandle, endpoint_drain: Duration, grace: Duration) {
        self.start_draining();
        info!(?endpoint_drain, ?grace, "draining");
        tokio::time::sleep(endpoint_drain).await;
        self.close_websockets().await;
        srv.graceful_shutdown(Some(grace));
    }
}

/// SIGTERM or SIGINT, whichever comes first.
pub async fn signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("a SIGTERM handler can be installed");
        tokio::select! {
            _ = interrupt => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = interrupt.await;
    }
}

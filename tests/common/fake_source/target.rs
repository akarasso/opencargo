//! A target that answers like opencargo on the few routes the npm sink
//! uses, and refuses the first publishes with a chosen status.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use super::{serve, Log};

pub struct Inner {
    pub refuse: StatusCode,
    pub refusals: usize,
    pub publishes: AtomicUsize,
    pub accepted: AtomicUsize,
    pub log: Log,
}

#[derive(Clone)]
pub struct FakeTarget {
    pub url: String,
    pub inner: Arc<Inner>,
}

async fn handle(State(s): State<Arc<Inner>>, req: Request) -> Response {
    s.log.record(req.method().as_str(), req.uri(), req.headers());
    let path = req.uri().path().to_string();
    if path == "/api/v1/me/permissions" {
        return Json(json!({ "username": "importer", "role": "publisher", "permissions": [{
            "repository": "npm", "type": "hosted", "format": "npm", "visibility": "private",
            "can_read": true, "can_write": true, "can_delete": false, "can_admin": false, "source": "role" }] }))
        .into_response();
    }
    match req.method().as_str() {
        "GET" => StatusCode::NOT_FOUND.into_response(),
        "PUT" if path.contains("/dist-tags/") => Json(json!({ "ok": true })).into_response(),
        "PUT" => {
            let _ = axum::body::to_bytes(req.into_body(), 64 << 20).await;
            let n = s.publishes.fetch_add(1, Ordering::SeqCst);
            if n < s.refusals {
                return (s.refuse, Json(json!({ "error": "vulnerability scan unavailable: osv.dev down" }))).into_response();
            }
            s.accepted.fetch_add(1, Ordering::SeqCst);
            Json(json!({ "ok": true })).into_response()
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

impl FakeTarget {
    pub async fn start(refuse: StatusCode, refusals: usize) -> Self {
        let inner = Arc::new(Inner {
            refuse,
            refusals,
            publishes: AtomicUsize::new(0),
            accepted: AtomicUsize::new(0),
            log: Log::default(),
        });
        let url = serve(axum::Router::new().fallback(handle).with_state(inner.clone())).await;
        Self { url, inner }
    }

    pub fn accepted(&self) -> usize {
        self.inner.accepted.load(Ordering::SeqCst)
    }

    pub fn publishes(&self) -> usize {
        self.inner.publishes.load(Ordering::SeqCst)
    }
}

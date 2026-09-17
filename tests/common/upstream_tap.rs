use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CONNECTION, CONTENT_LENGTH, HOST, TRANSFER_ENCODING};
use axum::http::{HeaderName, Method, Response, StatusCode};
use axum::Router;

/// A recording reverse proxy: every request lands in `hits` before it is
/// forwarded; while `fail` is set, the tap answers 503 without forwarding.
pub struct Tap {
    pub base_url: String,
    pub hits: Arc<Mutex<Vec<(Method, String)>>>,
    pub fail: Arc<AtomicBool>,
}

impl Tap {
    /// Number of recorded requests whose path (with query) equals `path`.
    pub fn count(&self, path: &str) -> usize {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p == path)
            .count()
    }
}

#[derive(Clone)]
struct Relay {
    target: String,
    http: reqwest::Client,
    hits: Arc<Mutex<Vec<(Method, String)>>>,
    fail: Arc<AtomicBool>,
}

pub async fn start(target: &str) -> Tap {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind the tap");
    let addr = listener.local_addr().expect("no local addr");
    let relay = Relay {
        target: target.trim_end_matches('/').to_string(),
        http: reqwest::Client::new(),
        hits: Arc::new(Mutex::new(Vec::new())),
        fail: Arc::new(AtomicBool::new(false)),
    };
    let tap = Tap {
        base_url: format!("http://{addr}"),
        hits: relay.hits.clone(),
        fail: relay.fail.clone(),
    };
    let app = Router::new().fallback(forward).with_state(relay);
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tap
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    *name == CONNECTION || *name == TRANSFER_ENCODING || *name == CONTENT_LENGTH
}

async fn forward(State(relay): State<Relay>, req: Request) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "/".to_string());
    relay
        .hits
        .lock()
        .unwrap()
        .push((parts.method.clone(), path.clone()));
    if relay.fail.load(Ordering::SeqCst) {
        return status_only(StatusCode::SERVICE_UNAVAILABLE);
    }

    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let mut upstream = relay
        .http
        .request(parts.method, format!("{}{}", relay.target, path));
    for (name, value) in &parts.headers {
        if *name != HOST && !is_hop_by_hop(name) {
            upstream = upstream.header(name, value);
        }
    }
    let resp = match upstream.body(body).send().await {
        Ok(resp) => resp,
        Err(_) => return status_only(StatusCode::BAD_GATEWAY),
    };

    let mut out = Response::builder().status(resp.status());
    for (name, value) in resp.headers() {
        if !is_hop_by_hop(name) {
            out = out.header(name, value);
        }
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    out.body(Body::from(bytes)).expect("valid relayed response")
}

fn status_only(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("valid status response")
}

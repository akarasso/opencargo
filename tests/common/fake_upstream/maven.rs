use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Response, StatusCode};
use axum::Router;

/// One answer the fake repository gives for a path.
#[derive(Clone)]
pub struct Entry {
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

#[derive(Clone, Default)]
struct Shared {
    files: Arc<Mutex<HashMap<String, Entry>>>,
    hits: Arc<Mutex<Vec<String>>>,
    down: Arc<AtomicBool>,
}

/// A Maven repository whose every answer the test sets: bodies, their
/// headers, and whether it is reachable at all.
pub struct FakeMaven {
    pub base_url: String,
    shared: Shared,
}

impl FakeMaven {
    pub fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.put_with(path, body, Vec::new());
    }

    pub fn put_with(&self, path: &str, body: impl Into<Vec<u8>>, headers: Vec<(&'static str, String)>) {
        self.shared.files.lock().unwrap().insert(
            path.trim_start_matches('/').to_string(),
            Entry {
                body: body.into(),
                headers,
            },
        );
    }

    pub fn remove(&self, path: &str) {
        self.shared.files.lock().unwrap().remove(path);
    }

    /// While down, every request answers 503.
    pub fn set_down(&self, down: bool) {
        self.shared.down.store(down, Ordering::SeqCst);
    }

    pub fn count(&self, path: &str) -> usize {
        self.shared
            .hits
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.as_str() == path)
            .count()
    }
}

pub async fn start() -> FakeMaven {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind the fake Maven repository");
    let addr = listener.local_addr().expect("no local addr");
    let shared = Shared::default();
    let app = Router::new().fallback(serve).with_state(shared.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    FakeMaven {
        base_url: format!("http://{addr}"),
        shared,
    }
}

async fn serve(State(shared): State<Shared>, req: Request) -> Response<Body> {
    let path = req.uri().path().trim_start_matches('/').to_string();
    shared.hits.lock().unwrap().push(path.clone());
    if shared.down.load(Ordering::SeqCst) {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("down"))
            .unwrap();
    }
    let entry = shared.files.lock().unwrap().get(&path).cloned();
    match entry {
        Some(entry) => {
            let mut builder = Response::builder().status(StatusCode::OK);
            for (name, value) in entry.headers {
                builder = builder.header(name, value);
            }
            builder.body(Body::from(entry.body)).unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap(),
    }
}

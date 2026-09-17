use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Response, StatusCode};
use axum::Router;
use serde_json::Value;

/// One request the fake saw: its path and the `If-None-Match` it carried.
#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub if_none_match: Option<String>,
}

/// A fake npm registry holding one packument under a fixed ETag: a request
/// carrying that ETag in `If-None-Match` is answered 304, anything else 404.
pub struct FakeNpm {
    pub base_url: String,
    pub hits: Arc<Mutex<Vec<Hit>>>,
}

impl FakeNpm {
    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }
}

#[derive(Clone)]
struct Registry {
    path: String,
    body: Arc<String>,
    etag: String,
    hits: Arc<Mutex<Vec<Hit>>>,
}

pub async fn start(packument: Value, etag: &str) -> FakeNpm {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind the fake npm registry");
    let addr = listener.local_addr().expect("no local addr");
    let name = packument["name"].as_str().expect("packument name");
    let registry = Registry {
        path: format!("/{name}"),
        body: Arc::new(packument.to_string()),
        etag: etag.to_string(),
        hits: Arc::new(Mutex::new(Vec::new())),
    };
    let fake = FakeNpm {
        base_url: format!("http://{addr}"),
        hits: registry.hits.clone(),
    };
    let app = Router::new().fallback(serve).with_state(registry);
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    fake
}

async fn serve(State(registry): State<Registry>, req: Request) -> Response<Body> {
    let if_none_match = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let path = req.uri().path().to_string();
    registry.hits.lock().unwrap().push(Hit {
        path: path.clone(),
        if_none_match: if_none_match.clone(),
    });
    let status = if path != registry.path {
        StatusCode::NOT_FOUND
    } else if if_none_match.as_deref() == Some(registry.etag.as_str()) {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    let body = if status == StatusCode::OK {
        Body::from(registry.body.as_str().to_string())
    } else {
        Body::empty()
    };
    Response::builder()
        .status(status)
        .header(header::ETAG, &registry.etag)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .expect("valid fake response")
}

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Response, StatusCode};
use axum::Router;

/// A GOPROXY that knows one module, `example.com/gone`, with `v1.0.0`
/// intact and `v1.1.0` withdrawn (410 on every per-version file).
pub struct FakeGoProxy {
    pub base_url: String,
    pub hits: Arc<Mutex<Vec<String>>>,
}

pub const MODULE: &str = "example.com/gone";

impl FakeGoProxy {
    pub fn count(&self, path: &str) -> usize {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p == &path)
            .count()
    }
}

pub async fn start() -> FakeGoProxy {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind the fake GOPROXY");
    let addr = listener.local_addr().expect("no local addr");
    let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new().fallback(serve).with_state(hits.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    FakeGoProxy {
        base_url: format!("http://{addr}"),
        hits,
    }
}

async fn serve(State(hits): State<Arc<Mutex<Vec<String>>>>, req: Request) -> Response<Body> {
    let path = req.uri().path().to_string();
    hits.lock().unwrap().push(path.clone());
    let Some(rest) = path.strip_prefix(&format!("/{MODULE}/")) else {
        return reply(StatusCode::NOT_FOUND, "text/plain", "unknown module");
    };
    match rest {
        "@v/list" => reply(StatusCode::OK, "text/plain", "v1.0.0\n"),
        "@latest" | "@v/v1.0.0.info" => reply(
            StatusCode::OK,
            "application/json",
            r#"{"Version":"v1.0.0","Time":"2026-01-01T00:00:00Z"}"#,
        ),
        "@v/v1.0.0.mod" => reply(StatusCode::OK, "text/plain", "module example.com/gone\n"),
        "@v/v1.1.0.info" | "@v/v1.1.0.mod" | "@v/v1.1.0.zip" => {
            reply(StatusCode::GONE, "text/plain", "withdrawn")
        }
        _ => reply(StatusCode::NOT_FOUND, "text/plain", "not found"),
    }
}

fn reply(status: StatusCode, content_type: &'static str, body: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("valid fake response")
}

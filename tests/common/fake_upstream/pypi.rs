use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Response, StatusCode};
use axum::Router;

/// What one path answers.
#[derive(Clone)]
pub enum Answer {
    Body(&'static str, Vec<u8>),
    Status(u16),
    Redirect(String),
}

#[derive(Default)]
pub struct Script {
    pub paths: HashMap<String, Answer>,
    /// Every request: path and the `Authorization` it carried.
    pub hits: Vec<(String, Option<String>)>,
}

/// A scripted index or file host: every path answers what the test set.
#[derive(Clone)]
pub struct FakePypi {
    pub base_url: String,
    pub script: Arc<Mutex<Script>>,
}

impl FakePypi {
    pub fn set(&self, path: &str, answer: Answer) {
        self.script.lock().unwrap().paths.insert(path.to_string(), answer);
    }

    pub fn hits(&self, path: &str) -> Vec<Option<String>> {
        self.script
            .lock()
            .unwrap()
            .hits
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, a)| a.clone())
            .collect()
    }
}

pub async fn start() -> FakePypi {
    start_on("127.0.0.1").await
}

/// Bound to `host`, so two fakes can be two hosts (`127.0.0.1`, `127.0.0.2`).
pub async fn start_on(host: &str) -> FakePypi {
    let listener = tokio::net::TcpListener::bind(format!("{host}:0"))
        .await
        .expect("failed to bind the fake PyPI");
    let addr = listener.local_addr().expect("no local addr");
    let script = Arc::new(Mutex::new(Script::default()));
    let app = Router::new().fallback(serve).with_state(script.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    FakePypi {
        base_url: format!("http://{addr}"),
        script,
    }
}

async fn serve(State(script): State<Arc<Mutex<Script>>>, req: Request) -> Response<Body> {
    let path = req.uri().path().to_string();
    let auth = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let answer = {
        let mut script = script.lock().unwrap();
        script.hits.push((path.clone(), auth));
        script.paths.get(&path).cloned()
    };
    let builder = Response::builder();
    match answer {
        Some(Answer::Body(content_type, body)) => builder
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body)),
        Some(Answer::Status(code)) => builder.status(code).body(Body::empty()),
        Some(Answer::Redirect(to)) => builder
            .status(StatusCode::FOUND)
            .header(header::LOCATION, to)
            .body(Body::empty()),
        None => builder.status(StatusCode::NOT_FOUND).body(Body::empty()),
    }
    .expect("valid fake response")
}

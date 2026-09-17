use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Response, StatusCode};
use axum::Router;
use serde_json::{json, Value};

/// One request the fake saw: its path and the `If-None-Match` it carried.
#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub if_none_match: Option<String>,
}

/// A fake npm registry holding one packument under an ETag that changes
/// with every `add_version`: a request carrying the current ETag in
/// `If-None-Match` is answered 304; tarballs are served under
/// `/{name}/-/{filename}`; anything else 404.
pub struct FakeNpm {
    pub base_url: String,
    pub hits: Arc<Mutex<Vec<Hit>>>,
    state: Arc<Mutex<Registry>>,
    name: String,
}

struct Registry {
    packument: Value,
    etag: String,
    tarballs: HashMap<String, Vec<u8>>,
    latency: Duration,
    gone: bool,
}

#[derive(Clone)]
struct App {
    path: String,
    state: Arc<Mutex<Registry>>,
    hits: Arc<Mutex<Vec<Hit>>>,
}

impl FakeNpm {
    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }

    /// Requests on the packument path, in order.
    pub fn packument_hits(&self) -> Vec<Hit> {
        let path = format!("/{}", self.name);
        self.hits().into_iter().filter(|h| h.path == path).collect()
    }

    pub fn add_tarball(&self, filename: &str, bytes: &[u8]) {
        self.state
            .lock()
            .unwrap()
            .tarballs
            .insert(filename.to_string(), bytes.to_vec());
    }

    /// A version with `time[version]` and a `dist.tarball` under this fake;
    /// the packument's ETag changes.
    pub fn add_version(&self, version: &str, time: &str) {
        let unscoped = self.name.rsplit('/').next().unwrap_or(&self.name);
        let mut state = self.state.lock().unwrap();
        state.packument["versions"][version] = json!({
            "name": self.name,
            "version": version,
            "dist": { "tarball": format!("{}/{}/-/{unscoped}-{version}.tgz", self.base_url, self.name) }
        });
        state.packument["time"][version] = json!(time);
        state.etag = format!(
            "\"v{}\"",
            state.packument["versions"]
                .as_object()
                .map_or(0, |v| v.len())
        );
    }

    pub fn etag(&self) -> String {
        self.state.lock().unwrap().etag.clone()
    }

    pub fn set_latency(&self, latency: Duration) {
        self.state.lock().unwrap().latency = latency;
    }

    /// The packument answers 404 while set; tarballs stay served.
    pub fn set_gone(&self, gone: bool) {
        self.state.lock().unwrap().gone = gone;
    }
}

pub async fn start(packument: Value, etag: &str) -> FakeNpm {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind the fake npm registry");
    let addr = listener.local_addr().expect("no local addr");
    let name = packument["name"]
        .as_str()
        .expect("packument name")
        .to_string();
    let state = Arc::new(Mutex::new(Registry {
        packument,
        etag: etag.to_string(),
        tarballs: HashMap::new(),
        latency: Duration::ZERO,
        gone: false,
    }));
    let hits = Arc::new(Mutex::new(Vec::new()));
    let app = App {
        path: format!("/{name}"),
        state: state.clone(),
        hits: hits.clone(),
    };
    let router = Router::new().fallback(serve).with_state(app);
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    FakeNpm {
        base_url: format!("http://{addr}"),
        hits,
        state,
        name,
    }
}

async fn serve(State(app): State<App>, req: Request) -> Response<Body> {
    let if_none_match = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let path = req.uri().path().to_string();
    app.hits.lock().unwrap().push(Hit {
        path: path.clone(),
        if_none_match: if_none_match.clone(),
    });
    let (status, body, etag, content_type, latency) = {
        let state = app.state.lock().unwrap();
        let tarball = path
            .strip_prefix(&format!("{}/-/", app.path))
            .and_then(|f| state.tarballs.get(f));
        let (status, body, content_type) = if let Some(bytes) = tarball {
            (StatusCode::OK, bytes.clone(), "application/octet-stream")
        } else if path != app.path || state.gone {
            (StatusCode::NOT_FOUND, Vec::new(), "application/json")
        } else if if_none_match.as_deref() == Some(state.etag.as_str()) {
            (StatusCode::NOT_MODIFIED, Vec::new(), "application/json")
        } else {
            (
                StatusCode::OK,
                state.packument.to_string().into_bytes(),
                "application/json",
            )
        };
        (
            status,
            body,
            state.etag.clone(),
            content_type,
            state.latency,
        )
    };
    tokio::time::sleep(latency).await;
    Response::builder()
        .status(status)
        .header(header::ETAG, etag)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("valid fake response")
}

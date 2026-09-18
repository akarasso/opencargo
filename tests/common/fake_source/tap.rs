//! A recording reverse proxy in front of a real target, which can answer
//! chosen requests itself: a status of its own, before or after
//! forwarding, or no answer at all.

use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::{serve, Log};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Answer this status without forwarding.
    Instead(StatusCode),
    /// Forward, then answer this status instead of the target's.
    After(StatusCode),
    /// Forward, then drop the connection without a status.
    Drop,
}

pub struct Rule {
    pub method: &'static str,
    pub path_contains: &'static str,
    /// Skip this many matching requests before the fault fires.
    pub skip: usize,
    pub times: usize,
    pub fault: Fault,
    pub range: Option<&'static str>,
}

pub struct Inner {
    target: String,
    client: reqwest::Client,
    rules: Mutex<Vec<(Rule, usize)>>,
    pub log: Log,
}

#[derive(Clone)]
pub struct Tap {
    pub url: String,
    pub inner: Arc<Inner>,
}

async fn forward(s: &Inner, req: Request) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let body = axum::body::to_bytes(req.into_body(), 1 << 30).await.unwrap_or_default();
    let mut out = s.client.request(method, format!("{}{}", s.target, uri));
    for (k, v) in headers.iter() {
        if k != "host" && k != "content-length" {
            out = out.header(k, v);
        }
    }
    let resp = out.body(body).send().await.expect("the tapped target answers");
    let status = resp.status();
    let mut builder = axum::http::Response::builder().status(status);
    for (k, v) in resp.headers() {
        if k != "transfer-encoding" && k != "content-length" {
            builder = builder.header(k, v);
        }
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    builder.body(axum::body::Body::from(bytes)).unwrap()
}

async fn handle(State(s): State<Arc<Inner>>, req: Request) -> Response {
    s.log.record(req.method().as_str(), req.uri(), req.headers());
    let fault = {
        let mut rules = s.rules.lock().unwrap();
        let path = req.uri().path().to_string();
        let method = req.method().as_str().to_string();
        rules
            .iter_mut()
            .find(|(r, seen)| r.method == method && path.contains(r.path_contains) && *seen < r.skip + r.times)
            .and_then(|(r, seen)| {
                *seen += 1;
                (*seen > r.skip).then_some((r.fault, r.range))
            })
    };
    match fault {
        None => forward(&s, req).await,
        Some((Fault::Instead(status), range)) => {
            let mut resp = (status, "tap").into_response();
            if let Some(r) = range {
                resp.headers_mut().insert("range", r.parse().unwrap());
            }
            resp
        }
        Some((Fault::After(status), _)) => {
            let _ = forward(&s, req).await;
            (status, "tap").into_response()
        }
        Some((Fault::Drop, _)) => {
            let _ = forward(&s, req).await;
            panic!("tap: dropping the connection on purpose");
        }
    }
}

impl Tap {
    pub async fn start(target: &str, rules: Vec<Rule>) -> Self {
        let inner = Arc::new(Inner {
            target: target.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            rules: Mutex::new(rules.into_iter().map(|r| (r, 0)).collect()),
            log: Log::default(),
        });
        let url = serve(axum::Router::new().fallback(handle).with_state(inner.clone())).await;
        Self { url, inner }
    }

    pub fn count(&self, method: &str, path_contains: &str) -> usize {
        self.inner.log.count(|h| h.method == method && h.path.contains(path_contains))
    }
}

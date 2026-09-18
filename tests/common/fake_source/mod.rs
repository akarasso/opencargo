//! In-process fakes of the registries `opencargo import` reads, shaped like
//! their real APIs, recording every request they answer.

pub mod manager;
pub mod target;
pub mod verdaccio;

use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;

#[derive(Debug, Clone)]
pub struct Hit {
    pub method: String,
    pub path: String,
    pub query: String,
    pub auth: Option<String>,
    pub accept: Option<String>,
}

#[derive(Default, Clone)]
pub struct Log(pub Arc<Mutex<Vec<Hit>>>);

impl Log {
    pub fn record(&self, method: &str, uri: &axum::http::Uri, headers: &HeaderMap) {
        let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(String::from);
        self.0.lock().unwrap().push(Hit {
            method: method.to_string(),
            path: uri.path().to_string(),
            query: uri.query().unwrap_or_default().to_string(),
            auth: h("authorization"),
            accept: h("accept"),
        });
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.0.lock().unwrap().clone()
    }

    pub fn count(&self, f: impl Fn(&Hit) -> bool) -> usize {
        self.0.lock().unwrap().iter().filter(|h| f(h)).count()
    }
}

pub async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

pub fn sha1_hex(data: &[u8]) -> String {
    use sha1::Digest;
    sha1::Sha1::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

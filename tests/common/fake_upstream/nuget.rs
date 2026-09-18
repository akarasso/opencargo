//! A nuget.org-shaped feed: registration pages without `packageHash`,
//! catalog leaves with it, search on another origin. Switches make it lie
//! about a hash, redirect a package off its origin or gzip its
//! registrations.

use std::io::Write as _;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Response, StatusCode};
use axum::Router;
use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha512};

#[derive(Default)]
pub struct Switches {
    pub wrong_hash: bool,
    pub redirect_nupkg: bool,
    pub redirect_index: bool,
    pub gzip_registration: bool,
    pub hash_in_registration: bool,
}

#[derive(Default)]
struct Inner {
    packages: Vec<(String, String, Vec<u8>)>,
    switches: Switches,
    hits: Vec<(String, bool)>,
}

#[derive(Clone)]
struct Shared {
    inner: Arc<Mutex<Inner>>,
    base: String,
    other: String,
    main: bool,
}

pub struct FakeNuget {
    pub base_url: String,
    pub other_url: String,
    inner: Arc<Mutex<Inner>>,
}

impl FakeNuget {
    pub fn service_index(&self) -> String {
        format!("{}/v3/index.json", self.base_url)
    }

    pub fn add(&self, id: &str, version: &str, bytes: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .packages
            .retain(|(i, v, _)| !(i.eq_ignore_ascii_case(id) && v == version));
        inner.packages.push((id.to_string(), version.to_string(), bytes));
    }

    pub fn switch(&self, set: impl FnOnce(&mut Switches)) {
        set(&mut self.inner.lock().unwrap().switches);
    }

    /// Requests whose path contains `part`.
    pub fn count(&self, part: &str) -> usize {
        self.inner.lock().unwrap().hits.iter().filter(|(p, _)| p.contains(part)).count()
    }

    /// Whether any request to a path ending with `suffix` carried credentials.
    pub fn credentials_sent(&self, suffix: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .hits
            .iter()
            .any(|(p, auth)| p.ends_with(suffix) && *auth)
    }
}

async fn listener() -> (tokio::net::TcpListener, String) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    (l, url)
}

pub async fn start() -> FakeNuget {
    let (main, base) = listener().await;
    let (other, other_url) = listener().await;
    let inner = Arc::new(Mutex::new(Inner::default()));
    let shared = Shared {
        inner: inner.clone(),
        base: base.clone(),
        other: other_url.clone(),
        main: true,
    };
    let app = Router::new().fallback(serve).with_state(shared.clone());
    tokio::spawn(async move {
        axum::serve(main, app).await.ok();
    });
    let app = Router::new().fallback(serve).with_state(Shared { main: false, ..shared });
    tokio::spawn(async move {
        axum::serve(other, app).await.ok();
    });
    FakeNuget {
        base_url: base,
        other_url,
        inner,
    }
}

fn reply(status: StatusCode, body: impl Into<Body>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.into())
        .unwrap()
}

/// A catalog leaf is immutable: a republished package gets a new one.
fn generation(bytes: &[u8]) -> String {
    Sha512::digest(bytes)[..4].iter().map(|b| format!("{b:02x}")).collect()
}

fn hash(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
}

async fn serve(State(s): State<Shared>, req: Request) -> Response<Body> {
    let path = req.uri().path().to_string();
    let authorized = req.headers().contains_key(header::AUTHORIZATION);
    let mut inner = s.inner.lock().unwrap();
    inner.hits.push((path.clone(), authorized));
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let (base, other) = (&s.base, &s.other);
    match segments.as_slice() {
        ["v3", "index.json"] if inner.switches.redirect_index => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, format!("{other}/v3/index.json"))
            .body(Body::empty())
            .unwrap(),
        ["v3", "index.json"] => reply(
            StatusCode::OK,
            json!({"version": "3.0.0", "resources": [
                {"@id": format!("{base}/flat/"), "@type": "PackageBaseAddress/3.0.0"},
                {"@id": format!("{base}/reg/"), "@type": "RegistrationsBaseUrl/3.6.0"},
                {"@id": format!("{other}/query"), "@type": "SearchQueryService"}
            ]})
            .to_string(),
        ),
        ["reg", id, "index.json"] => {
            let items: Vec<serde_json::Value> = inner
                .packages
                .iter()
                .filter(|(i, _, _)| i.eq_ignore_ascii_case(id))
                .map(|(i, v, bytes)| {
                    let mut entry = json!({
                        "@id": format!("{base}/catalog/{}.{v}.{}.json", i.to_lowercase(), generation(bytes)),
                        "id": i, "version": v, "listed": true,
                        "published": "2024-01-01T00:00:00+00:00",
                        "packageContent": format!("{base}/flat/{}/{v}/{}.{v}.nupkg", i.to_lowercase(), i.to_lowercase()),
                        "description": "from upstream",
                    });
                    if inner.switches.hash_in_registration {
                        entry["packageHash"] = json!(hash(bytes));
                        entry["packageHashAlgorithm"] = json!("SHA512");
                    }
                    json!({"@id": format!("{base}/reg/{i}/{v}.json"), "catalogEntry": entry})
                })
                .collect();
            if items.is_empty() {
                return reply(StatusCode::NOT_FOUND, "{}");
            }
            let doc = json!({"count": 1, "items": [{"count": items.len(), "items": items,
                "lower": "0.0.0", "upper": "9.9.9", "@id": format!("{base}/reg/{id}/index.json#page")}]})
            .to_string();
            if inner.switches.gzip_registration {
                let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                gz.write_all(doc.as_bytes()).unwrap();
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(gz.finish().unwrap()))
                    .unwrap();
            }
            reply(StatusCode::OK, doc)
        }
        ["catalog", leaf] => {
            let found = inner.packages.iter().find(|(i, v, bytes)| {
                format!("{}.{v}.{}.json", i.to_lowercase(), generation(bytes)) == *leaf
            });
            match found {
                Some((i, v, bytes)) => {
                    let h = if inner.switches.wrong_hash {
                        hash(b"something else")
                    } else {
                        hash(bytes)
                    };
                    reply(
                        StatusCode::OK,
                        json!({"id": i, "version": v, "packageHash": h, "packageHashAlgorithm": "SHA512"})
                            .to_string(),
                    )
                }
                None => reply(StatusCode::NOT_FOUND, "{}"),
            }
        }
        ["flat", id, v, file] if file.ends_with(".nupkg") => {
            if inner.switches.redirect_nupkg && s.main {
                return Response::builder()
                    .status(StatusCode::FOUND)
                    .header(header::LOCATION, format!("{other}/flat/{id}/{v}/{file}"))
                    .body(Body::empty())
                    .unwrap();
            }
            match inner.packages.iter().find(|(i, ver, _)| i.eq_ignore_ascii_case(id) && ver == v) {
                Some((_, _, bytes)) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from(bytes.clone()))
                    .unwrap(),
                None => reply(StatusCode::NOT_FOUND, "{}"),
            }
        }
        ["flat", id, _, file] if file.ends_with(".nuspec") => {
            reply(StatusCode::OK, format!("<package><metadata><id>{id}</id></metadata></package>"))
        }
        ["query"] => {
            let data: Vec<serde_json::Value> = inner
                .packages
                .iter()
                .map(|(i, v, _)| json!({"@id": format!("{base}/reg/{i}/index.json"), "id": i, "version": v,
                    "description": "from upstream", "totalDownloads": 7,
                    "versions": [{"version": v, "@id": format!("{base}/reg/{i}/{v}.json")}]}))
                .collect();
            reply(StatusCode::OK, json!({"totalHits": data.len(), "data": data}).to_string())
        }
        _ => reply(StatusCode::NOT_FOUND, "{}"),
    }
}

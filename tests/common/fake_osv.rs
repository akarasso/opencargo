use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

/// A deterministic OSV: `POST /v1/querybatch` answers from `affect`, `GET
/// /v1/vulns/{id}` from `record`, counting hits per id and the peak number
/// of record fetches in flight; `down` answers 503 to everything.
#[derive(Clone)]
pub struct FakeOsv {
    pub base_url: String,
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    affected: Mutex<HashMap<(String, String, String), Vec<String>>>,
    records: Mutex<HashMap<String, Value>>,
    hits: Mutex<HashMap<String, usize>>,
    inflight: AtomicUsize,
    peak: AtomicUsize,
    down: AtomicBool,
    record_delay: Mutex<Duration>,
}

pub async fn start() -> FakeOsv {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind the fake OSV");
    let addr = listener.local_addr().expect("no local addr");
    let inner = Arc::new(Inner::default());
    let app = Router::new()
        .route("/v1/querybatch", post(query_batch))
        .route("/v1/vulns/{id}", get(get_record))
        .with_state(inner.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    FakeOsv {
        base_url: format!("http://{addr}"),
        inner,
    }
}

impl FakeOsv {
    /// Make `name@version` in `ecosystem` match the given advisory ids.
    pub fn affect(&self, ecosystem: &str, name: &str, version: &str, ids: &[&str]) {
        self.inner.affected.lock().unwrap().insert(
            (ecosystem.to_string(), name.to_string(), version.to_string()),
            ids.iter().map(|s| s.to_string()).collect(),
        );
    }

    /// Serve a full advisory record for its `id` field.
    pub fn record(&self, record: Value) {
        let id = record["id"].as_str().expect("record id").to_string();
        self.inner.records.lock().unwrap().insert(id, record);
    }

    pub fn set_down(&self, down: bool) {
        self.inner.down.store(down, Ordering::SeqCst);
    }

    /// Hold every record response for `delay`, so overlapping fetches are observable.
    pub fn set_record_delay(&self, delay: Duration) {
        *self.inner.record_delay.lock().unwrap() = delay;
    }

    /// Number of `GET /v1/vulns/{id}` requests served for `id`.
    pub fn hits(&self, id: &str) -> usize {
        self.inner
            .hits
            .lock()
            .unwrap()
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    /// The most record fetches ever in flight at once.
    pub fn peak_inflight(&self) -> usize {
        self.inner.peak.load(Ordering::SeqCst)
    }
}

/// An advisory whose severity comes from one CVSS vector of `kind` (`CVSS_V3`, `CVSS_V4`).
pub fn cvss_record(id: &str, kind: &str, vector: &str) -> Value {
    json!({
        "id": id,
        "summary": format!("{id} summary"),
        "severity": [{ "type": kind, "score": vector }],
    })
}

/// An advisory carrying a `database_specific.severity` label beside an optional vector.
pub fn labelled_record(id: &str, label: &str, vector: Option<&str>) -> Value {
    let mut record = json!({
        "id": id,
        "summary": format!("{id} summary"),
        "database_specific": { "severity": label },
    });
    if let Some(v) = vector {
        record["severity"] = json!([{ "type": "CVSS_V3", "score": v }]);
    }
    record
}

async fn query_batch(
    State(inner): State<Arc<Inner>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if inner.down.load(Ordering::SeqCst) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    let affected = inner.affected.lock().unwrap();
    let results: Vec<Value> = body["queries"]
        .as_array()
        .map(|qs| qs.iter().map(|q| batch_result(&affected, q)).collect())
        .unwrap_or_default();
    (StatusCode::OK, Json(json!({ "results": results })))
}

fn batch_result(affected: &HashMap<(String, String, String), Vec<String>>, q: &Value) -> Value {
    let key = (
        q["package"]["ecosystem"].as_str().unwrap_or("").to_string(),
        q["package"]["name"].as_str().unwrap_or("").to_string(),
        q["version"].as_str().unwrap_or("").to_string(),
    );
    let vulns: Vec<Value> = affected
        .get(&key)
        .into_iter()
        .flatten()
        .map(|id| json!({ "id": id, "modified": "2026-01-01T00:00:00Z" }))
        .collect();
    json!({ "vulns": vulns })
}

async fn get_record(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if inner.down.load(Ordering::SeqCst) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    let now = inner.inflight.fetch_add(1, Ordering::SeqCst) + 1;
    inner.peak.fetch_max(now, Ordering::SeqCst);
    let delay = *inner.record_delay.lock().unwrap();
    tokio::time::sleep(delay).await;
    *inner.hits.lock().unwrap().entry(id.clone()).or_insert(0) += 1;
    let record = inner.records.lock().unwrap().get(&id).cloned();
    inner.inflight.fetch_sub(1, Ordering::SeqCst);
    match record {
        Some(r) => (StatusCode::OK, Json(r)),
        None => (StatusCode::NOT_FOUND, Json(json!({ "code": 404 }))),
    }
}

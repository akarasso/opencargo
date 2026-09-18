use chrono::DateTime;
use serde_json::{json, Value};

use super::*;
use crate::domain::Visibility;
use crate::testing::fixture::{timeouts, Fx};

pub fn repo(id: i64, name: &str, format: Format) -> Repository {
    Repository {
        id,
        name: name.into(),
        repo_type: "proxy".into(),
        format: format.as_str().into(),
        visibility: Visibility::Public,
        upstream_url: Some("http://127.0.0.1:1/".into()),
        config: None,
        created_at: DateTime::UNIX_EPOCH,
        updated_at: DateTime::UNIX_EPOCH,
    }
}

pub fn pending(
    member: &Repository,
    up: &Upstream,
    format: Format,
    name: &str,
    source: Source,
) -> Pending {
    Pending {
        requested_repo: "requested".into(),
        member: member.clone(),
        upstream: up.clone(),
        format,
        name: name.into(),
        version: Some("1.0.0".into()),
        actor: Actor::of(None),
        source,
    }
}

/// An engine over the proxy fixture with scanning off: its member records
/// when `cfg` is on.
pub fn engine_over(
    fx: &Fx,
    cfg: PolicyConfig,
    tuning: Tuning,
) -> (PolicyEngine, impl Future<Output = ()>) {
    engine_with(fx, cfg, tuning, scanner(None))
}

pub fn engine_with(
    fx: &Fx,
    cfg: PolicyConfig,
    tuning: Tuning,
    scanner: Arc<VulnScanner>,
) -> (PolicyEngine, impl Future<Output = ()>) {
    let config = HashMap::from([(fx.repo.name.clone(), cfg)]);
    PolicyEngine::unspawned(
        fx.pool.clone(),
        &config,
        scanner,
        Arc::new(EventBus::new()),
        fx.engine(timeouts()),
        tuning,
    )
}

/// A scanner over `osv`, disabled when `None`.
pub fn scanner(osv: Option<&FakeOsv>) -> Arc<VulnScanner> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cfg = crate::config::VulnScanConfig {
        enabled: osv.is_some(),
        osv_base_url: osv.map_or_else(|| "http://127.0.0.1:1".to_string(), |o| o.base_url.clone()),
        max_concurrency: 8,
        ..Default::default()
    };
    Arc::new(VulnScanner::new(&cfg).unwrap())
}

pub fn fast() -> Tuning {
    Tuning {
        child_ttl: Duration::from_millis(200),
        pacer_period: Duration::from_millis(20),
        pacer_cooldown: Duration::from_millis(200),
        gather_timeout: Duration::from_secs(2),
        notify_period: Duration::from_millis(50),
        refresh_floor: Duration::from_secs(60),
        flush_period: Duration::from_millis(100),
    }
}

/// One record per advisory id, one CVSS 3.1 vector each.
fn record(id: &str, vector: &str) -> Value {
    json!({ "id": id, "summary": id, "severity": [{ "type": "CVSS_V3", "score": vector }] })
}

#[derive(Default)]
struct OsvState {
    affected: HashMap<(String, String, String), Vec<String>>,
    records: HashMap<String, Value>,
    batches: Vec<usize>,
    hold_next: Option<Duration>,
    down: bool,
}

/// An in-crate OSV: `affect` names the advisories of a triple, `record`
/// their severity; `batches` are the query counts of every `querybatch`
/// POST, `hold_next` delays the next one.
#[derive(Clone)]
pub struct FakeOsv {
    pub base_url: String,
    state: Arc<Mutex<OsvState>>,
}

impl FakeOsv {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(OsvState::default()));
        let app = axum::Router::new()
            .route("/v1/querybatch", axum::routing::post(query_batch))
            .route("/v1/vulns/{id}", axum::routing::get(get_record))
            .with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        Self { base_url, state }
    }

    pub fn affect(&self, ecosystem: &str, name: &str, version: &str, id: &str, vector: &str) {
        let mut st = self.state.lock().unwrap();
        st.affected.insert(
            (ecosystem.into(), name.into(), version.into()),
            vec![id.to_string()],
        );
        st.records.insert(id.to_string(), record(id, vector));
    }

    pub fn batches(&self) -> Vec<usize> {
        self.state.lock().unwrap().batches.clone()
    }

    pub fn hold_next(&self, delay: Duration) {
        self.state.lock().unwrap().hold_next = Some(delay);
    }

    pub fn set_down(&self, down: bool) {
        self.state.lock().unwrap().down = down;
    }
}

async fn query_batch(
    axum::extract::State(state): axum::extract::State<Arc<Mutex<OsvState>>>,
    axum::Json(body): axum::Json<Value>,
) -> (axum::http::StatusCode, axum::Json<Value>) {
    let queries = body["queries"].as_array().cloned().unwrap_or_default();
    let (results, hold, down) = {
        let mut st = state.lock().unwrap();
        st.batches.push(queries.len());
        let results: Vec<Value> = queries
            .iter()
            .map(|q| {
                let key = (
                    q["package"]["ecosystem"].as_str().unwrap_or("").to_string(),
                    q["package"]["name"].as_str().unwrap_or("").to_string(),
                    q["version"].as_str().unwrap_or("").to_string(),
                );
                let vulns: Vec<Value> = st
                    .affected
                    .get(&key)
                    .into_iter()
                    .flatten()
                    .map(|id| json!({ "id": id }))
                    .collect();
                json!({ "vulns": vulns })
            })
            .collect();
        (results, st.hold_next.take(), st.down)
    };
    if let Some(delay) = hold {
        tokio::time::sleep(delay).await;
    }
    if down {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({})),
        );
    }
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({ "results": results })),
    )
}

async fn get_record(
    axum::extract::State(state): axum::extract::State<Arc<Mutex<OsvState>>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (axum::http::StatusCode, axum::Json<Value>) {
    match state.lock().unwrap().records.get(&id) {
        Some(r) => (axum::http::StatusCode::OK, axum::Json(r.clone())),
        None => (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({}))),
    }
}

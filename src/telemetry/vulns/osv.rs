use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::severity::{classify, OsvSeverityEntry, Severity};
use super::ScanError;

const BATCH_TIMEOUT: Duration = Duration::from_secs(30);
const RECORD_TIMEOUT: Duration = Duration::from_secs(15);
const CACHE_CAPACITY: usize = 10_000;

#[derive(Debug, Serialize)]
struct OsvQueryBatch {
    queries: Vec<OsvQuery>,
}

#[derive(Debug, Serialize)]
struct OsvQuery {
    package: OsvPackage,
    version: String,
}

#[derive(Debug, Serialize)]
struct OsvPackage {
    name: String,
    ecosystem: String,
}

#[derive(Debug, Deserialize)]
struct OsvBatchResponse {
    results: Vec<OsvResult>,
}

#[derive(Debug, Deserialize)]
struct OsvResult {
    #[serde(default)]
    vulns: Vec<OsvVulnRef>,
}

#[derive(Debug, Deserialize)]
struct OsvVulnRef {
    id: String,
}

/// The full `GET /v1/vulns/{id}` record, reduced to what severity needs.
#[derive(Debug, Deserialize)]
struct OsvRecord {
    id: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    severity: Vec<OsvSeverityEntry>,
    #[serde(default)]
    database_specific: Option<serde_json::Value>,
}

#[derive(Debug)]
pub struct Advisory {
    pub id: String,
    pub summary: Option<String>,
    pub severity: Severity,
    pub score: Option<f64>,
}

impl From<OsvRecord> for Advisory {
    fn from(r: OsvRecord) -> Self {
        let (severity, score) = classify(&r.id, r.database_specific.as_ref(), &r.severity);
        Self {
            id: r.id,
            summary: r.summary,
            severity,
            score,
        }
    }
}

#[derive(Default)]
struct CacheInner {
    map: HashMap<String, Arc<Advisory>>,
    order: VecDeque<String>,
}

/// Process-wide advisory memory, bounded FIFO: a record never changes
/// severity within a process lifetime often enough to matter.
#[derive(Clone, Default)]
struct AdvisoryCache {
    inner: Arc<Mutex<CacheInner>>,
}

impl AdvisoryCache {
    fn get(&self, id: &str) -> Option<Arc<Advisory>> {
        self.inner.lock().unwrap().map.get(id).cloned()
    }

    fn insert(&self, advisory: Arc<Advisory>) {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .map
            .insert(advisory.id.clone(), advisory.clone())
            .is_none()
        {
            inner.order.push_back(advisory.id.clone());
        }
        while inner.map.len() > CACHE_CAPACITY {
            if let Some(oldest) = inner.order.pop_front() {
                inner.map.remove(&oldest);
            }
        }
    }
}

#[derive(Clone)]
pub struct OsvClient {
    http: reqwest::Client,
    base: reqwest::Url,
    sem: Arc<Semaphore>,
    cache: AdvisoryCache,
}

impl OsvClient {
    pub fn new(base: reqwest::Url, max_concurrency: usize) -> Self {
        Self {
            http: reqwest::Client::new(),
            base,
            sem: Arc::new(Semaphore::new(max_concurrency.max(1))),
            cache: AdvisoryCache::default(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base.as_str().trim_end_matches('/'))
    }

    /// Advisory ids per dependency, positionally aligned with `deps`; the
    /// response is third-party controlled, so a short answer pads with empty
    /// lists and a long one is truncated.
    pub async fn query_batch(
        &self,
        ecosystem: &str,
        deps: &[(String, String)],
    ) -> Result<Vec<Vec<String>>, ScanError> {
        let queries = deps
            .iter()
            .map(|(name, version)| OsvQuery {
                package: OsvPackage {
                    name: name.clone(),
                    ecosystem: ecosystem.to_string(),
                },
                version: version.clone(),
            })
            .collect();
        let response = self
            .http
            .post(self.url("/v1/querybatch"))
            .json(&OsvQueryBatch { queries })
            .timeout(BATCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| ScanError::Upstream(format!("request failed: {e}")))?
            .error_for_status()
            .map_err(|e| ScanError::Upstream(format!("querybatch failed: {e}")))?
            .json::<OsvBatchResponse>()
            .await
            .map_err(|e| ScanError::Upstream(format!("invalid response: {e}")))?;
        let mut results = response.results.into_iter();
        Ok(deps
            .iter()
            .map(|_| {
                results
                    .next()
                    .map(|r| r.vulns.into_iter().map(|v| v.id).collect())
                    .unwrap_or_default()
            })
            .collect())
    }

    /// Fetch every id's record, at most `max_concurrency` requests in flight;
    /// a cached record costs nothing, a failed one stays uncached.
    pub async fn advisories(
        &self,
        ids: &[String],
    ) -> HashMap<String, Result<Arc<Advisory>, String>> {
        let mut tasks = JoinSet::new();
        for id in ids {
            let (client, id) = (self.clone(), id.clone());
            tasks.spawn(async move {
                let result = client.advisory(&id).await;
                (id, result)
            });
        }
        let mut out = HashMap::with_capacity(ids.len());
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((id, result)) => {
                    out.insert(id, result);
                }
                Err(e) => tracing::warn!(error = %e, "advisory fetch task failed"),
            }
        }
        out
    }

    async fn advisory(&self, id: &str) -> Result<Arc<Advisory>, String> {
        if let Some(hit) = self.cache.get(id) {
            return Ok(hit);
        }
        let _permit = self
            .sem
            .acquire()
            .await
            .map_err(|_| "scanner shut down".to_string())?;
        let record = self
            .http
            .get(self.url(&format!("/v1/vulns/{id}")))
            .timeout(RECORD_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("record fetch failed: {e}"))?
            .json::<OsvRecord>()
            .await
            .map_err(|e| format!("invalid record: {e}"))?;
        let advisory = Arc::new(Advisory::from(record));
        self.cache.insert(advisory.clone());
        Ok(advisory)
    }
}

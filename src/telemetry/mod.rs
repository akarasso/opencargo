pub mod cleanup;
pub mod http_metrics;
pub mod metrics;
pub mod vulns;
pub mod webhooks;

pub use http_metrics::http_metrics_middleware;
pub use metrics::{
    init_metrics, record_cache_hit, record_cache_miss, record_download, record_http_request,
    record_publish, set_storage_bytes,
};

use axum::response::IntoResponse;
use metrics_exporter_prometheus::PrometheusHandle;

/// Axum handler that renders all collected metrics in the Prometheus text
/// exposition format.
pub async fn metrics_endpoint(
    axum::extract::State(handle): axum::extract::State<PrometheusHandle>,
) -> impl IntoResponse {
    handle.render()
}

use std::time::Duration;

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Install the Prometheus metrics recorder and return a handle that can be
/// used to render the metrics endpoint.
pub fn init_metrics() -> PrometheusHandle {
    // The global recorder can only be set once. In tests, multiple
    // server instances may call init_metrics concurrently.
    // Use a static OnceLock to ensure we only install once.
    use std::sync::OnceLock;
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("failed to install Prometheus metrics recorder")
        })
        .clone()
}

/// Record an HTTP request (counter + duration histogram).
pub fn record_http_request(method: &str, path: &str, status: u16, duration: Duration) {
    let labels = [
        ("method", method.to_string()),
        ("path", path.to_string()),
        ("status", status.to_string()),
    ];

    counter!("opencargo_http_requests_total", &labels).increment(1);

    let duration_labels = [
        ("method", method.to_string()),
        ("path", path.to_string()),
    ];

    histogram!("opencargo_http_request_duration_seconds", &duration_labels)
        .record(duration.as_secs_f64());
}

/// Record a package download.
pub fn record_download(repo: &str, package: &str) {
    let labels = [
        ("repo", repo.to_string()),
        ("package", package.to_string()),
    ];
    counter!("opencargo_downloads_total", &labels).increment(1);
}

/// Record a package publish.
pub fn record_publish(repo: &str, package: &str) {
    let labels = [
        ("repo", repo.to_string()),
        ("package", package.to_string()),
    ];
    counter!("opencargo_publishes_total", &labels).increment(1);
}

/// Record a proxy cache hit.
pub fn record_cache_hit(repo: &str) {
    let labels = [("repo", repo.to_string())];
    counter!("opencargo_cache_hits_total", &labels).increment(1);
}

/// Record a proxy cache miss.
pub fn record_cache_miss(repo: &str) {
    let labels = [("repo", repo.to_string())];
    counter!("opencargo_cache_misses_total", &labels).increment(1);
}

/// Set the current storage usage in bytes for a repository.
pub fn set_storage_bytes(repo: &str, bytes: u64) {
    let labels = [("repo", repo.to_string())];
    gauge!("opencargo_storage_bytes", &labels).set(bytes as f64);
}

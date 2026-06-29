use std::time::Instant;

use axum::{body::Body, extract::MatchedPath, http::Request, middleware::Next, response::Response};

use super::metrics::record_http_request;

/// Axum middleware that records HTTP request count and duration for every
/// request using Prometheus metrics.
pub async fn http_metrics_middleware(
    request: Request<Body>,
    next: Next,
) -> Response {
    let method = request.method().to_string();
    // Label on the matched route TEMPLATE (e.g. "/{repo}/-/v1/search"), never
    // the raw URI path. The raw path is attacker-controlled and unbounded:
    // hammering random URLs (404s included) would spawn an unbounded number of
    // Prometheus series that are never reclaimed — a memory-exhaustion DoS
    // reachable anonymously. Unmatched requests collapse to one series.
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|mp| mp.as_str().to_string())
        .unwrap_or_else(|| "<unmatched>".to_string());
    let start = Instant::now();

    let response = next.run(request).await;

    let duration = start.elapsed();
    let status = response.status().as_u16();
    record_http_request(&method, &path, status, duration);

    response
}

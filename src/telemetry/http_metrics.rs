use std::time::Instant;

use axum::{body::Body, http::Request, middleware::Next, response::Response};

use super::metrics::record_http_request;

/// Axum middleware that records HTTP request count and duration for every
/// request using Prometheus metrics.
pub async fn http_metrics_middleware(
    request: Request<Body>,
    next: Next,
) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let start = Instant::now();

    let response = next.run(request).await;

    let duration = start.elapsed();
    let status = response.status().as_u16();
    record_http_request(&method, &path, status, duration);

    response
}

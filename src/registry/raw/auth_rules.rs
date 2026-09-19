use axum::http::{HeaderValue, Method};

use crate::auth::middleware::RouteRules;

/// `/raw/`: curl and the usual HTTP clients send credentials only once
/// challenged, so a 401 carries a Basic challenge.
pub struct RawRouteRules;

impl RouteRules for RawRouteRules {
    fn owns(&self, path: &str) -> bool {
        path.starts_with("/raw/")
    }

    fn challenge(&self, _method: &Method, _path: &str) -> Option<HeaderValue> {
        Some(HeaderValue::from_static("Basic realm=\"opencargo\""))
    }
}

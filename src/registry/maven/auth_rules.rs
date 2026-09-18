use axum::http::{HeaderValue, Method};

use crate::auth::middleware::RouteRules;

/// `/maven/`: Maven clients send credentials only once challenged, so a
/// 401 carries a Basic challenge; a miss stays a plain 404.
pub struct MavenRouteRules;

impl RouteRules for MavenRouteRules {
    fn owns(&self, path: &str) -> bool {
        path.starts_with("/maven/")
    }

    fn challenge(&self, _method: &Method, _path: &str) -> Option<HeaderValue> {
        Some(HeaderValue::from_static("Basic realm=\"opencargo\""))
    }
}

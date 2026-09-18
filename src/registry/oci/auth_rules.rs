use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::auth::middleware::RouteRules;

/// `/v2/`: the Bearer challenge naming the token realm, the distribution
/// error body, and registry tokens accepted.
pub struct OciRouteRules {
    pub base_url: String,
}

impl RouteRules for OciRouteRules {
    fn owns(&self, path: &str) -> bool {
        path.starts_with("/v2/")
    }

    fn challenge(&self, method: &Method, path: &str) -> Option<HeaderValue> {
        super::token::challenge(&self.base_url, method, path)
    }

    fn unauthorized(&self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [("Docker-Distribution-Api-Version", "registry/2.0")],
            Json(json!({
                "errors": [{"code": "UNAUTHORIZED", "message": "authentication required"}]
            })),
        )
            .into_response()
    }

    fn accepts_registry_tokens(&self) -> bool {
        true
    }
}

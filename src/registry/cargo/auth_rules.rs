use axum::http::Method;

use crate::auth::middleware::RouteRules;

/// `/{repo}/index/config.json`: cargo reads it before it knows whether to
/// send a token and learns to from `auth-required`, so a tokenless read
/// passes the anonymous gate; the handler discloses only existence and
/// format.
pub struct CargoRouteRules;

impl RouteRules for CargoRouteRules {
    fn owns(&self, path: &str) -> bool {
        let mut segments = path.split('/');
        matches!(
            (
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
            ),
            (Some(""), Some(repo), Some("index"), Some("config.json"), None) if !repo.is_empty()
        )
    }

    fn anonymous_exempt(&self, _method: &Method, _path: &str) -> bool {
        true
    }
}

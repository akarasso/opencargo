use axum::{body::Body, http::Request};

use crate::registry::{go, oci};

/// Every URI rewrite that must run before route matching, in one place.
pub fn pre_route(req: Request<Body>) -> Request<Body> {
    oci::routing::rewrite_nested_name(go::routes::rewrite_module_path(req))
}

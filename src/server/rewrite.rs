use axum::{body::Body, http::Request};

use crate::registry::{go, mcp, oci};

/// Every URI rewrite that must run before route matching, in one place and
/// one order: decoding first, because the MCP steps re-encode what it
/// decoded, and the gallery-base collapse before the name fold, whose
/// anchors the doubled prefix would otherwise sit between.
pub fn pre_route(req: Request<Body>) -> Request<Body> {
    let req = super::decode_percent_encoded_slashes(req);
    let req = oci::routing::rewrite_nested_name(go::routes::rewrite_module_path(req));
    mcp::routes::rewrite_server_name(mcp::routes::normalise_gallery_base(req))
}

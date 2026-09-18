use axum::{
    http::{header, Method, Request},
    routing::{get, post},
    Router,
};
use tower_http::cors::{Any, CorsLayer};

use super::catalog::{get_version, list_servers, list_versions};
use crate::server::AppState;

const VERSIONS: [&str; 2] = ["v0.1", "v0"];

/// The registry's own read API under `/v0.1`, and `/v0` over the same
/// handlers; the gallery a browser-hosted client loads answers CORS.
pub fn routes() -> Router<AppState> {
    let mut router = Router::new();
    router = router
        .route("/{repo}/v0.1/publish", post(super::publish::publish))
        .route("/{repo}/v0.1/surfaces", post(super::publish::attest))
        .route(
            "/{repo}/skills/{name}/{version}/skill.zip",
            get(super::skills::download).put(super::skills::upload).delete(super::skills::delete),
        )
        .route("/{repo}/.claude-plugin/marketplace.json", get(super::skills::marketplace))
        .route("/{repo}/clients/{client}/config.json", get(super::clients::config));
    for v in VERSIONS {
        router = router
            .route(&format!("/{{repo}}/{v}/servers"), get(list_servers))
            .route(&format!("/{{repo}}/{v}/servers/{{server}}/versions"), get(list_versions))
            .route(&format!("/{{repo}}/{v}/servers/{{server}}/versions/{{version}}"), get(get_version));
    }
    router.layer(
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([Method::GET, Method::OPTIONS])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
    )
}

fn is_version(segment: &str) -> bool {
    VERSIONS.contains(&segment)
}

fn with_path<B>(mut req: Request<B>, path: &str) -> Request<B> {
    let uri = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };
    if let Ok(uri) = uri.parse() {
        *req.uri_mut() = uri;
    }
    req
}

/// A gallery base given as an endpoint (`…/v0/servers`) gets the client's
/// own `/v0.1/servers` appended to it: the doubled prefix collapses to the
/// inner one, and every other path is left alone.
pub fn normalise_gallery_base<B>(req: Request<B>) -> Request<B> {
    let path = req.uri().path().to_string();
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() < 4 || !segments[0].is_empty() || !is_version(segments[2]) {
        return req;
    }
    let drop = if is_version(segments[3]) {
        1
    } else if segments.len() > 4 && segments[3] == "servers" && is_version(segments[4]) {
        2
    } else {
        return req;
    };
    let mut kept: Vec<&str> = segments[..2].to_vec();
    kept.extend(&segments[2 + drop..]);
    with_path(req, &kept.join("/"))
}

/// A server name carries one `/`, which the client percent-encodes and the
/// decoding step before routing turns back into a separator: the two
/// segments of the name are folded back into one encoded segment.
pub fn rewrite_server_name<B>(req: Request<B>) -> Request<B> {
    let path = req.uri().path().to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let folds = segments.len() >= 7
        && segments[0].is_empty()
        && is_version(segments[2])
        && segments[3] == "servers"
        && segments[6] == "versions"
        && !segments[4].is_empty()
        && !segments[5].is_empty();
    if !folds {
        return req;
    }
    let mut kept: Vec<String> = segments[..4].iter().map(|s| s.to_string()).collect();
    kept.push(format!("{}%2F{}", segments[4], segments[5]));
    kept.extend(segments[6..].iter().map(|s| s.to_string()));
    with_path(req, &kept.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(uri: &str, f: fn(Request<()>) -> Request<()>) -> String {
        let req = Request::builder().uri(uri).body(()).unwrap();
        f(req).uri().to_string()
    }

    #[test]
    fn rewrite_server_name_leaves_other_paths_alone() {
        assert_eq!(
            path("/mcp/v0.1/servers/io.github.acme/x/versions/1.0.0", rewrite_server_name),
            "/mcp/v0.1/servers/io.github.acme%2Fx/versions/1.0.0"
        );
        assert_eq!(
            path("/mcp/v0/servers/io.github.acme/x/versions?include_deleted=true", rewrite_server_name),
            "/mcp/v0/servers/io.github.acme%2Fx/versions?include_deleted=true"
        );
        for untouched in [
            "/mcp/v0.1/servers",
            "/mcp/v0.1/servers?cursor=a/b:1",
            "/npm/react",
            "/npm/@scope/name/-/name-1.0.0.tgz",
            "/go/github.com/a/b/@v/list",
            "/mcp/v0.1/publish",
        ] {
            assert_eq!(path(untouched, rewrite_server_name), untouched);
        }
    }

    #[test]
    fn a_doubled_version_prefix_collapses_and_other_paths_are_untouched() {
        assert_eq!(path("/mcp/v0/servers/v0.1/servers?limit=5", normalise_gallery_base), "/mcp/v0.1/servers?limit=5");
        assert_eq!(path("/mcp/v0.1/v0.1/servers", normalise_gallery_base), "/mcp/v0.1/servers");
        assert_eq!(path("/mcp/v0/v0.1/servers", normalise_gallery_base), "/mcp/v0.1/servers");
        let detail = path("/mcp/v0/servers/v0.1/servers/io.github.acme/x/versions/1.0.0", normalise_gallery_base);
        assert_eq!(path(&detail, rewrite_server_name), "/mcp/v0.1/servers/io.github.acme%2Fx/versions/1.0.0");
        for untouched in ["/mcp/v0.1/servers", "/mcp/v0/servers/a/b/versions", "/npm/v0/servers", "/v2/x/v0.1"] {
            assert_eq!(path(untouched, normalise_gallery_base), untouched);
        }
    }
}

use axum::{http::Request, routing::get, Router};

use super::publish::publish_module;
use super::read::{latest_version, list_versions, version_dispatch};
use crate::server::AppState;

/// Real module paths span several URL segments (`github.com/org/repo`) while
/// `{module}` matches exactly one; a `/{repo}/{*rest}` catch-all is rejected
/// by matchit because it conflicts with the npm/cargo routes sharing the
/// `/{repo}/` prefix. `rewrite_module_path` runs before routing and folds the
/// module into one percent-encoded segment that axum decodes back.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{repo}/{module}/@v/list", get(list_versions))
        .route(
            "/{repo}/{module}/@v/{version}",
            get(version_dispatch).put(publish_module),
        )
        .route("/{repo}/{module}/@latest", get(latest_version))
}

/// Percent-encode the slashes inside the module part of GOPROXY-shaped paths
/// (`/@v/` followed by one final segment, or an `/@latest` suffix, with at
/// least two module segments before it) so the single-segment routes match.
/// Never touches `/api/` or `/v2/`; an npm scope named `@v` always has more
/// than one segment after it. Everything else passes through untouched.
pub fn rewrite_module_path<B>(mut req: Request<B>) -> Request<B> {
    let path = req.uri().path();
    if path.starts_with("/api/") || path.starts_with("/v2/") {
        return req;
    }

    let (prefix, suffix) = if let Some(idx) = path.rfind("/@v/") {
        if path[idx + 4..].contains('/') {
            return req;
        }
        (&path[..idx], &path[idx..])
    } else if let Some(prefix) = path.strip_suffix("/@latest") {
        (prefix, "/@latest")
    } else {
        return req;
    };

    let mut parts = prefix.splitn(3, '/');
    let (Some(""), Some(repo), Some(module)) = (parts.next(), parts.next(), parts.next())
    else {
        return req;
    };
    if repo.is_empty() || module.is_empty() || !module.contains('/') {
        return req;
    }

    let encoded_module = module.replace('/', "%2F");
    let new_path = format!("/{repo}/{encoded_module}{suffix}");
    let new_uri_str = match req.uri().query() {
        Some(q) => format!("{new_path}?{q}"),
        None => new_path,
    };
    match new_uri_str.parse() {
        Ok(new_uri) => *req.uri_mut() = new_uri,
        Err(e) => {
            tracing::warn!(uri = %req.uri(), error = %e, "re-parse of rewritten Go module URI failed; keeping original");
        }
    }
    req
}

#[cfg(test)]
mod tests {
    use super::rewrite_module_path;

    fn req(uri: &str) -> axum::http::Request<()> {
        axum::http::Request::builder()
            .uri(uri)
            .body(())
            .expect("test URI should build")
    }

    #[test]
    fn rewrites_multi_segment_go_paths() {
        for (input, expected) in [
            (
                "/go-hosted/github.com/org/repo/@v/list",
                "/go-hosted/github.com%2Forg%2Frepo/@v/list",
            ),
            (
                "/go-hosted/github.com/org/repo/@v/v1.0.0.info",
                "/go-hosted/github.com%2Forg%2Frepo/@v/v1.0.0.info",
            ),
            (
                "/go-hosted/example.com/mod/@latest",
                "/go-hosted/example.com%2Fmod/@latest",
            ),
        ] {
            let out = rewrite_module_path(req(input));
            assert_eq!(out.uri().path(), expected, "for {input}");
        }

        let out = rewrite_module_path(req("/go-hosted/a/b/@v/list?x=1"));
        assert_eq!(out.uri().path(), "/go-hosted/a%2Fb/@v/list");
        assert_eq!(out.uri().query(), Some("x=1"));
    }

    #[test]
    fn leaves_non_go_paths_untouched() {
        for uri in [
            "/go-hosted/mymodule/@v/list",
            "/go-hosted/mymodule/@v/v1.0.0",
            "/go-hosted/mymodule/@latest",
            "/npm-dev/@scope/pkg",
            "/npm-dev/@v/pkg",
            "/npm-dev/@v/pkg/-/pkg-1.0.0.tgz",
            "/npm-dev/-/package/@v/pkg/dist-tags",
            "/cargo-repo/api/v1/crates/new",
            "/api/v1/packages",
            "/v2/oci-repo/img/manifests/latest",
            "/health/live",
        ] {
            let out = rewrite_module_path(req(uri));
            assert_eq!(out.uri().path(), uri, "{uri} must not be rewritten");
        }
    }
}

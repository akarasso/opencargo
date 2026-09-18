use axum::http::{HeaderValue, Method};

use crate::auth::middleware::RouteRules;

/// The PyPI routes answer a 401 with a Basic challenge, which is what makes
/// pip, uv and poetry send the `__token__` credentials they hold.
pub struct PypiRouteRules;

impl RouteRules for PypiRouteRules {
    fn owns(&self, path: &str) -> bool {
        let segments: Vec<&str> = path.split('/').collect();
        match segments.as_slice() {
            ["", repo, "simple", ""] => !repo.is_empty(),
            ["", repo, "simple", project, ""] | ["", repo, "simple", project] => {
                !repo.is_empty() && !project.is_empty()
            }
            ["", repo, "files", project, file] => {
                !repo.is_empty() && !project.is_empty() && *project != "-" && !file.is_empty()
            }
            ["", repo, "legacy"] | ["", repo, "legacy", ""] => !repo.is_empty(),
            ["", repo, "pypi", project, ..] => !repo.is_empty() && !project.is_empty(),
            _ => false,
        }
    }

    fn challenge(&self, _method: &Method, _path: &str) -> Option<HeaderValue> {
        Some(HeaderValue::from_static("Basic realm=\"opencargo\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_the_pypi_routes_and_nothing_of_npm_cargo_or_oci() {
        let rules = PypiRouteRules;
        for path in [
            "/py/simple/",
            "/py/simple/demo/",
            "/py/simple/demo",
            "/py/files/demo/demo-1.0.tar.gz",
            "/py/legacy/",
            "/py/pypi/demo/1.0/yank",
        ] {
            assert!(rules.owns(path), "{path}");
        }
        for path in [
            "/npm/simple",
            "/npm/files/-/files-1.0.0.tgz",
            "/npm/@scope/pkg",
            "/cargo/index/config.json",
            "/v2/",
            "/v2/img/manifests/latest",
            "/api/v1/repositories",
            "/-/whoami",
        ] {
            assert!(!rules.owns(path), "{path}");
        }
    }
}

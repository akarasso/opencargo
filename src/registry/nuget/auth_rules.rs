use axum::http::{HeaderMap, HeaderName, HeaderValue, Method};

use crate::app::authenticate::{Credential, Presented, Transport};
use crate::auth::middleware::RouteRules;

pub const API_KEY_HEADER: HeaderName = HeaderName::from_static("x-nuget-apikey");

/// The NuGet surface of a repository: `/{repo}/v3/...` and the unannounced
/// `/{repo}/api/v2/package` push alias. Its 401 carries a Basic challenge
/// so `dotnet` asks for credentials instead of reporting NU1101, and
/// `X-NuGet-ApiKey` is one more transport of an API token, primary on the
/// write routes.
pub struct NugetRouteRules;

fn segments(path: &str) -> Vec<&str> {
    path.split('/').skip(1).collect()
}

fn is_write_route(path: &str) -> bool {
    let s = segments(path);
    matches!(
        s.as_slice(),
        [_, "v3", "package", ..] | [_, "api", "v2", "package", ..]
    )
}

impl RouteRules for NugetRouteRules {
    fn owns(&self, path: &str) -> bool {
        let s = segments(path);
        match s.as_slice() {
            [repo, "v3", "index.json"] | [repo, "v3", "search"] => !repo.is_empty(),
            [repo, "v3", "flatcontainer" | "registration", ..] => !repo.is_empty(),
            [repo, "v3", "package", ..] | [repo, "api", "v2", "package", ..] => !repo.is_empty(),
            _ => false,
        }
    }

    fn challenge(&self, _method: &Method, _path: &str) -> Option<HeaderValue> {
        Some(HeaderValue::from_static("Basic realm=\"opencargo\""))
    }

    fn extra_credentials(&self, headers: &HeaderMap) -> Vec<Presented> {
        headers
            .get(API_KEY_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|key| Presented {
                transport: Transport::ApiKeyHeader,
                credential: Credential::ApiKey(key.to_string()),
            })
            .into_iter()
            .collect()
    }

    fn primary(&self, _method: &Method, path: &str) -> Option<Transport> {
        Some(if is_write_route(path) {
            Transport::ApiKeyHeader
        } else {
            Transport::Authorization
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_only_the_nuget_surface() {
        let r = NugetRouteRules;
        for p in [
            "/nuget/v3/index.json",
            "/nuget/v3/search",
            "/nuget/v3/flatcontainer/a/index.json",
            "/nuget/v3/registration/a/index.json",
            "/nuget/v3/package",
            "/nuget/v3/package/a/1.0.0",
            "/nuget/api/v2/package",
        ] {
            assert!(r.owns(p), "{p}");
        }
        for p in [
            "/npm/v3",
            "/npm/v3/-/v3-1.0.0.tgz",
            "/v2/oci/manifests/latest",
            "/api/v1/repositories",
            "/cargo/api/v1/crates/new",
        ] {
            assert!(!r.owns(p), "{p}");
        }
    }

    #[test]
    fn the_api_key_is_primary_on_writes_only() {
        let r = NugetRouteRules;
        assert_eq!(
            r.primary(&Method::PUT, "/n/v3/package"),
            Some(Transport::ApiKeyHeader)
        );
        assert_eq!(
            r.primary(&Method::DELETE, "/n/v3/package/a/1.0.0"),
            Some(Transport::ApiKeyHeader)
        );
        assert_eq!(
            r.primary(&Method::GET, "/n/v3/index.json"),
            Some(Transport::Authorization)
        );
        let mut headers = HeaderMap::new();
        assert!(r.extra_credentials(&headers).is_empty());
        headers.insert(API_KEY_HEADER, HeaderValue::from_static("trg_x"));
        assert_eq!(r.extra_credentials(&headers).len(), 1);
    }
}

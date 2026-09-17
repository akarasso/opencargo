use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};

use crate::error::{AppError, AppResult};
use crate::proxy::auth::is_docker_hub_host;
use crate::proxy::strategy::{CacheKey, CachePolicy, Classified, Transfer, Ttl, UpstreamStrategy};
use crate::registry::resolve::Upstream;

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
    application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json, \
    application/vnd.docker.distribution.manifest.list.v2+json";
const MAX_MANIFEST_BYTES: u64 = 10 * 1024 * 1024;
const MAX_BLOB_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const HUB_REGISTRY: &str = "registry-1.docker.io";
pub const DOCKER_CONTENT_DIGEST: HeaderName = HeaderName::from_static("docker-content-digest");

/// What one upstream request addresses; `name` is already the upstream's
/// name (`upstream_name`), digests keep their `sha256:` prefix.
#[derive(Debug)]
pub enum OciArtifact {
    Manifest { name: String, digest: String },
    Tag { name: String, tag: String },
    Blob { name: String, digest: String },
}

impl OciArtifact {
    fn endpoint(&self) -> String {
        match self {
            Self::Manifest { digest, .. } => format!("manifests/{digest}"),
            Self::Tag { tag, .. } => format!("manifests/{tag}"),
            Self::Blob { digest, .. } => format!("blobs/{digest}"),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Manifest { name, .. } | Self::Tag { name, .. } | Self::Blob { name, .. } => name,
        }
    }

    fn digest_hex(&self) -> Option<&str> {
        match self {
            Self::Manifest { digest, .. } | Self::Blob { digest, .. } => {
                Some(digest.trim_start_matches("sha256:"))
            }
            Self::Tag { .. } => None,
        }
    }
}

fn is_hub(up: &Upstream) -> bool {
    up.base.host_str().is_some_and(is_docker_hub_host)
}

/// The name the upstream knows: Docker Hub keeps official images under
/// `library/`, every other registry takes the name as is.
pub fn upstream_name(up: &Upstream, name: &str) -> String {
    if is_hub(up) && !name.contains('/') {
        format!("library/{name}")
    } else {
        name.to_string()
    }
}

pub struct OciUpstream;

impl UpstreamStrategy for OciUpstream {
    type Artifact = OciArtifact;

    fn upstream_url(&self, up: &Upstream, a: &OciArtifact) -> AppResult<reqwest::Url> {
        let mut url = up.base.clone();
        if is_hub(up) {
            url.set_host(Some(HUB_REGISTRY))
                .map_err(|e| AppError::Internal(format!("hub host rewrite failed: {e}")))?;
        }
        let prefix = up.base.path().trim_matches('/');
        let path = if prefix.is_empty() {
            format!("/v2/{}/{}", a.name(), a.endpoint())
        } else {
            format!("/v2/{prefix}/{}/{}", a.name(), a.endpoint())
        };
        url.set_path(&path);
        url.set_query(None);
        Ok(url)
    }

    fn cache_key(&self, a: &OciArtifact) -> CacheKey {
        match a {
            OciArtifact::Manifest { digest, .. } => CacheKey {
                kind: "oci-manifest",
                key: format!("sha256/{}", digest.trim_start_matches("sha256:")),
            },
            OciArtifact::Tag { name, tag } => CacheKey {
                kind: "oci-tag",
                key: format!("{name}/{tag}"),
            },
            OciArtifact::Blob { digest, .. } => CacheKey {
                kind: "oci-blob",
                key: format!("sha256/{}", digest.trim_start_matches("sha256:")),
            },
        }
    }

    /// A tag's body is the manifest it points at, stored once by digest.
    fn store_key(&self, a: &OciArtifact, body_sha256: &str) -> CacheKey {
        match a {
            OciArtifact::Tag { .. } => CacheKey {
                kind: "oci-manifest",
                key: format!("sha256/{body_sha256}"),
            },
            _ => self.cache_key(a),
        }
    }

    fn verify_headers(&self, _a: &OciArtifact, h: &HeaderMap, body_sha256: &str) -> AppResult<()> {
        match h.get(DOCKER_CONTENT_DIGEST).and_then(|v| v.to_str().ok()) {
            Some(claimed) if claimed != format!("sha256:{body_sha256}") => Err(AppError::BadGateway(
                format!("upstream Docker-Content-Digest {claimed} does not match the body"),
            )),
            _ => Ok(()),
        }
    }

    fn cache_policy(&self, a: &OciArtifact) -> CachePolicy {
        match a {
            OciArtifact::Manifest { .. } | OciArtifact::Blob { .. } => CachePolicy::Immutable,
            OciArtifact::Tag { .. } => CachePolicy::Ttl(Ttl::Default),
        }
    }

    fn transfer(&self, a: &OciArtifact) -> Transfer {
        match a {
            OciArtifact::Blob { .. } => Transfer::Streamed,
            _ => Transfer::Buffered,
        }
    }

    fn max_bytes(&self, a: &OciArtifact) -> u64 {
        match a {
            OciArtifact::Blob { .. } => MAX_BLOB_BYTES,
            _ => MAX_MANIFEST_BYTES,
        }
    }

    fn request_headers(&self, a: &OciArtifact) -> Vec<(HeaderName, HeaderValue)> {
        match a {
            OciArtifact::Manifest { .. } | OciArtifact::Tag { .. } => {
                vec![(header::ACCEPT, HeaderValue::from_static(MANIFEST_ACCEPT))]
            }
            OciArtifact::Blob { .. } => Vec::new(),
        }
    }

    fn expected_sha256(&self, a: &OciArtifact) -> Option<String> {
        a.digest_hex().map(String::from)
    }

    fn bearer_scope(&self, a: &OciArtifact) -> Option<String> {
        Some(format!("repository:{}:pull", a.name()))
    }

    fn head_via_get(&self, a: &OciArtifact) -> bool {
        !matches!(a, OciArtifact::Blob { .. })
    }

    /// Hub, GHCR and Quay never 404 a repository: after a token they answer
    /// 401/403 for an unknown or private one, which is a miss, not an outage.
    fn classify_status(&self, a: &OciArtifact, s: StatusCode) -> Classified {
        let post_token_refusal = matches!(s.as_u16(), 401 | 403) && self.bearer_scope(a).is_some();
        if matches!(s.as_u16(), 404 | 410) || post_token_refusal {
            Classified::Miss
        } else {
            Classified::Fail
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(base: &str) -> Upstream {
        Upstream {
            base: reqwest::Url::parse(base).unwrap(),
            auth: None,
            token_realms: Vec::new(),
            dl_allow_private: false,
        }
    }

    fn tag(up: &Upstream, name: &str) -> OciArtifact {
        OciArtifact::Tag {
            name: upstream_name(up, name),
            tag: "latest".into(),
        }
    }

    #[test]
    fn classify_status_post_token_401_is_miss() {
        let up = upstream("https://ghcr.io");
        let a = tag(&up, "org/app");
        for status in [401, 403, 404, 410] {
            let s = StatusCode::from_u16(status).unwrap();
            assert_eq!(OciUpstream.classify_status(&a, s), Classified::Miss, "{status}");
        }
        for status in [429, 500, 502, 503] {
            let s = StatusCode::from_u16(status).unwrap();
            assert_eq!(OciUpstream.classify_status(&a, s), Classified::Fail, "{status}");
        }
    }

    #[test]
    fn hub_aliases_map_to_registry_1_with_library_prefix() {
        for base in ["https://docker.io", "https://index.docker.io", "https://registry-1.docker.io"] {
            let up = upstream(base);
            let url = OciUpstream.upstream_url(&up, &tag(&up, "alpine")).unwrap();
            assert_eq!(
                url.as_str(),
                "https://registry-1.docker.io/v2/library/alpine/manifests/latest",
                "{base}"
            );
            let nested = OciUpstream.upstream_url(&up, &tag(&up, "org/app")).unwrap();
            assert_eq!(nested.as_str(), "https://registry-1.docker.io/v2/org/app/manifests/latest");
        }
        let second = upstream("http://127.0.0.1:5000/oci-hosted");
        let a = OciArtifact::Blob {
            name: upstream_name(&second, "alpine"),
            digest: "sha256:abc".into(),
        };
        assert_eq!(
            OciUpstream.upstream_url(&second, &a).unwrap().as_str(),
            "http://127.0.0.1:5000/v2/oci-hosted/alpine/blobs/sha256:abc"
        );
        assert_eq!(OciUpstream.bearer_scope(&a).unwrap(), "repository:alpine:pull");
        let key = OciUpstream.store_key(&tag(&second, "a"), "ff");
        assert_eq!((key.kind, key.key.as_str()), ("oci-manifest", "sha256/ff"));
    }
}

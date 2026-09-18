use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};

use crate::proxy::auth::is_docker_hub_host;
use crate::proxy::strategy::{
    CacheKey, CachePolicy, Classified, DigestAlgorithm, DigestSource, ExpectedDigests, Transfer,
    Ttl, UpstreamStrategy, MAX_METADATA_BYTES,
};
use crate::registry::resolve::{ResolveError, Upstream};

use super::MAX_TAGS;

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
    application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json, \
    application/vnd.docker.distribution.manifest.list.v2+json";
const MAX_MANIFEST_BYTES: u64 = 10 * 1024 * 1024;
const MAX_BLOB_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const TAGS_TTL_SECS: u64 = 600;
const HUB_REGISTRY: &str = "registry-1.docker.io";
pub const DOCKER_CONTENT_DIGEST: HeaderName = HeaderName::from_static("docker-content-digest");

/// What one upstream request addresses; `name` is already the upstream's
/// name (`upstream_name`), digests keep their `sha256:` prefix.
#[derive(Debug)]
pub enum OciArtifact {
    Manifest { name: String, digest: String },
    Tag { name: String, tag: String },
    Blob { name: String, digest: String },
    Tags { name: String },
}

impl OciArtifact {
    fn endpoint(&self) -> String {
        match self {
            Self::Manifest { digest, .. } => format!("manifests/{digest}"),
            Self::Tag { tag, .. } => format!("manifests/{tag}"),
            Self::Blob { digest, .. } => format!("blobs/{digest}"),
            Self::Tags { .. } => "tags/list".to_string(),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Manifest { name, .. }
            | Self::Tag { name, .. }
            | Self::Blob { name, .. }
            | Self::Tags { name } => name,
        }
    }

    fn digest_hex(&self) -> Option<&str> {
        match self {
            Self::Manifest { digest, .. } | Self::Blob { digest, .. } => {
                Some(digest.trim_start_matches("sha256:"))
            }
            Self::Tag { .. } | Self::Tags { .. } => None,
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

    fn upstream_url(&self, up: &Upstream, a: &OciArtifact) -> Result<url::Url, ResolveError> {
        let mut url = up.base.clone();
        if is_hub(up) {
            url.set_host(Some(HUB_REGISTRY))
                .map_err(|e| ResolveError::Upstream(format!("hub host rewrite failed: {e}")))?;
        }
        let prefix = up.base.path().trim_matches('/');
        let path = if prefix.is_empty() {
            format!("/v2/{}/{}", a.name(), a.endpoint())
        } else {
            format!("/v2/{prefix}/{}/{}", a.name(), a.endpoint())
        };
        url.set_path(&path);
        // The whole list in one page, so the merged view paginates complete.
        let query = matches!(a, OciArtifact::Tags { .. }).then(|| format!("n={MAX_TAGS}"));
        url.set_query(query.as_deref());
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
            OciArtifact::Tags { name } => CacheKey {
                kind: "oci-tags",
                key: name.clone(),
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

    /// The digest of the address, and the one the upstream announces. An
    /// announced digest in another algorithm cannot be checked and is
    /// refused rather than ignored.
    fn expected_digests(&self, a: &OciArtifact, h: &HeaderMap) -> ExpectedDigests {
        let mut expected = ExpectedDigests::none();
        if let Some(hex) = a.digest_hex() {
            expected = expected.with(DigestAlgorithm::Sha256, hex, DigestSource::Known);
        }
        if let Some(claimed) = h.get(DOCKER_CONTENT_DIGEST).and_then(|v| v.to_str().ok()) {
            let (algorithm, hex) = match claimed.split_once(':') {
                Some(("sha256", hex)) => (DigestAlgorithm::Sha256, hex),
                Some(("sha512", hex)) => (DigestAlgorithm::Sha512, hex),
                _ => (DigestAlgorithm::Sha256, claimed),
            };
            expected = expected.with(algorithm, hex, DigestSource::Header);
        }
        expected
    }

    fn cache_policy(&self, a: &OciArtifact) -> CachePolicy {
        match a {
            OciArtifact::Manifest { .. } | OciArtifact::Blob { .. } => CachePolicy::Immutable,
            OciArtifact::Tag { .. } => CachePolicy::Ttl(Ttl::Default),
            OciArtifact::Tags { .. } => CachePolicy::Ttl(Ttl::Secs(TAGS_TTL_SECS)),
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
            OciArtifact::Manifest { .. } | OciArtifact::Tag { .. } => MAX_MANIFEST_BYTES,
            OciArtifact::Tags { .. } => MAX_METADATA_BYTES,
        }
    }

    fn request_headers(&self, a: &OciArtifact) -> Vec<(HeaderName, HeaderValue)> {
        match a {
            OciArtifact::Manifest { .. } | OciArtifact::Tag { .. } => {
                vec![(header::ACCEPT, HeaderValue::from_static(MANIFEST_ACCEPT))]
            }
            OciArtifact::Blob { .. } | OciArtifact::Tags { .. } => Vec::new(),
        }
    }

    fn bearer_scope(&self, a: &OciArtifact) -> Option<String> {
        Some(format!("repository:{}:pull", a.name()))
    }

    fn head_via_get(&self, a: &OciArtifact) -> bool {
        !matches!(a, OciArtifact::Blob { .. })
    }

    /// Hub, GHCR and Quay never 404 a repository: after a token they answer
    /// 401/403 for an unknown or private one, which the client sees as 404
    /// but which is never remembered, so a fixed credential takes effect at
    /// once.
    fn classify_status(&self, _a: &OciArtifact, s: StatusCode) -> Classified {
        match s.as_u16() {
            404 | 410 => Classified::Miss,
            401 | 403 => Classified::Refused,
            _ => Classified::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(base: &str) -> Upstream {
        Upstream {
            base: url::Url::parse(base).unwrap(),
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
    fn expected_digests_are_the_address_and_the_announced_digest() {
        let blob = OciArtifact::Blob {
            name: "a".into(),
            digest: format!("sha256:{}", "ab".repeat(32)),
        };
        let mut h = HeaderMap::new();
        h.insert(DOCKER_CONTENT_DIGEST, HeaderValue::from_static("sha256:CD"));
        let got = OciUpstream.expected_digests(&blob, &h);
        assert_eq!(got.known(DigestAlgorithm::Sha256), Some("ab".repeat(32).as_str()));
        assert_eq!(got.entries().len(), 2);
        assert_eq!(got.entries()[1].source, DigestSource::Header);
        assert_eq!(got.entries()[1].value, "cd");

        let tag = OciArtifact::Tag { name: "a".into(), tag: "latest".into() };
        assert!(OciUpstream.expected_digests(&tag, &HeaderMap::new()).is_empty());
        h.insert(DOCKER_CONTENT_DIGEST, HeaderValue::from_static("md5:zz"));
        let odd = OciUpstream.expected_digests(&tag, &h);
        assert!(
            odd.mismatch(|_| Some("ab".repeat(32))).is_some(),
            "an announced digest that cannot be checked is refused"
        );
    }

    #[test]
    fn classify_status_401_is_refused_404_is_miss() {
        let up = upstream("https://ghcr.io");
        let a = tag(&up, "org/app");
        for (status, expected) in [
            (401, Classified::Refused),
            (403, Classified::Refused),
            (404, Classified::Miss),
            (410, Classified::Miss),
        ] {
            let s = StatusCode::from_u16(status).unwrap();
            assert_eq!(OciUpstream.classify_status(&a, s), expected, "{status}");
        }
        for status in [429, 500, 502, 503] {
            let s = StatusCode::from_u16(status).unwrap();
            assert_eq!(
                OciUpstream.classify_status(&a, s),
                Classified::Fail,
                "{status}"
            );
        }
    }

    #[test]
    fn hub_aliases_map_to_registry_1_with_library_prefix() {
        for base in [
            "https://docker.io",
            "https://index.docker.io",
            "https://registry-1.docker.io",
        ] {
            let up = upstream(base);
            let url = OciUpstream.upstream_url(&up, &tag(&up, "alpine")).unwrap();
            assert_eq!(
                url.as_str(),
                "https://registry-1.docker.io/v2/library/alpine/manifests/latest",
                "{base}"
            );
            let nested = OciUpstream.upstream_url(&up, &tag(&up, "org/app")).unwrap();
            assert_eq!(
                nested.as_str(),
                "https://registry-1.docker.io/v2/org/app/manifests/latest"
            );
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
        assert_eq!(
            OciUpstream.bearer_scope(&a).unwrap(),
            "repository:alpine:pull"
        );
        let key = OciUpstream.store_key(&tag(&second, "a"), "ff");
        assert_eq!((key.kind, key.key.as_str()), ("oci-manifest", "sha256/ff"));
        let tags = OciArtifact::Tags { name: "a".into() };
        assert_eq!(
            OciUpstream.upstream_url(&second, &tags).unwrap().as_str(),
            "http://127.0.0.1:5000/v2/oci-hosted/a/tags/list?n=10000"
        );
        assert_eq!(
            OciUpstream.cache_policy(&tags),
            CachePolicy::Ttl(Ttl::Secs(600))
        );
        assert_eq!(OciUpstream.max_bytes(&tags), MAX_METADATA_BYTES);
    }
}

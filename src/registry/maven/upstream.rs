use axum::http::HeaderMap;

use crate::proxy::strategy::{
    CacheKey, CachePolicy, DigestAlgorithm, DigestSource, ExpectedDigests, Transfer, Ttl,
    UpstreamStrategy, MAX_METADATA_BYTES,
};
use crate::registry::resolve::{ResolveError, Upstream};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// A `maven-metadata.xml` changes with every deploy upstream; a release file
/// or a timestamped snapshot build never does.
const METADATA_TTL_SECS: u64 = 300;

/// A repository-relative path, as the client asked for it.
#[derive(Debug, Clone)]
pub enum MavenArtifact {
    Metadata { path: String },
    File {
        path: String,
        immutable: bool,
        /// The upstream's own `.sha1` of the file, fetched beside it.
        sidecar_sha1: Option<String>,
    },
    /// A file's `.sha1`, fetched to verify the file, never served as is.
    Sidecar { path: String, immutable: bool },
}

impl MavenArtifact {
    fn path(&self) -> &str {
        match self {
            MavenArtifact::Metadata { path }
            | MavenArtifact::File { path, .. }
            | MavenArtifact::Sidecar { path, .. } => path,
        }
    }
}

pub struct MavenUpstream;

/// The `X-Checksum-*` headers Maven Central and the usual repository managers
/// send with a file.
fn header_digests(headers: &HeaderMap) -> ExpectedDigests {
    let mut expected = ExpectedDigests::default();
    for (name, algorithm) in [
        ("x-checksum-sha1", DigestAlgorithm::Sha1),
        ("x-checksum-sha256", DigestAlgorithm::Sha256),
        ("x-checksum-sha512", DigestAlgorithm::Sha512),
    ] {
        let value = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_hexdigit()));
        if let Some(value) = value {
            expected = expected.with(algorithm, value, DigestSource::Header);
        }
    }
    expected
}

impl UpstreamStrategy for MavenUpstream {
    type Artifact = MavenArtifact;

    fn upstream_url(&self, up: &Upstream, a: &MavenArtifact) -> Result<url::Url, ResolveError> {
        let url = format!("{}/{}", up.base.as_str().trim_end_matches('/'), a.path());
        url::Url::parse(&url)
            .map_err(|e| ResolveError::Upstream(format!("invalid upstream URL {url}: {e}")))
    }

    fn cache_key(&self, a: &MavenArtifact) -> CacheKey {
        let kind = match a {
            MavenArtifact::Metadata { .. } => "maven-metadata",
            MavenArtifact::File { .. } => "maven-file",
            MavenArtifact::Sidecar { .. } => "maven-sidecar",
        };
        CacheKey {
            kind,
            key: a.path().to_string(),
        }
    }

    /// Headers always, and the upstream's `.sha1` of a file when it has one.
    fn expected_digests(&self, a: &MavenArtifact, headers: &HeaderMap) -> ExpectedDigests {
        let expected = header_digests(headers);
        match a {
            MavenArtifact::File {
                sidecar_sha1: Some(sha1),
                ..
            } => expected.with(DigestAlgorithm::Sha1, sha1, DigestSource::Sidecar),
            _ => expected,
        }
    }

    fn send_credentials(&self, _a: &MavenArtifact) -> bool {
        true
    }

    fn cache_policy(&self, a: &MavenArtifact) -> CachePolicy {
        match a {
            MavenArtifact::File { immutable: true, .. }
            | MavenArtifact::Sidecar { immutable: true, .. } => CachePolicy::Immutable,
            _ => CachePolicy::Ttl(Ttl::Secs(METADATA_TTL_SECS)),
        }
    }

    fn transfer(&self, a: &MavenArtifact) -> Transfer {
        match a {
            MavenArtifact::File { .. } => Transfer::Streamed,
            _ => Transfer::Buffered,
        }
    }

    fn max_bytes(&self, a: &MavenArtifact) -> u64 {
        match a {
            MavenArtifact::File { .. } => MAX_FILE_BYTES,
            _ => MAX_METADATA_BYTES,
        }
    }
}

/// The hex digest a `.sha1` document carries, first token only.
pub fn sidecar_value(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let value = text.split_whitespace().next()?.to_ascii_lowercase();
    (value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn a_file_is_held_to_its_headers_and_its_sidecar() {
        let mut headers = HeaderMap::new();
        headers.insert("x-checksum-sha1", HeaderValue::from_static("AB"));
        headers.insert("x-checksum-md5", HeaderValue::from_static("cd"));
        let file = MavenArtifact::File {
            path: "g/a/1/a-1.jar".into(),
            immutable: true,
            sidecar_sha1: Some("ef".into()),
        };
        let expected = MavenUpstream.expected_digests(&file, &headers);
        let sources: Vec<_> = expected.entries().iter().map(|d| (d.source, d.value.as_str())).collect();
        assert_eq!(sources, vec![(DigestSource::Header, "ab"), (DigestSource::Sidecar, "ef")]);
        let metadata = MavenArtifact::Metadata { path: "g/a/maven-metadata.xml".into() };
        assert!(MavenUpstream.expected_digests(&metadata, &HeaderMap::new()).is_empty());
        assert_eq!(MavenUpstream.cache_policy(&file), CachePolicy::Immutable);
        assert_eq!(MavenUpstream.cache_policy(&metadata), CachePolicy::Ttl(Ttl::Secs(METADATA_TTL_SECS)));
    }

    #[test]
    fn a_sidecar_value_is_its_first_hex_token() {
        let sha1 = "a".repeat(40);
        assert_eq!(sidecar_value(format!("{sha1}  a-1.jar\n").as_bytes()), Some(sha1));
        assert_eq!(sidecar_value(b"<html>"), None);
    }
}

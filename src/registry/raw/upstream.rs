use axum::http::HeaderMap;

use crate::proxy::strategy::{
    CacheKey, CachePolicy, DigestAlgorithm, DigestSource, ExpectedDigests, Transfer, Ttl,
    UpstreamStrategy,
};
use crate::registry::resolve::{ResolveError, Upstream};

/// A raw upstream serves files it may replace in place, so a cached body is
/// only ever trusted for a while.
const TTL_SECS: u64 = 300;

/// A repository-relative path, as the client asked for it.
#[derive(Debug, Clone)]
pub struct RawArtifact {
    pub path: String,
}

pub struct RawUpstream;

/// The `X-Checksum-Sha256` the usual repository managers send with a file;
/// a raw upstream announces nothing else this format keeps.
fn announced_sha256(headers: &HeaderMap) -> ExpectedDigests {
    let value = headers
        .get("x-checksum-sha256")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()));
    match value {
        Some(value) => ExpectedDigests::default().with(
            DigestAlgorithm::Sha256,
            value,
            DigestSource::Header,
        ),
        None => ExpectedDigests::default(),
    }
}

impl UpstreamStrategy for RawUpstream {
    type Artifact = RawArtifact;

    /// Segment by segment: a `#`, a `?` or a space in a path is part of the
    /// name asked for, never of the URL's syntax.
    fn upstream_url(&self, up: &Upstream, a: &RawArtifact) -> Result<url::Url, ResolveError> {
        let mut url = up.base.clone();
        url.path_segments_mut()
            .map_err(|_| ResolveError::Upstream(format!("{} cannot be a base", up.base)))?
            .pop_if_empty()
            .extend(a.path.split('/'));
        Ok(url)
    }

    fn cache_key(&self, a: &RawArtifact) -> CacheKey {
        CacheKey {
            kind: "raw-file",
            key: a.path.clone(),
        }
    }

    fn expected_digests(&self, _a: &RawArtifact, headers: &HeaderMap) -> ExpectedDigests {
        announced_sha256(headers)
    }

    fn cache_policy(&self, _a: &RawArtifact) -> CachePolicy {
        CachePolicy::Ttl(Ttl::Secs(TTL_SECS))
    }

    fn transfer(&self, _a: &RawArtifact) -> Transfer {
        Transfer::Streamed
    }

    fn max_bytes(&self, _a: &RawArtifact) -> u64 {
        crate::app::raw::MAX_FILE_BYTES
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn a_file_is_held_to_the_sha256_its_headers_announce() {
        let artifact = RawArtifact {
            path: "dist/tool.bin".to_string(),
        };
        let mut headers = HeaderMap::new();
        assert!(RawUpstream.expected_digests(&artifact, &headers).is_empty());
        headers.insert("x-checksum-sha256", HeaderValue::from_static("nothex"));
        assert!(RawUpstream.expected_digests(&artifact, &headers).is_empty());
        let hex = "a".repeat(64);
        headers.insert("x-checksum-sha256", HeaderValue::from_str(&hex).unwrap());
        let expected = RawUpstream.expected_digests(&artifact, &headers);
        let entries: Vec<_> = expected
            .entries()
            .iter()
            .map(|d| (d.source, d.value.clone()))
            .collect();
        assert_eq!(entries, vec![(DigestSource::Header, hex)]);
        assert_eq!(RawUpstream.cache_key(&artifact).key, "dist/tool.bin");
        assert_eq!(RawUpstream.cache_policy(&artifact), CachePolicy::Ttl(Ttl::Secs(TTL_SECS)));
    }

    fn upstream(base: &str) -> Upstream {
        Upstream {
            base: url::Url::parse(base).unwrap(),
            auth: None,
            token_realms: Vec::new(),
            dl_allow_private: false,
        }
    }

    fn asked(base: &str, path: &str) -> String {
        RawUpstream
            .upstream_url(
                &upstream(base),
                &RawArtifact {
                    path: path.to_string(),
                },
            )
            .unwrap()
            .to_string()
    }

    #[test]
    fn an_upstream_url_is_the_base_and_the_path() {
        assert_eq!(
            asked("https://files.example.com/repo/", "dist/tool.bin"),
            "https://files.example.com/repo/dist/tool.bin"
        );
        assert_eq!(
            asked("https://files.example.com/repo", "dist/tool.bin"),
            "https://files.example.com/repo/dist/tool.bin",
            "a base without its trailing slash asks for the same file"
        );
    }

    /// A path is a name, not URL syntax: what follows a `#` or a `?` is part
    /// of the file asked for, so it may not become a fragment or a query.
    #[test]
    fn every_segment_of_a_path_is_encoded() {
        let url = asked("https://files.example.com/repo/", "notes#draft.txt");
        assert_eq!(url, "https://files.example.com/repo/notes%23draft.txt");
        assert_eq!(
            asked("https://files.example.com/repo/", "dist/a?b=c/tool bin"),
            "https://files.example.com/repo/dist/a%3Fb=c/tool%20bin"
        );
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.fragment(), None);
        assert_eq!(parsed.path(), "/repo/notes%23draft.txt");
    }
}

use axum::http::HeaderMap;

use crate::proxy::strategy::{
    CacheKey, CachePolicy, ExpectedDigests, Transfer, Ttl, UpstreamStrategy,
};
use crate::registry::resolve::{ResolveError, Upstream};

#[derive(Debug)]
pub enum NpmArtifact {
    Metadata { name: String },
    Tarball { name: String, filename: String },
}

pub struct NpmUpstream;

impl UpstreamStrategy for NpmUpstream {
    type Artifact = NpmArtifact;

    fn upstream_url(&self, up: &Upstream, a: &NpmArtifact) -> Result<url::Url, ResolveError> {
        let base = up.base.as_str().trim_end_matches('/');
        let url = match a {
            NpmArtifact::Metadata { name } => format!("{base}/{name}"),
            NpmArtifact::Tarball { name, filename } => format!("{base}/{name}/-/{filename}"),
        };
        url::Url::parse(&url)
            .map_err(|e| ResolveError::Upstream(format!("invalid upstream URL {url}: {e}")))
    }

    fn cache_key(&self, a: &NpmArtifact) -> CacheKey {
        match a {
            NpmArtifact::Metadata { name } => CacheKey {
                kind: "npm-metadata",
                key: name.clone(),
            },
            NpmArtifact::Tarball { name, filename } => CacheKey {
                kind: "npm-tarball",
                key: format!("{name}/{filename}"),
            },
        }
    }

    // A tarball's integrity lives in the packument, which this strategy
    // does not read; the ratchet lists npm among the strategies at none().
    fn expected_digests(&self, _a: &NpmArtifact, _h: &HeaderMap) -> ExpectedDigests {
        ExpectedDigests::none()
    }

    fn cache_policy(&self, a: &NpmArtifact) -> CachePolicy {
        match a {
            NpmArtifact::Metadata { .. } => CachePolicy::Ttl(Ttl::Default),
            NpmArtifact::Tarball { .. } => CachePolicy::Immutable,
        }
    }

    fn transfer(&self, a: &NpmArtifact) -> Transfer {
        match a {
            NpmArtifact::Metadata { .. } => Transfer::Buffered,
            NpmArtifact::Tarball { .. } => Transfer::Streamed,
        }
    }
}

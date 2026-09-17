use crate::error::{AppError, AppResult};
use crate::proxy::strategy::{CacheKey, CachePolicy, Transfer, Ttl, UpstreamStrategy};
use crate::registry::resolve::Upstream;

#[derive(Debug)]
pub enum NpmArtifact {
    Metadata { name: String },
    Tarball { name: String, filename: String },
}

pub struct NpmUpstream;

impl UpstreamStrategy for NpmUpstream {
    type Artifact = NpmArtifact;

    fn upstream_url(&self, up: &Upstream, a: &NpmArtifact) -> AppResult<reqwest::Url> {
        let base = up.base.as_str().trim_end_matches('/');
        let url = match a {
            NpmArtifact::Metadata { name } => format!("{base}/{name}"),
            NpmArtifact::Tarball { name, filename } => format!("{base}/{name}/-/{filename}"),
        };
        reqwest::Url::parse(&url)
            .map_err(|e| AppError::BadGateway(format!("invalid upstream URL {url}: {e}")))
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

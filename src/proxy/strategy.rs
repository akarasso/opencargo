use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};

use crate::registry::resolve::{ResolveError, Upstream};

/// The vocabulary a strategy answers in is registry semantics, not proxy
/// machinery, so it lives in the domain; the hooks below are its only
/// consumers in this module and re-exporting it keeps their spelling.
pub use crate::domain::{CachePolicy, Classified, Transfer, Ttl, UrlSource};

pub const DEFAULT_MAX_UPSTREAM_BYTES: u64 = 100 * 1024 * 1024;
/// Index lines, version lists, tag lists: far below this in practice.
pub const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CacheKey {
    pub kind: &'static str,
    pub key: String,
}

/// Per-format upstream behaviour; every hook is a pure function of the
/// artifact, so the engine never names a format.
pub trait UpstreamStrategy: Send + Sync {
    type Artifact: std::fmt::Debug + Send + Sync;

    fn upstream_url(&self, up: &Upstream, a: &Self::Artifact) -> Result<url::Url, ResolveError>;

    fn url_source(&self, _up: &Upstream, _a: &Self::Artifact) -> UrlSource {
        UrlSource::Admin
    }

    fn cache_key(&self, a: &Self::Artifact) -> CacheKey;
    fn cache_policy(&self, a: &Self::Artifact) -> CachePolicy;

    fn store_key(&self, a: &Self::Artifact, _body_sha256: &str) -> CacheKey {
        self.cache_key(a)
    }

    fn verify_headers(
        &self,
        _a: &Self::Artifact,
        _h: &HeaderMap,
        _body_sha256: &str,
    ) -> Result<(), ResolveError> {
        Ok(())
    }

    fn transfer(&self, _a: &Self::Artifact) -> Transfer {
        Transfer::Buffered
    }

    fn max_bytes(&self, _a: &Self::Artifact) -> u64 {
        DEFAULT_MAX_UPSTREAM_BYTES
    }

    fn request_headers(&self, _a: &Self::Artifact) -> Vec<(HeaderName, HeaderValue)> {
        Vec::new()
    }

    fn expected_sha256(&self, _a: &Self::Artifact) -> Option<String> {
        None
    }

    fn bearer_scope(&self, _a: &Self::Artifact) -> Option<String> {
        None
    }

    /// How a HEAD miss is answered: a full cached GET, or an upstream HEAD
    /// without download.
    fn head_via_get(&self, _a: &Self::Artifact) -> bool {
        true
    }

    /// Which upstream status is an authoritative miss (negative entry)
    /// rather than a failure.
    fn classify_status(&self, _a: &Self::Artifact, s: StatusCode) -> Classified {
        if matches!(s.as_u16(), 404 | 410) {
            Classified::Miss
        } else {
            Classified::Fail
        }
    }
}

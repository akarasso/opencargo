use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};

use crate::error::AppResult;
use crate::registry::resolve::Upstream;

pub const DEFAULT_MAX_UPSTREAM_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ttl {
    Default,
    Secs(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CachePolicy {
    Immutable,
    Ttl(Ttl),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transfer {
    Buffered,
    Streamed,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CacheKey {
    pub kind: &'static str,
    pub key: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Classified {
    Miss,
    Fail,
}

/// Who chose the host of an upstream URL: the admin (the configured
/// upstream, trusted as is) or upstream content (held to `is_blocked_ip`
/// unless the member opted in).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UrlSource {
    Admin,
    Content { allow_private: bool },
}

/// Per-format upstream behaviour; every hook is a pure function of the
/// artifact, so the engine never names a format.
pub trait UpstreamStrategy: Send + Sync {
    type Artifact: std::fmt::Debug + Send + Sync;

    fn upstream_url(&self, up: &Upstream, a: &Self::Artifact) -> AppResult<reqwest::Url>;

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
    ) -> AppResult<()> {
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

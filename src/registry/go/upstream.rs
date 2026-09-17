use crate::error::{AppError, AppResult};
use crate::proxy::strategy::{
    CacheKey, CachePolicy, Transfer, Ttl, UpstreamStrategy, DEFAULT_MAX_UPSTREAM_BYTES,
};
use crate::registry::resolve::Upstream;

use super::escape::is_canonical_version;

const MAX_ZIP_BYTES: u64 = 512 * 1024 * 1024;
const MAX_MOD_BYTES: u64 = 16 * 1024 * 1024;
const QUERY_TTL_SECS: u64 = 600;

/// The three per-version files GOPROXY serves under `@v/{version}.{ext}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Info,
    Mod,
    Zip,
}

impl FileKind {
    /// `v1.0.0.info` -> `(v1.0.0, Info)`; a version without a known suffix is `None`.
    pub fn split(version_raw: &str) -> Option<(&str, FileKind)> {
        [FileKind::Info, FileKind::Mod, FileKind::Zip]
            .into_iter()
            .find_map(|kind| {
                version_raw
                    .strip_suffix(kind.ext())
                    .and_then(|v| v.strip_suffix('.'))
                    .map(|v| (v, kind))
            })
    }

    pub const fn ext(self) -> &'static str {
        match self {
            FileKind::Info => "info",
            FileKind::Mod => "mod",
            FileKind::Zip => "zip",
        }
    }

    pub const fn cache_kind(self) -> &'static str {
        match self {
            FileKind::Info => "go-info",
            FileKind::Mod => "go-mod",
            FileKind::Zip => "go-zip",
        }
    }

    pub const fn content_type(self) -> &'static str {
        match self {
            FileKind::Info => "application/json",
            FileKind::Mod => "text/plain; charset=utf-8",
            FileKind::Zip => "application/zip",
        }
    }
}

/// Module and version exactly as received, GOPROXY-escaped.
#[derive(Debug)]
pub enum GoArtifact {
    List {
        module: String,
    },
    Latest {
        module: String,
    },
    File {
        module: String,
        version: String,
        kind: FileKind,
    },
}

pub struct GoUpstream;

impl UpstreamStrategy for GoUpstream {
    type Artifact = GoArtifact;

    fn upstream_url(&self, up: &Upstream, a: &GoArtifact) -> AppResult<reqwest::Url> {
        let base = up.base.as_str().trim_end_matches('/');
        let url = match a {
            GoArtifact::List { module } => format!("{base}/{module}/@v/list"),
            GoArtifact::Latest { module } => format!("{base}/{module}/@latest"),
            GoArtifact::File {
                module,
                version,
                kind,
            } => format!("{base}/{module}/@v/{version}.{}", kind.ext()),
        };
        reqwest::Url::parse(&url)
            .map_err(|e| AppError::BadGateway(format!("invalid upstream URL {url}: {e}")))
    }

    fn cache_key(&self, a: &GoArtifact) -> CacheKey {
        match a {
            GoArtifact::List { module } => CacheKey {
                kind: "go-list",
                key: module.clone(),
            },
            GoArtifact::Latest { module } => CacheKey {
                kind: "go-latest",
                key: module.clone(),
            },
            GoArtifact::File {
                module,
                version,
                kind,
            } => CacheKey {
                kind: kind.cache_kind(),
                key: format!("{module}/{version}"),
            },
        }
    }

    fn cache_policy(&self, a: &GoArtifact) -> CachePolicy {
        match a {
            GoArtifact::File { version, .. } if is_canonical_version(version) => {
                CachePolicy::Immutable
            }
            _ => CachePolicy::Ttl(Ttl::Secs(QUERY_TTL_SECS)),
        }
    }

    fn transfer(&self, a: &GoArtifact) -> Transfer {
        match a {
            GoArtifact::File {
                kind: FileKind::Zip,
                ..
            } => Transfer::Streamed,
            _ => Transfer::Buffered,
        }
    }

    fn max_bytes(&self, a: &GoArtifact) -> u64 {
        match a {
            GoArtifact::File {
                kind: FileKind::Zip,
                ..
            } => MAX_ZIP_BYTES,
            GoArtifact::File {
                kind: FileKind::Mod,
                ..
            } => MAX_MOD_BYTES,
            _ => DEFAULT_MAX_UPSTREAM_BYTES,
        }
    }
}

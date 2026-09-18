//! `PypiStrategy`: a project page from the index, a file from wherever the
//! page points within the allowed hosts, verified against the page's sha256.

use axum::http::{header, HeaderMap, HeaderName, HeaderValue};

use crate::proxy::strategy::{
    CacheKey, CachePolicy, DigestAlgorithm, DigestSource, ExpectedDigests, RedirectRule, Transfer,
    Ttl, UpstreamStrategy, UrlSource, MAX_METADATA_BYTES,
};
use crate::registry::resolve::{ResolveError, Upstream};

use super::simple::{HTML_V1, JSON_V1};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const PAGE_ACCEPT: &str =
    "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html;q=0.2, text/html;q=0.01";

/// The default host a PyPI index serves its files from.
pub const PYPI_FILES_HOST: &str = "files.pythonhosted.org";

#[derive(Debug, Clone)]
pub enum PypiArtifact {
    Page {
        project: String,
    },
    /// A file, or its `.metadata`, at the absolute URL its page gave.
    File {
        project: String,
        filename: String,
        url: url::Url,
        sha256: Option<String>,
        /// Whether the file lives on the index's own endpoint, the only one
        /// the upstream's credentials may reach.
        on_index: bool,
        allow_private: bool,
    },
}

/// Same scheme, host and port.
pub fn same_endpoint(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

/// The page URL of a project under an index base.
pub fn page_url(up: &Upstream, project: &str) -> Result<url::Url, ResolveError> {
    let raw = format!("{}/{project}/", up.base.as_str().trim_end_matches('/'));
    url::Url::parse(&raw).map_err(|e| ResolveError::Upstream(format!("invalid upstream URL {raw}: {e}")))
}

/// Whether `url` is on one of `hosts` (`host`, or `host:port` for one port).
pub fn allowed_host(url: &url::Url, hosts: &[String]) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let port = url.port_or_known_default();
    hosts.iter().any(|entry| match entry.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => h.eq_ignore_ascii_case(host) && p.parse::<u16>().ok() == port,
        _ => entry.eq_ignore_ascii_case(host),
    })
}

/// The configured file hosts, or PyPI's own file host plus the index's
/// endpoint.
pub fn file_hosts(up: &Upstream, configured: &[String]) -> Vec<String> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    let mut hosts = vec![PYPI_FILES_HOST.to_string()];
    if let Some(host) = up.base.host_str() {
        let port = up.base.port_or_known_default().unwrap_or(443);
        hosts.push(format!("{host}:{port}"));
    }
    hosts
}

pub struct PypiStrategy;

impl UpstreamStrategy for PypiStrategy {
    type Artifact = PypiArtifact;

    fn upstream_url(&self, up: &Upstream, a: &PypiArtifact) -> Result<url::Url, ResolveError> {
        match a {
            PypiArtifact::Page { project } => page_url(up, project),
            PypiArtifact::File { url, .. } => Ok(url.clone()),
        }
    }

    fn url_source(&self, _up: &Upstream, a: &PypiArtifact) -> UrlSource {
        match a {
            PypiArtifact::Page { .. } => UrlSource::Admin,
            PypiArtifact::File { allow_private, .. } => UrlSource::Content {
                allow_private: *allow_private,
            },
        }
    }

    fn cache_key(&self, a: &PypiArtifact) -> CacheKey {
        match a {
            PypiArtifact::Page { project } => CacheKey {
                kind: "pypi-page",
                key: project.clone(),
            },
            PypiArtifact::File {
                project, filename, ..
            } => CacheKey {
                kind: "pypi-file",
                key: format!("{project}/{filename}"),
            },
        }
    }

    fn cache_policy(&self, a: &PypiArtifact) -> CachePolicy {
        match a {
            PypiArtifact::Page { .. } => CachePolicy::Ttl(Ttl::Default),
            PypiArtifact::File { .. } => CachePolicy::Immutable,
        }
    }

    /// A file is checked against the sha256 its page announced; a page, and
    /// a file whose page announced none, carry nothing to check.
    fn expected_digests(&self, a: &PypiArtifact, _headers: &HeaderMap) -> ExpectedDigests {
        match a {
            PypiArtifact::File {
                sha256: Some(sha), ..
            } => ExpectedDigests::none().with(DigestAlgorithm::Sha256, sha, DigestSource::Known),
            PypiArtifact::Page { .. } | PypiArtifact::File { sha256: None, .. } => ExpectedDigests::none(),
        }
    }

    fn send_credentials(&self, a: &PypiArtifact) -> bool {
        match a {
            PypiArtifact::Page { .. } => true,
            PypiArtifact::File { on_index, .. } => *on_index,
        }
    }

    fn final_url_must_match(&self, a: &PypiArtifact) -> RedirectRule {
        if self.send_credentials(a) {
            RedirectRule::SameOrigin
        } else {
            RedirectRule::Unrestricted
        }
    }

    fn transfer(&self, a: &PypiArtifact) -> Transfer {
        match a {
            PypiArtifact::Page { .. } => Transfer::Buffered,
            PypiArtifact::File { .. } => Transfer::Streamed,
        }
    }

    fn max_bytes(&self, a: &PypiArtifact) -> u64 {
        match a {
            PypiArtifact::Page { .. } => MAX_METADATA_BYTES,
            PypiArtifact::File { .. } => MAX_FILE_BYTES,
        }
    }

    fn request_headers(&self, a: &PypiArtifact) -> Vec<(HeaderName, HeaderValue)> {
        match a {
            PypiArtifact::Page { .. } => vec![(header::ACCEPT, HeaderValue::from_static(PAGE_ACCEPT))],
            PypiArtifact::File { .. } => Vec::new(),
        }
    }
}

/// Which flavor a cached page body is, from its stored content type.
pub fn is_json(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| ct.starts_with(JSON_V1) || ct.contains("+json") || ct.starts_with("application/json"))
}

pub fn is_html(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|ct| ct.starts_with(HTML_V1) || ct.starts_with("text/html"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(base: &str) -> Upstream {
        Upstream {
            base: url::Url::parse(base).unwrap(),
            auth: None,
            token_realms: Vec::new(),
            dl_allow_private: false,
        }
    }

    fn file(url: &str, on_index: bool) -> PypiArtifact {
        PypiArtifact::File {
            project: "demo".into(),
            filename: "demo-1.0.tar.gz".into(),
            url: url::Url::parse(url).unwrap(),
            sha256: Some("AB".into()),
            on_index,
            allow_private: false,
        }
    }

    #[test]
    fn a_file_carries_its_page_digest_and_credentials_only_on_the_index() {
        let s = PypiStrategy;
        let on = file("https://mirror.example/files/demo-1.0.tar.gz", true);
        let off = file("https://files.pythonhosted.org/p/demo-1.0.tar.gz", false);
        assert_eq!(s.expected_digests(&on, &HeaderMap::new()).known(DigestAlgorithm::Sha256), Some("ab"));
        assert!(s.send_credentials(&on));
        assert_eq!(s.final_url_must_match(&on), RedirectRule::SameOrigin, "credentials never follow a hop");
        assert!(!s.send_credentials(&off), "never to another host, allowed or not");
        assert_eq!(s.url_source(&up("https://mirror.example/simple"), &off), UrlSource::Content { allow_private: false });
        let page = PypiArtifact::Page { project: "demo".into() };
        assert!(s.expected_digests(&page, &HeaderMap::new()).is_empty());
        assert_eq!(s.upstream_url(&up("https://pypi.org/simple/"), &page).unwrap().as_str(), "https://pypi.org/simple/demo/");
        assert_eq!(s.cache_policy(&on), CachePolicy::Immutable);
    }

    #[test]
    fn file_hosts_default_to_pypi_and_the_index_endpoint() {
        let u = up("https://mirror.example:8443/simple");
        let hosts = file_hosts(&u, &[]);
        let at = |s: &str| url::Url::parse(s).unwrap();
        assert!(allowed_host(&at("https://files.pythonhosted.org/x"), &hosts));
        assert!(allowed_host(&at("https://mirror.example:8443/f/x"), &hosts));
        assert!(!allowed_host(&at("https://mirror.example/f/x"), &hosts), "another port is another endpoint");
        assert!(!allowed_host(&at("https://evil.example/x"), &hosts));
        let configured = file_hosts(&u, &["cdn.example".to_string()]);
        assert!(allowed_host(&at("https://cdn.example:9/x"), &configured));
        assert!(!allowed_host(&at("https://files.pythonhosted.org/x"), &configured));
    }
}

use axum::http::{header, HeaderName, HeaderValue};

use crate::error::{AppError, AppResult};
use crate::proxy::strategy::{
    CacheKey, CachePolicy, Transfer, Ttl, UpstreamStrategy, UrlSource, DEFAULT_MAX_UPSTREAM_BYTES,
    MAX_METADATA_BYTES,
};
use crate::registry::resolve::Upstream;

use super::{compute_prefix, prefix_of};

const INDEX_TTL_SECS: u64 = 600;
const DL_MARKERS: [&str; 5] = [
    "{crate}",
    "{version}",
    "{prefix}",
    "{lowerprefix}",
    "{sha256-checksum}",
];

#[derive(Debug)]
pub enum CargoArtifact {
    Config,
    Index {
        name: String,
    },
    Crate {
        name: String,
        version: String,
        dl: String,
        cksum: String,
    },
    /// `{api}/api/v1/crates/{name}/{version}`: the only source of a
    /// publish date, an upstream-chosen host like `dl`.
    VersionMeta {
        api: reqwest::Url,
        name: String,
        version: String,
    },
}

pub struct CargoUpstream;

impl UpstreamStrategy for CargoUpstream {
    type Artifact = CargoArtifact;

    fn upstream_url(&self, up: &Upstream, a: &CargoArtifact) -> AppResult<reqwest::Url> {
        let base = up.base.as_str().trim_end_matches('/');
        let url = match a {
            CargoArtifact::Config => format!("{base}/config.json"),
            CargoArtifact::Index { name } => {
                format!("{base}/{}/{}", compute_prefix(name), name.to_lowercase())
            }
            CargoArtifact::Crate {
                name,
                version,
                dl,
                cksum,
            } => return download_url(dl, name, version, cksum, up),
            CargoArtifact::VersionMeta { api, name, version } => format!(
                "{}/api/v1/crates/{name}/{version}",
                api.as_str().trim_end_matches('/')
            ),
        };
        parse_url(&url)
    }

    fn url_source(&self, up: &Upstream, a: &CargoArtifact) -> UrlSource {
        match a {
            CargoArtifact::Crate { .. } | CargoArtifact::VersionMeta { .. } => {
                UrlSource::Content {
                    allow_private: up.dl_allow_private,
                }
            }
            CargoArtifact::Config | CargoArtifact::Index { .. } => UrlSource::Admin,
        }
    }

    fn cache_key(&self, a: &CargoArtifact) -> CacheKey {
        match a {
            CargoArtifact::Config => CacheKey {
                kind: "cargo-config",
                key: "config.json".to_string(),
            },
            CargoArtifact::Index { name } => CacheKey {
                kind: "cargo-index",
                key: name.to_lowercase(),
            },
            CargoArtifact::Crate { name, version, .. } => CacheKey {
                kind: "cargo-crate",
                key: format!("{}/{version}", name.to_lowercase()),
            },
            CargoArtifact::VersionMeta { name, version, .. } => CacheKey {
                kind: "cargo-meta",
                key: format!("{}/{version}", name.to_lowercase()),
            },
        }
    }

    fn cache_policy(&self, a: &CargoArtifact) -> CachePolicy {
        match a {
            CargoArtifact::Config => CachePolicy::Ttl(Ttl::Default),
            CargoArtifact::Index { .. } => CachePolicy::Ttl(Ttl::Secs(INDEX_TTL_SECS)),
            CargoArtifact::Crate { .. } | CargoArtifact::VersionMeta { .. } => {
                CachePolicy::Immutable
            }
        }
    }

    fn transfer(&self, a: &CargoArtifact) -> Transfer {
        match a {
            CargoArtifact::Crate { .. } => Transfer::Streamed,
            _ => Transfer::Buffered,
        }
    }

    fn max_bytes(&self, a: &CargoArtifact) -> u64 {
        match a {
            CargoArtifact::Crate { .. } => DEFAULT_MAX_UPSTREAM_BYTES,
            _ => MAX_METADATA_BYTES,
        }
    }

    // crates.io refuses API requests without an identifying agent.
    fn request_headers(&self, a: &CargoArtifact) -> Vec<(HeaderName, HeaderValue)> {
        match a {
            CargoArtifact::VersionMeta { .. } => vec![(
                header::USER_AGENT,
                HeaderValue::from_static(concat!("opencargo/", env!("CARGO_PKG_VERSION"))),
            )],
            _ => Vec::new(),
        }
    }

    fn expected_sha256(&self, a: &CargoArtifact) -> Option<String> {
        match a {
            CargoArtifact::Crate { cksum, .. } => Some(cksum.clone()),
            _ => None,
        }
    }
}

/// cargo's rule for the `dl` key of `config.json`: substitute the markers, or
/// append `/{crate}/{version}/download` when the template has none.
pub fn expand_dl_template(dl: &str, name: &str, version: &str, cksum: &str) -> String {
    if !DL_MARKERS.iter().any(|m| dl.contains(m)) {
        return format!("{}/{name}/{version}/download", dl.trim_end_matches('/'));
    }
    dl.replace("{crate}", name)
        .replace("{version}", version)
        .replace("{prefix}", &prefix_of(name))
        .replace("{lowerprefix}", &compute_prefix(name))
        .replace("{sha256-checksum}", cksum)
}

// The dl host is chosen by upstream content, not by the admin; the engine
// holds it to `is_blocked_ip` through `url_source`.
fn download_url(
    dl: &str,
    name: &str,
    version: &str,
    cksum: &str,
    up: &Upstream,
) -> AppResult<reqwest::Url> {
    let expanded = expand_dl_template(dl, name, version, cksum);
    crate::proxy::validate_upstream_url(&expanded)
        .map_err(|e| AppError::BadGateway(format!("upstream dl {expanded} refused: {e}")))?;
    let url = parse_url(&expanded)?;
    if up.auth.is_some() && !may_see_credentials(&url, up) {
        return Err(AppError::BadGateway(format!(
            "upstream dl {expanded} is off the index host {}; upstream_auth stays on that host (list the dl host in token_realms to allow it)",
            up.base.host_str().unwrap_or_default()
        )));
    }
    Ok(url)
}

// The engine sends `up.auth` with every fetch; a hostile index must not redirect it.
fn may_see_credentials(url: &reqwest::Url, up: &Upstream) -> bool {
    std::iter::once(&up.base)
        .chain(&up.token_realms)
        .any(|allowed| same_origin(url, allowed))
}

fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

fn parse_url(url: &str) -> AppResult<reqwest::Url> {
    reqwest::Url::parse(url)
        .map_err(|e| AppError::BadGateway(format!("invalid upstream URL {url}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(dl_allow_private: bool) -> Upstream {
        Upstream {
            base: reqwest::Url::parse("http://127.0.0.1:1/cargo-up/index").unwrap(),
            auth: None,
            token_realms: Vec::new(),
            dl_allow_private,
        }
    }

    fn crate_at(dl: &str) -> CargoArtifact {
        CargoArtifact::Crate {
            name: "Serde".into(),
            version: "1.0.0".into(),
            dl: dl.into(),
            cksum: "ab".repeat(32),
        }
    }

    #[test]
    fn expand_dl_template() {
        let sha = "ab".repeat(32);
        assert_eq!(
            super::expand_dl_template("https://crates.io/api/v1/crates", "Serde", "1.0.0", &sha),
            "https://crates.io/api/v1/crates/Serde/1.0.0/download"
        );
        assert_eq!(
            super::expand_dl_template("http://h/api/v1/crates/", "a", "0.1.0", &sha),
            "http://h/api/v1/crates/a/0.1.0/download"
        );
        assert_eq!(
            super::expand_dl_template(
                "https://static/{prefix}/{lowerprefix}/{crate}-{version}.crate?s={sha256-checksum}",
                "Serde",
                "1.0.0",
                &sha
            ),
            format!("https://static/Se/rd/se/rd/Serde-1.0.0.crate?s={sha}")
        );
        assert_eq!(
            super::expand_dl_template("https://static/{crate}", "abc", "2", &sha),
            "https://static/abc",
            "a template with one marker is never appended to"
        );
    }

    #[test]
    fn dl_is_upstream_content_held_to_the_private_optin() {
        let s = CargoUpstream;
        let private = crate_at("http://127.0.0.1:1/dl");
        for allow in [false, true] {
            assert_eq!(
                s.url_source(&upstream(allow), &private),
                UrlSource::Content {
                    allow_private: allow
                }
            );
            assert_eq!(
                s.upstream_url(&upstream(allow), &private).unwrap().as_str(),
                "http://127.0.0.1:1/dl/Serde/1.0.0/download"
            );
        }
        for admin_chosen in [CargoArtifact::Config, CargoArtifact::Index { name: "a".into() }] {
            assert_eq!(s.url_source(&upstream(false), &admin_chosen), UrlSource::Admin);
        }
        let link_local = crate_at("http://169.254.169.254/latest");
        assert!(
            matches!(
                s.upstream_url(&upstream(true), &link_local),
                Err(AppError::BadGateway(_))
            ),
            "link-local is refused even with the opt-in"
        );
        assert!(matches!(
            s.upstream_url(&upstream(true), &crate_at("ftp://h/x")),
            Err(AppError::BadGateway(_))
        ));
    }

    #[test]
    fn dl_off_host_refused_when_upstream_authenticated() {
        let s = CargoUpstream;
        let mut up = upstream(true);
        up.auth = Some(crate::proxy::UpstreamAuth::Bearer { token: "t".into() });
        assert!(
            s.upstream_url(&up, &crate_at("http://127.0.0.1:1/dl")).is_ok(),
            "a dl on the index host keeps working"
        );
        let off_host = crate_at("https://static.crates.io/crates");
        let err = s.upstream_url(&up, &off_host).unwrap_err();
        assert!(
            matches!(err, AppError::BadGateway(ref m) if m.contains("static.crates.io") && m.contains("token_realms")),
            "{err}"
        );
        up.token_realms = vec![reqwest::Url::parse("https://static.crates.io/token").unwrap()];
        assert!(
            s.upstream_url(&up, &off_host).is_ok(),
            "a host listed in token_realms may see the credentials"
        );
        let plaintext = crate_at("http://static.crates.io:443/crates");
        assert!(
            matches!(
                s.upstream_url(&up, &plaintext),
                Err(AppError::BadGateway(_))
            ),
            "the same host over another scheme is another origin"
        );
        up.auth = None;
        up.token_realms.clear();
        assert!(
            s.upstream_url(&up, &off_host).is_ok(),
            "without credentials there is nothing to protect"
        );
    }

    #[test]
    fn index_and_config_urls_and_keys() {
        let s = CargoUpstream;
        let up = upstream(false);
        assert_eq!(
            s.upstream_url(&up, &CargoArtifact::Config)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:1/cargo-up/index/config.json"
        );
        let index = CargoArtifact::Index {
            name: "MyCrate".into(),
        };
        assert_eq!(
            s.upstream_url(&up, &index).unwrap().as_str(),
            "http://127.0.0.1:1/cargo-up/index/my/cr/mycrate"
        );
        assert_eq!(s.cache_key(&index).key, "mycrate");
        assert_eq!(s.cache_policy(&index), CachePolicy::Ttl(Ttl::Secs(600)));
        assert_eq!(s.max_bytes(&index), MAX_METADATA_BYTES);
        assert_eq!(s.max_bytes(&CargoArtifact::Config), MAX_METADATA_BYTES);
        let c = crate_at("http://h/dl");
        assert_eq!(s.cache_key(&c).key, "serde/1.0.0", "one row per crate, any casing");
        assert_eq!(s.cache_policy(&c), CachePolicy::Immutable);
        assert_eq!(s.transfer(&c), Transfer::Streamed);
        assert_eq!(
            s.expected_sha256(&c).as_deref(),
            Some("ab".repeat(32).as_str())
        );
    }

    #[test]
    fn version_meta_is_paced_api_content_with_an_agent() {
        let s = CargoUpstream;
        let meta = CargoArtifact::VersionMeta {
            api: reqwest::Url::parse("https://crates.io/").unwrap(),
            name: "Serde".into(),
            version: "1.0.0".into(),
        };
        assert_eq!(
            s.upstream_url(&upstream(false), &meta).unwrap().as_str(),
            "https://crates.io/api/v1/crates/Serde/1.0.0"
        );
        assert_eq!(
            s.url_source(&upstream(true), &meta),
            UrlSource::Content { allow_private: true }
        );
        let key = s.cache_key(&meta);
        assert_eq!((key.kind, key.key.as_str()), ("cargo-meta", "serde/1.0.0"));
        assert_eq!(s.cache_policy(&meta), CachePolicy::Immutable);
        assert_eq!(s.transfer(&meta), Transfer::Buffered);
        assert_eq!(s.max_bytes(&meta), MAX_METADATA_BYTES);
        let headers = s.request_headers(&meta);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, header::USER_AGENT);
        assert!(headers[0].1.to_str().unwrap().starts_with("opencargo/"));
        assert!(s.request_headers(&CargoArtifact::Config).is_empty());
    }
}

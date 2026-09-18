//! A NuGet proxy member: its upstream is a v3 service index, and every other
//! URL is read out of upstream documents. Those are `UrlSource::Content`,
//! never sent credentials when they leave the declared origin, and every
//! redirect must stay on the origin it was asked of.
//!
//! The `.nupkg` is verified against the sha512 the registration carries, or
//! else the catalog leaf it names (nuget.org puts `packageHash` only there);
//! a package neither declares is the one `none()` of this strategy.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use base64::Engine;
use bytes::Bytes;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::domain::{CacheRepo, Outcome};
use crate::proxy::strategy::{
    CacheKey, CachePolicy, DigestAlgorithm, DigestSource, ExpectedDigests, RedirectRule, Transfer,
    Ttl, UpstreamStrategy, UrlSource, MAX_METADATA_BYTES,
};
use crate::proxy::{IntoPayload, Payload};
use crate::registry::resolve::{Cx, ResolveError, Upstream};

use super::leaves::Coordinates;
use super::merged::Sources;
use super::model::Entry;
use super::search::{Found, Hit, SearchParams};

const MAX_NUPKG_BYTES: u64 = 512 * 1024 * 1024;
const INDEX_TTL_SECS: u64 = 3600;
const SEARCH_TTL_SECS: u64 = 300;
const PARSE_PERMITS: usize = 4;
const PARSE_WAIT: Duration = Duration::from_secs(10);
const MAX_PAGES: usize = 64;

#[derive(Debug, Clone)]
pub enum NugetArtifact {
    ServiceIndex,
    Registration { url: url::Url },
    Page { url: url::Url },
    CatalogLeaf { url: url::Url },
    Nuspec { url: url::Url, id: String, key: String },
    Nupkg { url: url::Url, id: String, key: String, sha512: Option<String> },
    Search { url: url::Url },
}

impl NugetArtifact {
    fn url(&self) -> Option<&url::Url> {
        match self {
            NugetArtifact::ServiceIndex => None,
            NugetArtifact::Registration { url }
            | NugetArtifact::Page { url }
            | NugetArtifact::CatalogLeaf { url }
            | NugetArtifact::Nuspec { url, .. }
            | NugetArtifact::Nupkg { url, .. }
            | NugetArtifact::Search { url } => Some(url),
        }
    }
}

pub struct NugetUpstream {
    origin: url::Origin,
}

impl NugetUpstream {
    pub fn of(up: &Upstream) -> Self {
        Self {
            origin: up.base.origin(),
        }
    }

    fn on_origin(&self, a: &NugetArtifact) -> bool {
        a.url().is_none_or(|u| u.origin() == self.origin)
    }
}

impl UpstreamStrategy for NugetUpstream {
    type Artifact = NugetArtifact;

    fn upstream_url(&self, up: &Upstream, a: &NugetArtifact) -> Result<url::Url, ResolveError> {
        Ok(match a.url() {
            Some(url) => url.clone(),
            None => up.base.clone(),
        })
    }

    fn url_source(&self, up: &Upstream, a: &NugetArtifact) -> UrlSource {
        match a {
            NugetArtifact::ServiceIndex => UrlSource::Admin,
            _ => UrlSource::Content {
                allow_private: up.dl_allow_private,
            },
        }
    }

    fn cache_key(&self, a: &NugetArtifact) -> CacheKey {
        let (kind, key) = match a {
            NugetArtifact::ServiceIndex => ("nuget-index", String::new()),
            NugetArtifact::Registration { url } => ("nuget-registration", url.to_string()),
            NugetArtifact::Page { url } => ("nuget-page", url.to_string()),
            NugetArtifact::CatalogLeaf { url } => ("nuget-catalog", url.to_string()),
            NugetArtifact::Nuspec { id, key, .. } => ("nuget-nuspec", format!("{id}/{key}")),
            NugetArtifact::Nupkg { id, key, sha512, .. } => (
                "nuget-nupkg",
                format!("{id}/{key}/{}", sha512.as_deref().unwrap_or("-")),
            ),
            NugetArtifact::Search { url } => ("nuget-search", url.to_string()),
        };
        CacheKey { kind, key }
    }

    fn cache_policy(&self, a: &NugetArtifact) -> CachePolicy {
        match a {
            NugetArtifact::ServiceIndex => CachePolicy::Ttl(Ttl::Secs(INDEX_TTL_SECS)),
            NugetArtifact::Search { .. } => CachePolicy::Ttl(Ttl::Secs(SEARCH_TTL_SECS)),
            NugetArtifact::Registration { .. } | NugetArtifact::Page { .. } => {
                CachePolicy::Ttl(Ttl::Default)
            }
            NugetArtifact::CatalogLeaf { .. }
            | NugetArtifact::Nuspec { .. }
            | NugetArtifact::Nupkg { .. } => CachePolicy::Immutable,
        }
    }

    /// Documents verify nothing, and neither does a package no upstream
    /// document declares a hash for: listed by the C3 ratchet.
    fn expected_digests(&self, a: &NugetArtifact, _h: &HeaderMap) -> ExpectedDigests {
        match a {
            NugetArtifact::Nupkg {
                sha512: Some(hex), ..
            } => ExpectedDigests::none().with(DigestAlgorithm::Sha512, hex, DigestSource::Known),
            _ => ExpectedDigests::none(),
        }
    }

    fn send_credentials(&self, a: &NugetArtifact) -> bool {
        self.on_origin(a)
    }

    fn final_url_must_match(&self, _a: &NugetArtifact) -> RedirectRule {
        RedirectRule::SameOrigin
    }

    fn transfer(&self, a: &NugetArtifact) -> Transfer {
        match a {
            NugetArtifact::Nupkg { .. } => Transfer::Streamed,
            _ => Transfer::Buffered,
        }
    }

    fn max_bytes(&self, a: &NugetArtifact) -> u64 {
        match a {
            NugetArtifact::Nupkg { .. } => MAX_NUPKG_BYTES,
            _ => MAX_METADATA_BYTES,
        }
    }
}

fn permits() -> &'static Semaphore {
    static PERMITS: OnceLock<Semaphore> = OnceLock::new();
    PERMITS.get_or_init(|| Semaphore::new(PARSE_PERMITS))
}

/// The one place an upstream document is decoded: gunzipped when it is
/// gzip, parsed as JSON, under a process-wide permit whose wait is bounded.
pub async fn parse_metadata(bytes: Bytes) -> Result<Value, ResolveError> {
    let _permit = tokio::time::timeout(PARSE_WAIT, permits().acquire())
        .await
        .map_err(|_| ResolveError::Upstream("upstream document parsing is saturated".into()))?
        .map_err(|_| ResolveError::Internal("parse permits closed".into()))?;
    tokio::task::spawn_blocking(move || {
        let gz = bytes.starts_with(&[0x1f, 0x8b]);
        if gz {
            use std::io::Read as _;
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&bytes[..])
                .take(MAX_METADATA_BYTES + 1)
                .read_to_end(&mut out)
                .map_err(|e| ResolveError::Upstream(format!("invalid gzip from upstream: {e}")))?;
            if out.len() as u64 > MAX_METADATA_BYTES {
                return Err(ResolveError::Upstream("upstream document too large".into()));
            }
            serde_json::from_slice(&out)
        } else {
            serde_json::from_slice(&bytes)
        }
        .map_err(|e| ResolveError::Upstream(format!("invalid upstream document: {e}")))
    })
    .await
    .map_err(|e| ResolveError::Internal(format!("parse task failed: {e}")))?
}

/// A cached upstream document, parsed once per body: a document already
/// parsed is not parsed again, so a warm group takes no parse permit.
async fn document(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    a: &NugetArtifact,
) -> Result<Outcome<Arc<Value>>, ResolveError> {
    let engine = cx.proxy;
    let Outcome::Found(cached) = engine.fetch(&NugetUpstream::of(up), up, member, a).await? else {
        return Ok(Outcome::NotFound);
    };
    let parse = || async {
        let bytes = engine.bytes(&cached).await?;
        let size = bytes.len();
        Ok::<_, ResolveError>((Arc::new(parse_metadata(bytes).await?), size))
    };
    let (value, _) = match cached.entry.digest.clone() {
        Some(digest) => {
            super::merged::parsed()
                .get_or_compute(digest, Sources { stamps: Vec::new(), upstream: true }, || async {
                    parse().await.map(|parsed| (parsed, true))
                })
                .await?
        }
        None => parse().await?,
    };
    Ok(Outcome::Found(value))
}

/// The resources a service index announces.
pub struct Resources {
    flat: Option<url::Url>,
    registration: Option<url::Url>,
    search: Option<url::Url>,
}

fn resource(index: &Value, types: &[&str]) -> Option<url::Url> {
    let resources = index.get("resources")?.as_array()?;
    types.iter().find_map(|t| {
        resources
            .iter()
            .find(|r| r.get("@type").and_then(Value::as_str) == Some(t))
            .and_then(|r| r.get("@id")?.as_str())
            .and_then(|u| url::Url::parse(u).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
    })
}

pub fn resources_of(index: &Value) -> Resources {
    Resources {
        flat: resource(index, &["PackageBaseAddress/3.0.0"]),
        registration: resource(
            index,
            &[
                "RegistrationsBaseUrl/3.6.0",
                "RegistrationsBaseUrl/3.4.0",
                "RegistrationsBaseUrl",
            ],
        ),
        search: resource(index, &["SearchQueryService/3.5.0", "SearchQueryService"]),
    }
}

async fn resources(cx: &Cx<'_>, member: CacheRepo<'_>, up: &Upstream) -> Result<Resources, ResolveError> {
    match document(cx, member, up, &NugetArtifact::ServiceIndex).await? {
        Outcome::Found(index) => Ok(resources_of(&index)),
        Outcome::NotFound => Err(ResolveError::Upstream("the upstream has no service index".into())),
    }
}

/// `base` with `segments` appended one by one, each percent-encoded.
pub fn under(base: &url::Url, segments: &[&str]) -> Result<url::Url, ResolveError> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|_| ResolveError::Upstream(format!("{base} cannot be a base")))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

fn missing(what: &str) -> ResolveError {
    ResolveError::Upstream(format!("the upstream announces no {what}"))
}

/// Every `catalogEntry` of a registration, fetching the pages it does not
/// inline; each paired with the `packageHash` it may carry.
async fn catalog_entries(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    id: &str,
) -> Result<Outcome<Vec<(Entry, Option<String>)>>, ResolveError> {
    let base = resources(cx, member, up).await?.registration.ok_or_else(|| missing("registration"))?;
    let url = under(&base, &[id, "index.json"])?;
    let Outcome::Found(index) = document(cx, member, up, &NugetArtifact::Registration { url }).await? else {
        return Ok(Outcome::NotFound);
    };
    let mut out = Vec::new();
    let pages = index.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    for page in pages.iter().take(MAX_PAGES) {
        let items = match page.get("items").and_then(Value::as_array) {
            Some(items) => items.clone(),
            None => {
                let Some(url) = page.get("@id").and_then(Value::as_str).and_then(|u| url::Url::parse(u).ok()) else {
                    continue;
                };
                match document(cx, member, up, &NugetArtifact::Page { url }).await? {
                    Outcome::Found(doc) => doc.get("items").and_then(Value::as_array).cloned().unwrap_or_default(),
                    Outcome::NotFound => continue,
                }
            }
        };
        for item in items {
            let Some(catalog) = item.get("catalogEntry") else { continue };
            if let Some(entry) = Entry::from_catalog_entry(catalog) {
                out.push((entry, package_hash(catalog)));
            }
        }
    }
    Ok(Outcome::Found(out))
}

/// A `packageHash` as lowercase hex, when its algorithm is SHA512.
pub fn package_hash(doc: &Value) -> Option<String> {
    let alg = doc.get("packageHashAlgorithm").and_then(Value::as_str).unwrap_or("SHA512");
    if !alg.eq_ignore_ascii_case("sha512") {
        return None;
    }
    let raw = doc.get("packageHash").and_then(Value::as_str)?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(raw).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub async fn entries(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    id: &str,
) -> Result<Outcome<Vec<Entry>>, ResolveError> {
    Ok(match catalog_entries(cx, member, up, id).await? {
        Outcome::Found(found) if !found.is_empty() => {
            let mut entries: Vec<Entry> = found.into_iter().map(|(e, _)| e).collect();
            super::model::sort(&mut entries);
            Outcome::Found(entries)
        }
        _ => Outcome::NotFound,
    })
}

/// The sha512 a package must match: the registration's, else its catalog
/// leaf's, else none; and when the registration says, its publish date.
async fn expected_sha512(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    at: &Coordinates,
    key: &str,
) -> Result<(Option<String>, Option<DateTime<Utc>>), ResolveError> {
    let Outcome::Found(found) = catalog_entries(cx, member, up, &at.id).await? else {
        return Ok((None, None));
    };
    let Some((entry, hash)) = found.into_iter().find(|(e, _)| e.key == key) else {
        return Ok((None, None));
    };
    let published = entry.published.filter(|_| entry.listed);
    if hash.is_some() {
        return Ok((hash, published));
    }
    let Some(url) = entry.catalog.as_deref().and_then(|u| url::Url::parse(u).ok()) else {
        return Ok((None, published));
    };
    let hash = match document(cx, member, up, &NugetArtifact::CatalogLeaf { url }).await? {
        Outcome::Found(leaf) => package_hash(&leaf),
        Outcome::NotFound => None,
    };
    Ok((hash, published))
}

pub async fn nupkg(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    at: &Coordinates,
) -> Result<Outcome<Payload>, ResolveError> {
    let Some(key) = at.key.as_deref() else {
        return Ok(Outcome::NotFound);
    };
    let flat = resources(cx, member, up).await?.flat.ok_or_else(|| missing("flat container"))?;
    let (sha512, published) = expected_sha512(cx, member, up, at, key).await?;
    if sha512.is_none() {
        tracing::info!(member = %member.0.name, id = %at.id, key, "no upstream document declares this package's hash; served unverified");
    }
    let file = format!("{}.{key}.nupkg", at.id);
    let artifact = NugetArtifact::Nupkg {
        url: under(&flat, &[&at.id, key, &file])?,
        id: at.id.clone(),
        key: key.to_string(),
        sha512,
    };
    let cached = cx.proxy.fetch(&NugetUpstream::of(up), up, member, &artifact).await?;
    if let Outcome::Found(c) = &cached {
        crate::policy::record(cx, member, up, crate::domain::Format::Nuget, &at.id, Some(key.to_string()), || {
            crate::policy::Source::Nuget {
                body: c.clone(),
                published,
            }
        });
    }
    Ok(cached.into_payload())
}

pub async fn nuspec(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    at: &Coordinates,
) -> Result<Outcome<Payload>, ResolveError> {
    let Some(key) = at.key.as_deref() else {
        return Ok(Outcome::NotFound);
    };
    let flat = resources(cx, member, up).await?.flat.ok_or_else(|| missing("flat container"))?;
    let file = format!("{}.nuspec", at.id);
    let artifact = NugetArtifact::Nuspec {
        url: under(&flat, &[&at.id, key, &file])?,
        id: at.id.clone(),
        key: key.to_string(),
    };
    Ok(cx.proxy.fetch(&NugetUpstream::of(up), up, member, &artifact).await?.into_payload())
}

pub async fn search(
    cx: &Cx<'_>,
    member: CacheRepo<'_>,
    up: &Upstream,
    params: &SearchParams,
) -> Result<Outcome<Found>, ResolveError> {
    let Some(mut url) = resources(cx, member, up).await?.search else {
        return Ok(Outcome::NotFound);
    };
    {
        let mut q = url.query_pairs_mut();
        q.clear();
        q.append_pair("q", params.q.as_deref().unwrap_or(""));
        q.append_pair("skip", "0");
        q.append_pair("take", &(params.skip() + params.take()).to_string());
        q.append_pair("prerelease", if params.prerelease.unwrap_or(false) { "true" } else { "false" });
        if params.semver2() {
            q.append_pair("semVerLevel", "2.0.0");
        }
        if let Some(t) = &params.package_type {
            q.append_pair("packageType", t);
        }
    }
    let Outcome::Found(doc) = document(cx, member, up, &NugetArtifact::Search { url }).await? else {
        return Ok(Outcome::NotFound);
    };
    let total = doc.get("totalHits").and_then(Value::as_u64).unwrap_or(0);
    let hits = doc
        .get("data")
        .and_then(Value::as_array)
        .map(|data| data.iter().filter_map(search_hit).collect())
        .unwrap_or_default();
    Ok(Outcome::Found(Found { total, hits }))
}

/// One upstream search hit, its URLs dropped.
fn search_hit(item: &Value) -> Option<Hit> {
    let id = item.get("id")?.as_str()?;
    let mut base = item.clone();
    let versions: Vec<String> = item
        .get("versions")
        .and_then(Value::as_array)
        .map(|vs| vs.iter().filter_map(|v| v.get("version")?.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let mut entries: Vec<Entry> = versions
        .iter()
        .filter_map(|v| {
            base["version"] = Value::String(v.clone());
            base["listed"] = Value::Bool(true);
            if let Some(obj) = base.as_object_mut() {
                obj.remove("@id");
            }
            Entry::from_catalog_entry(&base)
        })
        .collect();
    super::model::sort(&mut entries);
    Some(Hit {
        key: id.to_ascii_lowercase(),
        entries,
        downloads: item.get("totalDownloads").and_then(Value::as_i64).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn up(base: &str) -> Upstream {
        Upstream {
            base: url::Url::parse(base).unwrap(),
            auth: None,
            token_realms: Vec::new(),
            dl_allow_private: false,
        }
    }

    #[test]
    fn nuget_org_shaped_registration_yields_declared_digest_policy() {
        let registration_entry = json!({"@id": "https://api.nuget.org/v3/catalog0/data/a.json", "id": "A", "version": "1.0.0"});
        assert_eq!(package_hash(&registration_entry), None, "nuget.org's registration carries no hash");
        let leaf = json!({"packageHash": base64::engine::general_purpose::STANDARD.encode([0xab, 0xcd]), "packageHashAlgorithm": "SHA512"});
        assert_eq!(package_hash(&leaf).as_deref(), Some("abcd"), "the catalog leaf does");
        assert_eq!(package_hash(&json!({"packageHash": "qw==", "packageHashAlgorithm": "SHA256"})), None);

        let s = NugetUpstream::of(&up("https://api.nuget.org/v3/index.json"));
        let url = url::Url::parse("https://api.nuget.org/v3-flatcontainer/a/1.0.0/a.1.0.0.nupkg").unwrap();
        let known = NugetArtifact::Nupkg { url: url.clone(), id: "a".into(), key: "1.0.0".into(), sha512: Some("abcd".into()) };
        assert_eq!(s.expected_digests(&known, &HeaderMap::new()).known(DigestAlgorithm::Sha512), Some("abcd"));
        let unknown = NugetArtifact::Nupkg { url, id: "a".into(), key: "1.0.0".into(), sha512: None };
        assert!(s.expected_digests(&unknown, &HeaderMap::new()).is_empty());
        assert_ne!(s.cache_key(&known), s.cache_key(&unknown), "a new announced digest refetches");
    }

    #[test]
    fn credentials_and_redirects_follow_the_declared_origin() {
        let u = up("https://feed.example/v3/index.json");
        let s = NugetUpstream::of(&u);
        let on = NugetArtifact::Search { url: url::Url::parse("https://feed.example/query").unwrap() };
        let off = NugetArtifact::Search { url: url::Url::parse("https://search.other.example/query").unwrap() };
        assert!(s.send_credentials(&on));
        assert!(!s.send_credentials(&off));
        assert!(s.send_credentials(&NugetArtifact::ServiceIndex));
        assert_eq!(s.final_url_must_match(&off), RedirectRule::SameOrigin);
        assert_eq!(s.url_source(&u, &NugetArtifact::ServiceIndex), UrlSource::Admin);
        assert!(matches!(s.url_source(&u, &on), UrlSource::Content { .. }));
    }

    #[test]
    fn urls_are_built_segment_by_segment() {
        let base = url::Url::parse("https://f.example/flat/").unwrap();
        let url = under(&base, &["a/../b", "1.0.0", "x y.nupkg"]).unwrap();
        assert_eq!(url.as_str(), "https://f.example/flat/a%2F..%2Fb/1.0.0/x%20y.nupkg");
        let index = json!({"resources": [
            {"@id": "https://f.example/flat/", "@type": "PackageBaseAddress/3.0.0"},
            {"@id": "https://f.example/reg/", "@type": "RegistrationsBaseUrl"},
            {"@id": "https://f.example/reg-gz/", "@type": "RegistrationsBaseUrl/3.6.0"},
            {"@id": "file:///etc/passwd", "@type": "SearchQueryService"}
        ]});
        let r = resources_of(&index);
        assert_eq!(r.registration.unwrap().as_str(), "https://f.example/reg-gz/");
        assert!(r.search.is_none(), "only http(s) resources");
        assert!(r.flat.is_some());
    }

    #[tokio::test]
    async fn gzipped_documents_decode_in_one_place() {
        use std::io::Write as _;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(br#"{"a":1}"#).unwrap();
        let v = parse_metadata(Bytes::from(gz.finish().unwrap())).await.unwrap();
        assert_eq!(v["a"], 1);
        assert!(parse_metadata(Bytes::from_static(b"nope")).await.is_err());
    }
}

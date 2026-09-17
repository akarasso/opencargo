mod npm;
mod oci;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;
use tokio::time::timeout;
use tracing::debug;

use crate::proxy::engine::Cached;
use crate::proxy::UpstreamStrategy;
use crate::registry::cargo::upstream::{CargoArtifact, CargoUpstream};
use crate::registry::go::escape::escape;
use crate::registry::go::upstream::{FileKind, GoArtifact, GoUpstream};
use crate::registry::resolve::{CacheRepo, Outcome, Upstream};

use super::rules::PolicyConfig;
use super::{Facts, Pending, Resolution, Shared, Source};

pub use npm::{npm_version, NpmSlot, PackageFacts, VersionFacts};
pub use oci::{oci_blob, oci_children};
pub(crate) use oci::{oci_classify, oci_published_at, release_parked, sweep_children};

/// RFC 3339, then SQLite's `%Y-%m-%d %H:%M:%S` as UTC, else `None`.
pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|t| t.and_utc())
        })
}

/// Every reader swallows its errors into `None` and names the outcome in
/// `facts.date_source`; the digest is moved out of the source, never gathered.
pub(crate) async fn gather(shared: &Shared, cfg: &PolicyConfig, p: Pending) -> Resolution {
    let Pending {
        requested_repo,
        member,
        upstream,
        format,
        name,
        version,
        actor,
        source,
    } = p;
    let repo = CacheRepo(&member);
    let mut facts = Facts::default();
    let (digest, version, published_at) = match source {
        Source::Npm { filename, digest } => {
            let (version, at, scripts, source) =
                npm::npm(shared, cfg, repo, &upstream, &name, &filename).await;
            facts.install_scripts = scripts;
            facts.date_source = source;
            (digest, version, at)
        }
        Source::Cargo { cksum } => {
            let (at, source) = match (&cfg.min_release_age, &version) {
                (Some(_), Some(v)) => {
                    cargo_published_at(shared, cfg, repo, &upstream, &name, v).await
                }
                _ => (None, "none"),
            };
            facts.date_source = source;
            (Some(cksum), version, at)
        }
        Source::Go { digest } => {
            let (at, source) = match (&cfg.min_release_age, &version) {
                (Some(_), Some(v)) => go_published_at(shared, cfg, repo, &upstream, &name, v).await,
                _ => (None, "none"),
            };
            facts.date_source = source;
            (digest, version, at)
        }
        Source::Oci { body, served } => {
            let (at, source) = if cfg.min_release_age.is_some() {
                oci_published_at(shared, cfg, repo, &upstream, &name, &body, served.as_ref()).await
            } else {
                (None, "none")
            };
            facts.date_source = source;
            let digest = body
                .entry
                .digest
                .as_deref()
                .map(|hex| format!("sha256:{hex}"));
            (digest, version, at)
        }
    };
    Resolution {
        requested_repo,
        member_repo: member.name,
        format,
        name,
        version,
        digest,
        actor,
        published_at,
        facts,
    }
}

/// `peek`, then a bounded `fetch` only under `fetch_missing_facts`.
pub(crate) async fn fetch_or_peek<S: UpstreamStrategy>(
    shared: &Shared,
    cfg: &PolicyConfig,
    s: &S,
    up: &Upstream,
    member: CacheRepo<'_>,
    a: &S::Artifact,
) -> (Option<Cached>, &'static str) {
    match shared.proxy.peek(s, member, a).await {
        Ok(Some(cached)) => return (Some(cached), "cache"),
        Ok(None) => {}
        Err(e) => {
            debug!(artifact = ?a, error = %e, "cache peek failed");
            return (None, "failed");
        }
    }
    fetch_missing(shared, cfg, s, up, member, a).await
}

/// The recorder's own upstream request goes through `observe`, never
/// `fetch`: a miss leaves no negative row, so recording never turns a
/// client's next request into a 404.
pub(crate) async fn fetch_missing<S: UpstreamStrategy>(
    shared: &Shared,
    cfg: &PolicyConfig,
    s: &S,
    up: &Upstream,
    member: CacheRepo<'_>,
    a: &S::Artifact,
) -> (Option<Cached>, &'static str) {
    if !cfg.fetch_missing_facts {
        return (None, "not-fetched");
    }
    match timeout(
        shared.tuning.gather_timeout,
        shared.proxy.observe(s, up, member, a),
    )
    .await
    {
        Err(_) => (None, "timeout"),
        Ok(Ok(Outcome::Found(cached))) => (Some(cached), "fetch"),
        Ok(Ok(Outcome::NotFound)) => (None, "not-found"),
        Ok(Err(e)) => {
            debug!(artifact = ?a, error = %e, "fact fetch failed");
            (None, "failed")
        }
    }
}

pub(crate) async fn read_json(shared: &Shared, cached: &Cached) -> Option<Value> {
    let bytes = shared.proxy.bytes(cached).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `config.json` (peeked) gives the API base; the version row is peeked,
/// else fetched through the crates.io pacer.
pub(crate) async fn cargo_published_at(
    shared: &Shared,
    cfg: &PolicyConfig,
    member: CacheRepo<'_>,
    up: &Upstream,
    name: &str,
    version: &str,
) -> (Option<DateTime<Utc>>, &'static str) {
    let config = match shared
        .proxy
        .peek(&CargoUpstream, member, &CargoArtifact::Config)
        .await
    {
        Ok(Some(cached)) => read_json(shared, &cached).await,
        _ => None,
    };
    let api = config
        .as_ref()
        .and_then(|c| c["api"].as_str())
        .and_then(|api| reqwest::Url::parse(api).ok());
    let Some(api) = api else {
        return (None, "none");
    };
    let a = CargoArtifact::VersionMeta {
        api,
        name: name.to_string(),
        version: version.to_string(),
    };
    let (cached, source) = match shared.proxy.peek(&CargoUpstream, member, &a).await {
        Ok(Some(cached)) => (Some(cached), "cache"),
        _ => shared.cargo_pacer.fetch(shared, cfg, member, up, &a).await,
    };
    let Some(cached) = cached else {
        return (None, source);
    };
    let created = read_json(shared, &cached)
        .await
        .and_then(|v| v["version"]["created_at"].as_str().and_then(parse_time));
    (created, source)
}

/// The `.info` document's `Time`, from the cache the client's own request
/// filled, else one fetch.
pub(crate) async fn go_published_at(
    shared: &Shared,
    cfg: &PolicyConfig,
    member: CacheRepo<'_>,
    up: &Upstream,
    name: &str,
    version: &str,
) -> (Option<DateTime<Utc>>, &'static str) {
    let a = GoArtifact::File {
        module: escape(name),
        version: escape(version),
        kind: FileKind::Info,
    };
    let (cached, source) = fetch_or_peek(shared, cfg, &GoUpstream, up, member, &a).await;
    let Some(cached) = cached else {
        return (None, source);
    };
    let time = read_json(shared, &cached)
        .await
        .and_then(|v| v["Time"].as_str().and_then(parse_time));
    (time, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::kinds::Format;
    use crate::db::proxy_cache::CacheEntry;
    use crate::policy::testing::{engine_over, fast, pending, repo};
    use crate::proxy::engine::fixture::Fx;

    #[test]
    fn parse_time_rfc3339_with_millis() {
        let t = parse_time("2026-09-17T07:10:00.123Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-09-17T07:10:00.123+00:00");
        let offset = parse_time("2026-09-17T09:10:00+02:00").unwrap();
        assert_eq!(offset.to_rfc3339(), "2026-09-17T07:10:00+00:00");
    }

    #[test]
    fn parse_time_sqlite_shape_is_utc() {
        let t = parse_time("2026-09-17 07:10:00").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-09-17T07:10:00+00:00");
    }

    #[test]
    fn parse_time_garbage_is_none() {
        for bad in [
            "",
            "yesterday",
            "2026-09-17",
            "1700000000",
            "2026-13-01T00:00:00Z",
        ] {
            assert!(parse_time(bad).is_none(), "{bad}");
        }
    }

    fn cached(hex: &str) -> Cached {
        Cached {
            entry: CacheEntry {
                id: 1,
                repository_id: 1,
                kind: "k".into(),
                cache_key: "c".into(),
                status: 200,
                storage_path: Some("nowhere".into()),
                content_type: None,
                etag: None,
                digest: Some(hex.into()),
                size: 0,
                fetched_at: String::new(),
                expires_at: None,
                last_used_at: String::new(),
            },
            stale: false,
        }
    }

    #[tokio::test]
    async fn digest_comes_from_source_for_every_format() {
        let fx = Fx::new().await;
        let (engine, _writer) = engine_over(&fx, PolicyConfig::default(), fast());
        let shared = engine.shared();
        let cfg = PolicyConfig {
            typosquat: true,
            ..Default::default()
        };
        let cases = [
            (
                Format::Npm,
                Source::Npm {
                    filename: "lodash-1.0.0.tgz".into(),
                    digest: Some("aa".into()),
                },
                Some("aa"),
            ),
            (
                Format::Cargo,
                Source::Cargo { cksum: "bb".into() },
                Some("bb"),
            ),
            (Format::Go, Source::Go { digest: None }, None),
            (
                Format::Oci,
                Source::Oci {
                    body: cached("cc"),
                    served: None,
                },
                Some("sha256:cc"),
            ),
        ];
        for (format, source, digest) in cases {
            let member = repo(fx.repo.id, "p", format);
            let p = pending(&member, &fx.up, format, "lodash", source);
            let r = gather(shared, &cfg, p).await;
            assert_eq!(r.digest.as_deref(), digest, "{format:?}");
            assert_eq!(
                r.facts.date_source, "none",
                "{format:?}: no rule needs a date"
            );
            assert_eq!(r.published_at, None);
            assert_eq!(r.member_repo, "p");
            assert_eq!(r.requested_repo, "requested");
        }
        assert!(fx.hits().is_empty(), "no rule on, no request");
    }

    #[tokio::test]
    async fn missing_fact_never_writes_a_negative_row() {
        use crate::registry::npm::upstream::{NpmArtifact, NpmUpstream};

        let fx = Fx::new().await;
        fx.set(|s| s.gone = true);
        let cfg = PolicyConfig {
            min_release_age: Some("1h".parse().unwrap()),
            ..Default::default()
        };
        let (engine, _writer) = engine_over(&fx, cfg.clone(), fast());
        let shared = engine.shared();
        let member = CacheRepo(&fx.repo);
        let mut up = fx.up.clone();
        up.dl_allow_private = true;
        let packument = NpmArtifact::Metadata {
            name: "widget".into(),
        };
        let (cached, source) =
            fetch_missing(shared, &cfg, &NpmUpstream, &up, member, &packument).await;
        assert!(cached.is_none());
        assert_eq!(source, "not-found");
        let meta = CargoArtifact::VersionMeta {
            api: fx.up.base.clone(),
            name: "serde".into(),
            version: "1.0.0".into(),
        };
        let (cached, source) = shared
            .cargo_pacer
            .fetch(shared, &cfg, member, &up, &meta)
            .await;
        assert!(cached.is_none());
        assert_eq!(source, "not-found");
        assert_eq!(fx.hits().len(), 2, "both asked upstream once");
        for key in [
            NpmUpstream.cache_key(&packument),
            CargoUpstream.cache_key(&meta),
        ] {
            assert!(
                fx.row(key.kind, &key.key).await.is_none(),
                "{}/{}: a recorder miss leaves no negative row",
                key.kind,
                key.key
            );
        }
        fx.set(|s| s.gone = false);
        fx.set(|s| s.body = br#"{"name":"widget","versions":{},"time":{}}"#.to_vec());
        let served = shared
            .proxy
            .fetch(&NpmUpstream, &up, member, &packument)
            .await
            .unwrap();
        assert!(
            matches!(served, Outcome::Found(_)),
            "the next client request reaches upstream"
        );
        assert_eq!(fx.hits().len(), 3);
    }
}

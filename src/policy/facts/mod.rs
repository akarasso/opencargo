pub mod mcp;
mod npm;
mod oci;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;
use tracing::debug;

use crate::proxy::engine::Cached;
use crate::proxy::UpstreamStrategy;
use crate::registry::cargo::upstream::{CargoArtifact, CargoUpstream};
use crate::registry::go::escape::escape;
use crate::registry::go::upstream::{FileKind, GoArtifact, GoUpstream};
use crate::domain::{CacheRepo, Outcome};
use crate::registry::resolve::Upstream;

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
        Source::Pypi { digest, uploaded } => {
            let at = uploaded.as_deref().and_then(parse_time);
            facts.date_source = match (&cfg.min_release_age, at) {
                (None, _) => "none",
                (Some(_), Some(_)) => "page",
                (Some(_), None) => "failed",
            };
            let at = cfg.min_release_age.as_ref().and(at);
            (digest, version, at)
        }
        Source::Nuget { body, published } => {
            facts.install_scripts = nuget_install_assets(&shared.proxy, &body).await;
            facts.date_source = if published.is_some() { "registration" } else { "none" };
            (body.entry.digest.clone(), version, published)
        }
        Source::Mcp {
            facts: gathered,
            digest,
            published,
        } => {
            facts.mcp = Some(gathered);
            facts.date_source = "registry";
            (digest, version, published)
        }
        Source::Oci {
            body,
            served,
            parsed,
        } => {
            let (at, source) = if cfg.min_release_age.is_some() {
                let dated = oci::dated_body(shared, &body, served.as_ref(), parsed).await;
                oci_published_at(shared, cfg, repo, &upstream, &name, dated).await
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
    let observed = shared
        .proxy
        .observe_within(s, up, member, a, shared.tuning.gather_timeout)
        .await;
    match observed {
        Ok(None) => (None, "timeout"),
        Ok(Some(Outcome::Found(cached))) => (Some(cached), "fetch"),
        Ok(Some(Outcome::NotFound)) => (None, "not-found"),
        Err(e) => {
            debug!(artifact = ?a, error = %e, "fact fetch failed");
            (None, "failed")
        }
    }
}

/// Whether a served `.nupkg` carries assets NuGet runs or imports on
/// install, read from its bytes through the storage port, never a path.
pub(crate) async fn nuget_install_assets(
    proxy: &crate::proxy::ProxyEngine,
    body: &Cached,
) -> Option<bool> {
    let bytes = proxy.bytes(body).await.ok()?;
    tokio::task::spawn_blocking(move || crate::registry::nuget::nuspec::entries(&bytes).ok())
        .await
        .ok()
        .flatten()
        .map(|entries| crate::registry::nuget::nuspec::executes_on_install(&entries))
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
    use crate::domain::Format;
    use crate::domain::CacheEntry;
    use crate::policy::testing::{engine_over, fast, pending, repo};
    use crate::testing::fixture::Fx;

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
                fetched_at: DateTime::UNIX_EPOCH,
                expires_at: None,
                last_used_at: DateTime::UNIX_EPOCH,
                fresh: true,
            },
            stale: false,
            exchanged: true,
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
                Format::Pypi,
                Source::Pypi {
                    digest: Some("dd".into()),
                    uploaded: None,
                },
                Some("dd"),
            ),
            (
                Format::Oci,
                Source::Oci {
                    body: cached("cc"),
                    served: None,
                    parsed: None,
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

    /// NuGet 3.4: the package is read through the storage port, which here
    /// has no filesystem behind it at all.
    #[tokio::test]
    async fn nuget_install_assets_verdicts() {
        use crate::policy::rules::install_scripts::InstallScripts;
        use crate::policy::rules::Rule;
        use crate::domain::Verdict;
        use crate::policy::{Actor, Facts};
        use crate::storage::StorageBackend;
        use crate::testing::storage::MemStorage;
        use std::io::Write as _;

        fn package(extra: Option<&str>) -> bytes::Bytes {
            let mut out = std::io::Cursor::new(Vec::new());
            let mut zip = zip::ZipWriter::new(&mut out);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("lib/net8.0/a.dll", options).unwrap();
            zip.write_all(b"MZ").unwrap();
            if let Some(path) = extra {
                zip.start_file(path, options).unwrap();
                zip.write_all(b"x").unwrap();
            }
            zip.finish().unwrap();
            bytes::Bytes::from(out.into_inner())
        }

        let fx = Fx::new().await;
        let mem = MemStorage::new();
        let engine = fx.engine_over(
            std::sync::Arc::new(mem.clone()),
            crate::proxy::Timeouts::from_connect_secs(1),
        );
        let cfg = PolicyConfig {
            install_scripts: true,
            ..Default::default()
        };
        for (extra, expected, verdict) in [
            (Some("tools/install.ps1"), Some(true), Verdict::WouldBlock),
            (Some("buildTransitive/a.targets"), Some(true), Verdict::WouldBlock),
            (None, Some(false), Verdict::Pass),
        ] {
            mem.put("r/i/_proxy/nuget-nupkg/a", package(extra)).await.unwrap();
            let mut body = cached("aa");
            body.entry.storage_path = Some("r/i/_proxy/nuget-nupkg/a".into());
            let fact = nuget_install_assets(&engine, &body).await;
            assert_eq!(fact, expected, "{extra:?}");
            let r = Resolution {
                requested_repo: "r".into(),
                member_repo: "m".into(),
                format: Format::Nuget,
                name: "a".into(),
                version: Some("1.0.0".into()),
                digest: None,
                actor: Actor::of(None),
                published_at: None,
                facts: Facts {
                    install_scripts: fact,
                    date_source: "none",
                    mcp: None,
                },
            };
            assert_eq!(InstallScripts.evaluate(&cfg, &r, Utc::now()).unwrap().verdict, verdict);
        }
        let mut missing = cached("aa");
        missing.entry.storage_path = Some("gone".into());
        assert_eq!(nuget_install_assets(&engine, &missing).await, None, "unread is unknown");
    }

    #[tokio::test]
    async fn oci_gather_dates_from_the_classified_body_without_a_read() {
        let fx = Fx::new().await;
        let (engine, _writer) = engine_over(&fx, PolicyConfig::default(), fast());
        let shared = engine.shared();
        let cfg = PolicyConfig {
            min_release_age: Some("1h".parse().unwrap()),
            ..Default::default()
        };
        let member = repo(fx.repo.id, "p", Format::Oci);
        let manifest = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "digest": "sha256:ee" },
            "annotations": { "org.opencontainers.image.created": "2026-01-01T00:00:00Z" }
        });
        let classified = pending(
            &member,
            &fx.up,
            Format::Oci,
            "app",
            Source::Oci {
                body: cached("cc"),
                served: None,
                parsed: Some(manifest),
            },
        );
        let r = gather(shared, &cfg, classified).await;
        assert_eq!(r.facts.date_source, "annotation");
        assert_eq!(
            r.published_at.unwrap().to_rfc3339(),
            "2026-01-01T00:00:00+00:00"
        );
        let unclassified = pending(
            &member,
            &fx.up,
            Format::Oci,
            "app",
            Source::Oci {
                body: cached("cc"),
                served: None,
                parsed: None,
            },
        );
        let r = gather(shared, &cfg, unclassified).await;
        assert_eq!(
            r.facts.date_source, "failed",
            "the row's file is unreadable: nothing but the parsed body could date it"
        );
        assert!(fx.hits().is_empty());
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

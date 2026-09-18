use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::OnceCell;
use tokio::time::timeout;
use tracing::debug;

use crate::error::AppError;
use crate::proxy::engine::Cached;
use crate::registry::npm::upstream::{NpmArtifact, NpmUpstream};
use crate::registry::resolve::{CacheRepo, Upstream};

use super::super::rules::PolicyConfig;
use super::super::Shared;
use super::{fetch_missing, parse_time};

const INSTALL_HOOKS: [&str; 3] = ["preinstall", "install", "postinstall"];

#[derive(Debug, Clone)]
pub struct VersionFacts {
    pub published_at: Option<DateTime<Utc>>,
    pub install_scripts: bool,
    pub tarball: Option<String>,
}

/// One packument parsed once per cache row.
#[derive(Debug)]
pub struct PackageFacts {
    pub versions: HashMap<String, VersionFacts>,
}

impl PackageFacts {
    pub fn parse(json: &Value) -> Self {
        let versions = json["versions"]
            .as_object()
            .map(|versions| {
                versions
                    .iter()
                    .map(|(v, meta)| (v.clone(), version_facts(json, v, meta)))
                    .collect()
            })
            .unwrap_or_default();
        Self { versions }
    }

    /// The stem's version, else the version whose `dist.tarball` ends in
    /// `filename`; a stem version absent from the packument still resolves.
    pub fn resolve(&self, name: &str, filename: &str) -> Option<String> {
        stem_version(name, filename).or_else(|| {
            self.versions
                .iter()
                .find(|(_, f)| f.tarball.as_deref() == Some(filename))
                .map(|(v, _)| v.clone())
        })
    }

    /// Whether this copy has the version `filename` names, with its date;
    /// a copy lacking either is a miss worth one conditional refresh.
    pub fn knows(&self, name: &str, filename: &str) -> bool {
        self.resolve(name, filename)
            .and_then(|v| self.versions.get(&v))
            .is_some_and(|f| f.published_at.is_some())
    }
}

fn version_facts(json: &Value, version: &str, meta: &Value) -> VersionFacts {
    let scripts = meta["scripts"].as_object();
    VersionFacts {
        published_at: json["time"][version].as_str().and_then(parse_time),
        install_scripts: meta["hasInstallScript"].as_bool() == Some(true)
            || scripts.is_some_and(|s| INSTALL_HOOKS.iter().any(|hook| s.contains_key(*hook))),
        tarball: tarball_filename(meta),
    }
}

fn tarball_filename(meta: &Value) -> Option<String> {
    meta["dist"]["tarball"]
        .as_str()
        .and_then(|url| url.rsplit('/').next())
        .map(String::from)
}

fn stem_version(name: &str, filename: &str) -> Option<String> {
    let unscoped = name.rsplit('/').next()?;
    let rest = filename
        .strip_suffix(".tgz")?
        .strip_prefix(unscoped)?
        .strip_prefix('-')?;
    (!rest.is_empty()).then(|| rest.to_string())
}

/// The version a tarball filename names: the `{unscoped}-{version}.tgz`
/// stem, else the packument version whose `dist.tarball` ends in it.
pub fn npm_version(name: &str, filename: &str, packument: Option<&Value>) -> Option<String> {
    stem_version(name, filename).or_else(|| {
        packument?["versions"]
            .as_object()?
            .iter()
            .find(|(_, meta)| tarball_filename(meta).as_deref() == Some(filename))
            .map(|(v, _)| v.clone())
    })
}

/// One memo entry per packument row: concurrent misses await one parse,
/// one conditional refresh.
pub struct NpmSlot {
    pub stamp: (i64, DateTime<Utc>, Option<String>),
    pub cell: Arc<OnceCell<Arc<PackageFacts>>>,
    pub refresh: Option<(Instant, Arc<OnceCell<()>>)>,
}

/// `(version, published_at, install_scripts, date_source)` of one tarball.
pub(crate) async fn npm(
    shared: &Shared,
    cfg: &PolicyConfig,
    member: CacheRepo<'_>,
    up: &Upstream,
    name: &str,
    filename: &str,
) -> (
    Option<String>,
    Option<DateTime<Utc>>,
    Option<bool>,
    &'static str,
) {
    let stem = stem_version(name, filename);
    if !cfg.needs_packument() {
        return match stem {
            Some(v) => (Some(v), None, None, "none"),
            None => (None, None, None, "filename-unparsed"),
        };
    }
    let (facts, source) = package_facts(shared, cfg, member, up, name, filename).await;
    let Some(facts) = facts else {
        return (stem, None, None, source);
    };
    let Some(version) = facts.resolve(name, filename) else {
        return (None, None, None, "filename-unparsed");
    };
    match facts.versions.get(&version) {
        Some(f) => (
            Some(version),
            f.published_at,
            Some(f.install_scripts),
            source,
        ),
        None => (Some(version), None, None, source),
    }
}

/// The parsed packument, from the memo, the cache or one fetch; a filename
/// absent from a peeked copy costs one coalesced conditional refresh per
/// `refresh_floor`.
pub(crate) async fn package_facts(
    shared: &Shared,
    cfg: &PolicyConfig,
    member: CacheRepo<'_>,
    up: &Upstream,
    name: &str,
    filename: &str,
) -> (Option<Arc<PackageFacts>>, &'static str) {
    let a = NpmArtifact::Metadata {
        name: name.to_string(),
    };
    let (cached, source) = match shared.proxy.peek(&NpmUpstream, member, &a).await {
        Ok(Some(cached)) => (Some(cached), "cache"),
        _ => fetch_missing(shared, cfg, &NpmUpstream, up, member, &a).await,
    };
    let Some(cached) = cached else {
        return (None, source);
    };
    let Some(facts) = memo_facts(shared, member, name, &cached).await else {
        return (None, "failed");
    };
    if source != "cache" || facts.knows(name, filename) {
        return (Some(facts), source);
    }
    if !cfg.fetch_missing_facts {
        return (Some(facts), "not-in-packument");
    }
    let cell = refresh_cell(shared, member, name, &cached);
    cell.get_or_init(|| async {
        let refreshed = timeout(
            shared.tuning.gather_timeout,
            shared.proxy.refresh(&NpmUpstream, up, member, &a),
        )
        .await;
        if let Ok(Err(e)) = refreshed {
            debug!(name, error = %e, "packument refresh failed");
        }
    })
    .await;
    let Ok(Some(cached)) = shared.proxy.peek(&NpmUpstream, member, &a).await else {
        return (Some(facts), "not-in-packument");
    };
    let Some(fresh) = memo_facts(shared, member, name, &cached).await else {
        return (Some(facts), "not-in-packument");
    };
    if fresh.knows(name, filename) {
        (Some(fresh), "refresh")
    } else {
        (Some(fresh), "not-in-packument")
    }
}

fn stamp(cached: &Cached) -> (i64, DateTime<Utc>, Option<String>) {
    (
        cached.entry.id,
        cached.entry.fetched_at,
        cached.entry.digest.clone(),
    )
}

/// The slot for `cached`'s row, replaced when the row was re-fetched;
/// the refresh floor is the package's, so it carries over to the new row.
fn slot_cell(
    shared: &Shared,
    member: CacheRepo<'_>,
    name: &str,
    cached: &Cached,
) -> Arc<OnceCell<Arc<PackageFacts>>> {
    let mut memo = shared.npm_facts.lock().unwrap();
    let key = (member.0.id, name.to_string());
    let stamp = stamp(cached);
    let refresh = match memo.get_mut(&key) {
        Some(slot) if slot.stamp == stamp => return slot.cell.clone(),
        Some(slot) => slot.refresh.take(),
        None => None,
    };
    memo.insert(
        key,
        NpmSlot {
            stamp,
            cell: Arc::new(OnceCell::new()),
            refresh,
        },
    )
    .cell
    .clone()
}

/// The refresh every waiter of this row shares: installed when none is
/// younger than `refresh_floor`.
fn refresh_cell(
    shared: &Shared,
    member: CacheRepo<'_>,
    name: &str,
    cached: &Cached,
) -> Arc<OnceCell<()>> {
    slot_cell(shared, member, name, cached);
    let mut memo = shared.npm_facts.lock().unwrap();
    let key = (member.0.id, name.to_string());
    let slot = memo.get_mut(&key).expect("slot installed above");
    match &slot.refresh {
        Some((at, cell)) if at.elapsed() < shared.tuning.refresh_floor => cell.clone(),
        _ => {
            let cell = Arc::new(OnceCell::new());
            slot.refresh = Some((Instant::now(), cell.clone()));
            cell
        }
    }
}

async fn memo_facts(
    shared: &Shared,
    member: CacheRepo<'_>,
    name: &str,
    cached: &Cached,
) -> Option<Arc<PackageFacts>> {
    let cell = slot_cell(shared, member, name, cached);
    let parsed = cell
        .get_or_try_init(|| async {
            let bytes = shared.proxy.bytes(cached).await?;
            let facts = tokio::task::spawn_blocking(move || {
                serde_json::from_slice::<Value>(&bytes).map(|json| PackageFacts::parse(&json))
            })
            .await
            .map_err(|e| AppError::Internal(format!("packument parse task failed: {e}")))?
            .map_err(|e| AppError::BadGateway(format!("invalid cached packument: {e}")))?;
            #[cfg(test)]
            shared
                .npm_parses
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, AppError>(Arc::new(facts))
        })
        .await;
    match parsed {
        Ok(facts) => Some(facts.clone()),
        Err(e) => {
            debug!(name, error = %e, "packument unreadable");
            None
        }
    }
}

#[cfg(test)]
#[path = "npm_tests.rs"]
mod tests;

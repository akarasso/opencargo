//! A hosted repository's reads: metadata rendered from port 18's visible
//! units, never a client's document, and files of visible units only.

use bytes::Bytes;
use chrono::{DateTime, Utc};

use super::metadata::{stamp, ArtifactLevel, GroupLevel, SnapshotEntry, SnapshotLevel};
use super::path::{is_snapshot, parse_build, Gav, MavenPath, Target};
use crate::error::StoreError;
use crate::ports::maven::{MavenFileStore, StoredFile, UnitKey, UnitView};
use crate::ports::packages::{NameMatch, PackageStore};

/// A rendered document with its validators: the `ETag` is the pair (the
/// package's version stamp, the scope's counter), `Last-Modified` the later
/// of their two instants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub body: Bytes,
    pub etag: String,
    pub last_modified: Option<DateTime<Utc>>,
}

pub fn scope_artifact(ga: &str) -> String {
    ga.to_string()
}

pub fn scope_snapshot(ga: &str, version: &str) -> String {
    format!("{ga}@{version}")
}

pub fn scope_group(group: &str) -> String {
    format!("group:{group}")
}

/// The counters a change to a unit of `gav` moves.
pub fn scopes_of(gav: &Gav) -> Vec<String> {
    let ga = gav.ga();
    let mut scopes = vec![scope_artifact(&ga)];
    if gav.is_snapshot() {
        scopes.push(scope_snapshot(&ga, &gav.version));
    }
    scopes
}

/// What the generic readers see of `ga`: moved only by publish, yank and
/// delete (A1 C5), read off the `versions` rows.
async fn version_stamp(
    packages: &dyn PackageStore,
    repository: i64,
    ga: &str,
) -> Result<(String, Option<DateTime<Utc>>), StoreError> {
    let Some(package) = packages.package(repository, ga, NameMatch::Exact).await? else {
        return Ok(("0".to_string(), None));
    };
    let versions = packages.versions(package.id).await?;
    let newest = versions.iter().map(|v| v.id).max().unwrap_or(0);
    let yanked = versions.iter().filter(|v| v.yanked).count();
    let at = versions.iter().map(|v| v.published_at).max();
    Ok((format!("{}-{newest}-{yanked}", versions.len()), at))
}

async fn rendered(
    maven: &dyn MavenFileStore,
    packages: &dyn PackageStore,
    repository: i64,
    ga: &str,
    scope: &str,
    body: String,
) -> Result<Rendered, StoreError> {
    let (stamp, published) = version_stamp(packages, repository, ga).await?;
    let counter = maven.counter(repository, scope).await?;
    Ok(Rendered {
        body: Bytes::from(body),
        etag: format!("\"{stamp}.{}\"", counter.value),
        last_modified: published.max(counter.updated_at),
    })
}

fn visible(units: Vec<UnitView>) -> Vec<UnitView> {
    units.into_iter().filter(UnitView::visible).collect()
}

pub async fn artifact_metadata(
    maven: &dyn MavenFileStore,
    packages: &dyn PackageStore,
    repository: i64,
    group: &str,
    artifact: &str,
) -> Result<Option<Rendered>, StoreError> {
    let ga = format!("{group}:{artifact}");
    let units = visible(maven.artifact(repository, &ga).await?);
    if units.is_empty() {
        return Ok(None);
    }
    let dir = format!("{}/{artifact}", group.replace('.', "/"));
    let hints = maven.client_metadata(repository, &dir).await?;
    let last = units.iter().filter_map(|u| u.visible_at).max().map(stamp);
    let level = ArtifactLevel::computed(
        group,
        artifact,
        units.iter().map(|u| u.version.clone()).collect(),
        (
            hints.as_ref().and_then(|h| h.release.as_deref()),
            hints.as_ref().and_then(|h| h.latest.as_deref()),
        ),
        last,
    );
    let body = level.render();
    rendered(maven, packages, repository, &ga, &scope_artifact(&ga), body)
        .await
        .map(Some)
}

/// The newest visible build only, every file of it: a build whose POM is
/// not committed is not visible, so its jar is never announced, and no
/// entry comes from an older build.
pub async fn snapshot_metadata(
    maven: &dyn MavenFileStore,
    packages: &dyn PackageStore,
    repository: i64,
    gav: &Gav,
) -> Result<Option<Rendered>, StoreError> {
    let ga = gav.ga();
    let units: Vec<UnitView> = visible(maven.artifact(repository, &ga).await?)
        .into_iter()
        .filter(|u| u.version == gav.version)
        .collect();
    let newest = units
        .iter()
        .filter_map(|u| parse_build(&u.build).map(|(ts, n, _)| ((ts.to_string(), n), u)))
        .max_by(|a, b| a.0.cmp(&b.0));
    let (snapshot, unit) = match newest {
        Some(((timestamp, number), unit)) => (Some((timestamp, number)), unit),
        None => match units.iter().find(|u| u.build.is_empty()) {
            Some(unit) => (None, unit),
            None => return Ok(None),
        },
    };
    let base = gav.version.trim_end_matches("-SNAPSHOT");
    let value = match &snapshot {
        Some(_) => format!("{base}-{}", unit.build),
        None => gav.version.clone(),
    };
    let updated = unit.visible_at.map(stamp).unwrap_or_default();
    let mut entries: Vec<SnapshotEntry> = unit
        .files
        .iter()
        .filter_map(|f| {
            let parsed = MavenPath::parse(&format!("{}/{}", gav.version_dir(), f.filename)).ok()?;
            let Target::File(file) = parsed.target else {
                return None;
            };
            Some(SnapshotEntry {
                classifier: file.classifier,
                extension: file.extension,
                value: value.clone(),
                updated: updated.clone(),
            })
        })
        .collect();
    entries.sort_by(|a, b| (&a.extension, &a.classifier).cmp(&(&b.extension, &b.classifier)));
    let level = SnapshotLevel {
        group: gav.group.clone(),
        artifact: gav.artifact.clone(),
        version: gav.version.clone(),
        timestamp: snapshot.as_ref().map(|(ts, _)| ts.clone()),
        build_number: snapshot.as_ref().map(|(_, n)| *n),
        last_updated: Some(updated.clone()),
        entries,
    };
    rendered(
        maven,
        packages,
        repository,
        &ga,
        &scope_snapshot(&ga, &gav.version),
        level.render(),
    )
    .await
    .map(Some)
}

/// The plugins a client declared for a group, when it declared any.
pub async fn group_metadata(
    maven: &dyn MavenFileStore,
    packages: &dyn PackageStore,
    repository: i64,
    dir: &[String],
) -> Result<Option<Rendered>, StoreError> {
    let Some(doc) = maven.client_metadata(repository, &dir.join("/")).await? else {
        return Ok(None);
    };
    if doc.plugins.is_empty() {
        return Ok(None);
    }
    let group = dir.join(".");
    let body = GroupLevel {
        plugins: doc.plugins,
    }
    .render();
    rendered(maven, packages, repository, &group, &scope_group(&group), body)
        .await
        .map(Some)
}

/// Whichever document the directory holds: a snapshot's, an artifact's, or
/// a group's plugins.
pub async fn metadata(
    maven: &dyn MavenFileStore,
    packages: &dyn PackageStore,
    repository: i64,
    dir: &[String],
) -> Result<Option<Rendered>, StoreError> {
    use super::path::MetadataLevel;
    match MetadataLevel::of(dir) {
        Some(MetadataLevel::Snapshot(gav)) => snapshot_metadata(maven, packages, repository, &gav).await,
        Some(MetadataLevel::Artifact { group, artifact }) => {
            match artifact_metadata(maven, packages, repository, &group, &artifact).await? {
                Some(found) => Ok(Some(found)),
                None => group_metadata(maven, packages, repository, dir).await,
            }
        }
        None => group_metadata(maven, packages, repository, dir).await,
    }
}

/// A stored file of a visible unit.
pub async fn visible_file(
    maven: &dyn MavenFileStore,
    repository: i64,
    gav: &Gav,
    build: &str,
    filename: &str,
) -> Result<Option<StoredFile>, StoreError> {
    let ga = gav.ga();
    let unit = maven
        .unit(&UnitKey {
            repository,
            ga: &ga,
            version: &gav.version,
            build,
        })
        .await?;
    Ok(unit
        .filter(|u| u.visible())
        .and_then(|u| u.file(filename).cloned()))
}

/// Whether a snapshot's metadata is refreshed like one: every version under a
/// `-SNAPSHOT` directory.
pub fn is_snapshot_dir(dir: &[String]) -> bool {
    dir.last().is_some_and(|v| is_snapshot(v))
}

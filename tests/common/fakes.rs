//! In-memory doubles for the ports, shared by the lib's unit tests (through
//! `src/testing.rs`) and by every integration test that declares
//! `mod common;`. Everything here spells the crate `opencargo::`, like the
//! rest of `tests/common/`, which is what lets one file compile on both
//! sides.
//!
//! One state behind one `Arc`, not one fake per port: a cascade crosses
//! aggregates, and independent maps cannot represent one. Handles are cheap
//! views onto it.
//!
//! The `allow` is this file's own: `tests/common/mod.rs`'s inner attribute
//! does not reach it once the lib includes it under a different parent, and
//! CI runs clippy over the test targets with `-D warnings`.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use opencargo::domain::{
    DistTag, Package, RepoConfig, RepoSpec, Repository, Version, Visibility, Webhook,
};
use opencargo::error::StoreError;
use opencargo::ports::packages::{NameMatch, NewRelease, PackageStore, Promotion, Release};
use opencargo::ports::repositories::{RepoPatch, RepositoryStore};
use opencargo::ports::search::{SearchIndex, SearchQuery, SearchScope};
use opencargo::ports::webhooks::{NewWebhook, WebhookPatch, WebhookStore};

/// Which port a queued failure belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortId {
    Webhooks,
    Repositories,
    Packages,
    Search,
}

/// One audit row, kept because a promotion writes it in the same transaction
/// as the version it records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub user_id: Option<i64>,
    pub username: String,
    pub action: String,
    pub target: String,
    pub repository: String,
    pub details_json: String,
}

#[derive(Default)]
struct State {
    webhooks: Vec<Webhook>,
    repositories: Vec<Repository>,
    packages: Vec<Package>,
    versions: Vec<Version>,
    dist_tags: Vec<DistTag>,
    audit: Vec<AuditRow>,
    next_id: i64,
    /// Queued refusals: a fake that cannot fail only ever proves the happy
    /// path, which the integration suite already covers.
    failures: Vec<(PortId, StoreError)>,
}

impl State {
    fn refusal(&mut self, port: PortId) -> Option<StoreError> {
        let at = self
            .failures
            .iter()
            .position(|(queued, _)| *queued == port)?;
        Some(self.failures.remove(at).1)
    }

    fn id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }
}

#[derive(Clone, Default)]
pub struct FakeDb(Arc<Mutex<State>>);

impl FakeDb {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn webhooks(&self) -> Arc<dyn WebhookStore> {
        Arc::new(Webhooks(self.0.clone()))
    }

    pub fn repositories(&self) -> Arc<dyn RepositoryStore> {
        Arc::new(Repositories(self.0.clone()))
    }

    pub fn packages(&self) -> Arc<dyn PackageStore> {
        Arc::new(Packages(self.0.clone()))
    }

    pub fn search(&self) -> Arc<dyn SearchIndex> {
        Arc::new(Search(self.0.clone()))
    }

    /// Make the next call on `port` fail with `err`.
    pub fn fail_next(&self, port: PortId, err: StoreError) {
        self.0.lock().unwrap().failures.push((port, err));
    }

    /// What a promotion recorded, which no port reads back yet.
    pub fn audit(&self) -> Vec<AuditRow> {
        self.0.lock().unwrap().audit.clone()
    }
}

/// The fake's own stamp spelling, deliberately not the SQLite adapter's: a
/// contract clause that only holds for one rendering is a clause about an
/// adapter, not about the port.
fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339()
}

/// The guard every handle runs its work behind: one lock, and the queued
/// refusal for its own port checked first.
fn with<T>(
    state: &Arc<Mutex<State>>,
    port: PortId,
    act: impl FnOnce(&mut State) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let mut state = state.lock().unwrap();
    match state.refusal(port) {
        Some(err) => Err(err),
        None => act(&mut state),
    }
}

struct Webhooks(Arc<Mutex<State>>);

impl Webhooks {
    fn with<T>(
        &self,
        act: impl FnOnce(&mut State) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        with(&self.0, PortId::Webhooks, act)
    }

    fn find(state: &mut State, id: i64) -> Result<&mut Webhook, StoreError> {
        state
            .webhooks
            .iter_mut()
            .find(|hook| hook.id == id)
            .ok_or(StoreError::NotFound)
    }

    fn insert(state: &mut State, hook: &NewWebhook<'_>, now: DateTime<Utc>) -> Webhook {
        let stored = Webhook {
            id: state.id(),
            url: hook.url.to_string(),
            events: hook.events.clone(),
            secret: hook.secret.map(|secret| secret.to_string()),
            active: true,
            created_at: stamp(now),
            updated_at: stamp(now),
        };
        state.webhooks.push(stored.clone());
        stored
    }
}

#[async_trait]
impl WebhookStore for Webhooks {
    async fn all(&self) -> Result<Vec<Webhook>, StoreError> {
        self.with(|state| Ok(state.webhooks.clone()))
    }

    async fn by_id(&self, id: i64) -> Result<Option<Webhook>, StoreError> {
        self.with(|state| Ok(state.webhooks.iter().find(|hook| hook.id == id).cloned()))
    }

    async fn create(
        &self,
        hook: &NewWebhook<'_>,
        now: DateTime<Utc>,
    ) -> Result<Webhook, StoreError> {
        self.with(|state| Ok(Self::insert(state, hook, now)))
    }

    async fn update(
        &self,
        id: i64,
        patch: &WebhookPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<Webhook, StoreError> {
        self.with(|state| {
            let stored = Self::find(state, id)?;
            if patch.touches_nothing() {
                return Ok(stored.clone());
            }
            if let Some(url) = patch.url {
                stored.url = url.to_string();
            }
            if let Some(events) = patch.events {
                stored.events = events.clone();
            }
            if let Some(secret) = patch.secret {
                stored.secret = secret.map(|secret| secret.to_string());
            }
            if let Some(active) = patch.active {
                stored.active = active;
            }
            stored.updated_at = stamp(now);
            Ok(stored.clone())
        })
    }

    async fn delete(&self, id: i64) -> Result<(), StoreError> {
        self.with(|state| {
            let at = state
                .webhooks
                .iter()
                .position(|hook| hook.id == id)
                .ok_or(StoreError::NotFound)?;
            state.webhooks.remove(at);
            Ok(())
        })
    }

    async fn active(&self) -> Result<Vec<Webhook>, StoreError> {
        self.with(|state| {
            Ok(state
                .webhooks
                .iter()
                .filter(|hook| hook.active)
                .cloned()
                .collect())
        })
    }

    async fn ensure_seeded(
        &self,
        hooks: &[NewWebhook<'_>],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.with(|state| {
            if hooks.is_empty() || !state.webhooks.is_empty() {
                return Ok(());
            }
            for hook in hooks {
                Self::insert(state, hook, now);
            }
            Ok(())
        })
    }
}

struct Repositories(Arc<Mutex<State>>);

impl Repositories {
    fn with<T>(
        &self,
        act: impl FnOnce(&mut State) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        with(&self.0, PortId::Repositories, act)
    }

    fn insert(state: &mut State, spec: &RepoSpec<'_>, now: DateTime<Utc>) -> Repository {
        let stored = Repository {
            id: state.id(),
            name: spec.name.to_string(),
            repo_type: spec.kind.as_str().to_string(),
            format: spec.format.as_str().to_string(),
            visibility: spec.visibility,
            upstream_url: spec.upstream.map(|url| url.to_string()),
            config: spec.config(),
            created_at: now,
            updated_at: now,
        };
        state.repositories.push(stored.clone());
        stored
    }
}

#[async_trait]
impl RepositoryStore for Repositories {
    async fn by_name(&self, name: &str) -> Result<Option<Repository>, StoreError> {
        self.with(|state| Ok(found(state, name).cloned()))
    }

    async fn all(&self) -> Result<Vec<Repository>, StoreError> {
        self.with(|state| {
            let mut all = state.repositories.clone();
            all.sort_by(|left, right| left.name.cmp(&right.name));
            Ok(all)
        })
    }

    async fn create(
        &self,
        spec: &RepoSpec<'_>,
        now: DateTime<Utc>,
    ) -> Result<Repository, StoreError> {
        self.with(|state| {
            if found(state, spec.name).is_some() {
                return Err(StoreError::Conflict);
            }
            Ok(Self::insert(state, spec, now))
        })
    }

    async fn update(
        &self,
        name: &str,
        patch: &RepoPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<Repository, StoreError> {
        let config: Option<RepoConfig> = patch.config.cloned();
        self.with(|state| {
            let stored = found_mut(state, name).ok_or(StoreError::NotFound)?;
            if patch.touches_nothing() {
                return Ok(stored.clone());
            }
            if let Some(visibility) = patch.visibility {
                stored.visibility = visibility;
            }
            if let Some(upstream) = patch.upstream {
                stored.upstream_url = Some(upstream.to_string());
            }
            if let Some(config) = config {
                stored.config = Some(config);
            }
            stored.updated_at = now;
            Ok(stored.clone())
        })
    }

    async fn delete_empty(&self, name: &str) -> Result<(), StoreError> {
        self.with(|state| {
            let repo = found(state, name).ok_or(StoreError::NotFound)?.id;
            if state.packages.iter().any(|pkg| pkg.repository_id == repo) {
                return Err(StoreError::Conflict);
            }
            state.repositories.retain(|stored| stored.id != repo);
            Ok(())
        })
    }

    async fn ensure_seeded(
        &self,
        specs: &[RepoSpec<'_>],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.with(|state| {
            for spec in specs {
                if found(state, spec.name).is_none() {
                    Self::insert(state, spec, now);
                }
            }
            Ok(())
        })
    }
}

fn found<'a>(state: &'a State, name: &str) -> Option<&'a Repository> {
    state.repositories.iter().find(|repo| repo.name == name)
}

fn found_mut<'a>(state: &'a mut State, name: &str) -> Option<&'a mut Repository> {
    state.repositories.iter_mut().find(|repo| repo.name == name)
}

struct Packages(Arc<Mutex<State>>);

impl Packages {
    fn with<T>(
        &self,
        act: impl FnOnce(&mut State) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        with(&self.0, PortId::Packages, act)
    }

    fn matched(package: &Package, name: &str, how: NameMatch) -> bool {
        match how {
            NameMatch::Exact => package.name == name,
            NameMatch::Insensitive => package.name.eq_ignore_ascii_case(name),
        }
    }

    fn lookup(state: &State, repository: i64, name: &str, how: NameMatch) -> Option<Package> {
        state
            .packages
            .iter()
            .find(|pkg| pkg.repository_id == repository && Self::matched(pkg, name, how))
            .cloned()
    }

    /// The package row a release lands on, created when it is not there yet.
    fn upsert_package(state: &mut State, spec: &ReleaseSpec<'_>) -> Package {
        if let Some(package) = Self::lookup(state, spec.repository, spec.package, spec.how) {
            return package;
        }
        let stored = Package {
            id: state.id(),
            repository_id: spec.repository,
            name: spec.package.to_string(),
            description: spec.description.map(str::to_string),
            readme: None,
            license: None,
            created_at: spec.now,
            updated_at: spec.now,
        };
        state.packages.push(stored.clone());
        stored
    }

    /// Package, version and dist-tags: what both coarse methods write, and
    /// all of it or none of it, like the transaction it stands for.
    fn write_release(
        state: &mut State,
        spec: &ReleaseSpec<'_>,
    ) -> Result<(Package, Version), StoreError> {
        // Refused before the first write, which is what the adapter's
        // transaction buys it: a duplicate version leaves no package row.
        let existing = Self::lookup(state, spec.repository, spec.package, spec.how);
        if existing.iter().any(|package| {
            state
                .versions
                .iter()
                .any(|v| v.package_id == package.id && v.version == spec.version.version)
        }) {
            return Err(StoreError::Conflict);
        }

        let package = Self::upsert_package(state, spec);
        if let Some(readme) = spec.readme {
            if let Some(stored) = state.packages.iter_mut().find(|pkg| pkg.id == package.id) {
                stored.readme = Some(readme.to_string());
                stored.updated_at = spec.now;
            }
        }
        let version = Version {
            id: state.id(),
            package_id: package.id,
            published_at: spec.now,
            yanked: false,
            ..spec.version.clone()
        };
        state.versions.push(version.clone());
        for tag in spec.dist_tags {
            tag_version(state, package.id, tag, version.id);
        }
        Ok((package, version))
    }
}

/// The version values a release carries, plus where they land.
struct ReleaseSpec<'a> {
    repository: i64,
    package: &'a str,
    how: NameMatch,
    description: Option<&'a str>,
    readme: Option<&'a str>,
    version: Version,
    dist_tags: &'a [String],
    now: DateTime<Utc>,
}

fn blank_version(version: &str, metadata_json: &str) -> Version {
    Version {
        id: 0,
        package_id: 0,
        version: version.to_string(),
        metadata_json: metadata_json.to_string(),
        checksum_sha1: None,
        checksum_sha256: None,
        integrity: None,
        size: 0,
        tarball_path: String::new(),
        published_at: DateTime::UNIX_EPOCH,
        yanked: false,
    }
}

fn tag_version(state: &mut State, package: i64, tag: &str, version: i64) {
    match state
        .dist_tags
        .iter_mut()
        .find(|dt| dt.package_id == package && dt.tag == tag)
    {
        Some(stored) => stored.version_id = version,
        None => {
            let id = state.id();
            state.dist_tags.push(DistTag {
                id,
                package_id: package,
                tag: tag.to_string(),
                version_id: version,
            });
        }
    }
}

#[async_trait]
impl PackageStore for Packages {
    async fn package(
        &self,
        repository: i64,
        name: &str,
        how: NameMatch,
    ) -> Result<Option<Package>, StoreError> {
        self.with(|state| Ok(Self::lookup(state, repository, name, how)))
    }

    async fn versions(&self, package: i64) -> Result<Vec<Version>, StoreError> {
        self.with(|state| {
            Ok(state
                .versions
                .iter()
                .filter(|v| v.package_id == package)
                .cloned()
                .collect())
        })
    }

    async fn version(
        &self,
        package: i64,
        version: &str,
    ) -> Result<Option<Version>, StoreError> {
        self.with(|state| {
            Ok(state
                .versions
                .iter()
                .find(|v| v.package_id == package && v.version == version)
                .cloned())
        })
    }

    async fn dist_tags(&self, package: i64) -> Result<Vec<DistTag>, StoreError> {
        self.with(|state| {
            Ok(state
                .dist_tags
                .iter()
                .filter(|dt| dt.package_id == package)
                .cloned()
                .collect())
        })
    }

    async fn publish_version(&self, release: &NewRelease<'_>) -> Result<Release, StoreError> {
        let mut version = blank_version(release.version, release.metadata_json);
        version.checksum_sha1 = release.checksum_sha1.map(str::to_string);
        version.checksum_sha256 = release.checksum_sha256.map(str::to_string);
        version.integrity = release.integrity.map(str::to_string);
        version.size = release.size;
        version.tarball_path = release.tarball_path.to_string();
        let spec = ReleaseSpec {
            repository: release.repository,
            package: release.package,
            how: release.match_name,
            description: release.description,
            readme: release.readme,
            version,
            dist_tags: release.dist_tags,
            now: release.now,
        };
        self.with(|state| {
            let (package, version) = Self::write_release(state, &spec)?;
            Ok(Release { package, version })
        })
    }

    async fn promote_metadata(
        &self,
        promotion: &Promotion<'_>,
    ) -> Result<Version, StoreError> {
        let mut version = blank_version(&promotion.source.version, promotion.metadata_json);
        version.checksum_sha1 = promotion.source.checksum_sha1.clone();
        version.checksum_sha256 = promotion.source.checksum_sha256.clone();
        version.integrity = promotion.source.integrity.clone();
        version.size = promotion.source.size;
        version.tarball_path = promotion.tarball_path.to_string();
        let spec = ReleaseSpec {
            repository: promotion.target_repository,
            package: promotion.package,
            how: NameMatch::Exact,
            description: promotion.description,
            readme: None,
            version,
            dist_tags: promotion.dist_tags,
            now: promotion.now,
        };
        self.with(|state| {
            let (_, version) = Self::write_release(state, &spec)?;
            state.audit.push(AuditRow {
                user_id: promotion.audit.user_id,
                username: promotion.audit.username.to_string(),
                action: "package.promote".to_string(),
                target: promotion.audit.target.to_string(),
                repository: promotion.audit.repository.to_string(),
                details_json: promotion.audit.details_json.to_string(),
            });
            Ok(version)
        })
    }

    async fn set_dist_tag(
        &self,
        package: i64,
        tag: &str,
        version: i64,
    ) -> Result<(), StoreError> {
        self.with(|state| {
            tag_version(state, package, tag, version);
            Ok(())
        })
    }

    async fn clear_dist_tag(&self, package: i64, tag: &str) -> Result<(), StoreError> {
        self.with(|state| {
            let at = state
                .dist_tags
                .iter()
                .position(|dt| dt.package_id == package && dt.tag == tag)
                .ok_or(StoreError::NotFound)?;
            state.dist_tags.remove(at);
            Ok(())
        })
    }

    async fn set_readme(
        &self,
        package: i64,
        readme: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.with(|state| {
            let stored = state
                .packages
                .iter_mut()
                .find(|pkg| pkg.id == package)
                .ok_or(StoreError::NotFound)?;
            stored.readme = Some(readme.to_string());
            stored.updated_at = now;
            Ok(())
        })
    }

    async fn set_metadata(&self, version: i64, metadata_json: &str) -> Result<(), StoreError> {
        self.with(|state| {
            let stored = state
                .versions
                .iter_mut()
                .find(|v| v.id == version)
                .ok_or(StoreError::NotFound)?;
            stored.metadata_json = metadata_json.to_string();
            Ok(())
        })
    }

    async fn set_yanked(&self, version: i64, yanked: bool) -> Result<(), StoreError> {
        self.with(|state| {
            let stored = state
                .versions
                .iter_mut()
                .find(|v| v.id == version)
                .ok_or(StoreError::NotFound)?;
            stored.yanked = yanked;
            Ok(())
        })
    }
}

struct Search(Arc<Mutex<State>>);

#[async_trait]
impl SearchIndex for Search {
    async fn search(
        &self,
        scope: SearchScope,
        query: Option<&SearchQuery>,
        limit: u32,
    ) -> Result<Vec<Package>, StoreError> {
        with(&self.0, PortId::Search, |state| {
            let public: Vec<i64> = state
                .repositories
                .iter()
                .filter(|repo| repo.visibility == Visibility::Public)
                .map(|repo| repo.id)
                .collect();
            let hits = state
                .packages
                .iter()
                .filter(|pkg| match scope {
                    SearchScope::Repo(repo) => pkg.repository_id == repo,
                    SearchScope::PublicOnly => public.contains(&pkg.repository_id),
                    SearchScope::All => true,
                })
                .filter(|pkg| query.is_none_or(|q| matches_tokens(pkg, q)))
                .take(limit as usize)
                .cloned()
                .collect();
            Ok(hits)
        })
    }
}

/// Every token has to appear, which is what FTS5's implicit `AND` of quoted
/// phrases does; the ranking a real index adds is not something a fake can
/// stand in for, so the contract never asserts a full ordering.
fn matches_tokens(package: &Package, query: &SearchQuery) -> bool {
    query.tokens().iter().all(|token| {
        let token = token.to_lowercase();
        package.name.to_lowercase().contains(&token)
            || package
                .description
                .as_deref()
                .is_some_and(|text| text.to_lowercase().contains(&token))
    })
}

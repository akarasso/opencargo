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
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use opencargo::domain::{
    ApiToken, CacheEntry, CacheEntryId, DistTag, NewEntry, Package, RepoConfig, RepoId, RepoSpec,
    Repository, Rights, ScanResult, User, Verdict, Version, Visibility, Webhook,
};
use opencargo::error::StoreError;
use opencargo::ports::audit::{AuditEntry, AuditStore, NewAuditEntry};
use opencargo::ports::deps::{Dependency, DependencyStore, Dependent, NewDependency};
use opencargo::ports::oci::{
    BeginComplete, Blob, Finished, LeaseToken, Manifest, NewBlob, NewManifest, OciStore, Orphaned,
    Segment, SegmentClaim, UploadSession,
};
use opencargo::ports::maven::{
    Changed, ClientMetadata, Counter, MavenFileStore, PendingUnit, StoredFile, Unit, UnitChange,
    UnitKey, UnitView, Unversioned,
};
use opencargo::ports::packages::{
    NameMatch, NewRelease, PackageStore, Promotion, Release, StalePrerelease,
};
use opencargo::ports::policy::{
    IdRange, NewResolution, PolicyStore, ReportFilter, ResolutionRow, RuleTotals, Subject, Totals,
    VerdictRow,
};
use opencargo::ports::permissions::{PermissionStore, RepoRights};
use opencargo::domain::layout;
use opencargo::ports::proxy_cache::ProxyCacheStore;
use opencargo::ports::pypi::{NewPypiFile, Published, PypiFile, PypiFileStore};
use opencargo::ports::reclaim::{
    Backlog, Candidate, Claim, ClaimToken, PinToken, Pinned, ReclaimStore, Renewal,
};
use opencargo::ports::referenced::{Referenced, ReferencedKeys, ReferencedStream};
use opencargo::ports::repositories::{RepoPatch, RepositoryStore};
use opencargo::ports::nuget::{FeedPage, FeedQuery, NugetFeedRead};
use opencargo::ports::search::{SearchIndex, SearchQuery, SearchScope};
use opencargo::ports::tokens::{NewToken, TokenStore};
use opencargo::ports::users::{NewUser, UserPatch, UserStore};
use opencargo::ports::vulns::{VulnScan, VulnStore};
use opencargo::ports::webhooks::{NewWebhook, WebhookPatch, WebhookStore};

/// Which port a queued failure belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortId {
    Webhooks,
    Repositories,
    Packages,
    Search,
    ProxyCache,
    Users,
    Tokens,
    Permissions,
    Oci,
    Audit,
    Dependencies,
    Vulns,
    Policy,
    Reclaim,
    Pypi,
    Maven,
}

/// One grant, keyed the way the table is.
#[derive(Clone)]
struct Grant {
    user_id: i64,
    repository_id: i64,
    rights: Rights,
}

/// One stored blob of an image.
#[derive(Clone)]
struct BlobRow {
    repository_id: i64,
    digest: String,
    size: i64,
    content_type: Option<String>,
    key: String,
}

/// One layer a manifest lists, the row a blob's orphanhood is decided from.
#[derive(Clone)]
struct LinkRow {
    repository_id: i64,
    manifest_digest: String,
    blob_digest: String,
}

/// One chunked push still assembling its blob.
#[derive(Clone)]
struct UploadRow {
    id: String,
    repository_id: i64,
    /// `None` with `received` for a session started before upload progress.
    prefix: Option<String>,
    received: Option<u64>,
    segments: u32,
    touched: DateTime<Utc>,
    lease: Option<(String, DateTime<Utc>)>,
}

/// One stored manifest of an image.
#[derive(Clone)]
struct ManifestRow {
    repository_id: i64,
    name: String,
    digest: String,
    content_type: String,
    size: i64,
    key: String,
}

/// One tag, which is a name for a manifest digest.
#[derive(Clone)]
struct TagRow {
    repository_id: i64,
    name: String,
    tag: String,
    manifest_digest: String,
}

/// One recorded edge of the dependency graph.
#[derive(Clone)]
struct DependencyRow {
    version_id: i64,
    name: String,
    requirement: String,
    kind: String,
}

/// One recorded scan.
#[derive(Clone)]
struct ScanRow {
    version_id: i64,
    scanned_at: DateTime<Utc>,
    result: ScanResult,
}

/// One recorded resolution, kept with its verdicts: they are written
/// together and erased together, which is the whole of what the port owes.
#[derive(Clone)]
struct ResolutionEntry {
    row: ResolutionRow,
    verdicts: Vec<VerdictRow>,
}

#[derive(Default)]
struct State {
    webhooks: Vec<Webhook>,
    repositories: Vec<Repository>,
    packages: Vec<Package>,
    versions: Vec<Version>,
    dist_tags: Vec<DistTag>,
    audit: Vec<AuditEntry>,
    dependencies: Vec<DependencyRow>,
    scans: Vec<ScanRow>,
    resolutions: Vec<ResolutionEntry>,
    cache: Vec<CacheEntry>,
    users: Vec<User>,
    tokens: Vec<ApiToken>,
    grants: Vec<Grant>,
    oci_blobs: Vec<BlobRow>,
    oci_manifests: Vec<ManifestRow>,
    oci_tags: Vec<TagRow>,
    oci_links: Vec<LinkRow>,
    oci_uploads: Vec<UploadRow>,
    oci_segments: Vec<(String, Segment)>,
    pypi_files: Vec<PypiFile>,
    reclaim: ReclaimState,
    maven: MavenState,
    next_id: i64,
    next_cache_id: i64,
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

    pub fn nuget_feed(&self) -> Arc<dyn NugetFeedRead> {
        Arc::new(NugetFeed(self.0.clone()))
    }

    pub fn proxy_cache(&self) -> Arc<dyn ProxyCacheStore> {
        Arc::new(ProxyCache(self.0.clone()))
    }

    pub fn oci(&self) -> Arc<dyn OciStore> {
        Arc::new(Oci(self.0.clone()))
    }

    pub fn pypi(&self) -> Arc<dyn PypiFileStore> {
        Arc::new(Pypi(self.0.clone()))
    }

    pub fn reclaim(&self) -> Arc<dyn ReclaimStore> {
        Arc::new(Reclaim(self.0.clone()))
    }

    pub fn referenced(&self) -> Arc<dyn ReferencedKeys> {
        Arc::new(Reclaim(self.0.clone()))
    }

    pub fn maven(&self) -> Arc<dyn MavenFileStore> {
        Arc::new(Maven(self.0.clone()))
    }

    /// The keys waiting for reclamation, sorted.
    pub fn candidates(&self) -> Vec<String> {
        let state = self.0.lock().unwrap();
        let mut keys: Vec<String> = state
            .reclaim
            .candidates
            .iter()
            .map(|c| c.key.clone())
            .collect();
        keys.sort();
        keys
    }

    /// A blob a push is not being asked to land: the read half's tests want
    /// the row, not the upload that would have written it.
    pub fn add_blob(&self, repository: i64, digest: &str, size: i64, content_type: Option<&str>) {
        self.0.lock().unwrap().oci_blobs.push(BlobRow {
            repository_id: repository,
            digest: digest.to_string(),
            size,
            content_type: content_type.map(str::to_string),
            key: format!("oci/{repository}/_blobs/{digest}"),
        });
    }

    /// A session as a server that predates upload progress left it.
    pub fn add_legacy_upload(&self, id: &str, repository: i64) {
        self.0.lock().unwrap().oci_uploads.push(UploadRow {
            id: id.to_string(),
            repository_id: repository,
            prefix: None,
            received: None,
            segments: 0,
            touched: DateTime::UNIX_EPOCH,
            lease: None,
        });
    }

    pub fn add_manifest(
        &self,
        repository: i64,
        name: &str,
        digest: &str,
        content_type: &str,
        size: i64,
    ) {
        self.0.lock().unwrap().oci_manifests.push(ManifestRow {
            repository_id: repository,
            name: name.to_string(),
            digest: digest.to_string(),
            content_type: content_type.to_string(),
            size,
            key: format!("oci/{repository}/{name}/manifests/{digest}"),
        });
    }

    pub fn add_tag(&self, repository: i64, name: &str, tag: &str, digest: &str) {
        self.0.lock().unwrap().oci_tags.push(TagRow {
            repository_id: repository,
            name: name.to_string(),
            tag: tag.to_string(),
            manifest_digest: digest.to_string(),
        });
    }

    pub fn users(&self) -> Arc<dyn UserStore> {
        Arc::new(Users(self.0.clone()))
    }

    pub fn tokens(&self) -> Arc<dyn TokenStore> {
        Arc::new(Tokens(self.0.clone()))
    }

    pub fn perms(&self) -> Arc<dyn PermissionStore> {
        Arc::new(Permissions(self.0.clone()))
    }

    /// Make the next call on `port` fail with `err`.
    pub fn fail_next(&self, port: PortId, err: StoreError) {
        self.0.lock().unwrap().failures.push((port, err));
    }

    pub fn audit(&self) -> Arc<dyn AuditStore> {
        Arc::new(Audit(self.0.clone()))
    }

    pub fn dependencies(&self) -> Arc<dyn DependencyStore> {
        Arc::new(Dependencies(self.0.clone()))
    }

    pub fn vulns(&self) -> Arc<dyn VulnStore> {
        Arc::new(Vulns(self.0.clone()))
    }

    pub fn policy(&self) -> Arc<dyn PolicyStore> {
        Arc::new(Policy(self.0.clone()))
    }

    /// The trail as rows, for the assertions that are about what a write
    /// recorded rather than about what the read method returns.
    pub fn audit_rows(&self) -> Vec<(Option<i64>, String, String)> {
        self.0
            .lock()
            .unwrap()
            .audit
            .iter()
            .map(|e| {
                (
                    e.user_id,
                    e.action.clone(),
                    e.target.clone().unwrap_or_default(),
                )
            })
            .collect()
    }
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
            created_at: now,
            updated_at: now,
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
            stored.updated_at = now;
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
        let incarnation = uuid::Uuid::new_v4().simple().to_string();
        state.reclaim.incarnations.push((stored.id, incarnation.clone()));
        let legacy = layout::name_keyed_prefixes(spec.format.as_str(), spec.name);
        for prefix in &legacy {
            state.reclaim.candidates.retain(|c| !(c.prefix && c.key == *prefix));
            state.reclaim.claims.retain(|c| c.key != *prefix);
        }
        let own = std::iter::once(layout::incarnation_prefix(&incarnation));
        for prefix in own.chain(legacy) {
            state.reclaim.prefixes.retain(|(p, _)| *p != prefix);
            state.reclaim.prefixes.push((prefix, incarnation.clone()));
        }
        stored
    }
}

#[async_trait]
impl RepositoryStore for Repositories {
    async fn by_name(&self, name: &str) -> Result<Option<Repository>, StoreError> {
        self.with(|state| Ok(found(state, name).cloned()))
    }

    async fn by_id(&self, id: i64) -> Result<Option<Repository>, StoreError> {
        self.with(|state| Ok(state.repositories.iter().find(|r| r.id == id).cloned()))
    }

    async fn all(&self) -> Result<Vec<Repository>, StoreError> {
        self.with(|state| {
            let mut all = state.repositories.clone();
            all.sort_by(|left, right| left.name.cmp(&right.name));
            Ok(all)
        })
    }

    async fn names(&self) -> Result<Vec<String>, StoreError> {
        self.with(|state| {
            let mut names: Vec<String> =
                state.repositories.iter().map(|r| r.name.clone()).collect();
            names.sort();
            Ok(names)
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
            let claimed = layout::name_keyed_prefixes(spec.format.as_str(), spec.name)
                .iter()
                .any(|p| state.reclaim.claims.iter().any(|c| c.key == *p && c.until > now));
            if claimed {
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

    async fn retire(&self, name: &str, now: DateTime<Utc>) -> Result<Vec<String>, StoreError> {
        self.with(|state| {
            let repo = found(state, name).ok_or(StoreError::NotFound)?.clone();
            if state.packages.iter().any(|pkg| pkg.repository_id == repo.id)
                || state.maven.values.iter().any(|v| v.repository == repo.id)
            {
                return Err(StoreError::Conflict);
            }
            let held = state
                .repositories
                .iter()
                .any(|r| r.repo_type == "group" && r.members().iter().any(|m| m == name));
            if held {
                return Err(StoreError::Conflict);
            }
            state.repositories.retain(|stored| stored.id != repo.id);
            state.grants.retain(|g| g.repository_id != repo.id);
            state.cache.retain(|e| e.repository_id != repo.id);
            state.maven.client.retain(|m| m.repository != repo.id);
            state.maven.counters.retain(|(r, _, _)| *r != repo.id);
            let Some(at) = state
                .reclaim
                .incarnations
                .iter()
                .position(|(id, _)| *id == repo.id)
            else {
                return Ok(Vec::new());
            };
            let (_, incarnation) = state.reclaim.incarnations.remove(at);
            state.reclaim.retired.push(incarnation.clone());
            let mut prefixes: Vec<String> = state
                .reclaim
                .prefixes
                .iter()
                .filter(|(_, i)| *i == incarnation)
                .map(|(p, _)| p.clone())
                .collect();
            prefixes.sort();
            for prefix in &prefixes {
                state.reclaim.revoke_under(prefix);
            }
            if repo.repo_type == "group" {
                return Ok(Vec::new());
            }
            for prefix in &prefixes {
                state.reclaim.enqueue(prefix, true, now);
            }
            Ok(prefixes)
        })
    }

    async fn incarnation(&self, repository: i64) -> Result<Option<String>, StoreError> {
        self.with(|state| {
            Ok(state
                .reclaim
                .incarnations
                .iter()
                .find(|(id, _)| *id == repository)
                .map(|(_, i)| i.clone()))
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

    /// The sweep's view of a version, when its repository is one of the two
    /// formats whose `-` really marks a pre-release.
    fn stale(state: &State, version: &Version) -> Option<StalePrerelease> {
        let package = state
            .packages
            .iter()
            .find(|pkg| pkg.id == version.package_id)?;
        let repo = state
            .repositories
            .iter()
            .find(|repo| repo.id == package.repository_id)?;
        matches!(repo.format.as_str(), "npm" | "cargo").then(|| StalePrerelease {
            id: version.id,
            package: package.name.clone(),
            version: version.version.clone(),
            tarball_path: version.tarball_path.clone(),
        })
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

    async fn anywhere(
        &self,
        name: &str,
        public_only: bool,
    ) -> Result<Option<Package>, StoreError> {
        self.with(|state| {
            let public: Vec<i64> = state
                .repositories
                .iter()
                .filter(|repo| repo.visibility == Visibility::Public)
                .map(|repo| repo.id)
                .collect();
            Ok(state
                .packages
                .iter()
                .find(|pkg| {
                    pkg.name == name && (!public_only || public.contains(&pkg.repository_id))
                })
                .cloned())
        })
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

    async fn stamp(&self, package: i64) -> Result<String, StoreError> {
        self.with(|state| {
            Ok(state
                .versions
                .iter()
                .filter(|v| v.package_id == package)
                .map(|v| format!("{}:{}", v.id, u8::from(v.yanked)))
                .collect::<Vec<_>>()
                .join(","))
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
            state.reclaim.live_pins(release.pins)?;
            let (package, version) = Self::write_release(state, &spec)?;
            for dep in release.dependencies {
                state.dependencies.push(DependencyRow {
                    version_id: version.id,
                    name: dep.name.to_string(),
                    requirement: dep.requirement.to_string(),
                    kind: dep.kind.to_string(),
                });
            }
            state.reclaim.spend(release.pins);
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
            state.reclaim.live_pins(promotion.pins)?;
            let (_, version) = Self::write_release(state, &spec)?;
            state.reclaim.spend(promotion.pins);
            let id = state.id();
            state.audit.push(AuditEntry {
                id,
                user_id: promotion.audit.user_id,
                username: Some(promotion.audit.username.to_string()),
                action: "package.promote".to_string(),
                target: Some(promotion.audit.target.to_string()),
                repository: Some(promotion.audit.repository.to_string()),
                ip: None,
                user_agent: None,
                details_json: Some(promotion.audit.details_json.to_string()),
                created_at: promotion.now,
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

    async fn stale_prereleases(
        &self,
        older_than: Duration,
        now: DateTime<Utc>,
    ) -> Result<Vec<StalePrerelease>, StoreError> {
        let cutoff = now - older_than;
        self.with(|state| {
            Ok(state
                .versions
                .iter()
                .filter(|v| v.version.contains('-') && v.published_at < cutoff)
                .filter_map(|v| Self::stale(state, v))
                .collect())
        })
    }

    async fn delete_version(&self, version: i64, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.with(|state| {
            let at = state
                .versions
                .iter()
                .position(|v| v.id == version)
                .ok_or(StoreError::NotFound)?;
            let gone = state.versions.remove(at);
            state.dist_tags.retain(|tag| tag.version_id != version);
            state.reclaim.enqueue(&gone.tarball_path, false, now);
            Ok(())
        })
    }

    /// Nothing to keep: the tally has no read side on this port, so a fake
    /// that stored it could never be asserted against.
    async fn record_download(&self, _version: i64) -> Result<(), StoreError> {
        self.with(|_| Ok(()))
    }
}

struct NugetFeed(Arc<Mutex<State>>);

#[async_trait]
impl NugetFeedRead for NugetFeed {
    async fn search(&self, query: &FeedQuery<'_>) -> Result<FeedPage, StoreError> {
        with(&self.0, PortId::Search, |state| {
            let candidates = state
                .packages
                .iter()
                .filter(|p| p.repository_id == query.repository)
                .map(|p| {
                    let versions = state
                        .versions
                        .iter()
                        .filter(|v| v.package_id == p.id)
                        .cloned()
                        .collect();
                    (p.clone(), versions, 0)
                })
                .collect();
            Ok(query.page(candidates))
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

struct Users(Arc<Mutex<State>>);

impl Users {
    fn find<'s>(state: &'s mut State, username: &str) -> Result<&'s mut User, StoreError> {
        state
            .users
            .iter_mut()
            .find(|user| user.username == username)
            .ok_or(StoreError::NotFound)
    }
}

#[async_trait]
impl UserStore for Users {
    async fn by_name(&self, username: &str) -> Result<Option<User>, StoreError> {
        with(&self.0, PortId::Users, |state| {
            Ok(state
                .users
                .iter()
                .find(|user| user.username == username)
                .cloned())
        })
    }

    async fn by_id(&self, id: i64) -> Result<Option<User>, StoreError> {
        with(&self.0, PortId::Users, |state| {
            Ok(state.users.iter().find(|user| user.id == id).cloned())
        })
    }

    async fn all(&self) -> Result<Vec<User>, StoreError> {
        with(&self.0, PortId::Users, |state| Ok(state.users.clone()))
    }

    async fn create(&self, user: &NewUser<'_>, now: DateTime<Utc>) -> Result<User, StoreError> {
        with(&self.0, PortId::Users, |state| {
            if state.users.iter().any(|u| u.username == user.username) {
                return Err(StoreError::Conflict);
            }
            state.next_id += 1;
            let stored = User {
                id: state.next_id,
                username: user.username.to_string(),
                email: user.email.map(str::to_string),
                password_hash: user.password_hash.to_string(),
                role: user.role.to_string(),
                must_change_password: false,
                created_at: now,
                updated_at: now,
            };
            state.users.push(stored.clone());
            Ok(stored)
        })
    }

    async fn update(
        &self,
        username: &str,
        patch: &UserPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<User, StoreError> {
        with(&self.0, PortId::Users, |state| {
            let stored = Self::find(state, username)?;
            if patch.touches_nothing() {
                return Ok(stored.clone());
            }
            if let Some(email) = patch.email {
                stored.email = Some(email.to_string());
            }
            if let Some(hash) = patch.password_hash {
                stored.password_hash = hash.to_string();
            }
            if let Some(role) = patch.role {
                stored.role = role.to_string();
            }
            if let Some(must_change) = patch.must_change_password {
                stored.must_change_password = must_change;
            }
            stored.updated_at = now;
            Ok(stored.clone())
        })
    }

    async fn delete(&self, username: &str) -> Result<(), StoreError> {
        with(&self.0, PortId::Users, |state| {
            let at = state
                .users
                .iter()
                .position(|user| user.username == username)
                .ok_or(StoreError::NotFound)?;
            let gone = state.users.remove(at).id;
            state.tokens.retain(|token| token.user_id != gone);
            state.grants.retain(|grant| grant.user_id != gone);
            Ok(())
        })
    }
}

struct Tokens(Arc<Mutex<State>>);

#[async_trait]
impl TokenStore for Tokens {
    async fn by_prefix(&self, prefix: &str) -> Result<Option<ApiToken>, StoreError> {
        with(&self.0, PortId::Tokens, |state| {
            Ok(state
                .tokens
                .iter()
                .find(|token| token.prefix == prefix)
                .cloned())
        })
    }

    async fn by_id(&self, id: &str) -> Result<Option<ApiToken>, StoreError> {
        with(&self.0, PortId::Tokens, |state| {
            Ok(state.tokens.iter().find(|token| token.id == id).cloned())
        })
    }

    async fn of_user(&self, user_id: i64) -> Result<Vec<ApiToken>, StoreError> {
        with(&self.0, PortId::Tokens, |state| {
            let mut mine: Vec<ApiToken> = state
                .tokens
                .iter()
                .filter(|token| token.user_id == user_id)
                .cloned()
                .collect();
            mine.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            Ok(mine)
        })
    }

    async fn create(
        &self,
        token: &NewToken<'_>,
        now: DateTime<Utc>,
    ) -> Result<ApiToken, StoreError> {
        with(&self.0, PortId::Tokens, |state| {
            if state.tokens.iter().any(|stored| stored.id == token.id) {
                return Err(StoreError::Conflict);
            }
            let stored = ApiToken {
                id: token.id.to_string(),
                user_id: token.user_id,
                name: token.name.to_string(),
                prefix: token.prefix.to_string(),
                token_hash: token.token_hash.to_string(),
                expires_at: token.expires_at,
                last_used_at: None,
                created_at: now,
            };
            state.tokens.push(stored.clone());
            Ok(stored)
        })
    }

    async fn delete(&self, id: &str) -> Result<(), StoreError> {
        with(&self.0, PortId::Tokens, |state| {
            let at = state
                .tokens
                .iter()
                .position(|token| token.id == id)
                .ok_or(StoreError::NotFound)?;
            state.tokens.remove(at);
            Ok(())
        })
    }

    async fn touch(&self, id: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        with(&self.0, PortId::Tokens, |state| {
            let stored = state
                .tokens
                .iter_mut()
                .find(|token| token.id == id)
                .ok_or(StoreError::NotFound)?;
            stored.last_used_at = Some(now);
            Ok(())
        })
    }
}

struct Permissions(Arc<Mutex<State>>);

#[async_trait]
impl PermissionStore for Permissions {
    async fn rights(
        &self,
        user_id: i64,
        repository_id: i64,
    ) -> Result<Option<Rights>, StoreError> {
        with(&self.0, PortId::Permissions, |state| {
            Ok(state
                .grants
                .iter()
                .find(|grant| grant.user_id == user_id && grant.repository_id == repository_id)
                .map(|grant| grant.rights))
        })
    }

    async fn of_user(&self, user_id: i64) -> Result<Vec<RepoRights>, StoreError> {
        with(&self.0, PortId::Permissions, |state| {
            Ok(state
                .grants
                .iter()
                .filter(|grant| grant.user_id == user_id)
                .map(|grant| RepoRights {
                    repository_id: grant.repository_id,
                    repository: state
                        .repositories
                        .iter()
                        .find(|repo| repo.id == grant.repository_id)
                        .map(|repo| repo.name.clone()),
                    rights: grant.rights,
                })
                .collect())
        })
    }

    async fn set(
        &self,
        user_id: i64,
        repository_id: i64,
        rights: Rights,
        _now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        with(&self.0, PortId::Permissions, |state| {
            match state
                .grants
                .iter_mut()
                .find(|grant| grant.user_id == user_id && grant.repository_id == repository_id)
            {
                Some(grant) => grant.rights = rights,
                None => state.grants.push(Grant {
                    user_id,
                    repository_id,
                    rights,
                }),
            }
            Ok(())
        })
    }

    async fn revoke(&self, user_id: i64, repository_id: i64) -> Result<(), StoreError> {
        with(&self.0, PortId::Permissions, |state| {
            state
                .grants
                .retain(|grant| !(grant.user_id == user_id && grant.repository_id == repository_id));
            Ok(())
        })
    }
}

struct ProxyCache(Arc<Mutex<State>>);

impl ProxyCache {
    fn at(state: &mut State, repo: RepoId, kind: &str, key: &str) -> Option<usize> {
        state
            .cache
            .iter()
            .position(|row| row.repository_id == repo && row.kind == kind && row.cache_key == key)
    }
}

#[async_trait]
impl ProxyCacheStore for ProxyCache {
    async fn entry(
        &self,
        repo: RepoId,
        kind: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<CacheEntry>, StoreError> {
        with(&self.0, PortId::ProxyCache, |state| {
            let found = Self::at(state, repo, kind, key).map(|at| {
                let mut row = state.cache[at].clone();
                row.fresh = CacheEntry::fresh_at(row.expires_at, now);
                row
            });
            Ok(found)
        })
    }

    async fn upsert(&self, entry: &NewEntry<'_>, now: DateTime<Utc>) -> Result<(), StoreError> {
        with(&self.0, PortId::ProxyCache, |state| {
            let at = Self::at(state, entry.repository_id, entry.kind, entry.cache_key);
            let id = match at {
                Some(at) => state.cache[at].id,
                None => {
                    state.next_cache_id += 1;
                    state.next_cache_id
                }
            };
            let stored = CacheEntry {
                id,
                repository_id: entry.repository_id,
                kind: entry.kind.to_string(),
                cache_key: entry.cache_key.to_string(),
                status: entry.status,
                storage_path: entry.storage_path.map(str::to_string),
                content_type: entry.content_type.map(str::to_string),
                etag: entry.etag.map(str::to_string),
                digest: entry.digest.map(str::to_string),
                size: entry.size,
                fetched_at: now,
                expires_at: CacheEntry::expiry(entry.ttl(), now),
                last_used_at: now,
                fresh: true,
            };
            match at {
                Some(at) => state.cache[at] = stored,
                None => state.cache.push(stored),
            }
            Ok(())
        })
    }

    async fn touch(
        &self,
        id: CacheEntryId,
        ttl: Option<Duration>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        with(&self.0, PortId::ProxyCache, |state| {
            if let Some(row) = state.cache.iter_mut().find(|row| row.id == id) {
                row.last_used_at = now;
                if let Some(until) = CacheEntry::expiry(ttl, now) {
                    row.expires_at = Some(until);
                }
            }
            Ok(())
        })
    }

    async fn delete_for_repo(&self, repo: RepoId) -> Result<u64, StoreError> {
        with(&self.0, PortId::ProxyCache, |state| {
            let before = state.cache.len();
            state.cache.retain(|row| row.repository_id != repo);
            Ok((before - state.cache.len()) as u64)
        })
    }

    async fn evictable(
        &self,
        idle: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<CacheEntry>, StoreError> {
        with(&self.0, PortId::ProxyCache, |state| {
            let unused_since = now - idle;
            let mut rows: Vec<CacheEntry> = state
                .cache
                .iter()
                .filter(|row| {
                    let expired = row.status != 200 && row.expires_at.is_some_and(|at| at <= now);
                    expired || row.last_used_at < unused_since
                })
                .cloned()
                .map(|mut row| {
                    row.fresh = CacheEntry::fresh_at(row.expires_at, now);
                    row
                })
                .collect();
            rows.sort_by_key(|row| row.id);
            rows.truncate(limit as usize);
            Ok(rows)
        })
    }

    async fn delete(&self, id: CacheEntryId) -> Result<(), StoreError> {
        with(&self.0, PortId::ProxyCache, |state| {
            state.cache.retain(|row| row.id != id);
            Ok(())
        })
    }

    async fn quarantine(
        &self,
        repo: RepoId,
        kind: &str,
        key: &str,
        announced: &str,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let key = format!("{kind}/{key}");
        let entry = NewEntry {
            repository_id: repo,
            kind: "quarantine",
            cache_key: &key,
            status: 502,
            storage_path: None,
            content_type: Some(reason),
            etag: None,
            digest: Some(announced),
            size: 0,
            ttl_secs: None,
        };
        self.upsert(&entry, now).await
    }

    async fn quarantined(
        &self,
        repo: RepoId,
        kind: &str,
        key: &str,
        announced: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let key = format!("{kind}/{key}");
        with(&self.0, PortId::ProxyCache, |state| {
            let at = Self::at(state, repo, "quarantine", &key)
                .filter(|&at| state.cache[at].digest.as_deref() == Some(announced));
            if let Some(at) = at {
                state.cache[at].last_used_at = now;
            }
            Ok(at.is_some())
        })
    }
}

struct Oci(Arc<Mutex<State>>);

fn after(now: DateTime<Utc>, ttl: Duration) -> DateTime<Utc> {
    now + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::MAX)
}

#[async_trait]
impl OciStore for Oci {
    async fn blob(&self, repository: i64, digest: &str) -> Result<Option<Blob>, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            Ok(state
                .oci_blobs
                .iter()
                .find(|row| row.repository_id == repository && row.digest == digest)
                .map(|row| Blob {
                    size: row.size,
                    content_type: row.content_type.clone(),
                    key: row.key.clone(),
                }))
        })
    }

    async fn manifest(
        &self,
        repository: i64,
        name: &str,
        digest: &str,
    ) -> Result<Option<Manifest>, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            Ok(state
                .oci_manifests
                .iter()
                .find(|row| {
                    row.repository_id == repository && row.name == name && row.digest == digest
                })
                .map(|row| Manifest {
                    content_type: row.content_type.clone(),
                    size: row.size,
                    key: row.key.clone(),
                }))
        })
    }

    async fn digest_for_ref(
        &self,
        repository: i64,
        name: &str,
        reference: &str,
    ) -> Result<Option<String>, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            Ok(state
                .oci_tags
                .iter()
                .find(|row| {
                    row.repository_id == repository && row.name == name && row.tag == reference
                })
                .map(|row| row.manifest_digest.clone()))
        })
    }

    async fn tags(&self, repository: i64, name: &str) -> Result<Vec<String>, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            let mut tags: Vec<String> = state
                .oci_tags
                .iter()
                .filter(|row| row.repository_id == repository && row.name == name)
                .map(|row| row.tag.clone())
                .collect();
            tags.sort();
            Ok(tags)
        })
    }

    async fn put_manifest(&self, m: NewManifest<'_>, now: DateTime<Utc>) -> Result<(), StoreError> {
        with(&self.0, PortId::Oci, |state| {
            let mut pins = vec![m.pin.clone()];
            pins.extend(m.blobs.iter().map(|(_, pin)| pin.clone()));
            state.reclaim.live_pins(&pins)?;
            let unknown = m.blobs.iter().any(|(digest, _)| {
                !state
                    .oci_blobs
                    .iter()
                    .any(|row| row.repository_id == m.repository && row.digest == *digest)
            });
            if unknown {
                return Err(StoreError::NotFound);
            }
            state.reclaim.spend(&pins);
            let previous = state
                .oci_manifests
                .iter()
                .find(|row| {
                    row.repository_id == m.repository && row.name == m.name && row.digest == m.digest
                })
                .map(|row| row.key.clone());
            if let Some(previous) = previous.filter(|k| *k != m.pin.physical_key) {
                state.reclaim.enqueue(&previous, false, now);
            }
            state.oci_manifests.retain(|row| {
                !(row.repository_id == m.repository
                    && row.name == m.name
                    && row.digest == m.digest)
            });
            state.oci_manifests.push(ManifestRow {
                repository_id: m.repository,
                name: m.name.to_string(),
                digest: m.digest.to_string(),
                content_type: m.content_type.to_string(),
                size: m.size,
                key: m.pin.physical_key.clone(),
            });
            state.oci_links.retain(|row| {
                !(row.repository_id == m.repository && row.manifest_digest == m.digest)
            });
            let linked = m.blobs.iter().map(|(d, _)| d).chain(m.children.iter());
            for blob in linked {
                let present = state.oci_links.iter().any(|row| {
                    row.repository_id == m.repository
                        && row.manifest_digest == m.digest
                        && row.blob_digest == *blob
                });
                if !present {
                    state.oci_links.push(LinkRow {
                        repository_id: m.repository,
                        manifest_digest: m.digest.to_string(),
                        blob_digest: blob.clone(),
                    });
                }
            }
            if let Some(tag) = m.tag {
                state.oci_tags.retain(|row| {
                    !(row.repository_id == m.repository && row.name == m.name && row.tag == tag)
                });
                state.oci_tags.push(TagRow {
                    repository_id: m.repository,
                    name: m.name.to_string(),
                    tag: tag.to_string(),
                    manifest_digest: m.digest.to_string(),
                });
            }
            Ok(())
        })
    }

    async fn delete_manifest(
        &self,
        repository: i64,
        name: &str,
        digest: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Orphaned>, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            let Some(at) = state.oci_manifests.iter().position(|row| {
                row.repository_id == repository && row.name == name && row.digest == digest
            }) else {
                return Ok(None);
            };
            let gone = state.oci_manifests.remove(at);
            let mut released = vec![gone.key];
            state.oci_tags.retain(|row| {
                !(row.repository_id == repository
                    && row.name == name
                    && row.manifest_digest == digest)
            });
            let still_listed = state
                .oci_manifests
                .iter()
                .any(|row| row.repository_id == repository && row.digest == digest);
            let mut blob_digests = Vec::new();
            if !still_listed {
                let linked: Vec<String> = state
                    .oci_links
                    .iter()
                    .filter(|row| row.repository_id == repository && row.manifest_digest == digest)
                    .map(|row| row.blob_digest.clone())
                    .collect();
                state.oci_links.retain(|row| {
                    !(row.repository_id == repository && row.manifest_digest == digest)
                });
                for blob in linked {
                    let still = state
                        .oci_links
                        .iter()
                        .any(|row| row.repository_id == repository && row.blob_digest == blob);
                    if still {
                        continue;
                    }
                    if let Some(at) = state
                        .oci_blobs
                        .iter()
                        .position(|row| row.repository_id == repository && row.digest == blob)
                    {
                        released.push(state.oci_blobs.remove(at).key);
                        blob_digests.push(blob);
                    }
                }
            }
            for key in &released {
                state.reclaim.enqueue(key, false, now);
            }
            Ok(Some(Orphaned { blob_digests }))
        })
    }

    async fn blob_references(&self, repository: i64, digest: &str) -> Result<i64, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            Ok(state
                .oci_links
                .iter()
                .filter(|row| row.repository_id == repository && row.blob_digest == digest)
                .count() as i64)
        })
    }

    async fn delete_blob(
        &self,
        repository: i64,
        digest: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            let listed = state
                .oci_links
                .iter()
                .any(|row| row.repository_id == repository && row.blob_digest == digest);
            if listed {
                return Err(StoreError::Conflict);
            }
            let Some(at) = state
                .oci_blobs
                .iter()
                .position(|row| row.repository_id == repository && row.digest == digest)
            else {
                return Ok(false);
            };
            let gone = state.oci_blobs.remove(at);
            state.reclaim.enqueue(&gone.key, false, now);
            Ok(true)
        })
    }

    async fn start_upload(
        &self,
        id: &str,
        repository: i64,
        _name: &str,
        prefix: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        with(&self.0, PortId::Oci, |state| {
            if state.oci_uploads.iter().any(|row| row.id == id) {
                return Err(StoreError::Conflict);
            }
            state.oci_uploads.push(UploadRow {
                id: id.to_string(),
                repository_id: repository,
                prefix: Some(prefix.to_string()),
                received: Some(0),
                segments: 0,
                touched: now,
                lease: None,
            });
            Ok(())
        })
    }

    async fn upload(&self, id: &str) -> Result<Option<UploadSession>, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            Ok(state.oci_uploads.iter().find(|row| row.id == id).and_then(|row| {
                Some(UploadSession {
                    repository: row.repository_id,
                    prefix: row.prefix.clone()?,
                    received: row.received?,
                    segments: row.segments,
                })
            }))
        })
    }

    async fn claim_segment(
        &self,
        id: &str,
        segment: &Segment,
        max_segments: u32,
        now: DateTime<Utc>,
    ) -> Result<SegmentClaim, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            let Some(row) = state.oci_uploads.iter_mut().find(|row| row.id == id) else {
                return Ok(SegmentClaim::Lost);
            };
            let completing = row.lease.as_ref().is_some_and(|(_, until)| *until > now);
            if completing || row.received != Some(segment.start) {
                return Ok(SegmentClaim::Lost);
            }
            if row.segments >= max_segments {
                return Ok(SegmentClaim::TooManySegments);
            }
            row.received = Some(segment.start + segment.len);
            row.segments += 1;
            row.touched = now;
            state.oci_segments.push((id.to_string(), segment.clone()));
            Ok(SegmentClaim::Won)
        })
    }

    async fn segments(&self, id: &str) -> Result<Vec<Segment>, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            let mut segments: Vec<Segment> = state
                .oci_segments
                .iter()
                .filter(|(upload, _)| upload == id)
                .map(|(_, s)| s.clone())
                .collect();
            segments.sort_by_key(|s| s.start);
            Ok(segments)
        })
    }

    async fn begin_complete(
        &self,
        id: &str,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> Result<BeginComplete, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            let Some(row) = state
                .oci_uploads
                .iter_mut()
                .find(|row| row.id == id && row.received.is_some())
            else {
                return Ok(BeginComplete::Unknown);
            };
            if row.lease.as_ref().is_some_and(|(_, until)| *until > now) {
                return Ok(BeginComplete::Held);
            }
            let token = uuid::Uuid::new_v4().to_string();
            row.lease = Some((token.clone(), after(now, ttl)));
            row.touched = now;
            Ok(BeginComplete::Lease(LeaseToken(token)))
        })
    }

    async fn release_complete(&self, id: &str, lease: &LeaseToken) -> Result<(), StoreError> {
        with(&self.0, PortId::Oci, |state| {
            if let Some(row) = state.oci_uploads.iter_mut().find(|row| {
                row.id == id && row.lease.as_ref().is_some_and(|(t, _)| *t == lease.0)
            }) {
                row.lease = None;
            }
            Ok(())
        })
    }

    async fn finish_upload(
        &self,
        id: &str,
        lease: &LeaseToken,
        pin: &PinToken,
        blob: NewBlob<'_>,
        now: DateTime<Utc>,
    ) -> Result<Finished, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            let held = state.oci_uploads.iter().any(|row| {
                row.id == id && row.lease.as_ref().is_some_and(|(t, _)| *t == lease.0)
            });
            if !held {
                return Ok(Finished::LeaseLost);
            }
            state.reclaim.live_pins(std::slice::from_ref(pin))?;
            state.reclaim.spend(std::slice::from_ref(pin));
            let existing = state
                .oci_blobs
                .iter()
                .find(|row| row.repository_id == blob.repository && row.digest == blob.digest)
                .map(|row| row.key.clone());
            let recorded = match existing {
                Some(key) => {
                    if key != pin.physical_key {
                        state.reclaim.enqueue(&pin.physical_key, false, now);
                    }
                    key
                }
                None => {
                    state.oci_blobs.push(BlobRow {
                        repository_id: blob.repository,
                        digest: blob.digest.to_string(),
                        size: blob.size,
                        content_type: Some(blob.content_type.to_string()),
                        key: pin.physical_key.clone(),
                    });
                    pin.physical_key.clone()
                }
            };
            state.oci_segments.retain(|(upload, _)| upload != id);
            state.oci_uploads.retain(|row| row.id != id);
            Ok(Finished::Recorded(recorded))
        })
    }

    async fn reap_uploads(
        &self,
        idle: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<u64, StoreError> {
        with(&self.0, PortId::Oci, |state| {
            let cut = cutoff(idle, now);
            let mut dead: Vec<(String, String)> = state
                .oci_uploads
                .iter()
                .filter(|row| {
                    row.received.is_none()
                        || (row.touched <= cut
                            && row.lease.as_ref().is_none_or(|(_, until)| *until <= now))
                })
                .map(|row| (row.id.clone(), upload_prefix(row)))
                .collect();
            dead.sort();
            dead.truncate(limit as usize);
            for (id, prefix) in &dead {
                state.oci_segments.retain(|(upload, _)| upload != id);
                state.oci_uploads.retain(|row| row.id != *id);
                state.reclaim.enqueue(prefix, true, now);
            }
            Ok(dead.len() as u64)
        })
    }
}

fn upload_prefix(row: &UploadRow) -> String {
    row.prefix
        .clone()
        .unwrap_or_else(|| format!("oci/_uploads/{}", row.id))
}

struct Audit(Arc<Mutex<State>>);

/// Newest first, then the page, exactly as the adapter's `ORDER BY
/// created_at DESC LIMIT ?1 OFFSET ?2` does.
fn page_of(mut rows: Vec<AuditEntry>, page: i64, size: i64) -> Vec<AuditEntry> {
    rows.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    let offset = page.saturating_sub(1).max(0).saturating_mul(size.max(0));
    rows.into_iter()
        .skip(offset.max(0) as usize)
        .take(size.max(0) as usize)
        .collect()
}

fn clone_entry(entry: &AuditEntry) -> AuditEntry {
    AuditEntry {
        id: entry.id,
        user_id: entry.user_id,
        username: entry.username.clone(),
        action: entry.action.clone(),
        target: entry.target.clone(),
        repository: entry.repository.clone(),
        ip: entry.ip.clone(),
        user_agent: entry.user_agent.clone(),
        details_json: entry.details_json.clone(),
        created_at: entry.created_at,
    }
}

#[async_trait]
impl AuditStore for Audit {
    async fn append(
        &self,
        entry: &NewAuditEntry<'_>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        with(&self.0, PortId::Audit, |state| {
            let id = state.id();
            state.audit.push(AuditEntry {
                id,
                user_id: entry.user_id,
                username: entry.username.map(str::to_string),
                action: entry.action.to_string(),
                target: entry.target.map(str::to_string),
                repository: entry.repository.map(str::to_string),
                ip: entry.ip.map(str::to_string),
                user_agent: entry.user_agent.map(str::to_string),
                details_json: entry.details_json.map(str::to_string),
                created_at: now,
            });
            Ok(())
        })
    }

    async fn recent(&self, page: i64, size: i64) -> Result<Vec<AuditEntry>, StoreError> {
        with(&self.0, PortId::Audit, |state| {
            Ok(page_of(
                state.audit.iter().map(clone_entry).collect(),
                page,
                size,
            ))
        })
    }

    async fn of_target(
        &self,
        action: &str,
        target: &str,
    ) -> Result<Vec<AuditEntry>, StoreError> {
        with(&self.0, PortId::Audit, |state| {
            let matching: Vec<AuditEntry> = state
                .audit
                .iter()
                .filter(|e| e.action == action && e.target.as_deref() == Some(target))
                .map(clone_entry)
                .collect();
            Ok(page_of(matching, 1, i64::MAX))
        })
    }
}

struct Dependencies(Arc<Mutex<State>>);

#[async_trait]
impl DependencyStore for Dependencies {
    async fn record(
        &self,
        dep: &NewDependency<'_>,
        _now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        with(&self.0, PortId::Dependencies, |state| {
            state.dependencies.push(DependencyRow {
                version_id: dep.version,
                name: dep.name.to_string(),
                requirement: dep.requirement.to_string(),
                kind: dep.kind.to_string(),
            });
            Ok(())
        })
    }

    async fn of_version(&self, version: i64) -> Result<Vec<Dependency>, StoreError> {
        with(&self.0, PortId::Dependencies, |state| {
            Ok(state
                .dependencies
                .iter()
                .filter(|row| row.version_id == version)
                .map(|row| Dependency {
                    name: row.name.clone(),
                    requirement: row.requirement.clone(),
                    kind: row.kind.clone(),
                })
                .collect())
        })
    }

    async fn dependents(
        &self,
        name: &str,
        public_only: bool,
    ) -> Result<Vec<Dependent>, StoreError> {
        with(&self.0, PortId::Dependencies, |state| {
            let public: Vec<i64> = state
                .repositories
                .iter()
                .filter(|repo| repo.visibility == Visibility::Public)
                .map(|repo| repo.id)
                .collect();
            let mut found: Vec<Dependent> = Vec::new();
            for row in state
                .dependencies
                .iter()
                .filter(|r| r.name == name)
            {
                let Some(version) = state.versions.iter().find(|v| v.id == row.version_id) else {
                    continue;
                };
                let Some(package) = state.packages.iter().find(|p| p.id == version.package_id)
                else {
                    continue;
                };
                if public_only && !public.contains(&package.repository_id) {
                    continue;
                }
                let hit = Dependent {
                    name: package.name.clone(),
                    version: version.version.clone(),
                };
                if !found
                    .iter()
                    .any(|d| d.name == hit.name && d.version == hit.version)
                {
                    found.push(hit);
                }
            }
            Ok(found)
        })
    }
}

struct Vulns(Arc<Mutex<State>>);

#[async_trait]
impl VulnStore for Vulns {
    async fn latest(&self, version: i64) -> Result<Option<VulnScan>, StoreError> {
        with(&self.0, PortId::Vulns, |state| {
            Ok(state
                .scans
                .iter()
                .filter(|row| row.version_id == version)
                .max_by_key(|row| row.scanned_at)
                .map(|row| VulnScan {
                    scanned_at: row.scanned_at,
                    total_deps: row.result.total_deps as i64,
                    vulnerable_deps: row.result.vulnerable_deps as i64,
                    status: row.result.status.clone(),
                    details: serde_json::to_value(&row.result.details).ok(),
                }))
        })
    }

    async fn record(
        &self,
        version: i64,
        result: &ScanResult,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        with(&self.0, PortId::Vulns, |state| {
            state.scans.push(ScanRow {
                version_id: version,
                scanned_at: now,
                result: result.clone(),
            });
            Ok(())
        })
    }

    async fn forget(&self, version: i64) -> Result<(), StoreError> {
        with(&self.0, PortId::Vulns, |state| {
            state.scans.retain(|row| row.version_id != version);
            Ok(())
        })
    }
}

struct Policy(Arc<Mutex<State>>);

/// The window predicate every report query shares, spelled once.
fn in_window(entry: &ResolutionEntry, f: &ReportFilter<'_>, range: Option<IdRange>) -> bool {
    let row = &entry.row;
    if row.created_at < f.since {
        return false;
    }
    if let Some(range) = range {
        if row.id <= range.after || row.id > range.upto {
            return false;
        }
    }
    if let Some(repo) = f.repo {
        if row.requested_repo != repo && row.member_repo != repo {
            return false;
        }
    }
    match f.subject {
        Some(Subject::User(id)) => row.user_id == Some(id),
        Some(Subject::Static) => row.actor_kind == "static",
        None => true,
    }
}

/// The rows a filter selects, with the flags the report reads: narrowed to
/// one rule, they are that rule's verdict rather than the row's own.
fn selected<'a>(
    state: &'a State,
    f: &ReportFilter<'_>,
    range: Option<IdRange>,
) -> Vec<(&'a ResolutionEntry, bool, bool)> {
    state
        .resolutions
        .iter()
        .filter(|entry| in_window(entry, f, range))
        .filter_map(|entry| match f.rule {
            Some(rule) => entry
                .verdicts
                .iter()
                .find(|v| v.rule == rule)
                .map(|v| (entry, v.verdict == "would_block", v.verdict == "unknown")),
            None => Some((entry, entry.row.would_block, entry.row.unknown)),
        })
        .collect()
}

#[async_trait]
impl PolicyStore for Policy {
    async fn insert_batch(
        &self,
        rows: &[NewResolution<'_>],
        now: DateTime<Utc>,
    ) -> Result<Vec<i64>, StoreError> {
        with(&self.0, PortId::Policy, |state| {
            let mut ids = Vec::with_capacity(rows.len());
            for row in rows {
                let id = state.id();
                let would_block = row
                    .verdicts
                    .iter()
                    .any(|v| v.verdict == Verdict::WouldBlock);
                let unknown =
                    !would_block && row.verdicts.iter().any(|v| v.verdict == Verdict::Unknown);
                state.resolutions.push(ResolutionEntry {
                    row: ResolutionRow {
                        id,
                        created_at: now,
                        requested_repo: row.requested_repo.to_string(),
                        member_repo: row.member_repo.to_string(),
                        format: row.format.to_string(),
                        name: row.name.to_string(),
                        version: row.version.map(str::to_string),
                        digest: row.digest.map(str::to_string),
                        date_source: row.date_source.to_string(),
                        actor: row.actor.to_string(),
                        actor_kind: row.actor_kind.to_string(),
                        user_id: row.user_id,
                        published_at: row.published_at,
                        would_block,
                        unknown,
                    },
                    verdicts: row
                        .verdicts
                        .iter()
                        .map(|v| VerdictRow {
                            resolution_id: id,
                            rule: v.rule.to_string(),
                            verdict: v.verdict.as_str().to_string(),
                            reason: v.reason.clone(),
                        })
                        .collect(),
                });
                ids.push(id);
            }
            Ok(ids)
        })
    }

    async fn max_id(&self) -> Result<i64, StoreError> {
        with(&self.0, PortId::Policy, |state| {
            Ok(state
                .resolutions
                .iter()
                .map(|entry| entry.row.id)
                .max()
                .unwrap_or(0))
        })
    }

    async fn totals(
        &self,
        filter: &ReportFilter<'_>,
        range: IdRange,
    ) -> Result<Totals, StoreError> {
        with(&self.0, PortId::Policy, |state| {
            let hits = selected(state, filter, Some(range));
            let mut totals = Totals {
                resolutions: hits.len() as u64,
                ..Totals::default()
            };
            for (entry, would_block, unknown) in hits {
                totals.would_block += u64::from(would_block);
                totals.unknown += u64::from(unknown);
                for verdict in entry
                    .verdicts
                    .iter()
                    .filter(|v| filter.rule.is_none_or(|rule| v.rule == rule))
                {
                    let counts: &mut RuleTotals =
                        totals.by_rule.entry(verdict.rule.clone()).or_default();
                    match verdict.verdict.as_str() {
                        "would_block" => counts.would_block += 1,
                        "unknown" => counts.unknown += 1,
                        "pass" => counts.pass += 1,
                        _ => counts.not_applicable += 1,
                    }
                }
            }
            Ok(totals)
        })
    }

    async fn resolutions(
        &self,
        filter: &ReportFilter<'_>,
        page: i64,
        size: i64,
    ) -> Result<Vec<ResolutionRow>, StoreError> {
        with(&self.0, PortId::Policy, |state| {
            let mut rows: Vec<ResolutionRow> = selected(state, filter, None)
                .into_iter()
                .map(|(entry, would_block, unknown)| ResolutionRow {
                    would_block,
                    unknown,
                    ..entry.row.clone()
                })
                .collect();
            rows.sort_by(|a, b| b.id.cmp(&a.id));
            let offset = page.saturating_sub(1).max(0).saturating_mul(size.max(0));
            Ok(rows
                .into_iter()
                .skip(offset.max(0) as usize)
                .take(size.max(0) as usize)
                .collect())
        })
    }

    async fn verdicts_for(
        &self,
        ids: &[i64],
        rule: Option<&str>,
    ) -> Result<Vec<VerdictRow>, StoreError> {
        with(&self.0, PortId::Policy, |state| {
            let mut found: Vec<VerdictRow> = state
                .resolutions
                .iter()
                .filter(|entry| ids.contains(&entry.row.id))
                .flat_map(|entry| entry.verdicts.iter())
                .filter(|v| rule.is_none_or(|wanted| v.rule == wanted))
                .map(|v| VerdictRow {
                    resolution_id: v.resolution_id,
                    rule: v.rule.clone(),
                    verdict: v.verdict.clone(),
                    reason: v.reason.clone(),
                })
                .collect();
            found.sort_by(|a, b| {
                a.resolution_id
                    .cmp(&b.resolution_id)
                    .then(a.rule.cmp(&b.rule))
            });
            Ok(found)
        })
    }

    async fn delete_older_than(
        &self,
        days: u64,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        with(&self.0, PortId::Policy, |state| {
            let cutoff = i64::try_from(days)
                .ok()
                .and_then(chrono::Duration::try_days)
                .and_then(|span| now.checked_sub_signed(span))
                .unwrap_or(DateTime::<Utc>::MIN_UTC);
            let before = state.resolutions.len();
            state.resolutions.retain(|e| e.row.created_at >= cutoff);
            Ok((before - state.resolutions.len()) as u64)
        })
    }

    async fn erase_user(&self, user_id: i64) -> Result<u64, StoreError> {
        with(&self.0, PortId::Policy, |state| {
            let before = state.resolutions.len();
            state.resolutions.retain(|e| e.row.user_id != Some(user_id));
            Ok((before - state.resolutions.len()) as u64)
        })
    }
}

struct Pypi(Arc<Mutex<State>>);

impl Pypi {
    fn with<T>(
        &self,
        act: impl FnOnce(&mut State) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        with(&self.0, PortId::Pypi, act)
    }
}

fn pypi_file(state: &State, row: &PypiFile) -> PypiFile {
    let mut file = row.clone();
    if let Some(p) = state.packages.iter().find(|p| p.id == row.package_id) {
        file.project = p.name.clone();
    }
    if let Some(v) = state.versions.iter().find(|v| v.id == row.version_id) {
        file.version = v.version.clone();
    }
    file
}

fn pypi_release_versions(
    state: &State,
    repository: i64,
    project: &str,
    version: Option<&str>,
) -> Vec<i64> {
    let Some(package) = state
        .packages
        .iter()
        .find(|p| p.repository_id == repository && p.name == project)
    else {
        return Vec::new();
    };
    state
        .versions
        .iter()
        .filter(|v| v.package_id == package.id && version.is_none_or(|want| v.version == want))
        .map(|v| v.id)
        .collect()
}

fn pypi_purge(state: &mut State, versions: &[i64], now: DateTime<Utc>) -> Vec<String> {
    let mut keys = Vec::new();
    for &version in versions {
        for f in state.pypi_files.iter().filter(|f| f.version_id == version) {
            keys.push(f.key.clone());
            keys.extend(f.metadata_key.clone());
        }
        if let Some(v) = state.versions.iter().find(|v| v.id == version) {
            keys.push(v.tarball_path.clone());
        }
        state.pypi_files.retain(|f| f.version_id != version);
        state.dist_tags.retain(|t| t.version_id != version);
        state.versions.retain(|v| v.id != version);
    }
    keys.sort();
    keys.dedup();
    for key in &keys {
        state.reclaim.enqueue(key, false, now);
    }
    keys
}

#[async_trait]
impl PypiFileStore for Pypi {
    async fn publish_file(&self, file: &NewPypiFile<'_>) -> Result<Published, StoreError> {
        self.with(|state| {
            let Some(artifact) = file.pins.first() else {
                return Err(StoreError::Other("a file row needs its artifact's pin".into()));
            };
            state.reclaim.live_pins(file.pins)?;
            if state
                .pypi_files
                .iter()
                .any(|f| f.repository == file.repository && f.filename == file.filename)
            {
                return Err(StoreError::Conflict);
            }
            let package = match state
                .packages
                .iter()
                .find(|p| p.repository_id == file.repository && p.name == file.project)
            {
                Some(p) => p.id,
                None => {
                    let id = state.id();
                    state.packages.push(Package {
                        id,
                        repository_id: file.repository,
                        name: file.project.to_string(),
                        description: file.summary.map(str::to_string),
                        readme: None,
                        license: None,
                        created_at: file.now,
                        updated_at: file.now,
                    });
                    id
                }
            };
            let existing = state
                .versions
                .iter()
                .find(|v| v.package_id == package && v.version == file.version)
                .map(|v| v.id);
            let (version, version_created) = match existing {
                Some(id) => (id, false),
                None => {
                    let id = state.id();
                    let mut row = blank_version(file.version, file.metadata_json);
                    row.id = id;
                    row.package_id = package;
                    row.checksum_sha256 = Some(file.sha256.to_string());
                    row.size = file.size;
                    row.tarball_path = artifact.physical_key.clone();
                    row.published_at = file.now;
                    state.versions.push(row);
                    (id, true)
                }
            };
            let row = PypiFile {
                id: state.id(),
                repository: file.repository,
                package_id: package,
                version_id: version,
                project: file.project.to_string(),
                version: file.version.to_string(),
                filename: file.filename.to_string(),
                packagetype: file.packagetype.to_string(),
                sha256: file.sha256.to_string(),
                size: file.size,
                key: artifact.physical_key.clone(),
                metadata_key: file.pins.get(1).map(|p| p.physical_key.clone()),
                metadata_sha256: file.metadata_sha256.map(str::to_string),
                requires_python: file.requires_python.map(str::to_string),
                yanked: false,
                yanked_reason: None,
                uploaded_at: file.now,
            };
            state.pypi_files.push(row.clone());
            state.reclaim.spend(file.pins);
            Ok(Published {
                version_created,
                file: row,
            })
        })
    }

    async fn file_by_name(
        &self,
        repository: i64,
        filename: &str,
    ) -> Result<Option<PypiFile>, StoreError> {
        self.with(|state| {
            Ok(state
                .pypi_files
                .iter()
                .find(|f| f.repository == repository && f.filename == filename)
                .map(|f| pypi_file(state, f)))
        })
    }

    async fn project_files(
        &self,
        repository: i64,
        project: &str,
    ) -> Result<Vec<PypiFile>, StoreError> {
        self.with(|state| {
            let mut files: Vec<PypiFile> = state
                .pypi_files
                .iter()
                .filter(|f| f.repository == repository)
                .map(|f| pypi_file(state, f))
                .filter(|f| f.project == project)
                .collect();
            files.sort_by_key(|f| f.id);
            Ok(files)
        })
    }

    async fn list_projects(&self, repository: i64) -> Result<Vec<String>, StoreError> {
        self.with(|state| {
            let mut names: Vec<String> = state
                .pypi_files
                .iter()
                .filter(|f| f.repository == repository)
                .map(|f| pypi_file(state, f).project)
                .collect();
            names.sort();
            names.dedup();
            Ok(names)
        })
    }

    async fn set_release_yanked(
        &self,
        repository: i64,
        project: &str,
        version: &str,
        reason: Option<&str>,
        yanked: bool,
        _now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.with(|state| {
            let Some(&id) = pypi_release_versions(state, repository, project, Some(version)).first()
            else {
                return Err(StoreError::NotFound);
            };
            if let Some(v) = state.versions.iter_mut().find(|v| v.id == id) {
                v.yanked = yanked;
            }
            for f in state.pypi_files.iter_mut().filter(|f| f.version_id == id) {
                f.yanked = yanked;
                f.yanked_reason = if yanked { reason.map(str::to_string) } else { None };
            }
            Ok(())
        })
    }

    async fn delete_release(
        &self,
        repository: i64,
        project: &str,
        version: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError> {
        self.with(|state| {
            let versions = pypi_release_versions(state, repository, project, Some(version));
            if versions.is_empty() {
                return Err(StoreError::NotFound);
            }
            Ok(pypi_purge(state, &versions, now))
        })
    }

    async fn delete_project_files(
        &self,
        repository: i64,
        project: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError> {
        self.with(|state| {
            let versions = pypi_release_versions(state, repository, project, None);
            if versions.is_empty() {
                return Err(StoreError::NotFound);
            }
            Ok(pypi_purge(state, &versions, now))
        })
    }
}

struct PinRow {
    token: String,
    physical_key: String,
    repo_prefix: String,
    until: DateTime<Utc>,
}

struct ClaimRow {
    key: String,
    token: String,
    until: DateTime<Utc>,
}

struct CandidateRow {
    key: String,
    prefix: bool,
    enqueued_at: DateTime<Utc>,
}

/// Port 22's tables: pins, claims, candidates, claimed generations, and the
/// incarnations with their prefixes and retired marks.
#[derive(Default)]
struct ReclaimState {
    incarnations: Vec<(i64, String)>,
    prefixes: Vec<(String, String)>,
    retired: Vec<String>,
    pins: Vec<PinRow>,
    claims: Vec<ClaimRow>,
    candidates: Vec<CandidateRow>,
    claimed: Vec<String>,
}

impl ReclaimState {
    fn enqueue(&mut self, key: &str, prefix: bool, now: DateTime<Utc>) {
        match self.candidates.iter_mut().find(|c| c.key == key) {
            Some(existing) => existing.prefix |= prefix,
            None => self.candidates.push(CandidateRow {
                key: key.to_string(),
                prefix,
                enqueued_at: now,
            }),
        }
    }

    /// The compare-and-set a committing method runs first: every token
    /// still names its pin, or the commit writes nothing.
    fn live_pins(&self, tokens: &[PinToken]) -> Result<(), StoreError> {
        let revoked: Vec<String> = tokens
            .iter()
            .filter(|t| {
                !self
                    .pins
                    .iter()
                    .any(|p| p.token == t.token && p.physical_key == t.physical_key)
            })
            .map(|t| t.physical_key.clone())
            .collect();
        if revoked.is_empty() {
            Ok(())
        } else {
            Err(StoreError::Superseded(revoked))
        }
    }

    fn spend(&mut self, tokens: &[PinToken]) {
        self.pins.retain(|p| !tokens.iter().any(|t| t.token == p.token));
    }

    fn revoke_under(&mut self, prefix: &str) {
        self.pins
            .retain(|p| p.repo_prefix != prefix && !layout::under(&p.physical_key, prefix));
    }

    fn retired_prefix(&self, prefix: &str) -> bool {
        self.prefixes
            .iter()
            .any(|(p, i)| p == prefix && self.retired.contains(i))
    }

    fn live_prefix(&self, prefix: &str) -> bool {
        self.prefixes
            .iter()
            .any(|(p, i)| p == prefix && !self.retired.contains(i))
    }
}

fn cutoff(grace: Duration, now: DateTime<Utc>) -> DateTime<Utc> {
    now - chrono::Duration::from_std(grace).unwrap_or(chrono::Duration::MAX)
}

/// What committed rows reference, the list the SQLite adapter's predicate
/// yields: a key, and whether it covers everything under it.
fn row_references(state: &State) -> Vec<(String, bool)> {
    let mut refs: Vec<(String, bool)> = Vec::new();
    refs.extend(state.versions.iter().map(|v| (v.tarball_path.clone(), false)));
    refs.extend(
        state
            .cache
            .iter()
            .filter_map(|e| e.storage_path.clone())
            .map(|k| (k, false)),
    );
    refs.extend(state.oci_blobs.iter().map(|b| (b.key.clone(), false)));
    refs.extend(state.oci_manifests.iter().map(|m| (m.key.clone(), false)));
    refs.extend(state.oci_uploads.iter().map(|u| (upload_prefix(u), true)));
    for f in &state.pypi_files {
        refs.push((f.key.clone(), false));
        refs.extend(f.metadata_key.clone().map(|k| (k, false)));
    }
    refs.extend(state.maven.keys().map(|k| (k.clone(), false)));
    refs
}

fn references(state: &State, key: &str, prefix: bool) -> bool {
    row_references(state).iter().any(|(k, covers)| {
        k == key || (prefix && layout::under(k, key)) || (*covers && layout::under(key, k))
    })
}

struct Reclaim(Arc<Mutex<State>>);

impl Reclaim {
    fn with<T>(
        &self,
        act: impl FnOnce(&mut State) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        with(&self.0, PortId::Reclaim, act)
    }
}

#[async_trait]
impl ReclaimStore for Reclaim {
    async fn pin(
        &self,
        repo_prefix: &str,
        logical_keys: &[String],
        until: DateTime<Utc>,
    ) -> Result<Pinned, StoreError> {
        self.with(|state| {
            if !state.reclaim.live_prefix(repo_prefix) {
                return Ok(Pinned::Retired);
            }
            let mut tokens = Vec::new();
            for logical in logical_keys {
                let stem = layout::physical_key(logical, "");
                let reusable = row_references(state).into_iter().find(|(k, covers)| {
                    !covers
                        && k.strip_prefix(&stem).is_some_and(|g| !g.contains('/'))
                        && !state.reclaim.claimed.contains(k)
                });
                let physical = match reusable {
                    Some((k, _)) => k,
                    None => {
                        layout::physical_key(logical, &uuid::Uuid::new_v4().simple().to_string())
                    }
                };
                let token = uuid::Uuid::new_v4().to_string();
                state.reclaim.pins.push(PinRow {
                    token: token.clone(),
                    physical_key: physical.clone(),
                    repo_prefix: repo_prefix.to_string(),
                    until,
                });
                tokens.push(PinToken {
                    token,
                    logical_key: logical.clone(),
                    physical_key: physical,
                });
            }
            Ok(Pinned::Tokens(tokens))
        })
    }

    async fn enqueue(
        &self,
        physical_keys: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.with(|state| {
            for key in physical_keys {
                state.reclaim.enqueue(key, false, now);
            }
            Ok(())
        })
    }

    async fn enqueue_prefix(&self, prefix: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.with(|state| {
            state.reclaim.enqueue(prefix, true, now);
            Ok(())
        })
    }

    async fn due(
        &self,
        grace: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Candidate>, StoreError> {
        self.with(|state| {
            let r = &state.reclaim;
            let mut due: Vec<&CandidateRow> = r
                .candidates
                .iter()
                .filter(|c| {
                    c.enqueued_at <= cutoff(grace, now) || (c.prefix && r.retired_prefix(&c.key))
                })
                .filter(|c| !r.claims.iter().any(|k| k.key == c.key && k.until > now))
                .collect();
            due.sort_by(|a, b| (a.enqueued_at, &a.key).cmp(&(b.enqueued_at, &b.key)));
            Ok(due
                .into_iter()
                .take(limit as usize)
                .map(|c| Candidate {
                    key: c.key.clone(),
                    prefix: c.prefix,
                })
                .collect())
        })
    }

    async fn claim(
        &self,
        key: &str,
        grace: Duration,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Claim, StoreError> {
        self.with(|state| {
            let Some(candidate) = state.reclaim.candidates.iter().find(|c| c.key == key) else {
                return Ok(Claim::NotDue);
            };
            let prefix = candidate.prefix;
            let retired = prefix && state.reclaim.retired_prefix(key);
            if prefix && !retired && state.reclaim.live_prefix(key) {
                state.reclaim.candidates.retain(|c| c.key != key);
                return Ok(Claim::Referenced);
            }
            if !retired && candidate.enqueued_at > cutoff(grace, now) {
                return Ok(Claim::NotDue);
            }
            if state
                .reclaim
                .claims
                .iter()
                .any(|c| c.key == key && c.until > now)
            {
                return Ok(Claim::NotDue);
            }
            if references(state, key, prefix) {
                state.reclaim.candidates.retain(|c| c.key != key);
                return Ok(Claim::Referenced);
            }
            let protected = state.reclaim.pins.iter().any(|p| {
                p.until > cutoff(grace, now)
                    && (p.physical_key == key || (prefix && layout::under(&p.physical_key, key)))
            });
            if protected && !retired {
                return Ok(Claim::Pinned);
            }
            if prefix {
                state.reclaim.revoke_under(key);
            } else {
                state.reclaim.pins.retain(|p| p.physical_key != key);
                if !state.reclaim.claimed.iter().any(|c| c == key) {
                    state.reclaim.claimed.push(key.to_string());
                }
            }
            let token = uuid::Uuid::new_v4().to_string();
            state.reclaim.claims.retain(|c| c.key != key);
            state.reclaim.claims.push(ClaimRow {
                key: key.to_string(),
                token: token.clone(),
                until,
            });
            Ok(Claim::Claimed(ClaimToken(token)))
        })
    }

    async fn renew(
        &self,
        token: &ClaimToken,
        _now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Renewal, StoreError> {
        self.with(|state| {
            match state.reclaim.claims.iter_mut().find(|c| c.token == token.0) {
                Some(claim) => {
                    claim.until = until;
                    Ok(Renewal::Renewed)
                }
                None => Ok(Renewal::Superseded),
            }
        })
    }

    async fn release(&self, token: &ClaimToken) -> Result<(), StoreError> {
        self.with(|state| {
            if let Some(at) = state.reclaim.claims.iter().position(|c| c.token == token.0) {
                let claim = state.reclaim.claims.remove(at);
                state.reclaim.candidates.retain(|c| c.key != claim.key);
            }
            Ok(())
        })
    }

    async fn forget_claimed(&self, physical_key: &str) -> Result<(), StoreError> {
        self.with(|state| {
            state.reclaim.claimed.retain(|k| k != physical_key);
            Ok(())
        })
    }

    async fn forget_retired(&self, prefix: &str) -> Result<(), StoreError> {
        self.with(|state| {
            let r = &mut state.reclaim;
            let Some(incarnation) = r
                .prefixes
                .iter()
                .find(|(p, i)| p == prefix && r.retired.contains(i))
                .map(|(_, i)| i.clone())
            else {
                return Ok(());
            };
            r.prefixes.retain(|(p, _)| p != prefix);
            if !r.prefixes.iter().any(|(_, i)| *i == incarnation) {
                r.retired.retain(|i| *i != incarnation);
            }
            Ok(())
        })
    }

    async fn backlog(&self) -> Result<Backlog, StoreError> {
        self.with(|state| {
            let candidates = &state.reclaim.candidates;
            Ok(Backlog {
                candidates: candidates.len() as u64,
                prefixes: candidates.iter().filter(|c| c.prefix).count() as u64,
            })
        })
    }

    async fn prune_pins(
        &self,
        grace: Duration,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<u64, StoreError> {
        self.with(|state| {
            let cut = cutoff(grace, now);
            let mut dead: Vec<(DateTime<Utc>, String)> = state
                .reclaim
                .pins
                .iter()
                .filter(|p| p.until <= cut)
                .map(|p| (p.until, p.token.clone()))
                .collect();
            dead.sort();
            dead.truncate(limit as usize);
            state
                .reclaim
                .pins
                .retain(|p| !dead.iter().any(|(_, t)| *t == p.token));
            Ok(dead.len() as u64)
        })
    }
}

impl ReferencedKeys for Reclaim {
    fn referenced(&self, grace: Duration, now: DateTime<Utc>) -> ReferencedStream {
        let state = self.0.lock().unwrap();
        let mut all: Vec<Referenced> = row_references(&state)
            .into_iter()
            .map(|(key, prefix)| Referenced { key, prefix })
            .collect();
        all.extend(
            state
                .reclaim
                .pins
                .iter()
                .filter(|p| p.until > cutoff(grace, now))
                .map(|p| Referenced {
                    key: p.physical_key.clone(),
                    prefix: false,
                }),
        );
        all.sort();
        Box::pin(futures_util::stream::iter(all.into_iter().map(Ok)))
    }
}

/// Port 18's tables: values, the units under them, their files and
/// declarations, the client documents and the counters.
#[derive(Default)]
struct MavenState {
    values: Vec<MavenValue>,
    units: Vec<MavenUnit>,
    client: Vec<ClientMetadata>,
    counters: Vec<(i64, String, Counter)>,
}

struct MavenValue {
    id: i64,
    repository: i64,
    ga: String,
    version: String,
    versioned: bool,
}

struct MavenUnit {
    value: i64,
    unit: Unit,
}

impl MavenState {
    fn value(&self, repository: i64, ga: &str, version: &str) -> Option<&MavenValue> {
        self.values
            .iter()
            .find(|v| v.repository == repository && v.ga == ga && v.version == version)
    }

    fn unit_mut(&mut self, key: &UnitKey<'_>) -> Option<&mut Unit> {
        let value = self.value(key.repository, key.ga, key.version)?.id;
        self.units
            .iter_mut()
            .find(|u| u.value == value && u.unit.build == key.build)
            .map(|u| &mut u.unit)
    }

    fn bump(&mut self, repository: i64, scopes: &[String], now: DateTime<Utc>) {
        for scope in scopes {
            match self
                .counters
                .iter_mut()
                .find(|(r, s, _)| *r == repository && s == scope)
            {
                Some((_, _, counter)) => {
                    counter.value += 1;
                    counter.updated_at = Some(now);
                }
                None => self.counters.push((
                    repository,
                    scope.clone(),
                    Counter {
                        value: 1,
                        updated_at: Some(now),
                    },
                )),
            }
        }
    }

    fn keys(&self) -> impl Iterator<Item = &String> {
        self.units
            .iter()
            .flat_map(|u| u.unit.files.iter().map(|f| &f.physical_key))
    }
}

struct Maven(Arc<Mutex<State>>);

impl Maven {
    fn with<T>(
        &self,
        act: impl FnOnce(&mut State) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        with(&self.0, PortId::Maven, act)
    }
}

fn change_unit(state: &mut State, change: &UnitChange<'_>) -> Result<Changed, StoreError> {
    state.reclaim.live_pins(change.pins)?;
    let key = change.key;
    let revision = match (change.revision, state.maven.unit_mut(&key)) {
        (None, Some(_)) => return Err(StoreError::Conflict),
        (Some(r), Some(unit)) if unit.revision != r => return Err(StoreError::Conflict),
        (Some(_), None) => return Err(StoreError::Conflict),
        (Some(r), Some(_)) => r + 1,
        (None, None) => {
            let value = match state.maven.value(key.repository, key.ga, key.version) {
                Some(v) => v.id,
                None => {
                    let id = state.id();
                    state.maven.values.push(MavenValue {
                        id,
                        repository: key.repository,
                        ga: key.ga.to_string(),
                        version: key.version.to_string(),
                        versioned: false,
                    });
                    id
                }
            };
            state.maven.units.push(MavenUnit {
                value,
                unit: Unit {
                    version: key.version.to_string(),
                    build: key.build.to_string(),
                    revision: 0,
                    depositor: change.depositor.to_string(),
                    contested: false,
                    refused: false,
                    visible_at: None,
                    created_at: change.now,
                    files: Vec::new(),
                    declarations: Vec::new(),
                },
            });
            1
        }
    };
    state.reclaim.spend(change.pins);
    let unit = state.maven.unit_mut(&key).expect("the unit exists");
    unit.revision = revision;
    let mut released = Vec::new();
    if let Some(file) = &change.file {
        if let Some(old) = unit.files.iter().position(|f| f.filename == file.filename) {
            let old = unit.files.remove(old);
            unit.declarations.retain(|d| d.filename != file.filename);
            if old.physical_key != file.physical_key {
                released.push(old.physical_key);
            }
        }
        unit.files.push(StoredFile {
            filename: file.filename.to_string(),
            physical_key: file.physical_key.to_string(),
            size: file.size,
            digests: file.digests.clone(),
            depositor: file.depositor.to_string(),
            created_at: change.now,
        });
        unit.files.sort_by(|a, b| a.filename.cmp(&b.filename));
    }
    for d in change.declarations {
        unit.declarations
            .retain(|x| !(x.filename == d.filename && x.algorithm == d.algorithm));
        unit.declarations.push(d.clone());
    }
    unit.declarations
        .sort_by(|a, b| (&a.filename, a.algorithm.as_str()).cmp(&(&b.filename, b.algorithm.as_str())));
    unit.contested |= change.contest;
    if change.reveal && unit.visible_at.is_none() {
        unit.visible_at = Some(change.now);
    }
    for key in &released {
        state.reclaim.enqueue(key, false, change.now);
    }
    state.maven.bump(key.repository, change.scopes, change.now);
    Ok(Changed { revision, released })
}

#[async_trait]
impl MavenFileStore for Maven {
    async fn unit(&self, key: &UnitKey<'_>) -> Result<Option<Unit>, StoreError> {
        self.with(|state| Ok(state.maven.unit_mut(key).map(|u| u.clone())))
    }

    async fn change(&self, change: &UnitChange<'_>) -> Result<Changed, StoreError> {
        self.with(|state| change_unit(state, change))
    }

    async fn refuse(
        &self,
        key: &UnitKey<'_>,
        revision: i64,
        scopes: &[String],
        now: DateTime<Utc>,
    ) -> Result<Changed, StoreError> {
        self.with(|state| {
            let unit = state.maven.unit_mut(key).ok_or(StoreError::NotFound)?;
            if unit.revision != revision {
                return Err(StoreError::Conflict);
            }
            unit.revision += 1;
            unit.refused = true;
            unit.declarations.clear();
            let released: Vec<String> =
                unit.files.drain(..).map(|f| f.physical_key).collect();
            let revision = unit.revision;
            for k in &released {
                state.reclaim.enqueue(k, false, now);
            }
            state.maven.bump(key.repository, scopes, now);
            Ok(Changed { revision, released })
        })
    }

    async fn artifact(&self, repository: i64, ga: &str) -> Result<Vec<UnitView>, StoreError> {
        self.with(|state| {
            let values: Vec<i64> = state
                .maven
                .values
                .iter()
                .filter(|v| v.repository == repository && v.ga == ga)
                .map(|v| v.id)
                .collect();
            Ok(state
                .maven
                .units
                .iter()
                .filter(|u| values.contains(&u.value))
                .map(|u| UnitView {
                    version: u.unit.version.clone(),
                    build: u.unit.build.clone(),
                    visible_at: u.unit.visible_at,
                    refused: u.unit.refused,
                    files: u.unit.files.clone(),
                })
                .collect())
        })
    }

    async fn counter(&self, repository: i64, scope: &str) -> Result<Counter, StoreError> {
        self.with(|state| {
            Ok(state
                .maven
                .counters
                .iter()
                .find(|(r, s, _)| *r == repository && s == scope)
                .map(|(_, _, c)| *c)
                .unwrap_or_default())
        })
    }

    async fn record_client_metadata(
        &self,
        metadata: &ClientMetadata,
        scopes: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.with(|state| {
            state
                .maven
                .client
                .retain(|m| !(m.repository == metadata.repository && m.dir == metadata.dir));
            state.maven.client.push(metadata.clone());
            state.maven.bump(metadata.repository, scopes, now);
            Ok(())
        })
    }

    async fn client_metadata(
        &self,
        repository: i64,
        dir: &str,
    ) -> Result<Option<ClientMetadata>, StoreError> {
        self.with(|state| {
            Ok(state
                .maven
                .client
                .iter()
                .find(|m| m.repository == repository && m.dir == dir)
                .cloned())
        })
    }

    async fn pending(
        &self,
        before: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<PendingUnit>, StoreError> {
        self.with(|state| {
            let mut pending: Vec<PendingUnit> = state
                .maven
                .units
                .iter()
                .filter(|u| {
                    let unit = &u.unit;
                    unit.visible_at.is_none()
                        && !unit.refused
                        && !unit.contested
                        && unit.created_at < before
                        && !unit.files.is_empty()
                        && unit.declarations.iter().all(|d| unit.file(&d.filename).is_some())
                })
                .filter_map(|u| {
                    let value = state.maven.values.iter().find(|v| v.id == u.value)?;
                    Some(PendingUnit {
                        repository: value.repository,
                        ga: value.ga.clone(),
                        version: value.version.clone(),
                        build: u.unit.build.clone(),
                        created_at: u.unit.created_at,
                    })
                })
                .collect();
            pending.sort_by_key(|p| p.created_at);
            pending.truncate(limit as usize);
            Ok(pending)
        })
    }

    async fn unversioned(
        &self,
        after: Option<&Unversioned>,
        limit: u32,
    ) -> Result<Vec<Unversioned>, StoreError> {
        let key = |r: i64, ga: &str, v: &str| (r, ga.to_string(), v.to_string());
        let after = after.map(|a| key(a.repository, &a.ga, &a.version));
        self.with(|state| {
            let mut values: Vec<_> = state
                .maven
                .values
                .iter()
                .filter(|v| !v.versioned)
                .filter(|v| after.as_ref().is_none_or(|a| key(v.repository, &v.ga, &v.version) > *a))
                .collect();
            values.sort_by_key(|v| key(v.repository, &v.ga, &v.version));
            Ok(values
                .into_iter()
                .filter(|v| {
                    state
                        .maven
                        .units
                        .iter()
                        .any(|u| u.value == v.id && u.unit.visible())
                })
                .take(limit as usize)
                .map(|v| Unversioned {
                    repository: v.repository,
                    ga: v.ga.clone(),
                    version: v.version.clone(),
                })
                .collect())
        })
    }

    async fn mark_versioned(
        &self,
        repository: i64,
        ga: &str,
        version: &str,
    ) -> Result<(), StoreError> {
        self.with(|state| {
            if let Some(v) = state
                .maven
                .values
                .iter_mut()
                .find(|v| v.repository == repository && v.ga == ga && v.version == version)
            {
                v.versioned = true;
            }
            Ok(())
        })
    }
}

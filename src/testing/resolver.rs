//! A resolver context over fakes: map literals where a whole `AppState`, a
//! temp directory and a migrated SQLite file used to be.
//!
//! Every field of [`Cx`] is a port or a borrowed value, so the only thing
//! still built for real here is the proxy engine — and since step 5 it holds
//! ports too, which is why its storage root is a path nothing ever touches.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::auth::middleware::AuthUser;
use crate::domain::UrlRepo;
use crate::policy::{Pending, ResolutionRecorder};
use crate::proxy::{ProxyEngine, Timeouts, TtlConfig, UpstreamCreds};
use crate::registry::resolve::Cx;
use crate::storage::{
    CheckReport, ObjectList, ObjectMeta, ObjectWriter, ReadStream, StorageBackend, StorageError,
    StoreIdentity, UploadPlan,
};
use crate::testing::storage::MemStorage;
use crate::testing::fakes::FakeDb;

/// Keeps what it was handed, and watches only the members it was told about.
#[derive(Default)]
pub struct Recorder {
    watched: Vec<String>,
    recorded: Mutex<Vec<String>>,
}

impl Recorder {
    pub fn watching(members: &[&str]) -> Self {
        Self {
            watched: members.iter().map(|m| m.to_string()).collect(),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// The members whose resolutions reached the report, in order.
    pub fn recorded(&self) -> Vec<String> {
        self.recorded.lock().unwrap().clone()
    }
}

impl ResolutionRecorder for Recorder {
    fn records(&self, member: &str) -> bool {
        self.watched.iter().any(|watched| watched == member)
    }

    fn record(&self, pending: Pending) {
        self.recorded.lock().unwrap().push(pending.member.name);
    }
}

/// Everything a `Cx` borrows, owned for the length of a test.
pub struct Resolver {
    /// Not `db`: what is behind these handles is a map, and the persistence
    /// ratchet counts the spelling.
    pub fakes: FakeDb,
    pub creds: HashMap<String, UpstreamCreds>,
    pub policy: Recorder,
    pub base_url: String,
    repos: Arc<dyn crate::ports::repositories::RepositoryStore>,
    perms: Arc<dyn crate::ports::permissions::PermissionStore>,
    packages: Arc<dyn crate::ports::packages::PackageStore>,
    oci: Arc<dyn crate::ports::oci::OciStore>,
    maven: Arc<dyn crate::ports::maven::MavenFileStore>,
    search: Arc<dyn crate::ports::search::SearchIndex>,
    nuget: Arc<dyn crate::ports::nuget::NugetFeedRead>,
    proxy: ProxyEngine,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new(FakeDb::new(), Recorder::default())
    }
}

impl Resolver {
    pub fn new(db: FakeDb, policy: Recorder) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let proxy = ProxyEngine::new(
            Arc::new(NoStorage),
            db.proxy_cache(),
            db.repositories(),
            db.reclaim(),
            Timeouts::from_connect_secs(1),
            TtlConfig {
                default_secs: 60,
                negative_secs: 60,
            },
        );
        Self {
            creds: HashMap::new(),
            policy,
            base_url: "http://localhost:8080".to_string(),
            repos: db.repositories(),
            perms: db.perms(),
            packages: db.packages(),
            oci: db.oci(),
            maven: db.maven(),
            search: db.search(),
            nuget: db.nuget_feed(),
            proxy,
            fakes: db,
        }
    }

    pub fn cx<'a>(&'a self, auth: Option<&'a AuthUser>, url: &'a str) -> Cx<'a> {
        Cx {
            repos: self.repos.as_ref(),
            perms: self.perms.as_ref(),
            packages: self.packages.as_ref(),
            oci: self.oci.as_ref(),
            maven: self.maven.as_ref(),
            search: self.search.as_ref(),
            nuget: self.nuget.as_ref(),
            proxy: &self.proxy,
            policy: &self.policy,
            creds: &self.creds,
            auth,
            anonymous_read: true,
            url: UrlRepo(url),
            base_url: &self.base_url,
        }
    }
}

/// Storage that refuses everything, because a walk over fakes reaches none of
/// it: an accidental read fails loudly instead of creating a directory.
struct NoStorage;

impl NoStorage {
    fn refuse<T>() -> Result<T, StorageError> {
        Err(StorageError::Other("the resolver fixture has no storage".into()))
    }
}

#[async_trait]
impl StorageBackend for NoStorage {
    async fn get(&self, _key: &str) -> Result<Bytes, StorageError> {
        Self::refuse()
    }

    async fn writer(&self, _key: &str) -> Result<Box<dyn ObjectWriter>, StorageError> {
        Self::refuse()
    }

    async fn read_stream(&self, _key: &str) -> Result<ReadStream, StorageError> {
        Self::refuse()
    }

    async fn copy_object(&self, _from: &str, _to: &str) -> Result<(), StorageError> {
        Self::refuse()
    }

    async fn relocate(&self, _from: &str, _to: &str) -> Result<(), StorageError> {
        Self::refuse()
    }

    async fn head(&self, _key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        Self::refuse()
    }

    async fn stat(&self, _key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        Self::refuse()
    }

    async fn delete(&self, _key: &str) -> Result<(), StorageError> {
        Self::refuse()
    }

    async fn delete_batch(&self, _keys: &[String]) -> Result<(), StorageError> {
        Self::refuse()
    }

    fn list(&self, _prefix: &str) -> ObjectList {
        Box::pin(futures_util::stream::once(async { Self::refuse() }))
    }

    async fn sweep_abandoned(
        &self,
        _older_than: Duration,
        _now: DateTime<Utc>,
    ) -> Result<u64, StorageError> {
        Self::refuse()
    }

    async fn probe(&self) -> Result<(), StorageError> {
        Self::refuse()
    }

    fn upload_plan(&self) -> UploadPlan {
        MemStorage::new().upload_plan()
    }

    async fn self_check(&self) -> CheckReport {
        CheckReport::default()
    }

    fn identity(&self) -> StoreIdentity {
        StoreIdentity("none".to_string())
    }
}

/// A caller the permission ladder has to look a grant up for.
pub fn user(id: i64, role: &str) -> AuthUser {
    AuthUser {
        token: "t".to_string(),
        user_id: Some(id),
        username: format!("u{id}"),
        role: role.to_string(),
        must_change_password: false,
        token_name: None,
        api_token_id: None,
        scope: crate::domain::TokenScope::Inherit,
    }
}

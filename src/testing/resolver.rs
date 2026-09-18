//! A resolver context over fakes: map literals where a whole `AppState`, a
//! temp directory and a migrated SQLite file used to be.
//!
//! Every field of [`Cx`] is a port or a borrowed value, so the only thing
//! still built for real here is the proxy engine — and since step 5 it holds
//! ports too, which is why its storage root is a path nothing ever touches.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::AsyncRead;

use crate::auth::middleware::AuthUser;
use crate::domain::UrlRepo;
use crate::policy::{Pending, ResolutionRecorder};
use crate::proxy::{ProxyEngine, Timeouts, TtlConfig, UpstreamCreds};
use crate::registry::resolve::Cx;
use crate::storage::{StorageBackend, StorageError};
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
    search: Arc<dyn crate::ports::search::SearchIndex>,
    proxy: ProxyEngine,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new(FakeDb::new(), Recorder::default())
    }
}

impl Resolver {
    pub fn new(db: FakeDb, policy: Recorder) -> Self {
        let proxy = ProxyEngine::new(
            Arc::new(NoStorage),
            db.proxy_cache(),
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
            search: db.search(),
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
            search: self.search.as_ref(),
            proxy: &self.proxy,
            policy: &self.policy,
            creds: &self.creds,
            auth,
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
    async fn get(&self, _path: &str) -> Result<Bytes, StorageError> {
        Self::refuse()
    }

    async fn put(&self, _path: &str, _data: Bytes) -> Result<(), StorageError> {
        Self::refuse()
    }

    async fn append(&self, _path: &str, _data: Bytes) -> Result<u64, StorageError> {
        Self::refuse()
    }

    async fn delete(&self, _path: &str) -> Result<(), StorageError> {
        Self::refuse()
    }

    async fn delete_prefix(&self, _prefix: &str) -> Result<(), StorageError> {
        Self::refuse()
    }

    async fn exists(&self, _path: &str) -> Result<bool, StorageError> {
        Self::refuse()
    }

    async fn rename(&self, _from: &str, _to: &str) -> Result<(), StorageError> {
        Self::refuse()
    }

    async fn read_stream(
        &self,
        _path: &str,
    ) -> Result<(u64, Pin<Box<dyn AsyncRead + Send>>), StorageError> {
        Self::refuse()
    }

    async fn remove_stale_parts(
        &self,
        _prefix: &str,
        _older_than: Duration,
    ) -> Result<u64, StorageError> {
        Self::refuse()
    }

    fn resolve(&self, _path: &str) -> Result<PathBuf, StorageError> {
        Self::refuse()
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
    }
}

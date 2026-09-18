//! What one member of a walk contributes: its page of a project, one of its
//! files, or its project names. Each leaf carries port 15; `Cx` names no
//! format.

use crate::domain::{CacheRepo, Outcome};
use crate::ports::pypi::{PypiFile, PypiFileStore};
use crate::proxy::Payload;
use crate::registry::resolve::{Cx, Leaf, ResolveError, Upstream};

use super::simple::{Page, PageFile, Yanked};
use super::version;

/// The URL a page lists a file under: our own files route, relative to
/// `/{repo}/simple/{project}/`.
pub fn file_url(project: &str, filename: &str) -> String {
    format!("../../files/{project}/{filename}")
}

fn page_file(f: &PypiFile) -> PageFile {
    PageFile {
        filename: f.filename.clone(),
        url: file_url(&f.project, &f.filename),
        sha256: Some(f.sha256.clone()),
        requires_python: f.requires_python.clone(),
        yanked: if f.yanked {
            Yanked::Yes(f.yanked_reason.clone())
        } else {
            Yanked::No
        },
        core_metadata: f.metadata_key.as_ref().map(|_| f.metadata_sha256.clone()),
        size: Some(f.size.max(0) as u64),
        upload_time: Some(f.uploaded_at.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()),
    }
}

pub fn hosted_page(project: &str, files: &[PypiFile]) -> Page {
    let mut versions: Vec<String> = files.iter().map(|f| f.version.clone()).collect();
    versions.sort_by(|a, b| version::compare(a, b));
    versions.dedup();
    Page {
        name: project.to_string(),
        files: files.iter().map(page_file).collect(),
        versions,
    }
}

/// One member's page of a project; `Found` only when it lists something.
pub struct PageLeaf<'a> {
    pub files: &'a dyn PypiFileStore,
    pub project: String,
}

#[async_trait::async_trait]
impl Leaf for PageLeaf<'_> {
    type Out = Page;

    async fn hosted(&self, _cx: &Cx<'_>, member: CacheRepo<'_>) -> Result<Outcome<Page>, ResolveError> {
        let files = self.files.project_files(member.0.id, &self.project).await?;
        if files.is_empty() {
            return Ok(Outcome::NotFound);
        }
        Ok(Outcome::Found(hosted_page(&self.project, &files)))
    }

    async fn proxy(
        &self,
        _cx: &Cx<'_>,
        _member: CacheRepo<'_>,
        _up: &Upstream,
    ) -> Result<Outcome<Page>, ResolveError> {
        Ok(Outcome::NotFound)
    }
}

/// What the member that lists a filename answers for it (D9): once a page
/// names the file, every outcome is that member's, a miss included.
#[derive(Debug)]
pub enum Served {
    Payload(Payload),
    Absent,
}

pub struct FileLeaf<'a> {
    pub files: &'a dyn PypiFileStore,
    pub project: String,
    pub filename: String,
    /// The PEP 658 document rather than the artifact.
    pub metadata: bool,
}

#[async_trait::async_trait]
impl Leaf for FileLeaf<'_> {
    type Out = Served;

    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> Result<Outcome<Served>, ResolveError> {
        let Some(file) = self.files.file_by_name(member.0.id, &self.filename).await? else {
            return Ok(Outcome::NotFound);
        };
        if file.project != self.project {
            return Ok(Outcome::NotFound);
        }
        if self.metadata {
            return Ok(Outcome::Found(match file.metadata_key {
                Some(key) => Served::Payload(Payload::file(key, 0)),
                None => Served::Absent,
            }));
        }
        let _ = cx.packages.record_download(file.version_id).await;
        crate::telemetry::record_download(&member.0.name, &file.project);
        let mut payload = Payload::file(file.key, file.size.max(0) as u64);
        payload.digest = Some(file.sha256);
        Ok(Outcome::Found(Served::Payload(payload)))
    }

    async fn proxy(
        &self,
        _cx: &Cx<'_>,
        _member: CacheRepo<'_>,
        _up: &Upstream,
    ) -> Result<Outcome<Served>, ResolveError> {
        Ok(Outcome::NotFound)
    }
}

/// A member's project names: hosted members only, an upstream index is
/// never enumerated.
pub struct IndexLeaf<'a> {
    pub files: &'a dyn PypiFileStore,
}

#[async_trait::async_trait]
impl Leaf for IndexLeaf<'_> {
    type Out = Vec<String>;

    async fn hosted(&self, _cx: &Cx<'_>, member: CacheRepo<'_>) -> Result<Outcome<Vec<String>>, ResolveError> {
        Ok(Outcome::Found(self.files.list_projects(member.0.id).await?))
    }

    async fn proxy(
        &self,
        _cx: &Cx<'_>,
        _member: CacheRepo<'_>,
        _up: &Upstream,
    ) -> Result<Outcome<Vec<String>>, ResolveError> {
        Ok(Outcome::NotFound)
    }
}

/// The merged page of a walk: members in order, the first to list a
/// filename keeps it.
pub fn merge(project: &str, pages: Vec<Page>) -> Page {
    let mut merged = Page {
        name: project.to_string(),
        ..Page::default()
    };
    for page in pages {
        for f in page.files {
            if !merged.files.iter().any(|m| m.filename == f.filename) {
                merged.files.push(f);
            }
        }
        merged.versions.extend(page.versions);
    }
    merged.versions.sort_by(|a, b| version::compare(a, b));
    merged.versions.dedup_by(|a, b| version::compare(a, b).is_eq());
    merged
}

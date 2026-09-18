//! What one member of a walk contributes: its page of a project, one of its
//! files, or its project names. Each leaf carries port 15; `Cx` names no
//! format.

use std::sync::Arc;

use crate::domain::{CacheRepo, Format, Outcome};
use crate::policy::{self, Source};
use crate::ports::pypi::{PypiFile, PypiFileStore};
use crate::proxy::Payload;
use crate::registry::resolve::{Cx, Leaf, ResolveError, Upstream};

use super::memo::{PageKey, PageMemo};
use super::names::parse_filename;
use super::parse::{parse_html, parse_json, UpstreamFile, UpstreamPage};
use super::simple::{Page, PageFile, Yanked};
use super::upstream::{allowed_host, file_hosts, is_json, page_url, same_endpoint, PypiArtifact, PypiStrategy};
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

/// A member's upstream page, parsed once per cache row it came from.
pub async fn fetch_page(
    cx: &Cx<'_>,
    memo: &PageMemo,
    member: CacheRepo<'_>,
    up: &Upstream,
    project: &str,
) -> Result<Outcome<Arc<UpstreamPage>>, ResolveError> {
    let artifact = PypiArtifact::Page {
        project: project.to_string(),
    };
    let cached = match cx.proxy.fetch(&PypiStrategy, up, member, &artifact).await? {
        Outcome::Found(cached) => cached,
        Outcome::NotFound => return Ok(Outcome::NotFound),
    };
    let key = PageKey {
        row: cached.entry.id,
        fetched_at: cached.entry.fetched_at,
        digest: cached.entry.digest.clone(),
    };
    if !cached.stale {
        if let Some(page) = memo.get(&key) {
            return Ok(Outcome::Found(page));
        }
    }
    let body = cx.proxy.bytes(&cached).await?;
    let url = page_url(up, project)?;
    let page = if is_json(cached.entry.content_type.as_deref()) {
        parse_json(&body, &url)
            .ok_or_else(|| ResolveError::Upstream(format!("unreadable JSON page for {project}")))?
    } else {
        parse_html(&String::from_utf8_lossy(&body), &url)
    };
    let page = Arc::new(page);
    if !cached.stale {
        memo.insert(key, page.clone());
    }
    Ok(Outcome::Found(page))
}

/// The files of an upstream page this server will serve: named for the
/// project, on an allowed host.
pub fn servable<'p>(
    page: &'p UpstreamPage,
    project: &'p str,
    hosts: &'p [String],
) -> impl Iterator<Item = &'p UpstreamFile> + 'p {
    page.files.iter().filter(move |f| {
        parse_filename(&f.filename).is_ok_and(|p| p.project == project) && allowed_host(&f.url, hosts)
    })
}

fn proxied_page(project: &str, page: &UpstreamPage, hosts: &[String]) -> Page {
    let files: Vec<PageFile> = servable(page, project, hosts)
        .map(|f| PageFile {
            filename: f.filename.clone(),
            url: file_url(project, &f.filename),
            sha256: f.sha256.clone(),
            requires_python: f.requires_python.clone(),
            yanked: f.yanked.clone(),
            core_metadata: f.core_metadata.clone(),
            size: None,
            upload_time: f.upload_time.clone(),
        })
        .collect();
    let mut versions: Vec<String> = files
        .iter()
        .filter_map(|f| parse_filename(&f.filename).ok())
        .map(|p| p.version.normalized())
        .collect();
    versions.sort_by(|a, b| version::compare(a, b));
    versions.dedup_by(|a, b| version::compare(a, b).is_eq());
    Page {
        name: project.to_string(),
        files,
        versions,
    }
}

fn hosts_of(cx: &Cx<'_>, member: CacheRepo<'_>, up: &Upstream) -> Vec<String> {
    file_hosts(up, &cx.creds.for_repo(&member.0.name).file_hosts)
}

/// One member's page of a project; `Found` only when it lists something.
pub struct PageLeaf<'a> {
    pub files: &'a dyn PypiFileStore,
    pub memo: &'a PageMemo,
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
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Page>, ResolveError> {
        let page = match fetch_page(cx, self.memo, member, up, &self.project).await? {
            Outcome::Found(page) => page,
            Outcome::NotFound => return Ok(Outcome::NotFound),
        };
        let page = proxied_page(&self.project, &page, &hosts_of(cx, member, up));
        Ok(if page.files.is_empty() {
            Outcome::NotFound
        } else {
            Outcome::Found(page)
        })
    }
}

/// What the member that lists a filename answers for it (D9): once a page
/// names the file, every outcome is that member's, a miss included.
#[derive(Debug)]
pub enum Served {
    Payload(Payload),
    Absent,
    /// The owning member's upstream failed; the walk does not go on.
    Unavailable(String),
}

pub struct FileLeaf<'a> {
    pub files: &'a dyn PypiFileStore,
    pub memo: &'a PageMemo,
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
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Served>, ResolveError> {
        let page = match fetch_page(cx, self.memo, member, up, &self.project).await? {
            Outcome::Found(page) => page,
            Outcome::NotFound => return Ok(Outcome::NotFound),
        };
        let hosts = hosts_of(cx, member, up);
        let Some(listed) = servable(&page, &self.project, &hosts).find(|f| f.filename == self.filename) else {
            return Ok(Outcome::NotFound);
        };
        let recorded = (!self.metadata).then(|| (listed.sha256.clone(), listed.upload_time.clone()));
        let (filename, url, sha256) = if self.metadata {
            let Some(sha256) = listed.core_metadata.clone() else {
                return Ok(Outcome::Found(Served::Absent));
            };
            let mut url = listed.url.clone();
            url.set_path(&format!("{}.metadata", url.path()));
            (format!("{}.metadata", listed.filename), url, sha256)
        } else {
            (listed.filename.clone(), listed.url.clone(), listed.sha256.clone())
        };
        let artifact = PypiArtifact::File {
            project: self.project.clone(),
            filename,
            on_index: same_endpoint(&url, &up.base),
            url,
            sha256,
            allow_private: up.dl_allow_private,
        };
        let served = match cx.proxy.fetch(&PypiStrategy, up, member, &artifact).await {
            Ok(Outcome::Found(cached)) => {
                if let Some((digest, uploaded)) = recorded {
                    let version = parse_filename(&self.filename).ok().map(|f| f.version.normalized());
                    policy::record(cx, member, up, Format::Pypi, &self.project, version, || Source::Pypi {
                        digest,
                        uploaded,
                    });
                }
                Served::Payload(cached.into_payload())
            }
            Ok(Outcome::NotFound) => Served::Absent,
            Err(err) => match ResolveError::from(err) {
                ResolveError::Upstream(why) => Served::Unavailable(why),
                ResolveError::NotFound(_) => Served::Absent,
                other => return Err(other),
            },
        };
        Ok(Outcome::Found(served))
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

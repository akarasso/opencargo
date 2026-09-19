use std::future::Future;
use std::pin::Pin;

use tracing::warn;

use crate::app::authorize::{Authorize, Verdict};
use crate::auth::middleware::AuthUser;
use crate::domain::{
    Action, CacheRepo, DomainError, Miss, Resource, Outcome, RepoKind, Repository, UrlRepo, Visit, Walk,
    MAX_GROUP_DEPTH,
};
use crate::error::{AppError, StoreError};
use crate::policy::ResolutionRecorder;
use crate::ports::maven::MavenFileStore;
use crate::ports::oci::OciStore;
use crate::ports::packages::PackageStore;
use crate::ports::permissions::PermissionStore;
use crate::ports::repositories::RepositoryStore;
use crate::ports::search::SearchIndex;
use crate::proxy::auth::{default_token_realms, UpstreamAuth, UpstreamCredsSource};
use crate::proxy::ProxyEngine;
use crate::storage::StorageError;

/// How resolving a name refuses, in the resolver's own vocabulary.
///
/// `Store` and `Internal` are not padding. A group walk reads repositories
/// and grants on its way down, and both refusals it can meet there carry a
/// status of their own — a store that is merely busy is retryable, a group
/// nested deeper than a write would ever allow is this server's fault — so
/// folding either into `Upstream` or `Domain` would answer the client wrong.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("{0}")]
    NotFound(String),

    /// A member could not be reached, or answered something unusable.
    #[error("{0}")]
    Upstream(String),

    #[error(transparent)]
    Domain(#[from] DomainError),

    #[error(transparent)]
    Store(#[from] StoreError),

    /// Our own storage failed: a 503, never a group miss.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// A configuration fault rather than a registry one: a row write-time
    /// validation should have refused, or an upstream URL that is not one.
    #[error("{0}")]
    Internal(String),
}

impl From<ResolveError> for AppError {
    fn from(err: ResolveError) -> Self {
        match err {
            ResolveError::NotFound(why) => AppError::NotFound(why),
            ResolveError::Upstream(why) => AppError::BadGateway(why),
            ResolveError::Domain(err) => AppError::from(err),
            ResolveError::Store(err) => AppError::from(err),
            ResolveError::Storage(err) => AppError::from(err),
            ResolveError::Internal(why) => AppError::Internal(why),
        }
    }
}

/// The proxy engine and the per-format packument builders still answer in
/// `AppError`; this is the one place their vocabulary crosses into the
/// resolver's, and every status the resolver serves survives it. A 4xx does
/// not: the engine's read path constructs only 404, 502 and 500, so a client
/// error arriving from it is the engine misreporting itself — a 500, like the
/// driver failures it wraps, which already reach the client as one.
impl From<AppError> for ResolveError {
    fn from(err: AppError) -> Self {
        match err {
            AppError::NotFound(why) => ResolveError::NotFound(why),
            AppError::BadGateway(why) => ResolveError::Upstream(why),
            AppError::Internal(why) => ResolveError::Internal(why),
            AppError::ServiceUnavailable(_) => ResolveError::Store(StoreError::Unavailable),
            other => ResolveError::Store(StoreError::Other(Box::new(other))),
        }
    }
}

/// One request's way to everything a leaf may reach: ports, the proxy engine
/// and who is asking. Built once by the HTTP adapter, where the composition
/// root's state lives; nothing below it knows a pool exists.
impl<'a> Cx<'a> {
    /// The authority the walk asks, built from the two stores it already
    /// holds: a group's entry was judged before the walk began.
    pub fn authorize(&self) -> Authorize<'_> {
        Authorize {
            perms: self.perms,
            repos: self.repos,
            anonymous_read: self.anonymous_read,
        }
    }
}

pub struct Cx<'a> {
    pub repos: &'a dyn RepositoryStore,
    pub perms: &'a dyn PermissionStore,
    pub packages: &'a dyn PackageStore,
    pub oci: &'a dyn OciStore,
    pub maven: &'a dyn MavenFileStore,
    pub search: &'a dyn SearchIndex,
    pub nuget: &'a dyn crate::ports::nuget::NugetFeedRead,
    pub proxy: &'a ProxyEngine,
    pub policy: &'a dyn ResolutionRecorder,
    pub creds: &'a dyn UpstreamCredsSource,
    pub auth: Option<&'a AuthUser>,
    /// The deployment's answer for a public repository, which the walk needs
    /// to judge a member the same way the entry was judged.
    pub anonymous_read: bool,
    pub url: UrlRepo<'a>,
    pub base_url: &'a str,
}

#[derive(Clone, Debug)]
pub struct Upstream {
    pub base: url::Url,
    pub auth: Option<UpstreamAuth>,
    pub token_realms: Vec<url::Url>,
    pub dl_allow_private: bool,
}

impl Upstream {
    /// The credentials are looked up rather than handed down: the member is
    /// discovered mid-walk, so the handler never saw its name. A member's
    /// broken upstream configuration is that member's failure (`Upstream`),
    /// so a group falls through to its next member.
    pub fn for_member(
        creds: &dyn UpstreamCredsSource,
        member: &Repository,
    ) -> Result<Self, ResolveError> {
        let raw = member.upstream_url.as_deref().ok_or_else(|| {
            ResolveError::Upstream(format!(
                "proxy repository {} has no upstream_url configured",
                member.name
            ))
        })?;
        let base = url::Url::parse(raw).map_err(|e| {
            ResolveError::Upstream(format!(
                "proxy repository {} has an invalid upstream_url: {e}",
                member.name
            ))
        })?;
        let creds = creds.for_repo(&member.name);
        let token_realms = if creds.token_realms.is_empty() {
            default_token_realms(&base)
        } else {
            creds.token_realms
        };
        Ok(Self {
            base,
            auth: creds.auth,
            token_realms,
            dl_allow_private: creds.dl_allow_private,
        })
    }
}

#[async_trait::async_trait]
pub trait Leaf: Send + Sync {
    type Out: Send;
    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Self::Out>, ResolveError>;
    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Self::Out>, ResolveError>;
}

pub struct Collected<T> {
    pub hits: Vec<T>,
    pub degraded: Option<String>,
}

/// First `Found` in member order; none is `NotFound`, or `BadGateway` when a
/// member failed on the way.
pub async fn first_hit<L: Leaf>(
    cx: &Cx<'_>,
    repo: &Repository,
    leaf: &L,
) -> Result<L::Out, ResolveError> {
    let mut w = Walk::new(repo, true);
    walk(cx, repo, leaf, 0, &mut w).await?;
    let miss = w.miss();
    let (hits, _) = w.finish();
    hits.into_iter().next().ok_or_else(|| missed(cx.url, miss))
}

/// Every `Found` in member order; zero hits after a failure is `BadGateway`,
/// hits after a failure are `degraded`.
pub async fn collect<L: Leaf>(
    cx: &Cx<'_>,
    repo: &Repository,
    leaf: &L,
) -> Result<Collected<L::Out>, ResolveError> {
    let mut w = Walk::new(repo, false);
    walk(cx, repo, leaf, 0, &mut w).await?;
    if !w.found() {
        if let degraded @ Miss::Degraded(_) = w.miss() {
            return Err(missed(cx.url, degraded));
        }
    }
    let (hits, degraded) = w.finish();
    Ok(Collected { hits, degraded })
}

/// The walk says why there was no hit; only the application knows which name
/// the client used to ask.
fn missed(url: UrlRepo<'_>, miss: Miss) -> ResolveError {
    match miss {
        Miss::Degraded(why) => ResolveError::Upstream(format!("group {}: {why}", url.0)),
        Miss::Nothing => ResolveError::NotFound(format!("not found in repository '{}'", url.0)),
    }
}

/// What one member's answer contributes. A member that has nothing is not a
/// failure however it says so; a member whose upstream failed does not end
/// the walk; every other refusal is ours and ends it with its own status.
fn visit<T>(member: &str, result: Result<Outcome<T>, ResolveError>) -> Result<Visit<T>, ResolveError> {
    match result {
        Ok(Outcome::Found(hit)) => Ok(Visit::Hit(hit)),
        Ok(Outcome::NotFound) | Err(ResolveError::NotFound(_)) => Ok(Visit::Nothing),
        Err(ResolveError::Upstream(why)) => {
            warn!(member, error = %why, "member failed; trying the next one");
            Ok(Visit::Failed(format!("member {member} failed: {why}")))
        }
        Err(
            e @ (ResolveError::Domain(_)
            | ResolveError::Store(_)
            | ResolveError::Storage(_)
            | ResolveError::Internal(_)),
        ) => Err(e),
    }
}

fn walk<'a, L: Leaf + 'a>(
    cx: &'a Cx<'a>,
    repo: &'a Repository,
    leaf: &'a L,
    depth: u32,
    w: &'a mut Walk<L::Out>,
) -> Pin<Box<dyn Future<Output = Result<(), ResolveError>> + Send + 'a>> {
    Box::pin(async move {
        let member = CacheRepo(repo);
        match repo.kind()? {
            RepoKind::Hosted => {
                let answer = leaf.hosted(cx, member).await;
                w.record(visit(&repo.name, answer)?);
            }
            RepoKind::Proxy => {
                let answer = match Upstream::for_member(cx.creds, repo) {
                    Ok(up) => leaf.proxy(cx, member, &up).await,
                    Err(e) => Err(e),
                };
                w.record(visit(&repo.name, answer)?);
            }
            RepoKind::Group => walk_members(cx, repo, leaf, depth, w).await?,
        }
        Ok(())
    })
}

async fn walk_members<'a, L: Leaf + 'a>(
    cx: &'a Cx<'a>,
    group: &Repository,
    leaf: &'a L,
    depth: u32,
    w: &'a mut Walk<L::Out>,
) -> Result<(), ResolveError> {
    // Only pre-validation rows can get here: writes refuse a deeper stack.
    if depth >= MAX_GROUP_DEPTH {
        return Err(ResolveError::Internal(
            "group nesting depth exceeded".to_string(),
        ));
    }
    let members = group.members();
    if members.is_empty() {
        warn!(group = %group.name, "group repository has no members configured");
        return Ok(());
    }
    let format = group.fmt()?;
    for name in &members {
        if w.done() {
            return Ok(());
        }
        let Some(member) = cx.repos.by_name(name).await? else {
            warn!(group = %group.name, member = %name, "group member repository not found, skipping");
            continue;
        };
        if !readable(cx, &member).await? {
            continue;
        }
        if member.fmt()? != format {
            warn!(group = %group.name, member = %name, "group member has another format, skipping");
            continue;
        }
        if !w.first_visit(member.id) {
            continue;
        }
        walk(cx, &member, leaf, depth + 1, w).await?;
    }
    Ok(())
}

/// For an anonymous caller only: `Forbidden(read)` when any member the
/// group would walk is unreadable to them, whatever the member order and
/// whether or not the package exists. Reads the configuration only, never
/// an upstream or a package store. An authenticated caller is not probed:
/// the walk skips an unreadable member silently.
pub async fn probe_access(cx: &Cx<'_>, repo: &Repository) -> Result<(), ResolveError> {
    if cx.auth.is_some() {
        return Ok(());
    }
    let mut seen = vec![repo.id];
    let mut level = vec![repo.clone()];
    for _ in 0..=MAX_GROUP_DEPTH {
        let mut next = Vec::new();
        for group in level.iter().filter(|r| r.kind().ok() == Some(RepoKind::Group)) {
            for name in group.members() {
                let Some(member) = cx.repos.by_name(&name).await? else {
                    continue;
                };
                if seen.contains(&member.id) {
                    continue;
                }
                seen.push(member.id);
                if !readable(cx, &member).await? {
                    return Err(ResolveError::Domain(DomainError::Forbidden(Action {
                        verb: "read",
                        on: Resource {
                            kind: "repository",
                            id: cx.url.0.to_string(),
                        },
                    })));
                }
                next.push(member);
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    Ok(())
}

/// The hosted and proxy repositories a `collect` over `repo` asks for this
/// caller, in walk order: the caller's permission view of `repo`, which a
/// memo of merged answers keys by. Reads the configuration and the grants,
/// never an upstream or a package store.
pub async fn view(cx: &Cx<'_>, repo: &Repository) -> Result<Vec<Repository>, ResolveError> {
    let mut seen = std::collections::HashSet::from([repo.id]);
    let mut out = Vec::new();
    view_of(cx, repo, 0, &mut seen, &mut out).await?;
    Ok(out)
}

fn view_of<'a>(
    cx: &'a Cx<'a>,
    repo: &'a Repository,
    depth: u32,
    seen: &'a mut std::collections::HashSet<i64>,
    out: &'a mut Vec<Repository>,
) -> Pin<Box<dyn Future<Output = Result<(), ResolveError>> + Send + 'a>> {
    Box::pin(async move {
        if repo.kind()? != RepoKind::Group {
            out.push(repo.clone());
            return Ok(());
        }
        if depth >= MAX_GROUP_DEPTH {
            return Err(ResolveError::Internal("group nesting depth exceeded".to_string()));
        }
        let format = repo.fmt()?;
        for name in repo.members() {
            let Some(member) = cx.repos.by_name(&name).await? else {
                continue;
            };
            if !readable(cx, &member).await? || member.fmt()? != format || !seen.insert(member.id) {
                continue;
            }
            view_of(cx, &member, depth + 1, seen, out).await?;
        }
        Ok(())
    })
}

/// Whether the caller may read this member, and the one refusal that is not a
/// verdict: while the grant cannot be read at all, "the store is unreliable"
/// and "authorization is unsafe to decide" are the same fact, so the walk
/// stops with a retryable answer instead of silently skipping the member.
async fn readable(cx: &Cx<'_>, member: &Repository) -> Result<bool, ResolveError> {
    let authz = cx.authorize();
    match authz
        .member(cx.auth, member, crate::domain::RepoAction::Read)
        .await
    {
        Verdict::Ok => Ok(true),
        Verdict::Unauthenticated | Verdict::Forbidden | Verdict::OutOfScope => Ok(false),
        Verdict::Unavailable => Err(ResolveError::Store(StoreError::Unavailable)),
    }
}

#[cfg(test)]
mod tests;

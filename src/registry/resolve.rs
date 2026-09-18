use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use tracing::warn;

use crate::auth::middleware::AuthUser;
use crate::domain::{RepoKind, Repository};
use crate::error::{AppError, AppResult};
use crate::proxy::auth::{default_token_realms, UpstreamAuth};
use crate::server::AppState;

pub const MAX_GROUP_DEPTH: u32 = 5;

/// The repository the client addressed: the only name in URLs it sees.
#[derive(Clone, Copy, Debug)]
pub struct UrlRepo<'a>(pub &'a str);

/// The member repository that owns cached bytes: the only name allowed in
/// cache keys, storage paths and `repository_id`.
#[derive(Clone, Copy, Debug)]
pub struct CacheRepo<'a>(pub &'a Repository);

pub struct Cx<'a> {
    pub state: &'a AppState,
    pub auth: Option<&'a AuthUser>,
    pub url: UrlRepo<'a>,
}

#[derive(Debug)]
pub enum Outcome<T> {
    Found(T),
    NotFound,
}

#[derive(Clone, Debug)]
pub struct Upstream {
    pub base: reqwest::Url,
    pub auth: Option<UpstreamAuth>,
    pub token_realms: Vec<reqwest::Url>,
    pub dl_allow_private: bool,
}

impl Upstream {
    pub fn for_member(state: &AppState, member: &Repository) -> AppResult<Self> {
        let raw = member.upstream_url.as_deref().ok_or_else(|| {
            AppError::Internal(format!(
                "proxy repository {} has no upstream_url configured",
                member.name
            ))
        })?;
        let base = reqwest::Url::parse(raw).map_err(|e| {
            AppError::Internal(format!(
                "proxy repository {} has an invalid upstream_url: {e}",
                member.name
            ))
        })?;
        let creds = state
            .upstream_auth
            .get(&member.name)
            .cloned()
            .unwrap_or_default();
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
    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Self::Out>>;
    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> AppResult<Outcome<Self::Out>>;
}

pub struct Collected<T> {
    pub hits: Vec<T>,
    pub degraded: Option<String>,
}

/// First `Found` in member order; none is `NotFound`, or `BadGateway` when a
/// member failed on the way.
pub async fn first_hit<L: Leaf>(cx: &Cx<'_>, repo: &Repository, leaf: &L) -> AppResult<L::Out> {
    let mut w = Walk::new(repo, true);
    walk(cx, repo, leaf, 0, &mut w).await?;
    let hits = std::mem::take(&mut w.hits);
    hits.into_iter().next().ok_or_else(|| w.miss(cx))
}

/// Every `Found` in member order; zero hits after a failure is `BadGateway`,
/// hits after a failure are `degraded`.
pub async fn collect<L: Leaf>(
    cx: &Cx<'_>,
    repo: &Repository,
    leaf: &L,
) -> AppResult<Collected<L::Out>> {
    let mut w = Walk::new(repo, false);
    walk(cx, repo, leaf, 0, &mut w).await?;
    if w.hits.is_empty() && w.failure.is_some() {
        return Err(w.miss(cx));
    }
    Ok(Collected {
        hits: w.hits,
        degraded: w.failure,
    })
}

struct Walk<T> {
    hits: Vec<T>,
    first_only: bool,
    seen: HashSet<i64>,
    failure: Option<String>,
}

impl<T> Walk<T> {
    fn new(root: &Repository, first_only: bool) -> Self {
        Self {
            hits: Vec::new(),
            first_only,
            seen: HashSet::from([root.id]),
            failure: None,
        }
    }

    fn done(&self) -> bool {
        self.first_only && !self.hits.is_empty()
    }

    fn record(&mut self, member: &str, result: AppResult<Outcome<T>>) {
        match result {
            Ok(Outcome::Found(hit)) => self.hits.push(hit),
            Ok(Outcome::NotFound) | Err(AppError::NotFound(_)) => {}
            Err(e) => {
                warn!(member, error = %e, "member failed; trying the next one");
                self.failure
                    .get_or_insert_with(|| format!("member {member} failed: {e}"));
            }
        }
    }

    fn miss(&self, cx: &Cx<'_>) -> AppError {
        match &self.failure {
            Some(why) => AppError::BadGateway(format!("group {}: {why}", cx.url.0)),
            None => AppError::NotFound(format!("not found in repository '{}'", cx.url.0)),
        }
    }
}

fn walk<'a, L: Leaf + 'a>(
    cx: &'a Cx<'a>,
    repo: &'a Repository,
    leaf: &'a L,
    depth: u32,
    w: &'a mut Walk<L::Out>,
) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send + 'a>> {
    Box::pin(async move {
        let member = CacheRepo(repo);
        match repo.kind()? {
            RepoKind::Hosted => w.record(&repo.name, leaf.hosted(cx, member).await),
            RepoKind::Proxy => {
                let result = match Upstream::for_member(cx.state, repo) {
                    Ok(up) => leaf.proxy(cx, member, &up).await,
                    Err(e) => Err(e),
                };
                w.record(&repo.name, result);
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
) -> AppResult<()> {
    // Only pre-validation rows can get here: writes refuse a deeper stack.
    if depth >= MAX_GROUP_DEPTH {
        return Err(AppError::Internal(
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
        let Some(member) = crate::db::get_repository_by_name(&cx.state.db, name).await? else {
            warn!(group = %group.name, member = %name, "group member repository not found, skipping");
            continue;
        };
        match super::ensure_can_read(&cx.state.db, &member, cx.auth).await {
            Ok(()) => {}
            Err(AppError::Unauthorized(_) | AppError::Forbidden(_)) => continue,
            Err(e) => return Err(e),
        }
        if member.fmt()? != format {
            warn!(group = %group.name, member = %name, "group member has another format, skipping");
            continue;
        }
        if !w.seen.insert(member.id) {
            continue;
        }
        walk(cx, &member, leaf, depth + 1, w).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DatabaseConfig, RepositoryConfig, ServerConfig, Visibility};
    use crate::domain::Format;

    struct Script;

    fn scripted(repo: &Repository) -> AppResult<Outcome<String>> {
        match repo.name.as_str() {
            n if n.ends_with("-miss") => Ok(Outcome::NotFound),
            n if n.ends_with("-err404") => Err(AppError::NotFound(n.to_string())),
            n if n.ends_with("-fail") => Err(AppError::BadGateway(format!("{n} is down"))),
            n => Ok(Outcome::Found(n.to_string())),
        }
    }

    #[async_trait::async_trait]
    impl Leaf for Script {
        type Out = String;

        async fn hosted(&self, _cx: &Cx<'_>, m: CacheRepo<'_>) -> AppResult<Outcome<String>> {
            scripted(m.0)
        }

        async fn proxy(
            &self,
            _cx: &Cx<'_>,
            m: CacheRepo<'_>,
            _up: &Upstream,
        ) -> AppResult<Outcome<String>> {
            scripted(m.0)
        }
    }

    fn hosted(name: &str, format: Format, vis: Visibility) -> RepositoryConfig {
        RepositoryConfig {
            name: name.into(),
            format,
            visibility: vis,
            ..Default::default()
        }
    }

    fn group(name: &str, members: &[&str]) -> RepositoryConfig {
        RepositoryConfig {
            name: name.into(),
            repo_type: RepoKind::Group,
            visibility: Visibility::Public,
            members: Some(members.iter().map(|m| m.to_string()).collect()),
            ..Default::default()
        }
    }

    fn fixture() -> Vec<RepositoryConfig> {
        let mut repos = vec![
            hosted("h-found", Format::Npm, Visibility::Public),
            hosted("h-miss", Format::Npm, Visibility::Public),
            hosted("h-err404", Format::Npm, Visibility::Public),
            hosted("h-fail", Format::Npm, Visibility::Public),
            hosted("h-private", Format::Npm, Visibility::Private),
            hosted("cargo-found", Format::Cargo, Visibility::Public),
            RepositoryConfig {
                name: "p-found".into(),
                repo_type: RepoKind::Proxy,
                visibility: Visibility::Public,
                upstream: Some("http://127.0.0.1:1".into()),
                ..Default::default()
            },
            group("g-order", &["h-miss", "h-err404", "h-found", "p-found"]),
            group("g-fail-then-found", &["h-fail", "h-found"]),
            group("g-fail-only", &["h-fail", "h-miss"]),
            group("g-nested", &["g-order"]),
        ];
        // d2 -> d3 -> d4 -> d5 -> d6 -> h-found: the five levels a write allows.
        for i in 2..7 {
            let next = if i == 6 { "h-found".to_string() } else { format!("d{}", i + 1) };
            repos.push(group(&format!("d{i}"), &[&next]));
        }
        repos
    }

    async fn state(tmp: &tempfile::TempDir) -> AppState {
        let config = Config {
            server: ServerConfig {
                storage_path: tmp.path().join("storage").display().to_string(),
                ..Default::default()
            },
            database: DatabaseConfig {
                url: format!("sqlite:{}?mode=rwc", tmp.path().join("t.db").display()),
            },
            repositories: fixture(),
            ..Default::default()
        };
        let state = crate::server::build_state(&config).await.unwrap();
        // Rows the seed now refuses but the resolver must still tolerate.
        for (name, members) in [
            ("g-skips", r#"["nope","h-private","cargo-found","h-miss"]"#),
            ("g-empty", "[]"),
            ("g-cycle-a", r#"["g-cycle-b"]"#),
            ("g-cycle-b", r#"["g-cycle-a","p-found"]"#),
            ("d1", r#"["d2"]"#),
            ("d0", r#"["d1"]"#),
        ] {
            sqlx::query(
                "INSERT INTO repositories (name, repo_type, format, visibility, config_json)
                 VALUES (?1, 'group', 'npm', 'public', ?2)",
            )
            .bind(name)
            .bind(format!(r#"{{"members":{members}}}"#))
            .execute(&state.db)
            .await
            .unwrap();
        }
        state
    }

    async fn repo(state: &AppState, name: &str) -> Repository {
        crate::db::get_repository_by_name(&state.db, name)
            .await
            .unwrap()
            .unwrap()
    }

    fn cx(state: &AppState) -> Cx<'_> {
        Cx {
            state,
            auth: None,
            url: UrlRepo("requested"),
        }
    }

    async fn first(state: &AppState, name: &str) -> AppResult<String> {
        first_hit(&cx(state), &repo(state, name).await, &Script).await
    }

    async fn all(state: &AppState, name: &str) -> AppResult<Collected<String>> {
        collect(&cx(state), &repo(state, name).await, &Script).await
    }

    #[tokio::test]
    async fn error_policy_table() {
        let tmp = tempfile::TempDir::new().unwrap();
        let st = state(&tmp).await;

        assert_eq!(first(&st, "h-found").await.unwrap(), "h-found");
        assert_eq!(first(&st, "p-found").await.unwrap(), "p-found");
        assert!(matches!(first(&st, "h-miss").await, Err(AppError::NotFound(_))));
        assert!(matches!(first(&st, "h-fail").await, Err(AppError::BadGateway(_))));
        assert!(matches!(all(&st, "h-fail").await, Err(AppError::BadGateway(_))));

        assert_eq!(first(&st, "g-order").await.unwrap(), "h-found");
        let ordered = all(&st, "g-order").await.unwrap();
        assert_eq!(ordered.hits, vec!["h-found", "p-found"]);
        assert_eq!(ordered.degraded, None);

        assert_eq!(first(&st, "g-fail-then-found").await.unwrap(), "h-found");
        let degraded = all(&st, "g-fail-then-found").await.unwrap();
        assert_eq!(degraded.hits, vec!["h-found"]);
        assert!(degraded.degraded.unwrap().contains("h-fail is down"));

        let Err(AppError::BadGateway(msg)) = first(&st, "g-fail-only").await else {
            panic!("no hit after a failure is 502");
        };
        assert!(msg.contains("group requested") && msg.contains("h-fail"), "{msg}");
        assert!(matches!(all(&st, "g-fail-only").await, Err(AppError::BadGateway(_))));

        assert!(matches!(first(&st, "g-skips").await, Err(AppError::NotFound(_))));
        let skipped = all(&st, "g-skips").await.unwrap();
        assert!(skipped.hits.is_empty() && skipped.degraded.is_none());
        assert!(matches!(first(&st, "g-empty").await, Err(AppError::NotFound(_))));
        assert!(all(&st, "g-empty").await.unwrap().hits.is_empty());

        assert_eq!(first(&st, "g-cycle-a").await.unwrap(), "p-found");
        assert_eq!(all(&st, "g-cycle-b").await.unwrap().hits, vec!["p-found"]);
        assert_eq!(first(&st, "g-nested").await.unwrap(), "h-found");

        assert_eq!(first(&st, "d2").await.unwrap(), "h-found");
        for too_deep in ["d1", "d0"] {
            let Err(AppError::Internal(msg)) = first(&st, too_deep).await else {
                panic!("{too_deep}: a sixth nested group exceeds the depth cap");
            };
            assert!(msg.contains("depth"), "{msg}");
        }
    }
}

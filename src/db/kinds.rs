use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use sqlx::SqlitePool;

use crate::domain::{Format, RepoConfig, RepoKind, Repository};
use crate::error::{AppError, AppResult};
use crate::registry::resolve::MAX_GROUP_DEPTH;

pub struct RepoSpec<'a> {
    pub name: &'a str,
    pub kind: RepoKind,
    pub format: Format,
    pub upstream: Option<&'a str>,
    pub members: &'a [String],
}

/// A config entry the seed has not inserted yet: members may be listed
/// later in the file, so validation sees the whole list.
pub struct Pending<'a> {
    pub name: &'a str,
    pub kind: RepoKind,
    pub format: Format,
    pub members: &'a [String],
}

impl RepoSpec<'_> {
    /// The `config` document: the member list for a group, nothing otherwise.
    pub fn config(&self) -> Option<RepoConfig> {
        (self.kind == RepoKind::Group).then(|| RepoConfig::of_members(self.members))
    }

    fn refuse_upstream(&self) -> AppResult<()> {
        match self.upstream {
            Some(_) => Err(AppError::BadRequest(format!(
                "{} repositories take no upstream",
                self.kind.as_str()
            ))),
            None => Ok(()),
        }
    }

    fn refuse_members(&self) -> AppResult<()> {
        if self.members.is_empty() {
            return Ok(());
        }
        Err(AppError::BadRequest(format!(
            "{} repositories take no members",
            self.kind.as_str()
        )))
    }
}

/// The name is a raw storage segment and the purge prefix, so it is one
/// lowercase segment without `..`.
fn validate_name(name: &str) -> AppResult<()> {
    let mut chars = name.chars();
    let head_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let tail_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if head_ok && tail_ok && name.len() <= 64 && !name.contains("..") {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "invalid repository name '{name}': [a-z0-9][a-z0-9._-]{{0,63}} without '..'"
    )))
}

/// Refuse a repository definition that could not be served or purged: name
/// rule, kind supported by the format, upstream on proxies only, members on
/// groups only, each member existing (in the DB or `pending`, the config list
/// being seeded), of the same format, neither the group itself nor reaching
/// it, and the whole stack at most `MAX_GROUP_DEPTH` groups deep.
pub async fn validate_spec(
    pool: &SqlitePool,
    spec: &RepoSpec<'_>,
    pending: &[Pending<'_>],
) -> AppResult<()> {
    validate_name(spec.name)?;
    if !spec.format.supports_kind(spec.kind) {
        return Err(AppError::BadRequest(format!(
            "{} repositories cannot be {}",
            spec.format.as_str(),
            spec.kind.as_str()
        )));
    }
    match spec.kind {
        RepoKind::Hosted => {
            spec.refuse_upstream()?;
            spec.refuse_members()
        }
        RepoKind::Proxy => {
            spec.refuse_members()?;
            let upstream = spec.upstream.ok_or_else(|| {
                AppError::BadRequest("proxy repositories need an upstream".to_string())
            })?;
            crate::proxy::validate_upstream_url(upstream)
        }
        RepoKind::Group => {
            spec.refuse_upstream()?;
            validate_members(pool, spec, pending).await
        }
    }
}

async fn validate_members(
    pool: &SqlitePool,
    spec: &RepoSpec<'_>,
    pending: &[Pending<'_>],
) -> AppResult<()> {
    if spec.members.is_empty() {
        return Err(AppError::BadRequest(
            "group repositories need at least one member".to_string(),
        ));
    }
    let mut deepest = 0;
    for member in spec.members {
        deepest = deepest.max(nesting(pool, member, pending, &mut HashSet::new()).await?);
        if member == spec.name {
            return Err(AppError::BadRequest(format!(
                "group '{member}' cannot be its own member"
            )));
        }
        let format = match super::get_repository_by_name(pool, member).await? {
            Some(row) => {
                if reaches(pool, &row, spec.name, &mut HashSet::new()).await? {
                    return Err(AppError::BadRequest(format!(
                        "group member '{member}' already contains '{}'",
                        spec.name
                    )));
                }
                row.fmt()?
            }
            None => pending
                .iter()
                .find(|p| p.name == member.as_str())
                .map(|p| p.format)
                .ok_or_else(|| AppError::BadRequest(format!("group member not found: {member}")))?,
        };
        if format != spec.format {
            return Err(AppError::BadRequest(format!(
                "group member '{member}' is {}, not {}",
                format.as_str(),
                spec.format.as_str()
            )));
        }
    }
    if deepest + 1 > MAX_GROUP_DEPTH {
        return Err(AppError::BadRequest(format!(
            "group '{}' would be {} groups deep, the limit is {MAX_GROUP_DEPTH}",
            spec.name,
            deepest + 1
        )));
    }
    Ok(())
}

/// Groups stacked below `name`, itself included: 0 for a hosted or proxy
/// repository, 1 for a group of those. A row wins over a pending entry, as
/// the seed's `INSERT OR IGNORE` does; a cycle counts once, `reaches`
/// refuses it.
fn nesting<'a>(
    pool: &'a SqlitePool,
    name: &'a str,
    pending: &'a [Pending<'a>],
    seen: &'a mut HashSet<String>,
) -> Pin<Box<dyn Future<Output = AppResult<u32>> + Send + 'a>> {
    Box::pin(async move {
        if !seen.insert(name.to_string()) {
            return Ok(0);
        }
        let members = match super::get_repository_by_name(pool, name).await? {
            Some(row) if row.kind()? == RepoKind::Group => row.members(),
            Some(_) => return Ok(0),
            None => match pending.iter().find(|p| p.name == name) {
                Some(p) if p.kind == RepoKind::Group => p.members.to_vec(),
                _ => return Ok(0),
            },
        };
        let mut deepest = 0;
        for member in &members {
            deepest = deepest.max(nesting(pool, member, pending, seen).await?);
        }
        Ok(deepest + 1)
    })
}

/// Whether `target` is reachable through `group`'s members; `seen` bounds the
/// walk over pre-upgrade cycles.
fn reaches<'a>(
    pool: &'a SqlitePool,
    group: &'a Repository,
    target: &'a str,
    seen: &'a mut HashSet<i64>,
) -> Pin<Box<dyn Future<Output = AppResult<bool>> + Send + 'a>> {
    Box::pin(async move {
        if !seen.insert(group.id) {
            return Ok(false);
        }
        for name in group.members() {
            if name == target {
                return Ok(true);
            }
            let Some(member) = super::get_repository_by_name(pool, &name).await? else {
                continue;
            };
            if member.kind()? == RepoKind::Group && reaches(pool, &member, target, seen).await? {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

/// Startup guard: every stored name must pass the rule `validate_spec` applies
/// on writes, or cache paths and purge prefixes would misbehave.
pub async fn check_repository_names(pool: &SqlitePool) -> anyhow::Result<()> {
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM repositories ORDER BY name")
        .fetch_all(pool)
        .await?;
    let offenders: Vec<String> = names
        .into_iter()
        .filter(|name| validate_name(name).is_err())
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "repository names no longer allowed, rename them by SQL before starting: {}",
        offenders.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_repository_names_refuses_pre_upgrade_slash() {
        for ok in [
            "a",
            "npm-all",
            "oci-hosted",
            "npm-private",
            "a.b_c-d",
            &"x".repeat(64),
        ] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "A", "a/b", "a..b", "-a", ".a", "a b", &"x".repeat(65)] {
            assert!(
                matches!(validate_name(bad), Err(AppError::BadRequest(_))),
                "{bad}"
            );
        }

        let (_tmp, pool) = crate::db::testing::pool().await;
        check_repository_names(&pool).await.unwrap();
        sqlx::query("INSERT INTO repositories (name, repo_type, format) VALUES ('a/b', 'hosted', 'npm'), ('ok', 'hosted', 'npm')")
            .execute(&pool)
            .await
            .unwrap();
        let err = check_repository_names(&pool).await.unwrap_err().to_string();
        assert!(err.contains("a/b"), "{err}");
        assert!(!err.contains("ok"), "{err}");
    }
}

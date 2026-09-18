use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::db::Repository;
use crate::error::{AppError, AppResult};
use crate::registry::resolve::MAX_GROUP_DEPTH;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoKind {
    #[default]
    Hosted,
    Proxy,
    Group,
}

impl RepoKind {
    pub const ALL: [RepoKind; 3] = [RepoKind::Hosted, RepoKind::Proxy, RepoKind::Group];

    pub const fn as_str(self) -> &'static str {
        match self {
            RepoKind::Hosted => "hosted",
            RepoKind::Proxy => "proxy",
            RepoKind::Group => "group",
        }
    }
}

impl FromStr for RepoKind {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, AppError> {
        RepoKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| AppError::BadRequest(format!("invalid repository type: {s}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Npm,
    Cargo,
    Oci,
    Go,
    Pypi,
}

impl Format {
    pub const ALL: [Format; 5] = [
        Format::Npm,
        Format::Cargo,
        Format::Oci,
        Format::Go,
        Format::Pypi,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Format::Npm => "npm",
            Format::Cargo => "cargo",
            Format::Oci => "oci",
            Format::Go => "go",
            Format::Pypi => "pypi",
        }
    }

    pub const fn osv_ecosystem(self) -> Option<&'static str> {
        match self {
            Format::Npm => Some("npm"),
            Format::Cargo => Some("crates.io"),
            Format::Go => Some("Go"),
            Format::Oci | Format::Pypi => None,
        }
    }

    pub const fn supports_kind(self, kind: RepoKind) -> bool {
        !matches!(
            (self, kind),
            (Format::Pypi, RepoKind::Proxy | RepoKind::Group)
        )
    }
}

impl FromStr for Format {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, AppError> {
        Format::ALL
            .into_iter()
            .find(|format| format.as_str() == s)
            .ok_or_else(|| AppError::BadRequest(format!("invalid repository format: {s}")))
    }
}

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
    /// The `config_json` column: the member list for a group, nothing otherwise.
    pub fn config_json(&self) -> Option<String> {
        (self.kind == RepoKind::Group)
            .then(|| serde_json::json!({ "members": self.members }).to_string())
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

impl Repository {
    pub fn kind(&self) -> AppResult<RepoKind> {
        self.repo_type
            .parse()
            .map_err(|_| self.corrupt_column("repo_type", &self.repo_type))
    }

    pub fn fmt(&self) -> AppResult<Format> {
        self.format
            .parse()
            .map_err(|_| self.corrupt_column("format", &self.format))
    }

    pub fn members(&self) -> Vec<String> {
        super::parse_group_members(self.config_json.as_deref())
    }

    fn corrupt_column(&self, column: &str, value: &str) -> AppError {
        AppError::Internal(format!(
            "repository '{}' has a corrupt {column} column: '{value}'",
            self.name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(repo_type: &str, format: &str) -> Repository {
        Repository {
            id: 1,
            name: "r".to_string(),
            repo_type: repo_type.to_string(),
            format: format.to_string(),
            visibility: "public".to_string(),
            upstream_url: None,
            config_json: Some(r#"{"members":["a","b"]}"#.to_string()),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// That these values are the ones the `repo_type` and `format` CHECKs admit
    /// is asserted by the SQLite adapter against a migrated database
    /// (`adapters/sqlite/migrate_tests.rs`): the dialect is the adapter's to
    /// know, and reading a migration file from here would only prove that two
    /// texts agree.
    #[test]
    fn values_round_trip_and_predicates_hold() {
        for kind in RepoKind::ALL {
            assert_eq!(kind.as_str().parse::<RepoKind>().unwrap(), kind);
        }

        for format in Format::ALL {
            assert_eq!(format.as_str().parse::<Format>().unwrap(), format);
            for kind in RepoKind::ALL {
                let expected = format != Format::Pypi || kind == RepoKind::Hosted;
                assert_eq!(format.supports_kind(kind), expected, "{format:?}/{kind:?}");
            }
        }
        assert_eq!(Format::Cargo.osv_ecosystem(), Some("crates.io"));
        assert_eq!(Format::Oci.osv_ecosystem(), None);

        let repo = repository("group", "cargo");
        assert_eq!(repo.kind().unwrap(), RepoKind::Group);
        assert_eq!(repo.fmt().unwrap(), Format::Cargo);
        assert_eq!(repo.members(), vec!["a".to_string(), "b".to_string()]);

        assert!(matches!(
            "mirror".parse::<RepoKind>(),
            Err(AppError::BadRequest(_))
        ));
        assert!(matches!(
            repository("mirror", "npm").kind(),
            Err(AppError::Internal(_))
        ));
        assert!(matches!(
            repository("hosted", "deb").fmt(),
            Err(AppError::Internal(_))
        ));
    }

    /// `corrupt_column` and `DomainError::CorruptColumn` build the same
    /// sentence, so moving these two methods into the domain cannot change an
    /// HTTP body.
    #[test]
    fn corrupt_column_message_matches_the_domain_error() {
        let repo = repository("mirror", "npm");
        assert_eq!(
            repo.kind().unwrap_err().to_string(),
            crate::domain::DomainError::CorruptColumn {
                repo: repo.name.clone(),
                column: "repo_type",
                value: repo.repo_type.clone(),
            }
            .to_string()
        );
    }

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

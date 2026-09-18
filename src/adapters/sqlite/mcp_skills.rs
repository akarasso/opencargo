//! The skill half of `McpStore` (023): archives placed through the shared
//! placement, their findings, and the approval of their exact surface.

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::super::{bind_ts, corrupt_row, immediate, read_ts, store_error, Tx};
use crate::error::StoreError;
use crate::ports::mcp::{NewSkill, SkillRow};

/// This port's contribution to the reclaim predicate: every skill archive.
pub(crate) const REFERENCED: &str = "SELECT storage_key AS k, 0 AS p FROM mcp_skills";

const SELECT: &str = "SELECT s.id, s.repository_id, s.name, s.version, s.storage_key, s.sha256, s.size, s.description,
        s.allowed_tools, s.surface_sha256, s.findings_high, s.findings_medium, s.blocking_findings, s.published_at,
        COALESCE(
            (SELECT state FROM mcp_approvals a WHERE a.repository_id = ?1 AND a.subject_kind = 'skill'
                AND a.name = s.name AND a.version = s.version AND a.remote_url = ''
                AND a.permissions_sha256 = s.surface_sha256),
            (SELECT state FROM mcp_approvals a WHERE a.repository_id = s.repository_id AND a.subject_kind = 'skill'
                AND a.name = s.name AND a.version = s.version AND a.remote_url = ''
                AND a.permissions_sha256 = s.surface_sha256)) AS decision
     FROM mcp_skills s";

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    repository_id: i64,
    name: String,
    version: String,
    storage_key: String,
    sha256: String,
    size: i64,
    description: Option<String>,
    allowed_tools: Option<String>,
    surface_sha256: String,
    findings_high: i64,
    findings_medium: i64,
    blocking_findings: i64,
    published_at: String,
    decision: Option<String>,
}

fn skill_row(r: Row) -> Result<SkillRow, StoreError> {
    Ok(SkillRow {
        published_at: read_ts(&r.name, "published_at", &r.published_at).map_err(corrupt_row)?,
        decision: r.decision.and_then(|d| d.parse().ok()),
        id: r.id,
        member: r.repository_id,
        name: r.name,
        version: r.version,
        key: r.storage_key,
        sha256: r.sha256,
        size: r.size,
        description: r.description,
        allowed_tools: r.allowed_tools,
        surface_sha256: r.surface_sha256,
        findings_high: r.findings_high,
        findings_medium: r.findings_medium,
        blocking: r.blocking_findings,
    })
}

async fn write(tx: &mut Tx, s: &NewSkill) -> Result<i64, sqlx::Error> {
    let high = s.findings.iter().filter(|f| f.high).count() as i64;
    let medium = s.findings.len() as i64 - high;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO mcp_skills (repository_id, name, version, storage_key, sha256, size, description, allowed_tools,
             surface_sha256, findings_high, findings_medium, blocking_findings, published_by, published_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) RETURNING id",
    )
    .bind(s.repository)
    .bind(&s.name)
    .bind(&s.version)
    .bind(&s.pins[0].physical_key)
    .bind(&s.sha256)
    .bind(s.size)
    .bind(&s.description)
    .bind(&s.allowed_tools)
    .bind(&s.surface_sha256)
    .bind(high)
    .bind(medium)
    .bind(s.blocking)
    .bind(&s.published_by)
    .bind(bind_ts(s.now))
    .fetch_one(&mut **tx)
    .await?;
    super::replace_findings(tx, "skill", id, &s.findings).await?;
    Ok(id)
}

pub(super) async fn publish(pool: &SqlitePool, s: &NewSkill) -> Result<i64, StoreError> {
    if s.pins.is_empty() {
        return Err(StoreError::Other("a skill row needs its archive's pin".into()));
    }
    immediate(pool, |mut tx| {
        Box::pin(async move {
            let done = match super::super::reclaim::spend_pins(&mut tx, &s.pins).await {
                Ok(Ok(())) => write(&mut tx, s).await.map(Ok),
                Ok(Err(revoked)) => Ok(Err(StoreError::Superseded(revoked))),
                Err(e) => Err(e),
            };
            (tx, done)
        })
    })
    .await
}

pub(super) async fn list(pool: &SqlitePool, member: i64, addressed: i64) -> Result<Vec<SkillRow>, StoreError> {
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "{SELECT} WHERE s.repository_id = ?2 ORDER BY s.name, s.published_at DESC, s.id DESC"
    ))
    .bind(addressed)
    .bind(member)
    .fetch_all(pool)
    .await
    .map_err(store_error)?;
    rows.into_iter().map(skill_row).collect()
}

pub(super) async fn one(pool: &SqlitePool, member: i64, addressed: i64, name: &str, version: &str) -> Result<Option<SkillRow>, StoreError> {
    let row: Option<Row> = sqlx::query_as(&format!(
        "{SELECT} WHERE s.repository_id = ?2 AND s.name = ?3 AND s.version = ?4"
    ))
    .bind(addressed)
    .bind(member)
    .bind(name)
    .bind(version)
    .fetch_optional(pool)
    .await
    .map_err(store_error)?;
    row.map(skill_row).transpose()
}

pub(super) async fn delete(pool: &SqlitePool, repository: i64, name: &str, version: &str, now: DateTime<Utc>) -> Result<Vec<String>, StoreError> {
    immediate(pool, |mut tx| {
        Box::pin(async move {
            let done = async {
                let found: Option<(i64, String)> = sqlx::query_as(
                    "SELECT id, storage_key FROM mcp_skills WHERE repository_id = ?1 AND name = ?2 AND version = ?3",
                )
                .bind(repository)
                .bind(name)
                .bind(version)
                .fetch_optional(&mut *tx)
                .await?;
                let Some((id, key)) = found else {
                    return Ok(Err(StoreError::NotFound));
                };
                sqlx::query("DELETE FROM mcp_findings WHERE subject_kind = 'skill' AND subject_id = ?1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM mcp_skills WHERE id = ?1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                let keys = vec![key];
                super::super::reclaim::enqueue_keys(&mut tx, &keys, now).await?;
                Ok(Ok(keys))
            }
            .await;
            (tx, done)
        })
    })
    .await
}

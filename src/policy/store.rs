use chrono::SecondsFormat;
use sqlx::SqlitePool;

use super::{Resolution, RuleVerdict, Verdict};

/// One transaction for a whole batch; verdicts follow their resolution.
pub async fn insert_batch(
    pool: &SqlitePool,
    rows: &[(Resolution, Vec<RuleVerdict>)],
) -> Result<Vec<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut ids = Vec::with_capacity(rows.len());
    for (r, verdicts) in rows {
        let would_block = verdicts.iter().any(|v| v.verdict == Verdict::WouldBlock);
        let unknown = !would_block && verdicts.iter().any(|v| v.verdict == Verdict::Unknown);
        let id = sqlx::query(
            "INSERT INTO policy_resolutions
                 (requested_repo, member_repo, format, name, version, digest, published_at,
                  date_source, actor, actor_kind, user_id, would_block, unknown)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )
        .bind(&r.requested_repo)
        .bind(&r.member_repo)
        .bind(r.format.as_str())
        .bind(&r.name)
        .bind(&r.version)
        .bind(&r.digest)
        .bind(
            r.published_at
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true)),
        )
        .bind(r.facts.date_source)
        .bind(&r.actor.name)
        .bind(r.actor.kind.as_str())
        .bind(r.actor.user_id)
        .bind(would_block)
        .bind(unknown)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        for v in verdicts {
            sqlx::query(
                "INSERT INTO policy_verdicts (resolution_id, rule, verdict, reason)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(id)
            .bind(v.rule)
            .bind(v.verdict.as_str())
            .bind(&v.reason)
            .execute(&mut *tx)
            .await?;
        }
        ids.push(id);
    }
    tx.commit().await?;
    Ok(ids)
}

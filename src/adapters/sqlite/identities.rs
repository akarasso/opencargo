//! Ports 19, 20 and 21 over SQLite: identities and their provenance,
//! login handoffs, server secrets.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::{bind_ts, immediate, read_ts, store_error, Tx};
use crate::domain::identity::{Authority, IdentityKey, LinkState, LoginState, Outage};
use crate::domain::{Rights, User};
use crate::error::StoreError;
use crate::ports::handoffs::{Consumption, LoginHandoffStore, NewHandoff};
use crate::ports::identities::{Admission, DisabledBy, IdentityLink, IdentityStore};
use crate::ports::secrets::ServerSecretStore;
use crate::ports::users::NewUser;

const LINK_COLUMNS: &str =
    "provider, issuer, subject, user_id, email, provisioned, disabled, linked_at, last_login_at";

type LinkRow = (
    String,
    String,
    String,
    i64,
    Option<String>,
    bool,
    bool,
    String,
    String,
);

fn decode_link(row: LinkRow) -> Result<IdentityLink, StoreError> {
    let (provider, issuer, subject, user_id, email, provisioned, disabled, linked, last) = row;
    let ts = |column, v: &str| read_ts(&subject, column, v).map_err(super::corrupt_row);
    Ok(IdentityLink {
        linked_at: ts("linked_at", &linked)?,
        last_login_at: ts("last_login_at", &last)?,
        key: IdentityKey {
            authority: Authority { provider, issuer },
            subject: subject.clone(),
        },
        user_id,
        email,
        provisioned,
        disabled,
    })
}

type Step<T> = Result<Result<T, StoreError>, sqlx::Error>;

async fn revoke_identity(tx: &mut Tx, key: &IdentityKey) -> Result<u64, sqlx::Error> {
    let done = sqlx::query(
        "DELETE FROM api_tokens WHERE id IN (SELECT token_id FROM sso_token_provenance
         WHERE provider = ?1 AND issuer = ?2 AND subject = ?3)",
    )
    .bind(&key.authority.provider)
    .bind(&key.authority.issuer)
    .bind(&key.subject)
    .execute(&mut **tx)
    .await?;
    Ok(done.rows_affected())
}

async fn revoke_user(tx: &mut Tx, user_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM api_tokens WHERE id IN (SELECT p.token_id FROM sso_token_provenance p
         JOIN sso_identities i ON i.provider = p.provider AND i.issuer = p.issuer
         AND i.subject = p.subject WHERE i.user_id = ?1)",
    )
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn user_row(tx: &mut Tx, user_id: i64) -> Result<Option<User>, sqlx::Error> {
    super::users::by_id_in(tx, user_id).await
}

async fn apply_grants(
    tx: &mut Tx,
    user_id: i64,
    a: &Admission<'_>,
    now: DateTime<Utc>,
) -> Step<()> {
    for repo in a.managed {
        let granted = a.grants.iter().find(|(r, _)| r == repo).map(|(_, g)| *g);
        match granted {
            Some(Rights {
                read,
                write,
                delete,
                admin,
            }) => {
                sqlx::query(
                    "INSERT INTO user_permissions
                     (user_id, repository_id, can_read, can_write, can_delete, can_admin, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(user_id, repository_id) DO UPDATE SET can_read = excluded.can_read,
                     can_write = excluded.can_write, can_delete = excluded.can_delete,
                     can_admin = excluded.can_admin",
                )
                .bind(user_id)
                .bind(repo)
                .bind(read)
                .bind(write)
                .bind(delete)
                .bind(admin)
                .bind(bind_ts(now))
                .execute(&mut **tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "DELETE FROM user_permissions WHERE user_id = ?1 AND repository_id = ?2",
                )
                .bind(user_id)
                .bind(repo)
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    Ok(Ok(()))
}

async fn provision_in(
    tx: &mut Tx,
    user: &NewUser<'_>,
    a: &Admission<'_>,
    now: DateTime<Utc>,
) -> Step<User> {
    let created = match super::users::insert_in(tx, user, now).await? {
        Ok(created) => created,
        Err(refusal) => return Ok(Err(refusal)),
    };
    sqlx::query(
        "INSERT INTO sso_identities (provider, issuer, subject, user_id, email, provisioned,
         role_before_link, disabled, linked_at, last_login_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, NULL, 0, ?6, ?6)",
    )
    .bind(&a.key.authority.provider)
    .bind(&a.key.authority.issuer)
    .bind(&a.key.subject)
    .bind(created.id)
    .bind(a.email)
    .bind(bind_ts(now))
    .execute(&mut **tx)
    .await?;
    if let Err(e) = apply_grants(tx, created.id, a, now).await? {
        return Ok(Err(e));
    }
    Ok(Ok(created))
}

async fn admit_in(tx: &mut Tx, a: &Admission<'_>, now: DateTime<Utc>) -> Step<User> {
    let link: Option<(i64, bool, bool)> = sqlx::query_as(
        "SELECT user_id, provisioned, disabled FROM sso_identities
         WHERE provider = ?1 AND issuer = ?2 AND subject = ?3",
    )
    .bind(&a.key.authority.provider)
    .bind(&a.key.authority.issuer)
    .bind(&a.key.subject)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((user_id, provisioned, false)) = link else {
        return Ok(Err(StoreError::NotFound));
    };
    sqlx::query(
        "UPDATE sso_identities SET email = ?4, last_login_at = ?5
         WHERE provider = ?1 AND issuer = ?2 AND subject = ?3",
    )
    .bind(&a.key.authority.provider)
    .bind(&a.key.authority.issuer)
    .bind(&a.key.subject)
    .bind(a.email)
    .bind(bind_ts(now))
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM sso_user_states WHERE user_id = ?1 AND disabled_by = 'denied'")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    if provisioned {
        sqlx::query("UPDATE users SET role = ?2, updated_at = ?3 WHERE id = ?1")
            .bind(user_id)
            .bind(a.role)
            .bind(bind_ts(now))
            .execute(&mut **tx)
            .await?;
    }
    if let Err(e) = apply_grants(tx, user_id, a, now).await? {
        return Ok(Err(e));
    }
    match user_row(tx, user_id).await? {
        Some(user) => Ok(Ok(user)),
        None => Ok(Err(StoreError::NotFound)),
    }
}

async fn attach_in(
    tx: &mut Tx,
    user_id: i64,
    key: &IdentityKey,
    email: Option<&str>,
    now: DateTime<Utc>,
) -> Step<()> {
    let Some(user) = user_row(tx, user_id).await? else {
        return Ok(Err(StoreError::NotFound));
    };
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sso_identities WHERE provider = ?1 AND issuer = ?2 AND subject = ?3)",
    )
    .bind(&key.authority.provider)
    .bind(&key.authority.issuer)
    .bind(&key.subject)
    .fetch_one(&mut **tx)
    .await?;
    if taken {
        return Ok(Err(StoreError::Conflict));
    }
    sqlx::query(
        "INSERT INTO sso_identities (provider, issuer, subject, user_id, email, provisioned,
         role_before_link, disabled, linked_at, last_login_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, 0, ?7, ?7)",
    )
    .bind(&key.authority.provider)
    .bind(&key.authority.issuer)
    .bind(&key.subject)
    .bind(user_id)
    .bind(email)
    .bind(&user.role)
    .bind(bind_ts(now))
    .execute(&mut **tx)
    .await?;
    Ok(Ok(()))
}

async fn detach_in(tx: &mut Tx, user_id: i64, key: &IdentityKey) -> Step<()> {
    let before: Option<Option<String>> = sqlx::query_scalar(
        "SELECT role_before_link FROM sso_identities
         WHERE provider = ?1 AND issuer = ?2 AND subject = ?3 AND user_id = ?4",
    )
    .bind(&key.authority.provider)
    .bind(&key.authority.issuer)
    .bind(&key.subject)
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(before) = before else {
        return Ok(Err(StoreError::NotFound));
    };
    revoke_identity(tx, key).await?;
    sqlx::query("DELETE FROM sso_identities WHERE provider = ?1 AND issuer = ?2 AND subject = ?3")
        .bind(&key.authority.provider)
        .bind(&key.authority.issuer)
        .bind(&key.subject)
        .execute(&mut **tx)
        .await?;
    if let Some(role) = before {
        sqlx::query("UPDATE users SET role = ?2 WHERE id = ?1")
            .bind(user_id)
            .bind(role)
            .execute(&mut **tx)
            .await?;
    }
    Ok(Ok(()))
}

async fn disable_link_in(tx: &mut Tx, key: &IdentityKey) -> Step<()> {
    let done = sqlx::query(
        "UPDATE sso_identities SET disabled = 1 WHERE provider = ?1 AND issuer = ?2 AND subject = ?3",
    )
    .bind(&key.authority.provider)
    .bind(&key.authority.issuer)
    .bind(&key.subject)
    .execute(&mut **tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(Err(StoreError::NotFound));
    }
    revoke_identity(tx, key).await?;
    Ok(Ok(()))
}

async fn disable_user_in(
    tx: &mut Tx,
    user_id: i64,
    by: DisabledBy,
    now: DateTime<Utc>,
) -> Step<()> {
    if user_row(tx, user_id).await?.is_none() {
        return Ok(Err(StoreError::NotFound));
    }
    sqlx::query(
        "INSERT INTO sso_user_states (user_id, disabled_by, disabled_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(user_id) DO UPDATE SET disabled_by = CASE
         WHEN sso_user_states.disabled_by = 'admin' THEN 'admin' ELSE excluded.disabled_by END",
    )
    .bind(user_id)
    .bind(by.as_str())
    .bind(bind_ts(now))
    .execute(&mut **tx)
    .await?;
    revoke_user(tx, user_id).await?;
    Ok(Ok(()))
}

async fn deprovision_in(tx: &mut Tx, key: &IdentityKey, now: DateTime<Utc>) -> Step<()> {
    let user: Option<i64> = sqlx::query_scalar(
        "SELECT user_id FROM sso_identities WHERE provider = ?1 AND issuer = ?2 AND subject = ?3",
    )
    .bind(&key.authority.provider)
    .bind(&key.authority.issuer)
    .bind(&key.subject)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(user_id) = user else {
        return Ok(Err(StoreError::NotFound));
    };
    disable_user_in(tx, user_id, DisabledBy::Denied, now).await
}

async fn revoke_authority_in(tx: &mut Tx, a: &Authority) -> Step<u64> {
    let done = sqlx::query(
        "DELETE FROM api_tokens WHERE id IN (SELECT token_id FROM sso_token_provenance
         WHERE provider = ?1 AND issuer = ?2)",
    )
    .bind(&a.provider)
    .bind(&a.issuer)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE sso_identities SET disabled = 1 WHERE provider = ?1 AND issuer = ?2")
        .bind(&a.provider)
        .bind(&a.issuer)
        .execute(&mut **tx)
        .await?;
    Ok(Ok(done.rows_affected()))
}

async fn migrate_authority_in(tx: &mut Tx, from: &Authority, to: &Authority) -> Step<()> {
    for table in ["sso_identities", "sso_token_provenance", "sso_outages"] {
        sqlx::query(&format!(
            "UPDATE {table} SET provider = ?3, issuer = ?4 WHERE provider = ?1 AND issuer = ?2"
        ))
        .bind(&from.provider)
        .bind(&from.issuer)
        .bind(&to.provider)
        .bind(&to.issuer)
        .execute(&mut **tx)
        .await?;
    }
    Ok(Ok(()))
}

async fn record_probe_in(
    tx: &mut Tx,
    a: &Authority,
    reachable: bool,
    now: DateTime<Utc>,
) -> Step<()> {
    let open: Option<String> = sqlx::query_scalar(
        "SELECT started_at FROM sso_outages WHERE provider = ?1 AND issuer = ?2 AND ended_at IS NULL",
    )
    .bind(&a.provider)
    .bind(&a.issuer)
    .fetch_optional(&mut **tx)
    .await?;
    match (open, reachable) {
        (Some(started), true) => {
            sqlx::query(
                "UPDATE sso_outages SET ended_at = ?4 WHERE provider = ?1 AND issuer = ?2 AND started_at = ?3",
            )
            .bind(&a.provider)
            .bind(&a.issuer)
            .bind(started)
            .bind(bind_ts(now))
            .execute(&mut **tx)
            .await?;
        }
        (None, false) => {
            sqlx::query(
                "INSERT INTO sso_outages (provider, issuer, started_at, ended_at) VALUES (?1, ?2, ?3, NULL)",
            )
            .bind(&a.provider)
            .bind(&a.issuer)
            .bind(bind_ts(now))
            .execute(&mut **tx)
            .await?;
        }
        _ => {}
    }
    Ok(Ok(()))
}

pub struct SqliteIdentityStore {
    pool: SqlitePool,
}

impl SqliteIdentityStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn select_links(
        &self,
        sql: &str,
        bind: &[&str],
    ) -> Result<Vec<IdentityLink>, StoreError> {
        let mut query = sqlx::query_as::<_, LinkRow>(sql);
        for value in bind {
            query = query.bind(*value);
        }
        let rows = query.fetch_all(&self.pool).await.map_err(store_error)?;
        rows.into_iter().map(decode_link).collect()
    }
}

#[async_trait]
impl IdentityStore for SqliteIdentityStore {
    async fn find(&self, key: &IdentityKey) -> Result<Option<IdentityLink>, StoreError> {
        let sql = format!(
            "SELECT {LINK_COLUMNS} FROM sso_identities WHERE provider = ?1 AND issuer = ?2 AND subject = ?3"
        );
        let found = self
            .select_links(
                &sql,
                &[&key.authority.provider, &key.authority.issuer, &key.subject],
            )
            .await?;
        Ok(found.into_iter().next())
    }

    async fn of_user(&self, user_id: i64) -> Result<Vec<IdentityLink>, StoreError> {
        let sql = format!(
            "SELECT {LINK_COLUMNS} FROM sso_identities WHERE user_id = ?1 ORDER BY linked_at"
        );
        self.select_links(&sql, &[&user_id.to_string()]).await
    }

    async fn provision(
        &self,
        user: &NewUser<'_>,
        admission: &Admission<'_>,
        now: DateTime<Utc>,
    ) -> Result<User, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = provision_in(&mut tx, user, admission, now).await;
                (tx, done)
            })
        })
        .await
    }

    async fn admit(
        &self,
        admission: &Admission<'_>,
        now: DateTime<Utc>,
    ) -> Result<User, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = admit_in(&mut tx, admission, now).await;
                (tx, done)
            })
        })
        .await
    }

    async fn attach(
        &self,
        user_id: i64,
        key: &IdentityKey,
        email: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = attach_in(&mut tx, user_id, key, email, now).await;
                (tx, done)
            })
        })
        .await
    }

    async fn detach(&self, user_id: i64, key: &IdentityKey) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = detach_in(&mut tx, user_id, key).await;
                (tx, done)
            })
        })
        .await
    }

    async fn disable_link(&self, key: &IdentityKey) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = disable_link_in(&mut tx, key).await;
                (tx, done)
            })
        })
        .await
    }

    async fn disable_user(
        &self,
        user_id: i64,
        by: DisabledBy,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = disable_user_in(&mut tx, user_id, by, now).await;
                (tx, done)
            })
        })
        .await
    }

    async fn enable_user(&self, user_id: i64) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sso_user_states WHERE user_id = ?1")
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn deprovision(&self, key: &IdentityKey, now: DateTime<Utc>) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = deprovision_in(&mut tx, key, now).await;
                (tx, done)
            })
        })
        .await
    }

    async fn revoke_authority(&self, authority: &Authority) -> Result<u64, StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = revoke_authority_in(&mut tx, authority).await;
                (tx, done)
            })
        })
        .await
    }

    async fn migrate_authority(&self, from: &Authority, to: &Authority) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = migrate_authority_in(&mut tx, from, to).await;
                (tx, done)
            })
        })
        .await
    }

    async fn authorities(&self) -> Result<Vec<Authority>, StoreError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT provider, issuer FROM sso_identities WHERE disabled = 0
             ORDER BY provider, issuer",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|(provider, issuer)| Authority { provider, issuer })
            .collect())
    }

    async fn mark_provenance(&self, token_id: &str, key: &IdentityKey) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO sso_token_provenance (token_id, provider, issuer, subject) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(token_id)
        .bind(&key.authority.provider)
        .bind(&key.authority.issuer)
        .bind(&key.subject)
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn provenance(&self, token_id: &str) -> Result<Option<IdentityKey>, StoreError> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT provider, issuer, subject FROM sso_token_provenance WHERE token_id = ?1",
        )
        .bind(token_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.map(|(provider, issuer, subject)| IdentityKey {
            authority: Authority { provider, issuer },
            subject,
        }))
    }

    async fn login_state(&self, user_id: i64) -> Result<LoginState, StoreError> {
        let disabled: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sso_user_states WHERE user_id = ?1)")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await
                .map_err(store_error)?;
        let mut links = Vec::new();
        for link in self.of_user(user_id).await? {
            if link.disabled {
                continue;
            }
            links.push(LinkState {
                last_login: link.last_login_at,
                outages: self.outages(&link.key.authority).await?,
            });
        }
        Ok(LoginState {
            disabled,
            bootstrap: false,
            links,
        })
    }

    async fn record_probe(
        &self,
        authority: &Authority,
        reachable: bool,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        immediate(&self.pool, |mut tx| {
            Box::pin(async move {
                let done = record_probe_in(&mut tx, authority, reachable, now).await;
                (tx, done)
            })
        })
        .await
    }

    async fn outages(&self, authority: &Authority) -> Result<Vec<Outage>, StoreError> {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT started_at, ended_at FROM sso_outages WHERE provider = ?1 AND issuer = ?2
             ORDER BY started_at",
        )
        .bind(&authority.provider)
        .bind(&authority.issuer)
        .fetch_all(&self.pool)
        .await
        .map_err(store_error)?;
        let subject = &authority.provider;
        rows.into_iter()
            .map(|(start, end)| {
                let start = read_ts(subject, "started_at", &start).map_err(super::corrupt_row)?;
                let end = end
                    .map(|e| read_ts(subject, "ended_at", &e).map_err(super::corrupt_row))
                    .transpose()?;
                Ok(Outage { start, end })
            })
            .collect()
    }
}

pub struct SqliteHandoffStore {
    pool: SqlitePool,
}

impl SqliteHandoffStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl LoginHandoffStore for SqliteHandoffStore {
    async fn deposit(&self, handoff: &NewHandoff<'_>) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO login_handoffs (code_hash, binding, payload, expires_at, consumed_at)
             VALUES (?1, ?2, ?3, ?4, NULL)",
        )
        .bind(handoff.code_hash)
        .bind(handoff.binding)
        .bind(handoff.payload)
        .bind(bind_ts(handoff.expires_at))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn consume(
        &self,
        code_hash: &str,
        binding: &str,
        now: DateTime<Utc>,
    ) -> Result<Consumption, StoreError> {
        let won: Option<(String, String, String)> = sqlx::query_as(
            "UPDATE login_handoffs SET consumed_at = ?2 WHERE code_hash = ?1 AND consumed_at IS NULL
             RETURNING binding, payload, expires_at",
        )
        .bind(code_hash)
        .bind(bind_ts(now))
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        let Some((bound, payload, expires)) = won else {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM login_handoffs WHERE code_hash = ?1)",
            )
            .bind(code_hash)
            .fetch_one(&self.pool)
            .await
            .map_err(store_error)?;
            return Ok(if exists {
                Consumption::AlreadyConsumed
            } else {
                Consumption::Unknown
            });
        };
        let expires = read_ts(code_hash, "expires_at", &expires).map_err(super::corrupt_row)?;
        if expires < now {
            return Ok(Consumption::Expired);
        }
        if !constant_eq(bound.as_bytes(), binding.as_bytes()) {
            return Ok(Consumption::BindingMismatch);
        }
        Ok(Consumption::Consumed(payload))
    }

    async fn peek(
        &self,
        code_hash: &str,
        binding: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, StoreError> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT binding, payload, expires_at FROM login_handoffs
             WHERE code_hash = ?1 AND consumed_at IS NULL",
        )
        .bind(code_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        let Some((bound, payload, expires)) = row else {
            return Ok(None);
        };
        let expires = read_ts(code_hash, "expires_at", &expires).map_err(super::corrupt_row)?;
        Ok(
            (expires >= now && constant_eq(bound.as_bytes(), binding.as_bytes()))
                .then_some(payload),
        )
    }

    async fn purge_expired(&self, now: DateTime<Utc>) -> Result<u64, StoreError> {
        let done = sqlx::query("DELETE FROM login_handoffs WHERE expires_at < ?1")
            .bind(bind_ts(now))
            .execute(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(done.rows_affected())
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub struct SqliteSecretStore {
    pool: SqlitePool,
}

impl SqliteSecretStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ServerSecretStore for SqliteSecretStore {
    async fn get_or_init(&self, name: &str, candidate: &[u8]) -> Result<Vec<u8>, StoreError> {
        sqlx::query(
            "INSERT INTO server_secrets (name, value, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO NOTHING",
        )
        .bind(name)
        .bind(candidate)
        .bind(bind_ts(Utc::now()))
        .execute(&self.pool)
        .await
        .map_err(store_error)?;
        sqlx::query_scalar("SELECT value FROM server_secrets WHERE name = ?1")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(store_error)
    }
}

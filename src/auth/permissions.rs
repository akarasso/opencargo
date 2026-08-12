use sqlx::SqlitePool;

/// Returns `true` if the role has admin permissions.
pub fn can_admin(role: &str) -> bool {
    role == "admin"
}

/// Check whether a user has a specific permission on a repository.
///
/// Resolution order:
/// 1. Admin role always has full access.
/// 2. Check `user_permissions` table for a specific grant.
/// 3. Fall back to role-based defaults:
///    - publisher: read + write on all repos
///    - reader: read on all repos
pub async fn check_repo_permission(
    db: &SqlitePool,
    user_id: Option<i64>,
    user_role: &str,
    repo_id: i64,
    action: &str, // "read", "write", "delete", "admin"
) -> bool {
    // Admin role always has full access
    if user_role == "admin" {
        return true;
    }

    // Check user_permissions table for a specific grant
    if let Some(uid) = user_id {
        if let Ok(Some(perm)) = crate::db::get_user_permission(db, uid, repo_id).await {
            return match action {
                "read" => perm.can_read != 0,
                "write" => perm.can_write != 0,
                "delete" => perm.can_delete != 0,
                "admin" => perm.can_admin != 0,
                _ => false,
            };
        }
    }

    // Fallback to role-based defaults
    match user_role {
        "publisher" => action == "read" || action == "write",
        "reader" => action == "read",
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
//
// Note: repository *visibility* (public/private) is intentionally absent from
// this function — it only resolves explicit grants and role defaults. The
// public-repo allowances live at the call sites (middleware / handlers).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Row ids created by `test_pool` fixtures (AUTOINCREMENT starts at 1).
    const ALICE: i64 = 1; // role "reader" in the users table
    const BOB: i64 = 2; // role "publisher" in the users table
    const REPO_A: i64 = 1;
    const REPO_B: i64 = 2;

    async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::TempDir::new().expect("failed to create temp dir");
        let url = format!("sqlite:{}?mode=rwc", tmp.path().join("test.db").display());
        let pool = SqlitePool::connect(&url).await.expect("connect failed");
        crate::db::migrate(&pool).await.expect("migrate failed");
        sqlx::query(
            "INSERT INTO users (username, password_hash, role) VALUES
             ('alice', 'x', 'reader'), ('bob', 'x', 'publisher')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO repositories (name, repo_type, format) VALUES
             ('repo-a', 'hosted', 'npm'), ('repo-b', 'hosted', 'npm')",
        )
        .execute(&pool)
        .await
        .unwrap();
        (tmp, pool)
    }

    async fn grant(
        pool: &SqlitePool,
        user: i64,
        repo: i64,
        read: bool,
        write: bool,
        delete: bool,
        admin: bool,
    ) {
        sqlx::query(
            "INSERT INTO user_permissions
             (user_id, repository_id, can_read, can_write, can_delete, can_admin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(user)
        .bind(repo)
        .bind(i64::from(read))
        .bind(i64::from(write))
        .bind(i64::from(delete))
        .bind(i64::from(admin))
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn admin_role_short_circuits_everything() {
        let (_tmp, pool) = test_pool().await;
        for action in ["read", "write", "delete", "admin", "frobnicate"] {
            assert!(
                check_repo_permission(&pool, Some(ALICE), "admin", REPO_A, action).await,
                "admin must be allowed action {action:?}"
            );
        }
        // Even an explicit deny-all grant cannot restrict an admin: the role
        // check runs before the grant lookup.
        grant(&pool, ALICE, REPO_A, false, false, false, false).await;
        assert!(check_repo_permission(&pool, Some(ALICE), "admin", REPO_A, "delete").await);
        // An admin without a user id (static config token) is also unrestricted.
        assert!(check_repo_permission(&pool, None, "admin", REPO_A, "admin").await);
    }

    #[tokio::test]
    async fn explicit_grant_overrides_role_default_in_both_directions() {
        let (_tmp, pool) = test_pool().await;
        // alice is a reader; her grant on repo-a revokes read but adds write.
        grant(&pool, ALICE, REPO_A, false, true, false, false).await;
        assert!(
            !check_repo_permission(&pool, Some(ALICE), "reader", REPO_A, "read").await,
            "explicit can_read=0 must beat the reader role default"
        );
        assert!(
            check_repo_permission(&pool, Some(ALICE), "reader", REPO_A, "write").await,
            "explicit can_write=1 must beat the reader role default"
        );
        assert!(!check_repo_permission(&pool, Some(ALICE), "reader", REPO_A, "delete").await);
        assert!(!check_repo_permission(&pool, Some(ALICE), "reader", REPO_A, "admin").await);
    }

    #[tokio::test]
    async fn unknown_action_is_denied_with_and_without_grant() {
        let (_tmp, pool) = test_pool().await;
        // Grant row present with everything allowed: unknown action → false.
        grant(&pool, ALICE, REPO_A, true, true, true, true).await;
        assert!(!check_repo_permission(&pool, Some(ALICE), "reader", REPO_A, "frobnicate").await);
        // No grant row: unknown action is false through role defaults too.
        assert!(!check_repo_permission(&pool, Some(BOB), "publisher", REPO_A, "frobnicate").await);
    }

    #[tokio::test]
    async fn grant_is_scoped_to_its_repository() {
        let (_tmp, pool) = test_pool().await;
        grant(&pool, ALICE, REPO_A, false, false, false, false).await;
        // The deny-all on repo-a does not leak onto repo-b: role default applies.
        assert!(check_repo_permission(&pool, Some(ALICE), "reader", REPO_B, "read").await);
        assert!(!check_repo_permission(&pool, Some(ALICE), "reader", REPO_A, "read").await);
    }

    #[tokio::test]
    async fn role_defaults_apply_without_grant() {
        let (_tmp, pool) = test_pool().await;
        // publisher: read + write, nothing destructive.
        assert!(check_repo_permission(&pool, Some(BOB), "publisher", REPO_A, "read").await);
        assert!(check_repo_permission(&pool, Some(BOB), "publisher", REPO_A, "write").await);
        assert!(!check_repo_permission(&pool, Some(BOB), "publisher", REPO_A, "delete").await);
        assert!(!check_repo_permission(&pool, Some(BOB), "publisher", REPO_A, "admin").await);
        // reader: read only.
        assert!(check_repo_permission(&pool, Some(ALICE), "reader", REPO_A, "read").await);
        assert!(!check_repo_permission(&pool, Some(ALICE), "reader", REPO_A, "write").await);
        // Unknown or empty role: nothing at all.
        assert!(!check_repo_permission(&pool, Some(ALICE), "ghost", REPO_A, "read").await);
        assert!(!check_repo_permission(&pool, Some(ALICE), "", REPO_A, "read").await);
    }

    #[tokio::test]
    async fn anonymous_and_unknown_users_fall_back_to_role_defaults() {
        let (_tmp, pool) = test_pool().await;
        // No user id: the grant lookup is skipped entirely.
        assert!(check_repo_permission(&pool, None, "reader", REPO_A, "read").await);
        assert!(!check_repo_permission(&pool, None, "reader", REPO_A, "write").await);
        assert!(!check_repo_permission(&pool, None, "anonymous", REPO_A, "read").await);
        // A user id with no user_permissions row behaves the same way.
        assert!(check_repo_permission(&pool, Some(999), "reader", REPO_A, "read").await);
    }

    /// OBSERVATION (documented, deliberately not fixed by this test change):
    /// a DB error during the grant lookup is swallowed by `if let Ok(...)`,
    /// so the role default applies — an explicit can_read=0 revocation stops
    /// being enforced while the DB is failing. With a closed pool, alice's
    /// deny-all grant on repo-a becomes invisible and her reader role grants
    /// read again. If this test starts failing, the behavior was changed —
    /// update the assertion.
    #[tokio::test]
    async fn db_error_falls_back_to_role_defaults_ignoring_explicit_deny() {
        let (_tmp, pool) = test_pool().await;
        grant(&pool, ALICE, REPO_A, false, false, false, false).await;
        assert!(!check_repo_permission(&pool, Some(ALICE), "reader", REPO_A, "read").await);
        pool.close().await;
        assert!(
            check_repo_permission(&pool, Some(ALICE), "reader", REPO_A, "read").await,
            "documents current fail-open-to-role-default behavior on DB error"
        );
    }
}

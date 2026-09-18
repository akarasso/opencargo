use crate::domain::{allows, Rights};
use crate::error::{AppError, AppResult};
use crate::ports::permissions::PermissionStore;

/// Check whether a user has a specific permission on a repository.
///
/// The ladder itself — admin role, then an explicit grant, then the role
/// default — is [`crate::domain::allows`]; all that happens here is the one
/// lookup it needs, and what to do when that lookup cannot be made.
///
/// A store failure during the grant lookup is an error, not a fallback:
/// silently applying the role default would stop enforcing an explicit deny
/// (e.g. `can_read=0`) for as long as the store is failing. Every failure
/// collapses to a retryable 503, `Unavailable` or not — while the grant
/// cannot be read, "the store is unreliable" and "authorization is unsafe to
/// decide" are the same fact.
pub async fn check_repo_permission(
    perms: &dyn PermissionStore,
    user_id: Option<i64>,
    user_role: &str,
    repo_id: i64,
    action: &str, // "read", "write", "delete", "admin"
) -> AppResult<bool> {
    let grant = match user_id {
        Some(uid) => grant_of(perms, uid, repo_id).await?,
        None => None,
    };
    Ok(allows(user_role, grant, action))
}

async fn grant_of(
    perms: &dyn PermissionStore,
    user_id: i64,
    repo_id: i64,
) -> AppResult<Option<Rights>> {
    perms.rights(user_id, repo_id).await.map_err(|e| {
        tracing::warn!(error = %e, "store error during permission check");
        AppError::ServiceUnavailable("permission check temporarily unavailable, try again".to_string())
    })
}

// ---------------------------------------------------------------------------
// Tests
//
// Note: repository *visibility* (public/private) is intentionally absent from
// this function — it only resolves explicit grants and role defaults. The
// public-repo allowances live at the call sites (middleware / handlers).
//
// The ladder's own truth table is asserted in `crate::domain::permission`,
// where it is a pure function; what is left to assert here is the lookup and
// what happens when it fails.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Rights;
    use crate::error::StoreError;
    use crate::testing::fakes::{FakeDb, PortId};

    const ALICE: i64 = 1;
    const REPO_A: i64 = 1;
    const REPO_B: i64 = 2;

    async fn with_deny_all_on_repo_a() -> FakeDb {
        let db = FakeDb::new();
        db.perms()
            .set(ALICE, REPO_A, Rights::NONE, chrono::DateTime::UNIX_EPOCH)
            .await
            .unwrap();
        db
    }

    #[tokio::test]
    async fn an_explicit_grant_is_looked_up_for_the_repository_it_names() {
        let db = with_deny_all_on_repo_a().await;
        let perms = db.perms();

        assert!(
            !check_repo_permission(&*perms, Some(ALICE), "reader", REPO_A, "read")
                .await
                .unwrap(),
            "the deny-all grant must beat the reader default"
        );
        assert!(
            check_repo_permission(&*perms, Some(ALICE), "reader", REPO_B, "read")
                .await
                .unwrap(),
            "and must not leak onto another repository"
        );
    }

    #[tokio::test]
    async fn a_caller_with_no_id_never_reaches_the_store() {
        let db = with_deny_all_on_repo_a().await;
        db.fail_next(PortId::Permissions, StoreError::Other("no".into()));

        assert!(
            check_repo_permission(&*db.perms(), None, "reader", REPO_A, "read")
                .await
                .unwrap(),
            "the grant lookup is skipped entirely without a user id"
        );
    }

    #[tokio::test]
    async fn an_unknown_user_falls_back_to_the_role_default() {
        let db = with_deny_all_on_repo_a().await;
        assert!(
            check_repo_permission(&*db.perms(), Some(999), "reader", REPO_A, "read")
                .await
                .unwrap()
        );
    }

    /// A failing lookup is propagated instead of silently falling back to the
    /// role default: alice's explicit `can_read=0` stays enforced, as
    /// "temporarily unavailable", while the store is failing.
    ///
    /// The injection is `Other`, not `Unavailable`: an `Unavailable` one
    /// passes under a mapping that only special-cases `Unavailable` and would
    /// prove nothing about the blanket this path deliberately keeps.
    #[tokio::test]
    async fn a_store_failure_is_a_retryable_503_not_a_role_default() {
        let db = with_deny_all_on_repo_a().await;
        db.fail_next(PortId::Permissions, StoreError::Other("disk".into()));

        let refused = check_repo_permission(&*db.perms(), Some(ALICE), "reader", REPO_A, "read")
            .await
            .unwrap_err();
        assert!(
            matches!(refused, AppError::ServiceUnavailable(_)),
            "a store error must propagate as ServiceUnavailable, got {refused:?}"
        );
    }
}

use sqlx::SqlitePool;

/// Check whether the given role is allowed to perform the given action.
///
/// - `admin`: can do everything
/// - `publisher`: can read + write (publish)
/// - `reader`: can only read
pub fn check_permission(user_role: &str, action: &str) -> bool {
    match user_role {
        "admin" => true,
        "publisher" => matches!(action, "read" | "write" | "publish"),
        "reader" => action == "read",
        _ => false,
    }
}

/// Returns `true` if the role has write (publish) permissions.
pub fn can_write(role: &str) -> bool {
    matches!(role, "admin" | "publisher")
}

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

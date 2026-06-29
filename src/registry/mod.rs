pub mod cargo;
pub mod go;
pub mod npm;
pub mod oci;

use crate::auth::middleware::AuthUser;
use crate::auth::permissions::check_repo_permission;
use crate::db::Repository;
use crate::error::{AppError, AppResult};

/// Enforce read access on a repository before serving any of its content.
///
/// - **Public** repositories are readable by anyone. Anonymous access still
///   depends on the global `anonymous_read` gate, which the auth middleware
///   enforces before the request reaches the handler.
/// - **Private** repositories require an authenticated caller that holds the
///   `read` permission on the repo (admin role, a matching `user_permissions`
///   grant, or the reader/publisher role default).
///
/// This closes the gap where read handlers served private repositories to
/// anyone, because `check_repo_permission` was only ever called for writes.
pub async fn ensure_can_read(
    db: &sqlx::SqlitePool,
    repo: &Repository,
    auth_user: Option<&AuthUser>,
) -> AppResult<()> {
    if repo.visibility == "public" {
        return Ok(());
    }
    match auth_user {
        Some(user) => {
            if check_repo_permission(db, user.user_id, &user.role, repo.id, "read").await {
                Ok(())
            } else {
                Err(AppError::Forbidden(format!(
                    "read access denied on repository '{}'",
                    repo.name
                )))
            }
        }
        None => Err(AppError::Unauthorized(
            "authentication required to read this repository".to_string(),
        )),
    }
}

/// Enforce write (publish) access on a repository, with an actionable error
/// message when denied. The previous generic "insufficient permissions" did not
/// tell the caller that their role — typically the default `reader` — lacks
/// write, which made "I generated a token but can't publish" hard to diagnose.
pub async fn ensure_can_write(
    db: &sqlx::SqlitePool,
    repo: &Repository,
    auth_user: &AuthUser,
) -> AppResult<()> {
    if check_repo_permission(db, auth_user.user_id, &auth_user.role, repo.id, "write").await {
        Ok(())
    } else {
        Err(AppError::Forbidden(format!(
            "write access denied on repository '{}': your role is '{}'. Publishing requires \
             the 'publisher' or 'admin' role, or an explicit write permission on this \
             repository granted by an admin.",
            repo.name, auth_user.role
        )))
    }
}

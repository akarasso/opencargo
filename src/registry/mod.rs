pub mod cargo;
pub mod go;
pub mod npm;
pub mod oci;

use crate::auth::middleware::AuthUser;
use crate::auth::permissions::check_repo_permission;
use crate::db::Repository;
use crate::error::{AppError, AppResult};
use crate::server::AppState;

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

/// Ensure the repository's declared format matches the protocol being used.
/// Without this guard a payload of one format could be published into a repo of
/// another (e.g. an npm tarball into a `cargo` repo), silently corrupting it
/// since the underlying tables are shared.
pub fn ensure_format(repo: &Repository, expected: &str) -> AppResult<()> {
    if repo.format == expected {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "repository '{}' is a '{}' repository, not '{}'",
            repo.name, repo.format, expected
        )))
    }
}

/// Shared post-publish side effects, factored out of the per-format publish
/// handlers (npm, cargo) where they were duplicated verbatim: fire the
/// `package.published` webhook, then run the vulnerability scan. With
/// `block_on_critical`, a critical finding aborts the publish (returns Err);
/// otherwise the scan runs in the background.
#[allow(clippy::too_many_arguments)]
pub async fn finalize_publish(
    state: &AppState,
    ecosystem: &str,
    repo_name: &str,
    package_name: &str,
    version_str: &str,
    version_id: i64,
    metadata_json: &str,
    published_by: &str,
) -> AppResult<()> {
    state
        .webhook_dispatcher
        .dispatch(
            "package.published",
            &serde_json::json!({
                "package": package_name,
                "version": version_str,
                "repository": repo_name,
                "published_by": published_by,
            }),
        )
        .await;

    if state.vuln_scan_config.block_on_critical {
        let scan_result = state
            .vuln_scanner
            .scan_version(&state.db, version_id, metadata_json, ecosystem)
            .await;
        if let Ok(ref result) = scan_result {
            if result.status == "critical" {
                return Err(AppError::BadRequest(
                    "publish blocked: critical vulnerabilities found in dependencies".to_string(),
                ));
            }
        }
    } else {
        let scanner = state.vuln_scanner.clone();
        let db = state.db.clone();
        let meta_json = metadata_json.to_string();
        let eco = ecosystem.to_string();
        tokio::spawn(async move {
            if let Err(e) = scanner.scan_version(&db, version_id, &meta_json, &eco).await {
                tracing::warn!(error = %e, "Background vulnerability scan failed");
            }
        });
    }
    Ok(())
}

pub mod audit;
pub mod dashboard;
pub mod deps;
pub mod me;
pub mod permissions;
pub mod promote;
pub mod repositories;
pub mod tokens;
pub mod users;
pub mod vulns;
pub mod webhooks;
pub mod ws;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};

/// Extract the authenticated user injected by the auth middleware, or 401.
/// Shared by the API handler modules (previously copied into each one).
pub(crate) fn require_auth(
    request: &axum::http::Request<axum::body::Body>,
) -> AppResult<AuthUser> {
    request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))
}

/// Require the caller to hold the admin role, or 403.
pub(crate) fn require_admin(caller: &AuthUser) -> AppResult<()> {
    if caller.role != "admin" {
        return Err(AppError::Forbidden("admin access required".to_string()));
    }
    Ok(())
}

/// Require the caller to be admin, or to be acting on their own account, or 403.
pub(crate) fn require_admin_or_self(caller: &AuthUser, target_username: &str) -> AppResult<()> {
    if caller.role != "admin" && caller.username != target_username {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }
    Ok(())
}

/// Best-effort audit-log write for a sensitive mutation. A logging failure must
/// never fail the underlying operation, so the error is swallowed with a warning
/// rather than propagated.
///
/// Every audited mutation is also broadcast on the real-time event bus as an
/// admin-visibility `audit.entry` event, so admin UIs (audit log, users,
/// repositories, webhooks) update live without polling.
pub(crate) async fn record_audit(
    state: &crate::server::AppState,
    caller: &AuthUser,
    action: &str,
    target: Option<&str>,
) {
    if let Err(e) = crate::db::create_audit_entry(
        &state.db,
        caller.user_id,
        Some(caller.username.as_str()),
        action,
        target,
        None,
        None,
        None,
        None,
    )
    .await
    {
        tracing::warn!(error = %e, action, "failed to write audit log entry");
    }

    state.events.emit(
        "audit.entry",
        crate::events::Visibility::Admin,
        serde_json::json!({
            "username": caller.username,
            "action": action,
            "target": target,
        }),
    );
}

pub mod audit;
pub mod auth_sso;
pub mod dashboard;
pub mod deps;
pub mod maven;
pub mod mcp;
pub mod mcp_admin;
pub mod me;
pub mod permissions;
pub mod policy;
pub mod promote;
pub mod repositories;
pub mod storage;
pub mod system;
pub mod tokens;
pub mod users;
pub mod vulns;
pub mod webhooks;
pub mod ws;

use crate::app::audit::Actor;
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

/// The caller's standing once their credential has had its say: a scoped
/// credential is never an administrator, whatever role stands behind it.
/// Every reader of the role in this layer asks this instead.
pub(crate) fn admin_standing(caller: &AuthUser) -> bool {
    caller.role == "admin" && caller.scope.is_inherit()
}

/// Require the caller to hold the admin role, or 403.
pub(crate) fn require_admin(caller: &AuthUser) -> AppResult<()> {
    scope_free(caller)?;
    if caller.role != "admin" {
        return Err(AppError::Forbidden("admin access required".to_string()));
    }
    Ok(())
}

/// Require the caller to be admin, or to be acting on their own account, or 403.
pub(crate) fn require_admin_or_self(caller: &AuthUser, target_username: &str) -> AppResult<()> {
    scope_free(caller)?;
    if caller.role != "admin" && caller.username != target_username {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }
    Ok(())
}

/// The administrative surface asks for a credential that is not narrowed.
/// The rule is about the credential and not about the route, so it holds for
/// a route added tomorrow, and it holds on the caller's own account: the
/// branch that acts on oneself is where a restricted token would otherwise
/// mint itself an unrestricted one.
fn scope_free(caller: &AuthUser) -> AppResult<()> {
    if caller.scope.is_inherit() {
        return Ok(());
    }
    Err(AppError::InsufficientScope(
        "a scoped credential does not reach the administrative API".to_string(),
    ))
}

/// The caller as a use case knows them: an identity and what they may do,
/// with nothing of the request left on it.
pub(crate) fn actor(caller: &AuthUser) -> Actor<'_> {
    Actor {
        user_id: caller.user_id,
        username: &caller.username,
        admin: admin_standing(caller),
        scoped: !caller.scope.is_inherit(),
    }
}

/// Best-effort audit-log write for a sensitive mutation, for the handlers
/// whose action is not a use case of its own; the rest record through the
/// same function from inside theirs.
pub(crate) async fn record_audit(
    state: &crate::server::AppState,
    caller: &AuthUser,
    action: &str,
    target: Option<&str>,
) {
    crate::app::audit::record(
        &*state.audit,
        &*state.events,
        &actor(caller),
        action,
        target,
        chrono::Utc::now(),
    )
    .await;
}

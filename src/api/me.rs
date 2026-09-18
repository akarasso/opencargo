//! Current-user introspection endpoints.
//!
//! `GET /api/v1/me/permissions` returns the caller's *effective* rights on
//! every repository they can see, with the rule that produced each right:
//!
//! - `admin` — the admin role grants everything;
//! - `grant` — an explicit `user_permissions` row for this user × repo
//!   (overrides the role default entirely);
//! - `role`  — the role fallback (publisher ⇒ read+write, reader ⇒ read);
//! - `anonymous` — the global `anonymous_read` gate on public repos.
//!
//! This is what lets the UI show users what they can actually do (and why),
//! instead of letting them discover a 403 the hard way.

use axum::{extract::State, response::IntoResponse, Json};
use serde_json::json;

use crate::auth::middleware::AuthUser;
use crate::domain::{effective_rights, Visibility};
use crate::error::AppResult;
use crate::server::AppState;

pub async fn my_permissions(
    State(state): State<AppState>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let repos = state.repos.all().await?;

    let Some(axum::Extension(user)) = auth else {
        // Anonymous caller (only reachable when anonymous_read is on).
        let permissions: Vec<serde_json::Value> = repos
            .iter()
            .filter(|r| r.visibility == Visibility::Public)
            .map(|r| {
                json!({
                    "repository": r.name,
                    "type": r.repo_type,
                    "format": r.format,
                    "visibility": r.visibility,
                    "can_read": true,
                    "can_write": false,
                    "can_delete": false,
                    "can_admin": false,
                    "source": "anonymous",
                })
            })
            .collect();
        return Ok(Json(json!({
            "username": "anonymous",
            "role": "anonymous",
            "permissions": permissions,
        })));
    };

    let mut permissions = Vec::with_capacity(repos.len());
    for repo in &repos {
        let grant = match user.user_id {
            Some(uid) => state.permissions.rights(uid, repo.id).await?,
            None => None,
        };
        let (rights, source) = effective_rights(&user.role, grant);

        // Skip repos the caller cannot even read: their existence stays hidden
        // unless the repo is public.
        if !rights.read && repo.visibility != Visibility::Public {
            continue;
        }

        permissions.push(json!({
            "repository": repo.name,
            "type": repo.repo_type,
            "format": repo.format,
            "visibility": repo.visibility,
            "can_read": rights.read,
            "can_write": rights.write,
            "can_delete": rights.delete,
            "can_admin": rights.admin,
            "source": source.as_str(),
        }));
    }

    Ok(Json(json!({
        "username": user.username,
        "role": user.role,
        "permissions": permissions,
    })))
}

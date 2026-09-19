pub mod cargo;
pub mod go;
pub mod maven;
pub mod npm;
pub mod archive;
pub mod nuget;
pub mod oci;
pub mod pypi;
pub mod resolve;
pub mod routing;
pub mod rules;

use std::collections::HashMap;

use crate::auth::middleware::AuthUser;
use crate::auth::permissions::check_repo_permission;
use crate::domain::{Format, RepoKind, Repository, UrlRepo, Visibility};
use crate::error::{AppError, AppResult};
use crate::ports::permissions::PermissionStore;
use crate::ports::repositories::RepositoryStore;
use crate::registry::resolve::Cx;
use crate::server::AppState;

/// Extract the full package name from path parameters.
/// Scoped: scope="acme", name="httpclient" -> "@acme/httpclient".
/// Unscoped: name="react" -> "react".
/// Single source of truth for the copy that previously lived in the npm
/// handler and in the promote/deps/vulns API modules.
pub fn extract_package_name(params: &HashMap<String, String>) -> String {
    match params.get("scope") {
        Some(scope) => format!("@{}/{}", scope, params.get("name").unwrap_or(&String::new())),
        None => params.get("name").cloned().unwrap_or_default(),
    }
}

/// The request's resolver context: the one place the composition root's
/// state is projected onto the ports a leaf may reach, so no leaf and no
/// format module ever holds the whole of it.
pub fn cx<'a>(state: &'a AppState, auth: Option<&'a AuthUser>, repo: &'a Repository) -> Cx<'a> {
    Cx {
        repos: state.repos.as_ref(),
        perms: state.permissions.as_ref(),
        packages: state.packages.as_ref(),
        oci: state.oci.as_ref(),
        maven: state.maven.as_ref(),
        search: state.search.as_ref(),
        nuget: state.nuget_feed.as_ref(),
        proxy: &state.proxy,
        policy: &state.policy,
        creds: state.upstream_auth.as_ref(),
        auth,
        routing: state.routing.as_ref(),
        refusals: state.refusals.as_ref(),
        url: UrlRepo(&repo.name),
        base_url: &state.base_url,
    }
}

/// Load a repository by name; a missing one is a 404 naming it.
pub async fn load_repo(repos: &dyn RepositoryStore, name: &str) -> AppResult<Repository> {
    repos
        .by_name(name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {name}")))
}

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
    perms: &dyn PermissionStore,
    repo: &Repository,
    auth_user: Option<&AuthUser>,
) -> AppResult<()> {
    if repo.visibility == Visibility::Public {
        return Ok(());
    }
    match auth_user {
        Some(user) => {
            if check_repo_permission(perms, user.user_id, &user.role, repo.id, "read").await? {
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
    perms: &dyn PermissionStore,
    repo: &Repository,
    auth_user: &AuthUser,
) -> AppResult<()> {
    if check_repo_permission(perms, auth_user.user_id, &auth_user.role, repo.id, "write").await? {
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
pub fn ensure_format(repo: &Repository, expected: Format) -> AppResult<()> {
    let format = repo.fmt()?;
    if format == expected {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "repository '{}' is a '{}' repository, not '{}'",
            repo.name,
            format.as_str(),
            expected.as_str()
        )))
    }
}

/// Ensure the repository is a `hosted` one before accepting a publish/push.
/// Factored out of the per-format publish handlers where the check was
/// duplicated verbatim.
pub fn ensure_hosted(repo: &Repository) -> AppResult<()> {
    if repo.kind()? == RepoKind::Hosted {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "can only publish to hosted repositories".to_string(),
        ))
    }
}

